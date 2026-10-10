//! 封号：自动停用。复盘直接查库里的流水（`usage_logs`），这里不另存取证。

/// 一次自动停用的上下文，见 [`CredentialStore::record_ban`]。
#[derive(Debug, Default, Clone)]
pub struct BanContext {
    /// 写进 `credentials.ban_reason` 的一句话（200 字符内）。
    pub reason: String,
    /// 触发来源：`forward`（转发 4xx）、`forward_401`（转发 401 换号）、`probe`（连通性
    /// 测试）、`keepalive`（保活端点 401/403）、`refresh`（刷新 token 被作废）、`proxy`
    /// （代理建不出来）、`manual`（其它，目前只有测试用）。
    pub source: &'static str,
    /// 上游 HTTP 状态码（有的话）。
    pub status: Option<u16>,
    /// 上游 `error.type` 与 `error.message`（reason 是截断过的；日志里截到 [`ERROR_MESSAGE_MAX`]）。
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    /// 触发那条请求的 luban 请求 id 与上游 `request-id`——拿它能在流水里精确找到那一发。
    pub request_id: Option<String>,
    pub upstream_request_id: Option<String>,
}

use anyhow::Result;
use sqlx::Arguments;
use sqlx::postgres::PgArguments;

use super::credential::SUBSCRIPTION_PAUSE_SQL;
use super::session_events::log_removed;
use super::{CredentialStore, ERROR_MESSAGE_MAX, head_chars, nul_free};

impl CredentialStore {
    /// 自动停用：停号、记原因（[`BanContext::reason`]），同时清掉它的设备与会话绑定。
    /// 上下文的其余几项（来源、状态码、上游原文、请求 id）记一条 warn 日志，复盘时拿 request id
    /// 去流水里找触发的那一发。
    ///
    /// 凭证不存在时返回 `false`。
    ///
    /// 号已经封着时（`disabled` 且没有恢复时刻、带着非订阅暂停的原因）只清绑定，不覆盖第一次
    /// 的原因，返回 `true`：同一个号上并发在飞的几发会先后吃到 401/403，保活、连通性测试也可能
    /// 又撞上一次。
    pub async fn record_ban(&self, id: i64, ctx: &BanContext) -> Result<bool> {
        // 原因与报错可能回显客户端塞进来的串，见 [`nul_free`]。
        let reason = nul_free(&ctx.reason);
        let mut tx = self.begin_write().await?;
        let account: Option<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COALESCE(disabled = 1 AND resume_at IS NULL AND ban_reason IS NOT NULL
                             AND NOT ({SUBSCRIPTION_PAUSE_SQL}), FALSE)
               FROM credentials WHERE id = $1 FOR UPDATE"
        )))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM device_bindings WHERE cred_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let mut args = PgArguments::default();
        args.add(id).map_err(anyhow::Error::from_boxed)?;
        log_removed(&mut tx, "unbound", Some("account_banned"), "cred_id = $1", args).await?;
        sqlx::query("DELETE FROM session_bindings WHERE cred_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let Some(already_banned) = account else {
            tx.commit().await?;
            return Ok(false);
        };
        if !already_banned {
            sqlx::query(
                "UPDATE credentials SET disabled = 1, ban_reason = $2, resume_at = NULL,
                        updated_at = unixepoch()
                  WHERE id = $1",
            )
            .bind(id)
            .bind(reason.as_ref())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        if already_banned {
            tracing::debug!(cred_id = id, "credential already banned, keeping the first reason");
        } else {
            tracing::warn!(
                cred_id = id,
                source = ctx.source,
                status = ctx.status,
                error_type = ctx.error_type.as_deref(),
                error_message = ctx
                    .error_message
                    .as_deref()
                    .map(|m| head_chars(m, ERROR_MESSAGE_MAX)),
                request_id = ctx.request_id.as_deref(),
                upstream_request_id = ctx.upstream_request_id.as_deref(),
                reason = %reason,
                "credential banned"
            );
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 停号、记原因；不存在的号回 `false`；已封着的号再撞一次不覆盖第一次的原因；
    /// 解封之后再被封照常记新原因。
    #[sqlx::test]
    async fn record_ban_disables_and_keeps_the_first_reason(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        let ctx = |reason: &str| BanContext {
            reason: reason.into(),
            source: "forward",
            status: Some(403),
            ..Default::default()
        };
        let reason = async || store.get(a).await.unwrap().unwrap().ban_reason;

        assert!(store.record_ban(a, &ctx("[403] first")).await.unwrap());
        assert!(!store.record_ban(9999, &ctx("x")).await.unwrap(), "不存在的号");
        let got = store.get(a).await.unwrap().unwrap();
        assert!(got.disabled && got.is_banned());
        assert!(store.record_ban(a, &ctx("[401] later")).await.unwrap());
        assert_eq!(reason().await.as_deref(), Some("[403] first"), "已封着的号不覆盖原因");

        store.set_disabled(a, false).await.unwrap();
        assert!(store.record_ban(a, &ctx("[403] again")).await.unwrap());
        assert_eq!(reason().await.as_deref(), Some("[403] again"), "解封后再封是新的一次");

        // 限流暂停中被封照常记，恢复时刻清掉。
        store.set_disabled(a, false).await.unwrap();
        let resume_at = crate::credentials::now_secs() + 600;
        assert!(store.pause_for_rate_limit(a, "rate limited", resume_at).await.unwrap());
        assert!(store.record_ban(a, &ctx("[403] paused")).await.unwrap());
        let got = store.get(a).await.unwrap().unwrap();
        assert_eq!(got.ban_reason.as_deref(), Some("[403] paused"));
        assert!(got.resume_at.is_none());
    }
}
