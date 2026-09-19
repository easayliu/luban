//! 转发代理：Claude Code → luban → 官方 Anthropic API。
//!
//! 透传请求体，仅替换鉴权：校验来访 API Key 后，注入选中凭证的 OAuth access_token
//! 与 `anthropic-beta: oauth-2025-04-20`，响应流式原样回传。

#[cfg(test)]
use axum::http::HeaderName;
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rand::RngExt;

#[cfg(test)]
use crate::config;
use crate::store;
use crate::web::AppState;

mod openai_marker;
#[cfg(test)]
use openai_marker::find_openai_marker;

mod digest;
#[cfg(test)]
use digest::request_digest;

mod ban;
pub(crate) use ban::{detect_account_ban, parse_upstream_error};
#[cfg(test)]
use ban::{header_text, is_third_party_rejection};

mod headers;
#[cfg(test)]
use headers::{
    build_forward_headers, build_forward_headers_for, ensure_fallback_beta,
    is_official_non_main_beta, merge_beta, orig_header_case, simulated_beta,
};
use headers::{has_beta, uuid_v4};

mod session_link;
#[cfg(test)]
use session_link::{CcRequestKind, CcSessionKey, CcSessionLink, client_session_link};

mod session_id;
use session_id::incoming_session_id;
#[cfg(test)]
use session_id::{
    account_session_id, bare_session_id, outbound_session_id, pin_session_id, session_id_conflict,
    session_id_for,
};

mod probe_detect;
#[cfg(test)]
use probe_detect::{
    LOCAL_REPLY_HEADER, PROBE_KIND_HEADER, PROBE_REPLY_ID_PREFIX, PROBE_REPLY_TEXT, ProbeKind,
    message_to_sse, probe_reply, probe_signature,
};

mod thinking;
#[cfg(test)]
use thinking::{
    block_site, demote_thinking_blocks, empty_thinking_shape, error_block_path,
    is_empty_thinking_error, is_redacted_thinking_data_error, is_thinking_modified_error,
    is_thinking_signature_error, latest_assistant_diff, latest_assistant_has_thinking,
    preserve_thinking_encoding, strip_empty_thinking_blocks, thinking_block_error_kind,
    trace_thinking_block,
};

mod rate_limit;
use rate_limit::RateLimitInfo;
#[cfg(test)]
use rate_limit::{
    DEFAULT_MODEL_COOLDOWN_SECS, LimitScope, MAX_RATE_LIMIT_COOLDOWN_SECS,
    MAX_TRANSIENT_COOLDOWN_SECS, park_if_quota_nearly_exhausted, park_rate_limited,
    rate_limit_scope, rate_limit_scope_for,
};

mod body;
pub(crate) use body::parse_version;
use body::ua_of;
#[cfg(test)]
use body::{
    CacheShape, FALLBACKS_FIELD, ToolNameMap, align_system_shape, apply_tool_names,
    below_min_client_version, body_has_pair, body_has_user_id, build_tool_name_map, cc_cli_version,
    cc_tools_core, client_supplied_fallbacks, device_fingerprint, drop_empty_system_messages,
    ensure_beta_query, ensure_billing_cch, ensure_fallbacks, extract_device_id, extract_session_id,
    flatten_tool_schemas, is_billable_messages, is_fallback_rejection, known_latest_release,
    normalize_tool_choice, outbound_carries_fallbacks, outbound_identity, refusal_fallbacks_for,
    remember_fallback_rejection, replace_json_str_field, rewrite_body, sim_device_fingerprint,
    sim_device_id, sim_session_key, stream_requested, strip_empty_text_blocks, strip_extra_fields,
    sync_metadata_session, trusted_cc_version, trusted_cc_version_against, with_outbound_identity,
};

mod upstream;
use upstream::Upstream;
#[cfg(test)]
use upstream::{
    Aggregated, InFlightGuard, SessionConcurrencyGuard, SseAggregator, UPSTREAM_SEND_WINDOW,
    aggregate_sse, error_status, note_upstream_send, upstream_load_snapshot,
};
pub(crate) use upstream::{SessionConcurrency, UpstreamLoad};

mod logging;
#[cfg(test)]
use logging::{RESPONSE_EXCERPT_BYTES, ReqLog, shape_summary};
use logging::{UsageSniffer, capture_forensics, spawn_usage_log};

mod connectivity;
#[cfg(test)]
use connectivity::{
    ProbeLog, ProbeQuota, aggregate_probe_sse, handshake_sequence, probe_body, probe_simulation,
};
pub(crate) use connectivity::{ProbeReport, probe};

mod learned_rules;
#[cfg(test)]
use learned_rules::{
    APP_COUNTER_MAX_KEYS, AppCounter, DEPRECATABLE_FIELDS, REFUSAL_REPLY_BYTES,
    REJECTION_LOG_WINDOW, SHAPE_MEMORY_CAP, SHAPE_PROBES, SSE_CONTENT_TYPE,
    TRANSIENT_BACKOFF_BASE_SECS, TRANSIENT_BACKOFF_RESET, TRANSIENT_MAX_ATTEMPTS,
    app_system_digest, empty_reply_class, has_learned_deprecated_field, known_app_refusal,
    known_empty_reply, known_refused_prompt, known_shape_rejection, maybe_strip_deprecated,
    next_transient_backoff_at, prompt_digest, record_app_request, remember_app_refusal,
    remember_deprecated_field, remember_empty_reply, remember_refused_prompt,
    remember_shape_rejection, replay_refusal, take_rejection_log_slot,
};
pub(crate) use learned_rules::{
    DeprecatedFieldMemory, EmptyReplyMemory, LEARNED_KINDS, RejectionLog, SeededMemories,
    ShapeMemory, TransientBackoff, clear_learned_memory_kind, forget_learned_memory,
    learned_memory_len, resync_learned_memories, seed_learned_memories,
};

mod simulation;
#[cfg(test)]
use simulation::SimSessionSeed;
#[cfg(test)]
use simulation::{
    CC_BASE_PROMPT_MIN_LEN, CLIENT_SYSTEM_REMINDER_LEAD, MAX_CACHE_BREAKPOINTS, SimEnv,
    SimulationReason, billing_header_text, cap_system_blocks, cc_identity_well_formed,
    cc_profile_for, cc_system_rest, cc_version_suffix, inbound_facts,
    is_official_thread_continuation, looks_like_uuid, relocate_long_client_system,
    render_system_rest, sim_env_for, simulate_system, simulates_cc, simulation_reason,
};
use simulation::{Simulation, is_cc_shaped};

mod handler;
use handler::handle_inner;

/// 转发 `/v1/*` 到官方 API。
/// 转发入口：给这条入站请求定一个 id，跑完 [`handle_inner`] 后把 id 写进响应头与错误体。
///
/// **id 的取法照业界惯例**（nginx / Envoy / 各家网关一致）：来访已带合法 `X-Request-Id` 就沿用，
/// 让同一个 id 贯穿客户端、New API、luban 三层；没带才生成一个，形态照 Stripe / Anthropic：
/// `req_` + 16 位 base62 随机串（见 [`new_request_id`]）。见 [`request_id_for`]。
///
/// 响应头回两份同值：`X-Request-Id` 是通用名字，给任何客户端与人看；`X-Oneapi-Request-Id`
/// 是给 New API 的——它读上游响应头里的这个名字，存进自己日志的 `upstream_request_id`
/// （它自己那份 id 从不发给上游，两边只能靠这条路对上）。**每条响应都带**，包括本地拒掉的
/// 那些：排查「为什么被拒」时最需要它。
pub async fn handle(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = std::time::Instant::now();
    let request_id = request_id_for(&headers);
    // 本地拒绝的流水要用到的几样先抠出来——`handle_inner` 拿走了 headers/uri/body 的所有权。
    // 都是头上的一次查找，体只克隆句柄（`Bytes` 引用计数），不在这儿解析。
    let local = LocalRejectCtx {
        method: method.to_string(),
        path: uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path()).to_string(),
        ua: ua_of(&headers),
        session_header: incoming_session_id(&headers, None),
    };
    let store = state.store.clone();
    let log_state = RequestLogState::default();
    let mut resp = handle_inner(state, method, uri, headers, body, &request_id, &log_state).await;
    // 错误体里也带上 id：New API 把上游错误的 `error.message` 原文展示给它的用户，响应头
    // 到不了那一层；人拿着报错截图来问时，id 得就在那句话里。
    let is_error = resp.status().is_client_error() || resp.status().is_server_error();
    let mut local_error = None;
    if is_error {
        (resp, local_error) = annotate_error_response(resp, &request_id).await;
    }
    // 没到上游就被 luban 自己拒掉的请求（坏形态、限流、鉴权、无可用账号……）也进流水：
    // 否则控制台里查不到那个 id，成功率与请求数里也看不见这一批。到过上游的不在这儿记：
    // 正常收尾的由 [`ReqLog`] 在流结束时落库，早退的由 [`log_early_upstream_failure`]
    // 就地写带账号的那条——两者都会把 `logged` 置真。
    // 本地回放的 200（[`RequestLogState::local_replay`]）同样补一条：它没到上游、没有凭证，
    // 但客户端确实拿到了一条响应，审计与统计都得看得见。
    let local_replay = log_state.local_replay.lock().take();
    if (is_error || local_replay.is_some())
        && !log_state.logged.load(std::sync::atomic::Ordering::Relaxed)
    {
        let parsed = log_state.parsed.lock().take();
        let mut rec =
            local_reject_record(&local, parsed, resp.status(), local_error, &request_id, started);
        if let Some(tag) = local_replay {
            rec.forensics.rewrites = Some(tag.into());
            rec.cost_usd = Some(0.0);
        }
        spawn_usage_log(store, rec);
    }
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        resp.headers_mut().insert("x-request-id", v.clone());
        resp.headers_mut().insert("x-oneapi-request-id", v);
    }
    resp
}

/// 来访 `X-Request-Id` 能沿用的形态：`[A-Za-z0-9._-]`，1 到 128 位。
///
/// 它要进日志、进库、进错误体，还要原样回给下游，所以只认这套最保守的字符集——UUID、
/// ULID、Stripe/Anthropic 那种 `req_…`、nginx 的 32 位 hex 都在内；带空格、引号、控制字符或
/// 长得离谱的一律当没带，重新生成。
fn acceptable_request_id(v: &str) -> bool {
    (1..=128).contains(&v.len())
        && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// 这条请求的 id：沿用来访合法的 `X-Request-Id`，否则生成一个（[`new_request_id`]）。
fn request_id_for(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| acceptable_request_id(v))
        .map(str::to_string)
        .unwrap_or_else(new_request_id)
}

/// 生成的请求 id 里随机部分的长度。base62 的 16 位约 95 bit，比 Stripe 的 14 位略长一点，
/// 碰撞可以不当回事；比 UUID 的 36 位短一半，日志与报错里更好认、好抄。
const REQUEST_ID_RANDOM_LEN: usize = 16;

/// 生成一个请求 id，形态照 Stripe / Anthropic 那套：`req_` + base62 随机串，如
/// `req_Q3k9ZpL2mNv7Xb1c`。前缀说明「这是一个请求 id」，混在别的 id（账号、设备、会话）里
/// 一眼能认出来；随机部分只用字母数字，进日志、进 URL、进错误体都不必转义。
pub(super) fn new_request_id() -> String {
    use rand::distr::Alphanumeric;
    let tail: String =
        rand::rng().sample_iter(Alphanumeric).take(REQUEST_ID_RANDOM_LEN).map(char::from).collect();
    format!("req_{tail}")
}

/// 一条请求进 [`handle_inner`] 之前留下的几样，本地拒绝时拿来写流水（见 [`local_reject_record`]）。
struct LocalRejectCtx {
    method: String,
    path: String,
    /// 来访 UA，缺失时是 `-` 占位（[`ua_of`]），入库前还原成 `None`。
    ua: String,
    /// 头上的会话 id；体里那个在 [`ParsedRequestBits`] 里（解析过体才有）。
    session_header: Option<String>,
}

/// [`handle`] 与 [`handle_inner`] 之间关于「这条的流水谁来写」的约定。
#[derive(Default)]
struct RequestLogState {
    /// 流水已经有人写了：建了 [`ReqLog`]（到过上游、正常收尾），或早退路径就地写了
    /// （[`log_early_upstream_failure`]）。仍为 false 的 4xx/5xx 才是真正的本地拒绝，由
    /// [`handle`] 按 [`local_reject_record`] 补一条。
    logged: std::sync::atomic::AtomicBool,
    /// 体解析出来之后随手放下的几样，本地拒绝的流水用它。**外层不再解析体**：API key、
    /// 最低版本、头上的会话 RPM/并发这些拒绝都发生在体解析之前，最大 64MB 的未鉴权 JSON
    /// 若在外层再解析一遍，就是给任何人一个白烧 CPU 的入口；解析过的那些也不必解析第二遍。
    parsed: parking_lot::Mutex<Option<ParsedRequestBits>>,
    /// 这条请求在本地以 **200** 收尾、没到上游：回放了学到的上游拒答（按提示词学的
    /// [`REWRITE_REFUSAL_REPLAY`]，或按应用学的 [`REWRITE_APP_REFUSAL_REPLAY`]），或是探针
    /// 命中后本地作答的那条最小回复（[`REWRITE_PROBE_REPLY`]）。[`handle`]
    /// 只给 4xx/5xx 补本地流水，
    /// 这类 200 若不标出来就从流水、请求查询与统计里消失——0.3.98 把本地 403 改成回放 200
    /// 时正是这样漏掉的。有标签的按本地流水补一条：无凭证、无用量、花费 0、`rewrites` 记标签。
    local_replay: parking_lot::Mutex<Option<&'static str>>,
}

/// 流水 `rewrites` 里标「本地回放了学到的上游拒答」（[`known_refused_prompt`] 命中）。
const REWRITE_REFUSAL_REPLAY: &str = "refusal_replay";
/// 流水 `rewrites` 里标「本地回放了按应用学到的上游拒答」（[`known_app_refusal`] 命中）。
const REWRITE_APP_REFUSAL_REPLAY: &str = "app_refusal_replay";
/// 流水 `rewrites` 里标「探针命中，本地回了一条最小的 200」（[`probe_signature`] 命中、
/// [`probe_reply`] 作答）。
pub(super) const REWRITE_PROBE_REPLY: &str = "probe_reply";

/// 从来访体里读出的、流水要用的三样。见 [`RequestLogState::parsed`]。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ParsedRequestBits {
    model: Option<String>,
    device_id: Option<String>,
    session_id: Option<String>,
}

/// 本地拒绝那条 4xx/5xx（或本地回放那条 200，调用方再改 `rewrites` 与花费）的流水：没选到
/// 账号（`cred_id` 为空——库层本来就把这类行当作「尚未选到凭证就失败的请求」，见
/// `store::prune_orphan_usage_logs`），没有用量、没有出站；记的是来访侧能看到的一切：
/// 路径、UA、模型、设备、会话、状态码、错误类型与文案、请求 id。
///
/// 模型/设备/体里的会话 id 来自 `parsed`——[`handle_inner`] 解析体时放下的那份；拒绝发生在
/// 体解析之前（API key、版本闸、头上的会话限流）时它是 `None`，那类行只有头上的会话 id，
/// 模型与设备为空。这里**绝不**自己解析体，理由见 [`RequestLogState::parsed`]。
fn local_reject_record(
    ctx: &LocalRejectCtx,
    parsed: Option<ParsedRequestBits>,
    status: StatusCode,
    error: Option<LocalErrorFields>,
    request_id: &str,
    started: std::time::Instant,
) -> store::UsageRecord {
    let parsed = parsed.unwrap_or_default();
    let (error_type, error_message) = error.map(|e| (e.etype, e.message)).unwrap_or_default();
    let session_id = ctx.session_header.clone().or(parsed.session_id);
    tracing::debug!(
        method = %ctx.method, path = %ctx.path, ua = %ctx.ua,
        status = status.as_u16(), request_id,
        error_type = %error_type.as_deref().unwrap_or("-"),
        "logging a local rejection to the usage log"
    );
    store::UsageRecord {
        cred_id: None,
        cred_label: String::new(),
        device_id: parsed.device_id,
        model: parsed.model,
        path: ctx.path.clone(),
        ua: Some(ctx.ua.clone()).filter(|u| u != "-"),
        ua_out: None,
        status: status.as_u16(),
        total_ms: i64::try_from(started.elapsed().as_millis()).ok(),
        request_id: Some(request_id.to_string()),
        forensics: store::Forensics {
            session_id,
            error_type,
            error_message,
            rewrites: Some("rejected_locally".into()),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 到过上游、却没走到 [`ReqLog`] 就得返回的那几条路，就地写一条**带账号**的流水并告诉外层
/// 已经写过。四个调用点：401 读体失败、401 换不到号（回 403 或原样透传）、429 判定「套餐不含
/// 这个模型」且换不到号（回 403）、`upstream.send()` 连接层失败（回 502）。
///
/// 不写的话外层会把它们当本地拒绝补一条：`cred_id` 空、没出站 UA、没上游 request-id，还挂着
/// `rejected_locally`——把一次真实的上游失败伪装成「没出门」。这里记的是与 [`ReqLog`] 同一套
/// 字段：账号、出站 UA、出站体形态摘要、出口代理、上游 request-id、上游错误类型与文案、
/// 429 那条还带额度窗口。`status` 是**客户端拿到的**状态码（同 `ReqLog` 的口径）。
///
/// 换号 `continue` 的那些不在这儿记：那一发在旧号上的失败由遥测报（[`record_early_failure`]），
/// 流水只记这条请求最终落在哪个号、什么结果——与 429 换号、签名降级重试的口径一致（一条
/// 请求一行）。
#[allow(clippy::too_many_arguments)]
fn log_early_upstream_failure(
    state: &AppState,
    log_state: &RequestLogState,
    cred: &crate::credentials::Credential,
    upstream: &Upstream<'_>,
    sent: &Bytes,
    f: EarlyUpstreamFailure<'_>,
) {
    let mut forensics = capture_forensics(upstream, sent, cred);
    forensics.error_type = f.error_type;
    forensics.error_message = f.error_message;
    forensics.third_party = f.third_party;
    forensics.rewrites = Some(f.tag.to_string());
    let ratelimit = f.ratelimit.cloned().unwrap_or_default();
    let rec = store::UsageRecord {
        cred_id: Some(cred.id),
        cred_label: cred.label.clone(),
        device_id: f.device_id,
        model: f.model,
        path: f.path.to_string(),
        ua: Some(f.client_ua.to_string()).filter(|u| u != "-"),
        ua_out: Some(ua_of(&upstream.headers)),
        status: f.status.as_u16(),
        total_ms: i64::try_from(f.started.elapsed().as_millis()).ok(),
        unified_status: ratelimit.unified_status.clone(),
        rl_5h_status: ratelimit.five_h_status.clone(),
        rl_5h_reset: ratelimit.five_h_reset,
        rl_5h_utilization: ratelimit.five_h_utilization,
        rl_7d_status: ratelimit.seven_d_status.clone(),
        rl_7d_reset: ratelimit.seven_d_reset,
        rl_7d_utilization: ratelimit.seven_d_utilization,
        rl_representative: ratelimit.representative.clone(),
        rl_overage_in_use: ratelimit.overage_in_use,
        windows: ratelimit.windows(),
        ratelimit_raw: (!ratelimit.raw.is_empty()).then(|| ratelimit.raw.clone()),
        request_id: Some(f.request_id.to_string()),
        upstream_request_id: f.upstream_request_id.map(str::to_string),
        forensics,
        ..Default::default()
    };
    log_state.logged.store(true, std::sync::atomic::Ordering::Relaxed);
    spawn_usage_log(state.store.clone(), rec);
}

/// [`log_early_upstream_failure`] 的入参：那几条路各自手里有的东西。
struct EarlyUpstreamFailure<'a> {
    path: &'a str,
    client_ua: &'a str,
    model: Option<String>,
    /// 与 [`ReqLog`] 的 `device_id` 同口径：来访的，没有就用模拟派生的（[`sim_device_id`]）。
    device_id: Option<String>,
    started: std::time::Instant,
    request_id: &'a str,
    upstream_request_id: Option<&'a str>,
    /// 客户端拿到的状态码。
    status: StatusCode,
    error_type: Option<String>,
    error_message: Option<String>,
    third_party: bool,
    ratelimit: Option<&'a RateLimitInfo>,
    /// 改写标签：`upstream_401` / `model_unsupported` / `connection_error`。
    tag: &'static str,
}

/// 从一条错误 JSON 里读出的 `error.type` / `error.message`（改写前的原文，不含追加的请求 id）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct LocalErrorFields {
    etype: Option<String>,
    message: Option<String>,
}

impl LocalErrorFields {
    fn from_json(bytes: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let err = v.get("error")?;
        let field = |k: &str| err.get(k).and_then(|x| x.as_str()).map(str::to_string);
        Some(Self { etype: field("type"), message: field("message") })
    }
}

/// 错误体最多收多大来改写。错误 JSON 通常几百字节；超过这个数的不是错误体，原样放行不碰。
const ERROR_BODY_ANNOTATE_LIMIT: usize = 256 * 1024;

/// 把 luban 的请求 id 写进一条 4xx/5xx 的 JSON 错误体，见 [`annotate_error_json`]。
///
/// 只碰 `content-type` 含 `json` 的响应：SSE、空体、非 JSON 的网关错误一律不动。收 body 会把
/// 上游流读完，这正好也驱动了 [`ReqLog`] 的收尾——它挂在流上，读完才落日志。收不下来
/// （超过上限）时 body 已经丢了，只能回一条自己的 502，至少把 id 带出去。
///
/// 顺手把改写前的 `error.type` / `error.message` 读出来交回去（[`LocalErrorFields`]）：本地拒绝
/// 写流水要用，而体在这儿已经收进内存了，别让调用方再收一遍。不是 JSON 或收不下来时为 `None`。
async fn annotate_error_response(
    resp: Response,
    request_id: &str,
) -> (Response, Option<LocalErrorFields>) {
    let is_json = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("json"));
    if !is_json {
        return (resp, None);
    }
    let (mut parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, ERROR_BODY_ANNOTATE_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(request_id, error = %e, "failed to buffer an error body for annotation");
            return (
                error_response(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!(
                        "failed to read the upstream error body: {e} (luban request id: {request_id})"
                    ),
                ),
                None,
            );
        }
    };
    let fields = LocalErrorFields::from_json(&bytes);
    let out = annotate_error_json(&bytes, request_id).map(Bytes::from).unwrap_or(bytes);
    // 长度变了，交给 hyper 按新 body 重算。
    parts.headers.remove(header::CONTENT_LENGTH);
    (Response::from_parts(parts, Body::from(out)), fields)
}

/// 在错误 JSON 里写入 luban 的请求 id：`error.message` 末尾追加 `(luban request id: req_…)`，
/// 顶层加 `luban_request_id`。不是 JSON 对象时返回 `None`（原样放行）。
///
/// 两处都写：`message` 是人最终看到的那句话（New API 把它原文转给用户，工单截图里就是它）；
/// 顶层字段给程序读。**不覆盖**上游自带的顶层 `request_id`（那是 Anthropic 的 `req_…`，
/// 对它们的工单要用），故另起一个名字。`message` 里已经带着这个 id 时不重复追加。
fn annotate_error_json(bytes: &[u8], request_id: &str) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let obj = v.as_object_mut()?;
    if let Some(msg) =
        obj.get_mut("error").and_then(|e| e.as_object_mut()).and_then(|e| e.get_mut("message"))
        && let Some(text) = msg.as_str()
        && !text.contains(request_id)
    {
        *msg = serde_json::Value::String(format!("{text} (luban request id: {request_id})"));
    }
    obj.insert("luban_request_id".into(), serde_json::Value::String(request_id.to_string()));
    serde_json::to_vec(&v).ok()
}

/// 来访请求头里带的请求 id（若有）：New API 用 `{client_header:…}` 透传的终端用户 id、
/// 官方 SDK 的 `x-client-request-id` 等。只记日志，不作主键——它不是 luban 发的，可能重复或缺失。
fn client_request_id(headers: &HeaderMap) -> Option<String> {
    ["x-oneapi-request-id", "x-request-id", "x-client-request-id"]
        .iter()
        .find_map(|k| headers.get(*k).and_then(|v| v.to_str().ok()))
        .map(|v| v.chars().take(128).collect())
}

/// 取一个响应头的文本值，缺失或非 UTF-8 时为 `None`（与 [`header_text`] 的 `"-"` 占位相对）。
pub(super) fn header_opt(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// 生效的接入 key：启动时 `--api-key`/env 覆盖优先，否则用库中网页配置的值。
fn effective_client_key(state: &AppState) -> Option<String> {
    if let Some(k) = &state.client_key {
        return Some(k.to_string());
    }
    state.store.get_setting(store::CLIENT_API_KEY).ok().flatten().filter(|s| !s.trim().is_empty())
}

/// 校验来访身份：`x-api-key: <key>` 或 `Authorization: Bearer <key>`。
fn client_authorized(headers: &HeaderMap, expected: &str) -> bool {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok())
        && v == expected
    {
        return true;
    }
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
        && v.strip_prefix("Bearer ").map(str::trim) == Some(expected)
    {
        return true;
    }
    false
}

/// 把 16 字节按 uuid v4 的形态格式化（打上 version/variant 位，小写带连字符）。
/// 随机来源见 [`uuid_v4`]，派生来源见 [`Simulation::session_id`]。
pub(crate) fn uuid_from_bytes(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!("{}-{}-{}-{}-{}", h(&b[0..4]), h(&b[4..6]), h(&b[6..8]), h(&b[8..10]), h(&b[10..16]))
}

/// 官方额度探测恒用的模型。四份抓包（`cap/2.1.258/00004`、`cap/2.1.260-2/00004`/`00021`/
/// `00047`）逐字相同——它是「查额度」用的最便宜的那个。
pub(super) const QUOTA_PROBE_MODEL: &str = "claude-haiku-4-5-20251001";

/// 这条请求是不是官方那条额度探测，**逐字比对整个形状**。
///
/// 官方那条（`cap/2.1.258/00004`、`cap/2.1.260-2/00004`/`00021`/`00047`，四份逐字节相同，
/// 只有 UA 的版本号不同）：
///
/// ```text
/// {"model":"claude-haiku-4-5-20251001","max_tokens":1,
///  "messages":[{"role":"user","content":"quota"}],"metadata":{…}}
/// ```
///
/// **判据必须窄。** 只看「没有 `system` + `max_tokens:1`」的话，任何客户端的一 token 探活
/// 都会被认成额度探测，跟着就被免掉流式化、免掉 `system` 前缀——而它需要那个前缀才能用上
/// 订阅额度。宁可漏认（退回 helper，照常走通用路径），也不能错认。
pub(super) fn is_quota_probe_shaped(v: &serde_json::Value) -> bool {
    if v.get("system").is_some() || v.get("tools").is_some() {
        return false;
    }
    if v.get("max_tokens").and_then(|m| m.as_u64()) != Some(1) {
        return false;
    }
    // **逐字比规范名**，不是「名字里带 haiku」。四份抓包里恒为
    // `claude-haiku-4-5-20251001`；只比子串的话，haiku-3 / 3.5 / 将来某个 haiku 的一 token
    // 请求都会被认成官方 4.5 那条探测。
    if v.get("model").and_then(|m| m.as_str()) != Some(QUOTA_PROBE_MODEL) {
        return false;
    }
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else { return false };
    let [only] = msgs.as_slice() else { return false };
    if only.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    // 正文恰好是 `quota` 这一个词（字符串形态，或单个 text 块）。
    match only.get("content") {
        Some(serde_json::Value::String(s)) => s == "quota",
        Some(serde_json::Value::Array(blocks)) => match blocks.as_slice() {
            [b] => {
                b.get("type").and_then(|t| t.as_str()) == Some("text")
                    && b.get("text").and_then(|t| t.as_str()) == Some("quota")
            }
            _ => false,
        },
        _ => false,
    }
}

/// 来访自己那串 `anthropic-beta`，切成逐项。给 [`CcRequestKind::of`] 用。
pub(super) fn inbound_beta_list(headers: &HeaderMap) -> Vec<String> {
    headers
        .get("anthropic-beta")
        .and_then(|x| x.to_str().ok())
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default()
}

/// 末条 `role:"user"` 消息的正文（首个 text 块，或字符串 content）是否以 `prefix` 开头。
pub(super) fn last_user_text_starts_with(v: &serde_json::Value, prefix: &str) -> bool {
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else { return false };
    let Some(last) =
        msgs.iter().rev().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    else {
        return false;
    };
    let text = match last.get("content") {
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        Some(serde_json::Value::Array(blocks)) => {
            blocks.iter().find_map(|b| b.get("text").and_then(|t| t.as_str()))
        }
        _ => None,
    };
    text.is_some_and(|t| t.trim_start().starts_with(prefix))
}

/// 写入一个顶层字段，并把**新增**的那个放到官方 key 序里该在的位置：`after` 里最靠后的
/// 那个已有键之后（一个都没有就追加在末尾）。字段本来就在时原位替换，位置不动。
///
/// 官方线序是 `model → messages → system → tools → metadata → max_tokens → … → stream`，
/// 直接 append 会让补出来的 `system`/`metadata` 落到 `stream` 后面。key 顺序是这条链路上
/// 唯一还留得住的形态信息（body 全程 `preserve_order`，见 [`rewrite_body`]），既然要装，
/// 就装到底。注意来访客户端自己那部分 key 序照旧不动——那是它的形态，不是我们要改的。
pub(super) fn insert_top_level(
    v: &mut serde_json::Value,
    key: &str,
    value: serde_json::Value,
    after: &[&str],
) {
    let Some(obj) = v.as_object_mut() else { return };
    if obj.contains_key(key) {
        obj.insert(key.into(), value);
        return;
    }
    let at = after.iter().filter_map(|k| obj.keys().position(|have| have == k)).max();
    match at {
        Some(at) => obj.shift_insert(at + 1, key.into(), value),
        None => obj.insert(key.into(), value),
    };
}

/// 递归数出 body 里现有的 `cache_control` 个数（上游按整条请求算，不只是 `system`）。
pub(super) fn count_cache_control(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Object(map) => {
            let here = usize::from(map.contains_key("cache_control"));
            here + map.values().map(count_cache_control).sum::<usize>()
        }
        serde_json::Value::Array(items) => items.iter().map(count_cache_control).sum(),
        _ => 0,
    }
}

/// 给没有 `metadata.user_id` 的请求造一个官方形态的身份（键序与 CC 一致：
/// `device_id` → `account_uuid` → `session_id`，紧凑 JSON 塞在字符串里）。
///
/// 两条路都用它，区别只在 `session_id` 从哪来：模拟路径取 [`Simulation::session_id`]，
/// 非模拟路径取 [`bare_session_id`]（优先用来访自己那个头的值）。
///
/// 客户端自己带了 `user_id` 就不动——那条交给 [`spoof_identity`] 按原格式定点改写，
/// 两条路只能有一条动它。凭证没有 `account_uuid`（旧库未回填）时返回 `false` 不造：
/// 一个 `account_uuid` 为空、`device_id` 却是 64 位 hex 的组合，真实客户端不产生。
pub(super) fn ensure_cc_metadata(
    v: &mut serde_json::Value,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    session_id: &str,
) -> bool {
    if v.get("metadata").and_then(|m| m.get("user_id")).is_some() {
        return false;
    }
    let account_uuid = match cred.account_uuid.as_deref() {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => return false,
    };
    let Some(device_id) = cred.spoof_device_id(device_fp) else { return false };

    let mut inner = serde_json::Map::new();
    inner.insert("device_id".into(), device_id.into());
    inner.insert("account_uuid".into(), account_uuid.into());
    inner.insert("session_id".into(), session_id.into());
    // 紧凑序列化（无空白），与 CC 发的那串形态一致。
    let user_id = serde_json::Value::Object(inner).to_string();

    // metadata 已经是个对象（只是没有 user_id）就往里塞，否则整个造一个——后者也覆盖掉
    // 「metadata 存在但不是对象」这种畸形值。位置按官方 key 序落在 tools/system 之后。
    if let Some(meta) = v.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        meta.insert("user_id".into(), user_id.into());
        return true;
    }
    let mut meta = serde_json::Map::new();
    meta.insert("user_id".into(), user_id.into());
    insert_top_level(
        v,
        "metadata",
        serde_json::Value::Object(meta),
        &["tools", "system", "messages"],
    );
    true
}

/// 读取请求体声明的模型名（顶层 `model`）。用于按模型分格的限流冷却，见
/// [`rate_limit_scope`]。解析失败或没有该字段时返回 `None`（退化为账号级冷却）。
fn request_model(body: Option<&serde_json::Value>) -> Option<String> {
    Some(body?.get("model")?.as_str()?.to_string())
}

/// 读取请求体声明的速度档（顶层 `speed` 字段，如 `"fast"`；配套 header
/// `anthropic-beta: fast-mode-*`）。解析失败或没有该字段时返回 `None`。
fn request_speed(body: Option<&serde_json::Value>) -> Option<String> {
    Some(body?.get("speed")?.as_str()?.to_string())
}

/// 读取请求体声明的输出上限（顶层 `max_tokens`）。解析失败或没有该字段时返回 `None`。
///
/// **我们从不改这个字段**（理由见 [`ensure_context_management`] 文档里的第 1 条：改掉它等于
/// 替客户端决定费用天花板），故它就是上游那套「每分钟输出 token」限额实际预扣的那个数，
/// 拿它来解释裸 429 是站得住的，见 [`UpstreamLoad`]。
pub(super) fn request_max_tokens(body: Option<&serde_json::Value>) -> Option<i64> {
    body?.get("max_tokens")?.as_i64()
}

/// 按 Anthropic 的错误体形态打一份 JSON（`{"type":"error","error":{...}}`）。
///
/// 本地拒绝也要长成上游那副样子，客户端才认得——它只会去读 `error.message`。
/// **不带 `request_id`**：这次请求根本没出去，编一个只会把人引去查一条不存在的记录。
fn error_body(etype: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type": "error",
        "error": {"type": etype, "message": message}}))
    .unwrap_or_else(|_| b"{\"type\":\"error\"}".to_vec())
}

/// luban 自己产生的一条错误响应：状态码 + `content-type: application/json` + [`error_body`]。
///
/// **转发路径上回给客户端的错误一律走它**，别再直接 `(StatusCode, "一句话")`：那样发出去的是
/// `text/plain`，而客户端（官方 SDK、各类第三方 SDK）都按 JSON 读错误体——解不出来时它们只
/// 能退回一句按状态码编的通用话，我们精心写的那句原因就此丢掉，客户端还可能因此走上与
/// 上游真实错误不同的重试分支。上游的错误体本来就是这个形态，本地拒绝长得一样，客户端才不必
/// 分辨这条错误是谁产生的。
///
/// `etype` 用 Anthropic 那套取值：`authentication_error` / `permission_error` /
/// `invalid_request_error` / `rate_limit_error` / `api_error`。
pub(super) fn error_response(
    status: StatusCode,
    etype: &str,
    message: impl AsRef<str>,
) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], error_body(etype, message.as_ref()))
        .into_response()
}

/// 限流那条错误响应：429 + `retry-after` + JSON 错误体。
///
/// 单拎出来是因为 `retry-after` 这个头不能漏——三处限流（会话、设备、账号/裸请求）的等待
/// 时间都是**算得准**的，把它带上客户端才知道该等多久，而不是立刻再撞一次。
pub(super) fn rate_limit_response(retry_after_secs: i64, message: impl AsRef<str>) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [
            (header::RETRY_AFTER, retry_after_secs.to_string()),
            (header::CONTENT_TYPE, "application/json".to_string()),
        ],
        error_body("rate_limit_error", message.as_ref()),
    )
        .into_response()
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::{HeaderValue, StatusCode, header, request_speed, store};

    /// 本地拒绝的响应体必须是**上游那副 JSON 形态**，且 `content-type` 说的就是 JSON。
    ///
    /// 曾经这几条是 `(StatusCode, "一句话")`，发出去是 `text/plain`：客户端按 JSON 读错误体，
    /// 读不出来就退回一句按状态码编的通用话，我们写的原因（等多久、缺哪个字段、该升到哪版）
    /// 全丢了。限流那条还要盯住 `retry-after`——它是「该等多久」的唯一来源，丢了客户端就只能
    /// 立刻再撞一次。
    #[tokio::test]
    async fn local_rejections_speak_the_upstream_error_shape() {
        async fn parts(
            resp: super::Response,
        ) -> (StatusCode, Option<String>, Option<String>, serde_json::Value) {
            let status = resp.status();
            let ctype = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let retry = resp
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
            (status, ctype, retry, serde_json::from_slice(&bytes).expect("错误体必须是 JSON"))
        }

        let (status, ctype, retry, body) =
            parts(super::error_response(StatusCode::FORBIDDEN, "permission_error", "nope")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(ctype.as_deref(), Some("application/json"));
        assert_eq!(retry, None, "非限流的错误不该凭空带上 retry-after");
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "permission_error");
        assert_eq!(body["error"]["message"], "nope");

        let (status, ctype, retry, body) = parts(super::rate_limit_response(41, "slow down")).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(ctype.as_deref(), Some("application/json"));
        assert_eq!(retry.as_deref(), Some("41"), "限流必须带 retry-after");
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["message"], "slow down");
    }

    /// 本地拒绝也进流水：没选到账号（`cred_id` 空），但路径、UA、模型、设备、会话、状态、
    /// 错误类型/文案与请求 id 都在，控制台按 id 能查到。模型/设备/体里的会话来自 handle_inner
    /// 解析体时放下的那份，这里自己不解析体。
    #[test]
    fn a_local_rejection_becomes_a_usage_row_without_a_credential() {
        let ctx = super::LocalRejectCtx {
            method: "POST".into(),
            path: "/v1/messages?beta=true".into(),
            ua: "python-httpx/0.27.0".into(),
            session_header: None,
        };
        let parsed = super::ParsedRequestBits {
            model: Some("claude-sonnet-5".into()),
            device_id: Some("abc123".into()),
            session_id: Some("11111111-2222-4333-8444-555555555555".into()),
        };
        let err = super::LocalErrorFields {
            etype: Some("invalid_request_error".into()),
            message: Some("messages.0.role: system is not accepted".into()),
        };
        let rec = super::local_reject_record(
            &ctx,
            Some(parsed),
            StatusCode::BAD_REQUEST,
            Some(err),
            "req_test0001",
            std::time::Instant::now(),
        );
        assert_eq!(rec.cred_id, None);
        assert_eq!(rec.cred_label, "");
        assert_eq!(rec.status, 400);
        assert_eq!(rec.path, "/v1/messages?beta=true");
        assert_eq!(rec.ua.as_deref(), Some("python-httpx/0.27.0"));
        assert_eq!(rec.ua_out, None, "没出站");
        assert_eq!(rec.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(rec.device_id.as_deref(), Some("abc123"));
        assert_eq!(
            rec.forensics.session_id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555"),
            "头上没有就取体里的会话段"
        );
        assert_eq!(rec.forensics.error_type.as_deref(), Some("invalid_request_error"));
        assert_eq!(
            rec.forensics.error_message.as_deref(),
            Some("messages.0.role: system is not accepted")
        );
        assert_eq!(rec.forensics.rewrites.as_deref(), Some("rejected_locally"));
        assert_eq!(rec.request_id.as_deref(), Some("req_test0001"));
        assert!(!rec.has_usage);
        assert!(rec.total_ms.is_some());

        // 体解析之前就拒掉的（API key / 版本闸 / 头上的会话限流）：没有 parsed，模型、设备都空，
        // 会话只认头上的；UA 占位 `-` 还原成 NULL。
        let ctx = super::LocalRejectCtx {
            ua: "-".into(),
            session_header: Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into()),
            ..ctx
        };
        let rec = super::local_reject_record(
            &ctx,
            None,
            StatusCode::TOO_MANY_REQUESTS,
            None,
            "req_test0002",
            std::time::Instant::now(),
        );
        assert_eq!(rec.status, 429);
        assert_eq!(rec.model, None);
        assert_eq!(rec.device_id, None);
        assert_eq!(rec.ua, None);
        assert_eq!(
            rec.forensics.session_id.as_deref(),
            Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee")
        );
        assert_eq!(rec.forensics.error_type, None);
    }

    /// 错误体改写时顺手读出的 type/message 是**改写前**的原文，不带追加的请求 id。
    #[tokio::test]
    async fn annotating_an_error_reports_the_original_error_fields() {
        let resp = super::error_response(StatusCode::BAD_REQUEST, "invalid_request_error", "nope");
        let (resp, fields) = super::annotate_error_response(resp, "req_x").await;
        assert_eq!(
            fields,
            Some(super::LocalErrorFields {
                etype: Some("invalid_request_error".into()),
                message: Some("nope".into())
            })
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["message"], "nope (luban request id: req_x)", "体照常改写");

        // 非 JSON 的响应不碰也不读。
        let plain =
            axum::response::IntoResponse::into_response((StatusCode::BAD_GATEWAY, "upstream down"));
        let (_, fields) = super::annotate_error_response(plain, "req_y").await;
        assert_eq!(fields, None);
    }

    /// 请求 id 取法：来访合法的 X-Request-Id 沿用（UUID / ULID / req_… / 32 位 hex 都算），
    /// 带空格、引号、超长或没带的一律生成 `req_` + 16 位 base62。
    #[test]
    fn request_id_reuses_a_sane_inbound_x_request_id_else_generates_one() {
        let with = |v: &str| {
            let mut h = super::HeaderMap::new();
            h.insert("x-request-id", HeaderValue::from_str(v).unwrap());
            super::request_id_for(&h)
        };
        for ok in [
            "550e8400-e29b-41d4-a716-446655440000",
            "01J9Z3QK5V8N2X6M4P7R9T1W3Y",
            "req_01ABCxyz",
            "9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d",
        ] {
            assert_eq!(with(ok), ok, "合法的应沿用");
        }
        let is_generated = |s: &str| {
            s.len() == 4 + super::REQUEST_ID_RANDOM_LEN
                && s.starts_with("req_")
                && s[4..].bytes().all(|b| b.is_ascii_alphanumeric())
        };
        for bad in ["has space", "quote\"d", "", &"x".repeat(129)] {
            let got = with(bad);
            assert!(is_generated(&got), "{bad:?} 不合法应重新生成，得到 {got}");
        }
        assert!(is_generated(&super::request_id_for(&super::HeaderMap::new())), "没带就生成");
        assert_ne!(super::new_request_id(), super::new_request_id(), "随机，不该重复");
        // 前后空白容忍。
        assert_eq!(with("  abc-123  "), "abc-123");
    }

    /// 错误体里带 luban 请求 id：message 末尾追加、顶层加 luban_request_id、不动上游的 request_id。
    #[test]
    fn error_bodies_carry_the_luban_request_id() {
        let rid = "lb-0000-test";
        // 上游形态（带 Anthropic 自己的 request_id）。
        let up = br#"{"type":"error","error":{"type":"invalid_request_error","message":"`temperature` is deprecated for this model."},"request_id":"req_01ABC"}"#;
        let out = super::annotate_error_json(up, rid).expect("是 JSON 对象");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["error"]["message"],
            "`temperature` is deprecated for this model. (luban request id: lb-0000-test)"
        );
        assert_eq!(v["request_id"], "req_01ABC", "上游的 request_id 不能被盖掉");
        assert_eq!(v["luban_request_id"], rid);
        // 再来一遍不重复追加。
        let again: serde_json::Value =
            serde_json::from_slice(&super::annotate_error_json(&out, rid).unwrap()).unwrap();
        assert_eq!(again["error"]["message"], v["error"]["message"]);
        // luban 自己的错误体同样处理。
        let own = super::error_body("permission_error", "nope");
        let v: serde_json::Value =
            serde_json::from_slice(&super::annotate_error_json(&own, rid).unwrap()).unwrap();
        assert_eq!(v["error"]["message"], "nope (luban request id: lb-0000-test)");
        // 不是 JSON 对象：放行。
        assert!(super::annotate_error_json(b"Error", rid).is_none());
        assert!(super::annotate_error_json(b"[1,2]", rid).is_none());
    }

    /// 请求体顶层 `speed` 字段能被读出；缺字段/非法 JSON 返回 None（不阻断转发）。
    ///
    /// 入参是**已解析**的 body（handler 全程只解析一次，见 [`super::handle`]），故「非法
    /// JSON」在这里表现为 `None`——解析失败那步已经在上游发生了。
    #[test]
    fn reads_speed_from_request_body() {
        let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).ok();
        let with = parse(r#"{"model":"claude-opus-5","speed":"fast","messages":[]}"#);
        assert_eq!(request_speed(with.as_ref()).as_deref(), Some("fast"));
        let without = parse(r#"{"model":"claude-opus-5","messages":[]}"#);
        assert_eq!(request_speed(without.as_ref()), None);
        assert_eq!(parse("not json"), None, "非法 JSON 在解析那步就是 None");
        assert_eq!(request_speed(None), None);
    }

    /// 不带设备身份的探活走**整条 [`handle`]**：拿到的是本地那条 200（而不是设备身份闸的
    /// 403），头上带标记、体是一条正常回复，并且流水里落了一条标 `probe_reply`、花费 0、
    /// 没有凭证的本地记录。
    ///
    /// 守的是两件真出过问题的事：一、`require_device_id` 默认开着，不带 `metadata.user_id` 的
    /// 计费请求在 2.2 就是一条 403，而封号复盘里 Go-http-client 那批探活恰恰不带身份、探针判据
    /// 也明确允许 `device_id` 为 `None`——两段一旦调了个个儿，这类探活又会拿到 403，下游照旧把
    /// 整个 key 摘下去；二、本地以 200 收尾的请求不标 `local_replay` 就会从流水、请求查询与
    /// 统计里整个消失（0.3.98 把本地 403 改成回放 200 时正是这样漏掉的）。
    #[tokio::test]
    async fn a_device_less_probe_is_answered_locally_and_logged() {
        let store = std::sync::Arc::new(store::CredentialStore::open_in_memory().unwrap());
        assert!(store.require_device_id(), "这条用例的前提是设备身份闸开着");
        assert!(store.forward_flags().reject_probes, "这条用例的前提是探针开关开着");
        let state = crate::web::AppState::for_test(store.clone());

        // 复盘里那批 Go-http-client 探活的形态：没有 metadata（即没有 device_id）、没有 system、
        // 没有 tools、一条消息、max_tokens 8。
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 8,
            "messages": [{ "role": "user", "content": "ping" }]});
        let mut headers = super::HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(header::USER_AGENT, HeaderValue::from_static("Go-http-client/1.1"));
        let resp = super::handle(
            axum::extract::State(state),
            axum::http::Method::POST,
            "/v1/messages".parse::<axum::http::Uri>().unwrap(),
            headers,
            axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK, "设备身份闸不该抢在探针前面回一条 403");
        assert_eq!(resp.headers().get(super::LOCAL_REPLY_HEADER).unwrap(), "probe_reply");
        assert_eq!(resp.headers().get(super::PROBE_KIND_HEADER).unwrap(), "ping");
        assert!(resp.headers().get("x-request-id").is_some(), "本地作答也要带请求 id");
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let msg: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(msg["content"][0]["text"], super::PROBE_REPLY_TEXT);
        assert_eq!(msg["model"], "claude-opus-5");
        assert!(msg["id"].as_str().unwrap().starts_with(super::PROBE_REPLY_ID_PREFIX));

        // 流水是 `spawn_blocking` 写的，等它落下来（最多 2 秒，正常几毫秒）。
        let mut rows = Vec::new();
        for _ in 0..200 {
            rows = store.list_usage_logs(10).unwrap();
            if !rows.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(rows.len(), 1, "本地以 200 收尾的探针也要进流水");
        let row = &rows[0];
        assert_eq!(row.status, 200);
        assert_eq!(row.path, "/v1/messages");
        assert_eq!(row.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(row.forensics.rewrites.as_deref(), Some(super::REWRITE_PROBE_REPLY));
        assert_eq!(row.cost_usd, Some(0.0));
        assert_eq!(row.cred_id, None, "没到上游，不该挂在任何账号上");
        assert!(!row.has_usage, "没到上游，没有用量");
    }

    /// 开关关掉之后同一条探活照常往下走（走到没有可用账号那步），确认上面那条 200 是
    /// `reject_probes` 给的，而不是别的什么早退路径顺手回的。
    #[tokio::test]
    async fn the_same_probe_goes_on_when_the_switch_is_off() {
        let store = std::sync::Arc::new(store::CredentialStore::open_in_memory().unwrap());
        // `"0"` / `"false"` 才算关，别的取值一律算开，见 `store::setting_is_on`。
        store.set_setting(store::REJECT_PROBES, "0").unwrap();
        store.set_setting(store::REQUIRE_DEVICE_ID, "0").unwrap();
        let state = crate::web::AppState::for_test(store.clone());
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 8,
            "messages": [{ "role": "user", "content": "ping" }]});
        let mut headers = super::HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(header::USER_AGENT, HeaderValue::from_static("Go-http-client/1.1"));
        let resp = super::handle(
            axum::extract::State(state),
            axum::http::Method::POST,
            "/v1/messages".parse::<axum::http::Uri>().unwrap(),
            headers,
            axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
        )
        .await;
        // 关掉之后这条探活照常往下走，最终停在「没有可用账号」那一步（本地库里一个号都没有），
        // 而不是由 2.1a 就地作答。
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(resp.headers().get(super::LOCAL_REPLY_HEADER).is_none());
    }

    /// `max_tokens` 的读法：只认顶层的整数，读不出来一律 `None`（日志里落成 0）。
    #[test]
    fn request_max_tokens_reads_the_declared_output_cap() {
        let mt = |s: &str| {
            let v: serde_json::Value = serde_json::from_str(s).unwrap();
            super::request_max_tokens(Some(&v))
        };
        assert_eq!(mt(r#"{"max_tokens":64000}"#), Some(64000));
        assert_eq!(mt(r#"{"model":"claude-sonnet-5"}"#), None, "没写就是没写，别猜一个默认值");
        assert_eq!(mt(r#"{"max_tokens":"64000"}"#), None, "字符串不算——上游认的是整数");
        assert_eq!(super::request_max_tokens(None), None, "body 不是 JSON 时同样读不出");
    }
}
