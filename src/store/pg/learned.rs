//! 从上游学到的规则的 PG 版，对应 `store::learned`：模型准入（套餐不含）与 400 形态规则。
//!
//! [`ModelDenial`] / [`LearnedRejection`] 等类型、[`model_denial_key`] 与保鲜期常量直接用
//! `store::learned` 的。

use std::collections::HashMap;

use anyhow::Result;
use sqlx::PgConnection;

use super::super::{
    LEARNED_REJECTION_TTL_SECS, LearnedRejection, LearnedReply, ModelDenial, model_denial_key,
};
use super::PgStore;

impl PgStore {
    // ---------- 模型准入（套餐不含某模型） ----------

    /// 记下「这个号用不了这个模型」。
    ///
    /// 依据是上游的 429 形态：一个额度窗口头都不带、只带 `overage-disabled-reason`——说明
    /// 这个模型根本不在该套餐的任何额度窗口里，要走按量计费的 usage credits，而组织又没开。
    /// 这与「超额池满」（`7d_oi` rejected，账号确实有 fable 额度只是用完了）是两回事，后者走
    /// 进程内冷却，见 [`super::super::RateLimitCooldown`]。
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
        let key = model_denial_key(model);
        let reason: String = reason.chars().take(300).collect();
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
            let message: String = r.message.chars().take(500).collect();
            // 回放体不截：拒答的体本来就只有几百字节到几 KB，学的那头已按上限把过大的挡掉了
            // （`proxy::UsageSniffer::refusal_reply`），截一刀等于回放一段残缺的 SSE。
            let (reply_sse, reply_body) = match &r.reply {
                Some(reply) => (reply.sse as i64, reply.body.as_str()),
                None => (0, ""),
            };
            sqlx::query(
                "INSERT INTO learned_rejections \
                    (kind, model, field, value, message, reply_sse, reply_body) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
            )
            .bind(&r.kind)
            .bind(&r.model)
            .bind(&r.field)
            .bind(&r.value)
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

    use super::super::super::premium_model;
    use super::super::super::{AllRateLimited, ModelUnsupported, Select};
    use super::super::settings::store_with_local;
    use super::*;

    /// 按模型选号，回选中的号 id（旧测试里的 `pick` 闭包）。
    async fn pick(store: &PgStore, model: &str) -> i64 {
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
        let store = PgStore::for_test(pool).await;
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
