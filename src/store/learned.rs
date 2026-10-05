//! 从上游学到的规则：模型准入（套餐不含）与 400 形态规则。

use super::*;

/// 一条「这个号用不了这个模型」的记录，见 [`CredentialStore::deny_model`]。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelDenial {
    /// 归一化后的模型键，见 [`model_denial_key`]。
    pub model: String,
    /// 上游给出的依据（限流头摘要），供控制台展示。
    pub reason: String,
    /// 学到这条记录的时刻（Unix 秒）。
    pub learned_at: i64,
    /// 到点自动失效重新试探的时刻（Unix 秒）；`None` 表示一直有效，直到被显式清掉。
    pub expires_at: Option<i64>,
}

/// 从上游响应里学到的一条规则，落库供重启后回填进程内记忆，见
/// [`CredentialStore::remember_rejections`]。三类共用一张表：
/// - `kind = "shape"`：某模型不接受某字段的某取值（`effort: 'xhigh'`），命中本地拒；
/// - `kind = "deprecated"`：某模型已废弃某字段（`temperature`），命中转发前剥掉，`value` 为空串；
/// - `kind = "empty_reply"`：某模型对「无 tools 的单条消息 + 这个 `max_tokens`」回过 200 却零
///   输出（`field = "max_tokens"`，`value` 是那个数），同类请求命中本地拒，`message` 是当时
///   截下的上游回复开头，见 `crate::proxy::known_empty_reply`；
/// - `kind = "refusal"`：上游分类器拒答过某条提示词（`field = "prompt_sha"`，`value` 是提示词
///   哈希），逐字相同的重发命中时**原样回放上游那次的响应**（[`Self::reply`]：200 + 同一段体），
///   `message` 是「[类别] stop_details=…」的判决文案，控制台与日志看，见
///   `crate::proxy::known_refused_prompt`；
/// - `kind = "app_refusal"`：上游分类器拒答过某个**识别不了会话的应用**（`field = "system_sha"`，
///   `value` 是来访 system 的哈希），同一模型 + 同一份 system 的请求命中时同样回放；拒答至少
///   3 条且占该应用请求数三成以上才学，见 `crate::proxy::record_app_request`。
#[derive(Debug, Clone, PartialEq)]
pub struct LearnedRejection {
    pub kind: String,
    pub model: String,
    pub field: String,
    pub value: String,
    /// 上游原话，日志与控制台列表用。
    pub message: String,
    /// 上游那次的**完整响应体**，命中时原样回放；只有拒答那类有，其余为 `None`。
    pub reply: Option<LearnedReply>,
}

/// 学到规则时上游那次回复的原样响应体（拒答那类专用），见 [`LearnedRejection::reply`]。
///
/// 存的是**上游发来的字节**：来访要流式时上游回的是 SSE（`sse = true`），要非流式时是整段
/// JSON；回放时按来访这次要的形态给——形态一致原样发，不一致才在两种形态间转换。状态码不存：
/// 拒答恒是 200（`stop_reason: "refusal"` 裹在正常 Message 里），非 200 的响应根本学不进来。
#[derive(Debug, Clone, PartialEq)]
pub struct LearnedReply {
    /// 体是 SSE 事件流（`text/event-stream`）还是整段 JSON。
    pub sse: bool,
    /// 响应体原文（UTF-8）。
    pub body: String,
}

/// 学到的规则落库后最多活多久（秒）：7 天。
///
/// 这是持久化这类推断的唯一安全阀。它们是从一条报错里学来的，上游哪天放开了某个取值或恢复了
/// 某个参数，本地没有任何信号能知道——不设期限就是「永久拒掉一个其实已经支持的取值」。
/// 7 天后丢掉重学，代价是每周每种组合白撞一次 400。
pub const LEARNED_REJECTION_TTL_SECS: i64 = 7 * 24 * 3600;

/// 把模型名归一成「套餐门禁」的粒度：小写、去掉 `[1m]` 上下文后缀与 `-YYYYMMDD` 日期后缀。
///
/// 上游按套餐放不放行看的是模型本身，`claude-fable-5-1[1m]` 与 `claude-fable-5-1` 不会一个
/// 放一个拒；分开记只会让每个变体各白撞一次。但**不**把 `fable-5` 与 `fable-5-1` 并成一族：
/// 两代的准入未必同步，宁可多撞一次也别猜。
pub fn model_denial_key(model: &str) -> String {
    let mut m = model.trim().to_ascii_lowercase();
    if let Some(stripped) = m.strip_suffix("[1m]") {
        m = stripped.to_string();
    }
    // 形如 `-20251114` 的日期后缀：最后一段全是数字且恰好 8 位。
    if let Some((head, tail)) = m.rsplit_once('-')
        && tail.len() == 8
        && tail.chars().all(|c| c.is_ascii_digit())
    {
        m = head.to_string();
    }
    m
}

/// 该模型是否属于「只有高档套餐才含」的那一族（fable / mythos）。
///
/// **只用于选号排序**（见 [`CredentialStore::select_for_device`] 的 `plan_rank`），不是准入
/// 判据：真正的「能不能用」由上游回答并记进 [`ModelDenial`]。这里猜错的代价仅是多一次换号。
pub fn premium_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("fable") || m.contains("mythos")
}

impl CredentialStore {
    // ---------- 模型准入（套餐不含某模型） ----------

    /// 记下「这个号用不了这个模型」。
    ///
    /// 依据是上游的 429 形态：一个额度窗口头都不带、只带 `overage-disabled-reason`——说明
    /// 这个模型根本不在该套餐的任何额度窗口里，要走按量计费的 usage credits，而组织又没开。
    /// 这与「超额池满」（`7d_oi` rejected，账号确实有 fable 额度只是用完了）是两回事，后者走
    /// 进程内冷却，见 [`RateLimitCooldown`]。
    ///
    /// `expires_at` 取上游给的 `unified-reset`（credits 的月度窗口）：到点让它自动失效、
    /// 下一条请求再去试一次——用户中途开了 extra usage 的话就此自愈，代价是每月每号白撞一发。
    /// 上游没给时间就一直有效。三条显式解除的路：该号对该模型连通性测试通过、等级刷新后变了
    /// （见 [`Self::set_tier`]）、控制台手动解除。
    pub fn deny_model(
        &self,
        cred_id: i64,
        model: &str,
        reason: &str,
        expires_at: Option<i64>,
    ) -> Result<()> {
        let key = model_denial_key(model);
        let reason: String = reason.chars().take(300).collect();
        self.conn.lock().execute(
            "INSERT INTO model_denials (cred_id, model, reason, learned_at, expires_at)              VALUES (?1, ?2, ?3, unixepoch(), ?4)              ON CONFLICT(cred_id, model) DO UPDATE SET                 reason = excluded.reason, learned_at = excluded.learned_at,                 expires_at = excluded.expires_at",
            params![cred_id, key, reason, expires_at],
        )?;
        Ok(())
    }

    /// 解除模型准入记录：`Some(model)` 只清那一个模型（连通性测试通过），`None` 清该号全部
    /// （控制台手动解除）。返回清掉的条数。
    pub fn clear_model_denials(&self, cred_id: i64, model: Option<&str>) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(match model {
            Some(m) => conn.execute(
                "DELETE FROM model_denials WHERE cred_id = ?1 AND model = ?2",
                params![cred_id, model_denial_key(m)],
            )?,
            None => conn.execute("DELETE FROM model_denials WHERE cred_id = ?1", [cred_id])?,
        })
    }

    /// 该号当前仍有效的模型准入记录（到期的顺手删掉），按学到的时间倒序。
    pub fn denied_models(&self, cred_id: i64) -> Result<Vec<ModelDenial>> {
        let conn = self.conn.lock();
        Self::purge_expired_denials(&conn)?;
        let mut stmt = conn.prepare(
            "SELECT model, reason, learned_at, expires_at FROM model_denials              WHERE cred_id = ?1 ORDER BY learned_at DESC, model ASC",
        )?;
        let rows = stmt.query_map([cred_id], Self::row_to_denial)?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// 全部号的有效准入记录，按号分组（列表页一次取齐，免得逐号查）。
    pub fn all_model_denials(&self) -> Result<HashMap<i64, Vec<ModelDenial>>> {
        let conn = self.conn.lock();
        Self::purge_expired_denials(&conn)?;
        let mut stmt = conn.prepare(
            "SELECT cred_id, model, reason, learned_at, expires_at FROM model_denials              ORDER BY learned_at DESC, model ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                ModelDenial {
                    model: r.get(1)?,
                    reason: r.get(2)?,
                    learned_at: r.get(3)?,
                    expires_at: r.get(4)?,
                },
            ))
        })?;
        let mut out: HashMap<i64, Vec<ModelDenial>> = HashMap::new();
        for row in rows {
            let (cid, d) = row?;
            out.entry(cid).or_default().push(d);
        }
        Ok(out)
    }

    fn row_to_denial(r: &Row) -> rusqlite::Result<ModelDenial> {
        Ok(ModelDenial {
            model: r.get(0)?,
            reason: r.get(1)?,
            learned_at: r.get(2)?,
            expires_at: r.get(3)?,
        })
    }

    fn purge_expired_denials(conn: &Connection) -> Result<()> {
        conn.execute(
            "DELETE FROM model_denials WHERE expires_at IS NOT NULL AND expires_at <= unixepoch()",
            [],
        )?;
        Ok(())
    }

    // ---------- 从上游 400 学到的规则 ----------

    /// 落库一批刚学到的规则（已存在的组合原样保留，不刷新时间——保鲜期从第一次学到算）。
    pub fn remember_rejections(&self, rows: &[LearnedRejection]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "INSERT OR IGNORE INTO learned_rejections (kind, model, field, value, message, reply_sse, reply_body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for r in rows {
            let message: String = r.message.chars().take(500).collect();
            // 回放体不截：拒答的体本来就只有几百字节到几 KB，学的那头已按上限把过大的挡掉了
            // （`proxy::UsageSniffer::refusal_reply`），截一刀等于回放一段残缺的 SSE。
            let (reply_sse, reply_body) = match &r.reply {
                Some(reply) => (reply.sse as i64, reply.body.as_str()),
                None => (0, ""),
            };
            stmt.execute(params![
                r.kind, r.model, r.field, r.value, message, reply_sse, reply_body
            ])?;
        }
        Ok(())
    }

    /// 读出仍在保鲜期内的全部规则（过期的顺手删掉），启动时回填进程内记忆用。
    pub fn learned_rejections(&self) -> Result<Vec<LearnedRejection>> {
        Ok(self.learned_rejections_with_time()?.into_iter().map(|(r, _)| r).collect())
    }

    /// 同 [`Self::learned_rejections`]，附每条学到的时刻（Unix 秒），控制台列表用。
    pub fn learned_rejections_with_time(&self) -> Result<Vec<(LearnedRejection, i64)>> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM learned_rejections WHERE learned_at <= unixepoch() - ?1",
            [LEARNED_REJECTION_TTL_SECS],
        )?;
        let mut stmt = conn.prepare(
            "SELECT kind, model, field, value, message, learned_at, reply_sse, reply_body \
             FROM learned_rejections ORDER BY learned_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            // 空体即「没有」：这两列是 0.3.98 补的，之前学的行读出来就是默认值。
            let reply_sse: i64 = r.get(6)?;
            let reply_body: String = r.get(7)?;
            let reply = (!reply_body.is_empty())
                .then_some(LearnedReply { sse: reply_sse != 0, body: reply_body });
            Ok((
                LearnedRejection {
                    kind: r.get(0)?,
                    model: r.get(1)?,
                    field: r.get(2)?,
                    value: r.get(3)?,
                    message: r.get(4)?,
                    reply,
                },
                r.get::<_, i64>(5)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// 清空全部学到的规则（控制台逃生口：上游放开了某个取值、本地却还在拒）。返回删掉的条数。
    pub fn clear_learned_rejections(&self) -> Result<usize> {
        Ok(self.conn.lock().execute("DELETE FROM learned_rejections", [])?)
    }

    /// 只清某一种类（`kind` 列）的规则，返回删掉的条数。控制台「清空这一类」用：几百条拒答
    /// 提示词淹没列表时，不必连 `deprecated` 那几条有用的一起清掉。
    /// 拒答规则不设条数上限（进程内与库里都是），只靠 7 天保鲜期与这里的手动清理收口。
    pub fn clear_learned_rejections_of_kind(&self, kind: &str) -> Result<usize> {
        Ok(self.conn.lock().execute("DELETE FROM learned_rejections WHERE kind = ?1", [kind])?)
    }

    /// 删掉一条学到的规则（按主键四元组），返回是否确有其行。
    pub fn forget_learned_rejection(&self, r: &LearnedRejection) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "DELETE FROM learned_rejections WHERE kind = ?1 AND model = ?2 AND field = ?3 AND value = ?4",
            params![r.kind, r.model, r.field, r.value],
        )? > 0)
    }
}
