//! 会话保活被上游拒绝时的处置：判定账号级与否、停用并记封号事件。

use super::*;

/// [`handle_keepalive_rejection`] 怎么处置的这一发 401/403，调用方据此决定要不要撤掉「首次握手
/// 已跑过」的标记。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum KeepaliveRejection {
    /// 确实封号了（落库成功）。
    Banned,
    /// 订阅未生效：暂停了，或号本就不在池里（人工停用、已处置）。
    SubscriptionInactive,
    /// 都没发生：判定不命中、封号落库失败、号已不存在。库写失败时号仍是启用的，调用方不能
    /// 把它当成处理完了。
    NotBanned,
}

/// 保活端点回 401/403：**诊断与停用判定分开**。先把完整上下文（端点、状态码、错误类型与
/// 文案、上游 request-id）记进日志，再按转发路径同一套 [`proxy::classify_account_rejection`] 决定
/// 要不要停用——命中账号级特征（401 `authentication_error`、`invalid_grant`、「账号 /
/// 组织 被停用」之类）才 [`CredentialStore::record_ban`]，事件里带同一份上下文；没命中的
/// （订阅未生效、`permission_error`、区域限制、网关页面……）不封号。
/// 返回是否停用了。
///
/// 此前这里是「任何 401/403 一律 `mark_banned`」：来源记成 `manual`、原因固定「upstream
/// 401/403」，状态码、正文、request-id 全丢，且组织权限配置一类**可恢复**的拒绝也被当成
/// 终态永久停用。转发路径早就不这么判了（见 `classify_account_rejection` 的三档说明），保活没理由
/// 更激进：它发的还是与账号状态无关的遥测/握手端点。
///
/// 不封号的里面，「订阅未生效」（上游原话是组织不允许 OAuth：付费档到期未续费、Free 档没订阅）
/// 单独**暂停调度**（[`proxy::park_org_oauth_disallowed`]）：续费 / 订阅之前每条真实请求都会吃
/// 同一发 403，放着不管等于让粘在它身上的客户端一直报错。
/// 其余的（`permission_error`、区域限制、网关页面……）仍只记日志。
///
/// 返回怎么处置的，见 [`KeepaliveRejection`]。
pub(super) fn handle_keepalive_rejection(
    store: &CredentialStore,
    cred: &Credential,
    rej: &oauth::AuthRejection,
) -> KeepaliveRejection {
    let ctx = keepalive_ban_context(rej);
    match keepalive_rejection_verdict(rej) {
        AccountRejection::Ban(_) => {}
        // 订阅未生效：不封号，但暂停调度——续费 / 订阅之前真实请求会一条条撞上同一发。
        // 暂停不会到点自己回来：等人手动启用，或连通性测试通过。停下的那一次由
        // `park_org_oauth_disallowed` 自己记 warn；已暂停的号保活不再发这些端点（只刷 token），
        // 走不到这里。
        AccountRejection::SubscriptionInactive => {
            if !proxy::park_org_oauth_disallowed(store, cred, rej.status, "keepalive") {
                tracing::debug!(
                    cred_id = cred.id, cred = %cred.label,
                    endpoint = rej.endpoint, status = rej.status,
                    "keepalive: subscription inactive, but the credential was not paused (it is manually disabled, already out of the pool, gone, or the write failed); leaving it as is"
                );
            }
            return KeepaliveRejection::SubscriptionInactive;
        }
        AccountRejection::Other => {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                endpoint = rej.endpoint, status = rej.status,
                error_type = ctx.error_type.as_deref().unwrap_or("-"),
                error_message = ctx.error_message.as_deref().unwrap_or("-"),
                upstream_request_id = ctx.upstream_request_id.as_deref().unwrap_or("-"),
                "keepalive: upstream rejected the token but it is not an account-level error; not banning the credential"
            );
            return KeepaliveRejection::NotBanned;
        }
    }
    tracing::warn!(
        cred_id = cred.id, cred = %cred.label,
        endpoint = rej.endpoint, status = rej.status,
        error_type = ctx.error_type.as_deref().unwrap_or("-"),
        upstream_request_id = ctx.upstream_request_id.as_deref().unwrap_or("-"),
        reason = %ctx.reason,
        "keepalive: account-level error from upstream, marking as banned"
    );
    match store.record_ban(cred.id, &ctx) {
        Ok(true) => KeepaliveRejection::Banned,
        Ok(false) => {
            tracing::warn!(cred_id = cred.id, cred = %cred.label, "keepalive: credential vanished before it could be disabled");
            KeepaliveRejection::NotBanned
        }
        Err(e) => {
            tracing::error!(
                cred_id = cred.id, cred = %cred.label, error = %e,
                "keepalive: failed to disable the credential; it stays enabled until the next tick or a forwarded request trips the same check"
            );
            KeepaliveRejection::NotBanned
        }
    }
}

/// 保活 401/403 对账号的结论：与转发路径共用 [`proxy::classify_account_rejection`]，两边对
/// 同一条报文给同一个答案，免得保活放行的号下一条真实请求又被转发路径停掉（或反过来）。
fn keepalive_rejection_verdict(rej: &oauth::AuthRejection) -> AccountRejection {
    StatusCode::from_u16(rej.status)
        .map(|status| proxy::classify_account_rejection(status, rej.body.as_bytes()))
        .unwrap_or(AccountRejection::Other)
}

/// 保活 401/403 是不是账号级错误（该封号）。
#[cfg(test)]
pub(super) fn keepalive_rejection_is_account_level(rej: &oauth::AuthRejection) -> bool {
    matches!(keepalive_rejection_verdict(rej), AccountRejection::Ban(_))
}

/// 把保活端点的 401/403 响应整理成封号上下文：`reason` 是 `[keepalive/<端点> <状态码>]
/// <类型>: <文案>` 截到 200 字符；`error_message` 是完整文案（体不是 Anthropic 错误 JSON
/// 时就是整段体，比如网关的 HTML）；空体写明 `(empty body)`，别留一个光秃秃的前缀。
pub(super) fn keepalive_ban_context(rej: &oauth::AuthRejection) -> store::BanContext {
    let (error_type, message) = proxy::parse_upstream_error(rej.body.as_bytes());
    let message = if message.trim().is_empty() { "(empty body)".to_string() } else { message };
    let head = match &error_type {
        Some(t) => format!("[keepalive/{} {}] {t}: {message}", rej.endpoint, rej.status),
        None => format!("[keepalive/{} {}] {message}", rej.endpoint, rej.status),
    };
    store::BanContext {
        reason: head.chars().take(200).collect(),
        source: "keepalive",
        status: Some(rej.status),
        error_type,
        error_message: Some(message),
        request_id: None,
        upstream_request_id: rej.request_id.clone(),
    }
}
