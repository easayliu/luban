//! 转发主流程（[`handle_inner`]）：入口闸门 → 选号 → 逐轮组装、发送、按状态换号 → 收尾。
//!
//! 各阶段一个子模块，彼此只经下面几个结构交接：
//! - [`admit`]：闸门放行后给出 [`Inbound`]（整条请求不变的事实）与 [`Guards`]；
//! - [`select`]：首发的号（[`Pick`]）；
//! - [`attempt`]：循环外的出站材料（[`Prepared`]）与当前号的一轮请求（[`Attempt`]）；
//! - [`retry`]：上游这一发之后换号重发、就地回复还是收尾（[`Flow`]）；
//! - [`respond`]：把最后那一发交回客户端。

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::Response,
};

use crate::config;
use crate::store;
use crate::web::AppState;

use super::ban::{
    AccountRejection, classify_account_rejection, header_text, is_third_party_rejection,
    log_third_party_rejection, park_org_oauth_disallowed, parse_upstream_error,
};
use super::body::{
    below_min_client_version, body_has_pair, body_has_user_id, build_tool_name_map, cc_cli_version,
    cc_ua_entrypoint, client_supplied_fallbacks, device_fingerprint, ensure_beta_query,
    extract_device_id, extract_session_id, is_billable_messages, is_fallback_rejection,
    known_latest_release, misplaced_system_role, outbound_carries_fallbacks, refusal_fallbacks_for,
    remember_fallback_rejection, sim_device_fingerprint, sim_device_id, sim_session_key,
    stream_requested, trusted_cc_version, ua_of,
};
use super::connectivity::{session_start, spawn_session_handshake};
use super::digest::redact_headers;
use super::headers::{BetaCtx, build_forward_headers_for, ensure_cache_ttl_beta};
use super::learned_rules::{
    MODEL_DENIAL_MAX_SWAPS, RejectionLog, TRANSIENT_MAX_ATTEMPTS, app_system_digest,
    empty_reply_class, has_outbound_shape_rules, is_max_plan, known_app_refusal, known_empty_reply,
    known_refused_prompt, known_shape_rejection, next_transient_backoff, prompt_digest,
    remember_shape_rejection, replay_refusal, take_rejection_log_slot,
};
use super::logging::{
    ReqLog, UsageSniffer, ban_context, capture_forensics_without_body, fill_shape_forensics,
    record_early_failure, telemetry_capture,
};
use super::openai_marker::find_openai_marker;
use super::probe_detect::{probe_reply, probe_signature};
use super::rate_limit::{
    LimitScope, RateLimitInfo, park_if_quota_nearly_exhausted, park_rate_limited,
    rate_limit_scope_for,
};
use super::session_id::{
    bare_session_id, incoming_session_id, needs_prefix_key, outbound_session_id,
    session_id_conflict, session_plan,
};
use super::session_link::{CcRequestKind, client_session_link};
use super::simulation::{SimSessionSeed, Simulation, inbound_facts, is_cc_shaped, simulates_cc};
use super::thinking::{
    block_site, error_block_path, is_redacted_thinking_data_error, is_thinking_signature_error,
    latest_assistant_diff, retry_demoted_thinking, thinking_block_error_kind, trace_thinking_block,
};
use super::upstream::{
    InFlightGuard, SessionConcurrencyGuard, Upstream, UpstreamRouteGuard, error_chain,
    note_upstream_send, rebuild_response, relay_upstream, resp_builder, resp_shape,
    retry_thread_as_create, retry_without_fallbacks, try_acquire_session_concurrency,
    upstream_error_kind, upstream_load_snapshot,
};
use super::{
    EarlyUpstreamFailure, ParsedRequestBits, REWRITE_APP_REFUSAL_REPLAY, REWRITE_PROBE_REPLY,
    REWRITE_REFUSAL_REPLAY, RequestLogState, client_access, client_request_id, error_response,
    header_opt, inbound_beta_list, log_early_upstream_failure, rate_limit_response,
    request_max_tokens, request_model, request_speed,
};

mod admit;
mod attempt;
mod respond;
mod retry;
mod select;

use self::admit::{Guards, Inbound};
use self::attempt::{Attempt, Prepared};
use self::retry::{Flow, Swaps, UpstreamResult};
use self::select::select;
use super::logging::ShapeBits;
use super::session_id::SessionPlan;

/// 各阶段都要的三样：状态、这条请求的 id、流水归属。
#[derive(Clone, Copy)]
pub(super) struct Ctx<'a> {
    pub(super) state: &'a AppState,
    pub(super) request_id: &'a str,
    pub(super) log_state: &'a RequestLogState,
}

/// 这一轮用的号：access token、凭证，以及会话在这个号上占的槽位。换号重试时整体换掉。
pub(super) struct Pick {
    pub(super) token: String,
    pub(super) cred: crate::credentials::Credential,
    pub(super) session_slot: Option<i64>,
}

pub(super) async fn handle_inner(
    state: AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
    request_id: &str,
    // 流水归属，见 [`RequestLogState`]：建 [`ReqLog`] 或早退路径就地写流水时置 `logged`，
    // 解析完体放下 `parsed`；仍没人写的 4xx/5xx 由 [`handle`] 按本地拒绝补一条。
    log_state: &RequestLogState,
) -> Response {
    let (inb, guards) = match admit::admit(&state, method, &uri, headers, body, log_state).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let mut pick = match select::first_pick(&state, &inb, log_state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let cx = Ctx { state: &state, request_id, log_state };
    let prepared = Prepared::of(&state, &inb);

    // 7) 发起上游请求并流式回传。头名的拼写与顺序由 orig_header_case 决定（关掉即退回
    //    「全小写 + Host/User-Agent/Content-Length 钉在队尾」，也就是换 wreq 之前的形态）。
    //    `body` 自此保持**客户端原始请求体**不变——改写后的那份直接交给 `send`，
    //    因为签名重试那条路要拿原始体重新走一遍改写，留着原件比留改写件更省事也更不易错。
    //    非计费路径（count_tokens 等）由 [`Upstream::shape`] 原样透传：那儿既没有 `metadata`
    //    可伪装，改写 `system` 形态反而会让计出来的 token 数偏离客户端实际要发的那份，还平白
    //    多担一份上游挑刺的风险。
    //
    //    **上游 429 换号重试**（`rate_limit_retry`）：某个号被限流时，客户端自己重试也只会
    //    继续撞同一个号——设备是粘性绑定的，而绑定只看凭证有没有被停用，不看它是不是刚被限流。
    //    于是这里在收到 429 时给该号打上冷却（时长取自上游的 `retry-after`/`*-reset`，见
    //    [`RateLimitInfo::cooldown`]），换一个**没试过的**号重发，并把设备**改绑**过去。
    //
    //    整套（选号 → 装头 → 改体）必须逐轮重来：`Authorization` 换了、`metadata` 里的伪装
    //    身份随号变、模拟路径的 session_id 也由账号派生——只换 token 会发出一条自相矛盾的请求。
    //    故首发也走这个循环，不存在「首发与重试形态不一致」的可能。
    let mut swaps = Swaps::new(&state, &inb);
    let (attempt, resp, upstream_limit) = loop {
        let attempt = match attempt::prepare(&cx, &inb, &prepared, &pick).await {
            Ok(a) => a,
            Err(resp) => return resp,
        };
        let resp = attempt.upstream.send(attempt.sent.clone()).await;
        match retry::on_response(&cx, &inb, &mut pick, &mut swaps, &attempt, &prepared, resp).await
        {
            Flow::Retry => continue,
            Flow::Reply(resp) => return resp,
            Flow::Done(resp, upstream_limit) => break (attempt, resp, upstream_limit),
        }
    };
    respond::respond(&cx, &inb, guards, &prepared, &pick, attempt, resp, upstream_limit).await
}

/// 早退路径上的流水 `device_id`，与正常路径 `logged_device` 同口径：来访自带的优先，没有就
/// 用模拟派生的那个（见 [`sim_device_id`]）。正常路径在 `match resp` 之前算一次；早退那几条
/// 在它之前就返回了，得各自算。
fn early_logged_device(
    device_id: &Option<String>,
    attempt: &Attempt<'_>,
    cred: &crate::credentials::Credential,
    device_fp: &str,
) -> Option<String> {
    device_id
        .clone()
        .or_else(|| sim_device_id(attempt.sent_bits.device_id_out.as_deref(), cred, device_fp))
}

/// 会话 RPM 超限那条 429：两个入口（头一路、body 一路）共用，免得两处的状态码、头、正文
/// 措辞哪天漂开。`source` 只进日志，用来分辨会话 id 是从哪儿读到的。
fn session_rpm_rejection(
    log: &RejectionLog,
    method: &Method,
    path_and_query: &str,
    client_ua: &str,
    session_id: &str,
    retry: i64,
    source: &'static str,
) -> Response {
    // 日志抑制同设备那道闸：憋掉的条数记在下一行的 `suppressed=` 上。
    if let Some(suppressed) = take_rejection_log_slot(log, &format!("session:{session_id}")) {
        // 会话 id 是 uuid，整串进日志只会把行撑长；取前 8 位足够把几个并发会话区分开，
        // 口径与设备那条拒绝日志一致。
        let session_short: String = session_id.chars().take(8).collect();
        tracing::warn!(
            %method, path = %path_and_query, ua = %client_ua,
            session = %session_short, %source, retry_after = retry, suppressed,
            "rejected: this session has reached its RPM limit"
        );
    }
    rate_limit_response(
        retry,
        format!("this session has reached its RPM limit; retry in {retry} seconds"),
    )
}

/// 会话并发在途超限那条 429。retry-after 给 1 秒：并发上限不像 RPM 那样有窗口要等，
/// 前面的请求走完一条就空出一个格子，等一拍就好。
///
/// 八个参数全是日志字段，只此一处调用；为它包一个结构体只会把同一份东西抄两遍。
#[allow(clippy::too_many_arguments)]
fn session_concurrency_rejection(
    log: &RejectionLog,
    method: &Method,
    path_and_query: &str,
    client_ua: &str,
    session_id: &str,
    current: u32,
    limit: i64,
    source: &'static str,
) -> Response {
    if let Some(suppressed) =
        take_rejection_log_slot(log, &format!("session_concurrency:{session_id}"))
    {
        let session_short: String = session_id.chars().take(8).collect();
        tracing::warn!(
            %method, path = %path_and_query, ua = %client_ua,
            session = %session_short, %source, current, limit, suppressed,
            "rejected: this session has reached its concurrency limit"
        );
    }
    rate_limit_response(
        1,
        format!(
            "this session already has {current} requests in flight (limit {limit}); retry in 1 second"
        ),
    )
}

#[cfg(test)]
mod tests;
