//! 3）选号：按设备 / 会话粘性挑一个号，选不出时按原因回 429 / 403 / 503。

// 闸门 / 选号 / 组装失败时的 `Err` 就是要回给客户端的那条响应：只在拒绝时出现，装箱不划算。
#![allow(clippy::result_large_err)]
use super::*;

/// [`SessionPlan`] 借出来给选号用的那一份：换号重试要反复构 `Select`，捆成 `Copy` 的一份
/// 免得那几处漏传哪一项。
#[derive(Clone, Copy)]
pub(super) struct SessionSel<'a> {
    pub(super) key: Option<&'a str>,
    pub(super) per_session: bool,
    pub(super) passthrough: bool,
    pub(super) follow_only: bool,
}

impl Inbound {
    pub(super) fn session_sel(&self) -> SessionSel<'_> {
        SessionSel {
            key: self.plan.key.as_deref().filter(|_| self.plan.binds),
            per_session: self.plan.per_session,
            passthrough: self.plan.passthrough,
            follow_only: self.plan.follow_only,
        }
    }
}

// 3) 按 device_id（或会话键，见 [`SessionSel`]）粘性选出凭证的 access_token（必要时刷新）。
// 首发与换号重试用同一份选号入参，只有「已试过哪些号」不同——写成函数而不是就地各构一份，
// 免得两处的 device_id/model 哪天漂开。
pub(super) fn select<'a>(
    device_id: Option<&'a str>,
    session: SessionSel<'a>,
    billable: bool,
    model: Option<&'a str>,
    exclude: &'a [i64],
) -> store::Select<'a> {
    store::Select {
        device_id,
        session_key: session.key,
        per_session: session.per_session,
        passthrough_session: session.passthrough,
        follow_only: session.follow_only,
        rate_limited: billable,
        exclude,
        model,
        ..Default::default()
    }
}

/// 首发的号。选不出来时按原因回一条本地响应，流水由外层按本地拒绝补。
pub(super) async fn first_pick(
    state: &AppState,
    inb: &Inbound,
    log_state: &RequestLogState,
) -> Result<Pick, Response> {
    let Inbound {
        ref method,
        ref path_and_query,
        ref client_ua,
        ref device_id,
        ref req_model,
        ref session_id,
        billable,
        ..
    } = *inb;
    let session_sel = inb.session_sel();
    let (token, cred, session_slot) = match store::valid_access_token_for_device(
        &state.store,
        &state.clients,
        select(device_id.as_deref(), session_sel, billable, req_model.as_deref(), &[]),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            // 这条同样要抑制，而且理由比前两道闸更硬：账号 RPM、裸请求上限、全员冷却这三种
            // 都是**明确让客户端稍后再来**的状态，不退避的客户端会照着重试节奏一条条刷日志，
            // 与撞设备/会话闸时一模一样。
            //
            // 抑制键带上**分类**：同一台设备可能一会儿是「号都在冷却」、一会儿是「没有可用
            // 账号」，共用一个桶会把后出现的那种整个盖掉，而那恰恰是状态变了的信号。
            let kind = if e.downcast_ref::<store::BareRateLimited>().is_some() {
                "bare-rate-limit"
            } else if e.downcast_ref::<store::RpmLimited>().is_some() {
                "account-rpm"
            } else if let Some(rl) = e.downcast_ref::<store::AllRateLimited>() {
                if rl.refresh_failed.is_some() { "refresh-failed" } else { "all-cooling-down" }
            } else if e.downcast_ref::<store::DeviceLimitReached>().is_some() {
                "device-limit"
            } else if e.downcast_ref::<store::SessionLimitReached>().is_some() {
                "session-limit"
            } else if e.downcast_ref::<store::ModelUnsupported>().is_some() {
                "model-unsupported"
            } else if e.downcast_ref::<store::RefreshFailed>().is_some() {
                "refresh-failed"
            } else {
                "unavailable"
            };
            *log_state.local_reject.lock() = Some(kind);
            // 失败出在一个具体的号上（刷新失败，或全池在等的那个号是刷新失败停的）：流水记到它名下。
            if let Some(rf) = e.downcast_ref::<store::RefreshFailed>().or_else(|| {
                e.downcast_ref::<store::AllRateLimited>().and_then(|rl| rl.refresh_failed.as_ref())
            }) {
                *log_state.refresh_failed.lock() = Some(rf.clone());
            }
            // 分桶用设备，没有设备身份就退到会话，都没有才并成一桶——后者本就是「裸请求」，
            // 它们由裸请求上限统一管着，日志上也没有更细的身份可分。
            let who = device_id.as_deref().or(session_id.as_deref()).unwrap_or("-");
            if let Some(suppressed) =
                take_rejection_log_slot(&state.rejection_log, &format!("forward:{kind}:{who}"))
            {
                tracing::warn!(%method, path = %path_and_query, ua = %client_ua, kind, suppressed, error = %e, "refusing to forward");
            }
            // 三类「等多久是算得出来的」限流 → 429 且带 `retry-after`，给出来客户端才知道该
            // 等多久，而不是立刻重试再撞一次：裸请求速率上限取窗口长度；账号 RPM 上限取窗口里
            // 最早那条滚出去的时刻；所有号都在上游 429 冷却中（硬门禁）取最早解冻的那个的
            // 剩余时间。
            let computable_retry = e
                .downcast_ref::<store::BareRateLimited>()
                .map(|rl| rl.retry_after_secs)
                .or_else(|| e.downcast_ref::<store::RpmLimited>().map(|rl| rl.retry_after_secs))
                .or_else(|| {
                    e.downcast_ref::<store::AllRateLimited>().map(|rl| rl.retry_after_secs)
                });
            if let Some(secs) = computable_retry {
                return Err(rate_limit_response(secs, e.to_string()));
            }
            // 所有号都被上游判过「套餐不含这个模型」→ 403 permission_error：等多久都没用，
            // 客户端该换模型。照上游拒绝无权限模型的口径回，别包装成 429 误导它退避重试。
            if e.downcast_ref::<store::ModelUnsupported>().is_some() {
                return Err(error_response(
                    StatusCode::FORBIDDEN,
                    "permission_error",
                    e.to_string(),
                ));
            }
            // 设备数 / 模拟会话数达硬上限 → 429（等多久取决于别人什么时候释放，给不出
            // retry-after，故这条不走 [`rate_limit_response`]）；其余（无凭证/刷新失败等）→ 503。
            // 刷新失败：细节（完整错误链）已记进流水与账号状态，回给客户端的只说是哪个号。
            if let Some(rf) = e.downcast_ref::<store::RefreshFailed>() {
                let msg = format!(
                    "token refresh for credential #{} failed; please retry shortly",
                    rf.cred_id
                );
                return Err(error_response(StatusCode::SERVICE_UNAVAILABLE, "api_error", msg));
            }
            let (status, etype) = if e.downcast_ref::<store::DeviceLimitReached>().is_some()
                || e.downcast_ref::<store::SessionLimitReached>().is_some()
            {
                (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error")
            } else {
                (StatusCode::SERVICE_UNAVAILABLE, "api_error")
            };
            return Err(error_response(status, etype, e.to_string()));
        }
    };
    Ok(Pick { token, cred, session_slot })
}
