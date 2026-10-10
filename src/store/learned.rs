//! 从上游学到的规则：模型准入（套餐不含）与 400 形态规则。

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

use std::collections::HashMap;

use super::{CredentialStore, nul_free};
use anyhow::Result;
use sqlx::PgConnection;

impl CredentialStore {
    // ---------- 模型准入（套餐不含某模型） ----------

    /// 记下「这个号用不了这个模型」。
    ///
    /// 依据是上游的 429 形态：一个额度窗口头都不带、只带 `overage-disabled-reason`——说明
    /// 这个模型根本不在该套餐的任何额度窗口里，要走按量计费的 usage credits，而组织又没开。
    /// 这与「超额池满」（`7d_oi` rejected，账号确实有 fable 额度只是用完了）是两回事，后者走
    /// 进程内冷却，见 [`super::RateLimitCooldown`]。
    ///
    /// `expires_at` 取上游给的 `unified-reset`（credits 的月度窗口）：到点让它自动失效、
    /// 下一条请求再去试一次——用户中途开了 extra usage 的话就此自愈，代价是每月每号白撞一发。
    /// 上游没给时间就一直有效。三条显式解除的路：该号对该模型连通性测试通过、等级刷新后变了
    /// （见 `set_tier`）、控制台手动解除。
    pub async fn deny_model(
        &self,
        cred_id: i64,
        model: &str,
        reason: &str,
        expires_at: Option<i64>,
    ) -> Result<()> {
        // 模型名取自来访体、原因是上游回显，都可能带 NUL，见 [`nul_free`]。
        let key = model_denial_key(&nul_free(model));
        let reason: String = nul_free(reason).chars().take(300).collect();
        sqlx::query(
            "INSERT INTO model_denials (cred_id, model, reason, learned_at, expires_at) \
             VALUES ($1, $2, $3, unixepoch(), $4) \
             ON CONFLICT (cred_id, model) DO UPDATE SET \
                reason = EXCLUDED.reason, learned_at = EXCLUDED.learned_at, \
                expires_at = EXCLUDED.expires_at",
        )
        .bind(cred_id)
        .bind(key)
        .bind(reason)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 解除模型准入记录：`Some(model)` 只清那一个模型（连通性测试通过），`None` 清该号全部
    /// （控制台手动解除）。返回清掉的条数。
    pub async fn clear_model_denials(&self, cred_id: i64, model: Option<&str>) -> Result<usize> {
        let done = match model {
            Some(m) => {
                sqlx::query("DELETE FROM model_denials WHERE cred_id = $1 AND model = $2")
                    .bind(cred_id)
                    .bind(model_denial_key(m))
                    .execute(&self.pool)
                    .await?
            }
            None => {
                sqlx::query("DELETE FROM model_denials WHERE cred_id = $1")
                    .bind(cred_id)
                    .execute(&self.pool)
                    .await?
            }
        };
        Ok(done.rows_affected() as usize)
    }

    /// 该号当前仍有效的模型准入记录（到期的顺手删掉），按学到的时间倒序。
    pub async fn denied_models(&self, cred_id: i64) -> Result<Vec<ModelDenial>> {
        let mut conn = self.pool.acquire().await?;
        purge_expired_denials(&mut conn).await?;
        let rows: Vec<(String, String, i64, Option<i64>)> = sqlx::query_as(
            "SELECT model, reason, learned_at, expires_at FROM model_denials \
             WHERE cred_id = $1 ORDER BY learned_at DESC, model ASC",
        )
        .bind(cred_id)
        .fetch_all(&mut *conn)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(model, reason, learned_at, expires_at)| ModelDenial {
                model,
                reason,
                learned_at,
                expires_at,
            })
            .collect())
    }

    /// 全部号的有效准入记录，按号分组（列表页一次取齐，免得逐号查）。
    pub async fn all_model_denials(&self) -> Result<HashMap<i64, Vec<ModelDenial>>> {
        let mut conn = self.pool.acquire().await?;
        purge_expired_denials(&mut conn).await?;
        let rows: Vec<(i64, String, String, i64, Option<i64>)> = sqlx::query_as(
            "SELECT cred_id, model, reason, learned_at, expires_at FROM model_denials \
             ORDER BY learned_at DESC, model ASC",
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut out: HashMap<i64, Vec<ModelDenial>> = HashMap::new();
        for (cid, model, reason, learned_at, expires_at) in rows {
            out.entry(cid).or_default().push(ModelDenial { model, reason, learned_at, expires_at });
        }
        Ok(out)
    }

    // ---------- 从上游 400 学到的规则 ----------

    /// 落库一批刚学到的规则（已存在的组合原样保留，不刷新时间——保鲜期从第一次学到算）。
    ///
    /// 逐条插入、包在一个事务里：批次本来就小（一次响应学到的几条），没必要拼多行 VALUES。
    pub async fn remember_rejections(&self, rows: &[LearnedRejection]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for r in rows {
            // 规则的各段取自来访体与上游报错，可能带 NUL，见 [`nul_free`]。
            let message: String = nul_free(&r.message).chars().take(500).collect();
            // 回放体不截：拒答的体本来就只有几百字节到几 KB，学的那头已按上限把过大的挡掉了
            // （`proxy::UsageSniffer::refusal_reply`），截一刀等于回放一段残缺的 SSE。
            let (reply_sse, reply_body) = match &r.reply {
                Some(reply) => (reply.sse as i64, nul_free(&reply.body)),
                None => (0, "".into()),
            };
            sqlx::query(
                "INSERT INTO learned_rejections \
                    (kind, model, field, value, message, reply_sse, reply_body) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
            )
            .bind(nul_free(&r.kind))
            .bind(nul_free(&r.model))
            .bind(nul_free(&r.field))
            .bind(nul_free(&r.value))
            .bind(message)
            .bind(reply_sse)
            .bind(reply_body)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 读出仍在保鲜期内的全部规则（过期的顺手删掉），启动时回填进程内记忆用。
    pub async fn learned_rejections(&self) -> Result<Vec<LearnedRejection>> {
        Ok(self.learned_rejections_with_time().await?.into_iter().map(|(r, _)| r).collect())
    }

    /// 同 [`Self::learned_rejections`]，附每条学到的时刻（Unix 秒），控制台列表用。
    pub async fn learned_rejections_with_time(&self) -> Result<Vec<(LearnedRejection, i64)>> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("DELETE FROM learned_rejections WHERE learned_at <= unixepoch() - $1")
            .bind(LEARNED_REJECTION_TTL_SECS)
            .execute(&mut *conn)
            .await?;
        let rows: Vec<LearnedRow> = sqlx::query_as(
            "SELECT kind, model, field, value, message, learned_at, reply_sse, reply_body \
                 FROM learned_rejections ORDER BY learned_at ASC",
        )
        .fetch_all(&mut *conn)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(kind, model, field, value, message, learned_at, reply_sse, reply_body)| {
                // 空体即「没有」：只有拒答那类才带回放体。
                let reply = (!reply_body.is_empty())
                    .then_some(LearnedReply { sse: reply_sse != 0, body: reply_body });
                (LearnedRejection { kind, model, field, value, message, reply }, learned_at)
            })
            .collect())
    }

    /// 清空全部学到的规则（控制台逃生口：上游放开了某个取值、本地却还在拒）。返回删掉的条数。
    pub async fn clear_learned_rejections(&self) -> Result<usize> {
        Ok(sqlx::query("DELETE FROM learned_rejections").execute(&self.pool).await?.rows_affected()
            as usize)
    }

    /// 只清某一种类（`kind` 列）的规则，返回删掉的条数。控制台「清空这一类」用：几百条拒答
    /// 提示词淹没列表时，不必连 `deprecated` 那几条有用的一起清掉。
    /// 拒答规则不设条数上限（进程内与库里都是），只靠 7 天保鲜期与这里的手动清理收口。
    pub async fn clear_learned_rejections_of_kind(&self, kind: &str) -> Result<usize> {
        Ok(sqlx::query("DELETE FROM learned_rejections WHERE kind = $1")
            .bind(kind)
            .execute(&self.pool)
            .await?
            .rows_affected() as usize)
    }

    /// 删掉一条学到的规则（按主键四元组），返回是否确有其行。
    pub async fn forget_learned_rejection(&self, r: &LearnedRejection) -> Result<bool> {
        self.update_one(
            sqlx::query(
                "DELETE FROM learned_rejections \
                  WHERE kind = $1 AND model = $2 AND field = $3 AND value = $4",
            )
            .bind(&r.kind)
            .bind(&r.model)
            .bind(&r.field)
            .bind(&r.value),
        )
        .await
    }
}

/// `learned_rejections` 的一行：kind, model, field, value, message, learned_at, reply_sse, reply_body。
type LearnedRow = (String, String, String, String, String, i64, i64, String);

/// 删掉已到期的模型准入记录（读列表前顺手做）。
async fn purge_expired_denials(conn: &mut PgConnection) -> Result<()> {
    sqlx::query(
        "DELETE FROM model_denials WHERE expires_at IS NOT NULL AND expires_at <= unixepoch()",
    )
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::premium_model;
    use super::super::settings::store_with_local;
    use super::super::{AllRateLimited, ModelUnsupported, Select};
    use super::*;

    /// 按模型选号，回选中的号 id（旧测试里的 `pick` 闭包）。
    async fn pick(store: &CredentialStore, model: &str) -> i64 {
        store
            .select_for_device(Select { model: Some(model), ..Default::default() })
            .await
            .unwrap()
            .id
    }

    /// 上游判过「套餐不含这个模型」的号，该模型选号时绕开它，其余模型照常；清掉后回来。
    #[sqlx::test]
    async fn denied_model_is_skipped_until_cleared(pool: PgPool) {
        let (store, ids) = store_with_local(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        store.deny_model(a, "claude-fable-5-1[1m]", "plan", None).await.unwrap();
        assert_eq!(
            pick(&store, "claude-fable-5-1").await,
            b,
            "[1m] 变体上学到的记录对裸 id 同样生效"
        );
        assert_eq!(pick(&store, "claude-sonnet-5").await, a, "其余模型不受影响");
        assert_eq!(store.denied_models(a).await.unwrap().len(), 1);
        assert!(store.all_model_denials().await.unwrap().contains_key(&a));
        assert_eq!(store.clear_model_denials(a, Some("claude-sonnet-5")).await.unwrap(), 0);
        assert_eq!(store.clear_model_denials(a, Some("claude-fable-5-1")).await.unwrap(), 1);
        assert_eq!(pick(&store, "claude-fable-5-1").await, a, "解除后该模型回到这个号");
    }

    /// 全部号都被判过不支持 → 是「换模型」而不是「等一会」：报 ModelUnsupported，不报限流。
    #[sqlx::test]
    async fn all_denied_is_model_unsupported_not_rate_limited(pool: PgPool) {
        let (store, ids) = store_with_local(pool.clone(), &["a", "b"]).await;
        for id in &ids {
            store.deny_model(*id, "claude-fable-5", "plan", None).await.unwrap();
        }
        let err = store
            .select_for_device(Select { model: Some("claude-fable-5"), ..Default::default() })
            .await
            .unwrap_err();
        let u = err.downcast_ref::<ModelUnsupported>().expect("应是 ModelUnsupported");
        assert_eq!(u.accounts, 2);
        assert!(store.select_for_device(Select::default()).await.is_ok(), "不带模型的请求不受影响");

        // 换号重试的形态：唯一的号刚被判过、同时也在 exclude 里——仍必须报 ModelUnsupported，
        // 而不是「排除后没号了」那条普通错误（那条会让转发环把上游 429 原样透传）。
        // 旧测试另开一个只有一个号的库；这里在同一个库上删掉 b 得到同样的形态。
        assert!(store.delete(ids[1]).await.unwrap());
        let only = ids[0];
        let err = store
            .select_for_device(Select {
                model: Some("claude-fable-5"),
                exclude: &[only],
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<ModelUnsupported>().is_some(), "{err}");
    }

    /// 全部启用号都被判过、但还有一个只是限流暂停的 Max 号：那是「等一会」不是「换模型」，
    /// 报 AllRateLimited + 它的恢复时刻，不能回 403 把两小时后回来的号说成不存在。
    #[sqlx::test]
    async fn paused_capable_account_turns_all_denied_into_rate_limited(pool: PgPool) {
        let (store, ids) = store_with_local(pool, &["pro", "max"]).await;
        let (pro, max) = (ids[0], ids[1]);
        store.deny_model(pro, "claude-fable-5", "plan", None).await.unwrap();
        let resume_at = crate::credentials::now_secs() + 7200;
        store.pause_for_rate_limit(max, "quota", resume_at).await.unwrap();
        let sel = Select { model: Some("claude-fable-5"), ..Default::default() };
        let err = store.select_for_device(sel).await.unwrap_err();
        let rl = err.downcast_ref::<AllRateLimited>().expect("应是 AllRateLimited");
        assert!(
            rl.retry_after_secs > 7000 && rl.retry_after_secs <= 7200,
            "{}",
            rl.retry_after_secs
        );
        // 暂停的那个号自己也被判过 → 真的没号能用，才是 ModelUnsupported。
        store.deny_model(max, "claude-fable-5", "plan", None).await.unwrap();
        assert!(
            store
                .select_for_device(sel)
                .await
                .unwrap_err()
                .downcast_ref::<ModelUnsupported>()
                .is_some()
        );
    }

    /// 到期的记录不再挡选号，且读列表时顺手清掉。
    #[sqlx::test]
    async fn expired_denial_is_ignored_and_purged(pool: PgPool) {
        let (store, ids) = store_with_local(pool, &["a"]).await;
        let a = ids[0];
        let past = crate::credentials::now_secs() as i64 - 1;
        store.deny_model(a, "claude-fable-5", "plan", Some(past)).await.unwrap();
        assert_eq!(pick(&store, "claude-fable-5").await, a);
        assert!(store.denied_models(a).await.unwrap().is_empty());
    }

    /// 删号连带清掉它的准入记录，别留孤儿行。
    #[sqlx::test]
    async fn deleting_a_credential_drops_its_denials(pool: PgPool) {
        let (store, ids) = store_with_local(pool, &["a", "b", "c"]).await;
        for id in &ids {
            store.deny_model(*id, "claude-fable-5", "plan", None).await.unwrap();
        }
        assert!(store.delete(ids[0]).await.unwrap());
        assert_eq!(store.delete_many(&ids[1..2]).await.unwrap(), 1);
        let left = store.all_model_denials().await.unwrap();
        assert_eq!(left.keys().copied().collect::<Vec<_>>(), vec![ids[2]]);
        store.clear().await.unwrap();
        assert!(store.all_model_denials().await.unwrap().is_empty());
    }

    /// 学到的规则落库、重启可读回；过期的读不到；清空接口清得干净。
    #[sqlx::test]
    async fn learned_rejections_round_trip_and_expire(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let row = |kind: &str, model: &str, field: &str, value: &str| LearnedRejection {
            kind: kind.into(),
            model: model.into(),
            field: field.into(),
            value: value.into(),
            message: "msg".into(),
            reply: None,
        };
        let rows = vec![
            row("shape", "claude-opus-5", "effort", "xhigh"),
            row("deprecated", "claude-haiku-4-5", "temperature", ""),
        ];
        store.remember_rejections(&rows).await.unwrap();
        // 重复写入是幂等的。
        store.remember_rejections(&rows[..1]).await.unwrap();
        let mut got = store.learned_rejections().await.unwrap();
        got.sort_by(|a, b| a.kind.cmp(&b.kind));
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], rows[0]);
        assert_eq!(got[0], rows[1]);
        // 拒答那类带上游响应体：两列原样往返，体不截（message 仍截 500 字）。
        let long_body =
            format!("event: message_start\ndata: {{\"pad\":\"{}\"}}\n\n", "x".repeat(4000));
        let refusal = LearnedRejection {
            kind: "refusal".into(),
            model: "claude-opus-5".into(),
            field: "prompt_sha".into(),
            value: "deadbeef".into(),
            message: "m".repeat(600),
            reply: Some(LearnedReply { sse: true, body: long_body.clone() }),
        };
        store.remember_rejections(&[refusal]).await.unwrap();
        let back = store
            .learned_rejections()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.kind == "refusal")
            .expect("拒答行落库");
        assert_eq!(back.message.chars().count(), 500);
        assert_eq!(back.reply, Some(LearnedReply { sse: true, body: long_body }));
        assert!(store.forget_learned_rejection(&back).await.unwrap());

        // 人为把一条改成 8 天前学到的：读取时被当过期清掉。
        sqlx::query(
            "UPDATE learned_rejections SET learned_at = unixepoch() - $1 WHERE kind = 'shape'",
        )
        .bind(LEARNED_REJECTION_TTL_SECS + 86400)
        .execute(&store.pool)
        .await
        .unwrap();
        let got = store.learned_rejections().await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "deprecated");

        // 单条删除按四元组精确命中。
        store.remember_rejections(&rows).await.unwrap();
        assert!(store.forget_learned_rejection(&rows[0]).await.unwrap());
        assert!(!store.forget_learned_rejection(&rows[0]).await.unwrap(), "再删一次应无行");
        let (_, at) = store.learned_rejections_with_time().await.unwrap()[0].clone();
        assert!(at > 0);

        assert_eq!(store.clear_learned_rejections().await.unwrap(), 1);
        assert!(store.learned_rejections().await.unwrap().is_empty());
    }

    #[test]
    fn model_denial_key_normalizes_variants() {
        assert_eq!(model_denial_key("Claude-Fable-5-1[1m]"), "claude-fable-5-1");
        assert_eq!(model_denial_key("claude-opus-4-6-20251114"), "claude-opus-4-6");
        assert_eq!(model_denial_key("claude-fable-5"), "claude-fable-5");
        assert_ne!(model_denial_key("claude-fable-5"), model_denial_key("claude-fable-5-1"));
        assert!(premium_model("claude-fable-5-1[1m]") && premium_model("claude-mythos-5"));
        assert!(!premium_model("claude-opus-5"));
    }
}
