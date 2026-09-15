use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::Response,
};

use crate::config;
use crate::store;
use crate::web::AppState;

use super::ban::{
    detect_account_ban, header_text, is_third_party_rejection, log_third_party_rejection,
    parse_upstream_error,
};
use super::body::{
    below_min_client_version, body_has_user_id, build_tool_name_map, cc_cli_version,
    client_supplied_fallbacks, device_fingerprint, ensure_beta_query, extract_device_id,
    extract_session_id, is_billable_messages, is_fallback_rejection, known_latest_release,
    outbound_carries_fallbacks, outbound_ua, refusal_fallbacks_for, remember_fallback_rejection,
    sim_device_id, stream_requested, trusted_cc_version, ua_of,
};
use super::connectivity::{session_start, spawn_session_handshake};
use super::digest::{redact_headers, request_digest};
use super::headers::build_forward_headers_for;
use super::learned_rules::{
    MODEL_DENIAL_MAX_SWAPS, RejectionLog, TRANSIENT_MAX_ATTEMPTS, app_system_digest,
    empty_reply_class, has_deprecated_sampling_field, has_learned_deprecated_field, is_max_plan,
    known_app_refusal, known_empty_reply, known_refused_prompt, known_shape_rejection,
    maybe_strip_deprecated, model_rejects_sampling, next_transient_backoff, prompt_digest,
    remember_deprecated_field, remember_shape_rejection, replay_refusal, take_rejection_log_slot,
};
use super::logging::{
    ReqLog, UsageSniffer, ban_context, capture_forensics, record_early_failure, telemetry_capture,
};
use super::openai_marker::find_openai_marker;
use super::probe_detect::{probe_reply, probe_signature};
use super::rate_limit::{
    LimitScope, RateLimitInfo, park_if_quota_nearly_exhausted, park_rate_limited,
    rate_limit_scope_for,
};
use super::session_id::{
    bare_session_id, incoming_session_id, outbound_session_id, session_id_conflict,
};
use super::session_link::{CcRequestKind, client_session_link};
use super::simulation::{Simulation, inbound_facts, is_cc_shaped, simulates_cc};
use super::thinking::{
    error_block_path, is_empty_thinking_error, is_redacted_thinking_data_error,
    is_thinking_modified_error, is_thinking_signature_error, retry_demoted_thinking,
    retry_without_prefill, thinking_block_error_kind, trace_thinking_block,
};
use super::upstream::{
    InFlightGuard, SessionConcurrencyGuard, Upstream, UpstreamRouteGuard, error_chain,
    has_trailing_assistant, is_prefill_not_supported_error, model_rejects_prefill,
    note_upstream_send, relay_upstream, resp_builder, resp_shape, retry_without_fallbacks,
    strip_assistant_prefill, try_acquire_session_concurrency, upstream_error_kind,
    upstream_load_snapshot,
};
use super::{
    EarlyUpstreamFailure, ParsedRequestBits, REWRITE_APP_REFUSAL_REPLAY, REWRITE_PROBE_REPLY,
    REWRITE_REFUSAL_REPLAY, RequestLogState, client_authorized, client_request_id,
    effective_client_key, error_response, header_opt, inbound_beta_list,
    log_early_upstream_failure, rate_limit_response, request_max_tokens, request_model,
    request_speed,
};

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
    let started = std::time::Instant::now();
    let client_request_id = client_request_id(&headers);
    let path_and_query =
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path()).to_string();
    // 来访 UA：转发日志与各条拒绝日志都带上（都是 info/warn，不必开 debug）。整组识别头那条
    // debug 留着不动——排查形态时才需要那六项，日常只要认出「谁在发」，一项就够。
    let client_ua = ua_of(&headers);
    // 在途计数：入口就 +1，随后 move 进 ReqLog 活到响应流结束，见 [`InFlightGuard`]。
    let in_flight = InFlightGuard::new(state.in_flight.clone());

    // 1) 校验来访 API Key（未配置则放行）。生效 key：环境覆盖优先，否则用库中配置。
    if let Some(expected) = effective_client_key(&state)
        && !client_authorized(&headers, &expected)
    {
        tracing::warn!(%method, path = %path_and_query, ua = %client_ua, "rejected: invalid inbound API key");
        return error_response(StatusCode::UNAUTHORIZED, "authentication_error", "invalid API key");
    }

    // 1.5) 最低客户端版本闸：只卡 UA 自报 `claude-cli/<版本>` 的请求，其余一律放行，
    //      判定见 [`below_min_client_version`]。放在这里是因为它只看一个头——比解析 body、
    //      挑账号都便宜，该拒的越早拒越好；也因此它在 API key 之后：先认人，再谈版本。
    if let Some((got, want)) =
        below_min_client_version(&client_ua, state.store.min_client_version().as_deref())
    {
        tracing::warn!(%method, path = %path_and_query, ua = %client_ua, %got, %want, "rejected: client version below the configured minimum");
        return error_response(
            StatusCode::FORBIDDEN,
            "permission_error",
            format!(
                "Claude Code {got} is no longer accepted here; upgrade to {want} or newer \
                 (npm i -g @anthropic-ai/claude-code)"
            ),
        );
    }

    // 1.6) 每会话 RPM 上限（头这一路）：这个会话最近 60 秒发得太多 → 直接 429 + `retry-after`。
    //
    //      **刻意排在 body 解析之前**，这是选会话维度顺带拿到的好处：会话 id 在
    //      `X-Claude-Code-Session-Id` 头上，而设备 id 只存在于 body 里（`metadata.user_id`），
    //      按设备限就非得先把整个 body 解析出来才判得了。长对话几 MB 是常态，一个不退避的
    //      客户端每秒撞十几次，那十几次全额解析纯属白烧 CPU——闸门前移正好把它省掉。
    //
    //      代价是形态拦截（2.3）与设备身份校验（2.2）都排在它后面，即「一条发都发不出去的
    //      请求也会占掉会话的名额」，与设备闸那句注释的取舍相反。这里认这个代价：反复发同一条
    //      坏形态本身就是该被节流的行为（那条路每次也要白解析一遍 body），把它算进窗口比放它
    //      过去更对。
    //
    //      头上没有这个值时不在这里判，等 body 解析出会话 id 再补判（见 2.2b）；两处互斥，
    //      同一条请求只会吃一个名额。官方客户端头体两处逐字相同，故先后两路落在同一个桶里。
    // 体还没解析，只看头；体里那个由 2.2b 补判。
    let session_from_header = incoming_session_id(&headers, None);
    if let Some(sid) = session_from_header.as_deref()
        && let Some(retry) = state.store.take_session_rpm_slot(sid)
    {
        return session_rpm_rejection(
            &state.rejection_log,
            &method,
            &path_and_query,
            &client_ua,
            sid,
            retry,
            "header",
        );
    }

    // 1.7) 每会话并发在途上限（头这一路）：这个会话同时在飞的请求已达上限 → 直接 429。
    //      与 RPM 同理：头上有就在这里判，没有等 body 里拿到 session id 再补判。
    let concurrency_limit = state.store.session_concurrency_limit();
    let mut session_concurrency_guard = if let Some(sid) = session_from_header.as_deref() {
        match try_acquire_session_concurrency(&state.session_concurrency, sid, concurrency_limit) {
            Ok(guard) => guard,
            Err(current) => {
                return session_concurrency_rejection(
                    &state.rejection_log,
                    &method,
                    &path_and_query,
                    &client_ua,
                    sid,
                    current,
                    concurrency_limit,
                    "header",
                );
            }
        }
    } else {
        SessionConcurrencyGuard::dummy(state.session_concurrency.clone())
    };

    // 2) 请求体只解析这一次，下面五项判定全从这份结果上读。
    //
    //    此前 extract_device_id / body_has_user_id / request_model / request_speed 各自
    //    `from_slice` 一遍整个 body，`Simulation::detect` 再来一遍，加上 `rewrite_body`
    //    自己那次，一条请求要把同一份 JSON 完整解析 6 次以上（429 换号重试时后两项还按轮次
    //    翻倍）。body 上限刚放到 64MB，长对话几 MB 是常态，这是白烧的 CPU。
    //
    //    `rewrite_body` 仍自己解析：它要一份**可变且每轮独立**的副本（每次重试都从客户端
    //    原始体重新改写），共用这份只读的反而要多克隆一次。
    //
    //    解析失败（不是 JSON）时为 `None`，各项判定按「读不出来」退化，与逐个解析时一致。
    let body_json: Option<serde_json::Value> = serde_json::from_slice(&body).ok();

    // 提取 device_id（在 metadata.user_id 里；兼容 CC 内嵌 JSON 与扁平串两种格式）。
    let device_id = extract_device_id(body_json.as_ref());
    // 给本地拒绝的流水留下这三样，外层就不必再解析体。见 [`RequestLogState::parsed`]。
    *log_state.parsed.lock() = Some(ParsedRequestBits {
        model: request_model(body_json.as_ref()),
        device_id: device_id.clone(),
        session_id: extract_session_id(body_json.as_ref()),
    });
    // 该字段在不在（与「能否解析出设备标识」是两回事）：决定要不要给它补一份官方身份。
    // body 逐轮不变，算一次即可。见 [`Upstream::bare_session`]。
    let has_user_id = body_has_user_id(body_json.as_ref());
    // 来访是不是本来就是 CC 形态（判据是 `system` 里那句话，见 [`is_cc_shaped`]）。
    // 这里只为日志算它：走不走模拟由 [`Simulation::detect`] 自己判，但它返回 `None` 时
    // 分不出是「本来就是 CC」还是「开关关着」，而这正是排查时要知道的那一位。
    let cc_shaped = body_json.as_ref().is_some_and(is_cc_shaped);
    // 来访是不是 Claude Code 客户端——这一位**只看 UA**：`claude-cli/<版本>` 在才算。
    // 它是 [`Simulation::detect`] 跳过模拟的必要条件之一，不是充分条件：还得体也是 CC 形态
    // （上面的 `cc_shaped`）且工具列表像 CC。带着正确 UA 与形态来的就是官方客户端，不动它：
    // 模拟那条路会把这串 UA 连同 `x-app`/`x-stainless-*` 一起换成 [`config::CC_SIM_HEADERS`]
    // 里的定值。
    //
    // `metadata.user_id` 和 `X-Claude-Code-Session-Id` **不**单独构成跳过模拟的理由：
    // 非 CC 的 UA（`Go-http-client`、`python-httpx`……）带着这些字段，只说明它抄了请求体
    // 或头，UA 不对齐照样是一条自相矛盾的请求，需要模拟接管。模拟路径下
    // [`rewrite_body`] 会先剥掉客户端已有的 `metadata.user_id`，再由
    // [`ensure_cc_metadata`] 用 `sim.session_id` 重建，确保头体自洽。
    //
    // **UA 可以伪造**，所以只认 UA 不够：照抄 `claude-cli/...` 却没抄 system 形态的第三方
    // 中转（封号复盘里的探活脚本就是），透传出去是一条头体矛盾的请求，比模拟更容易被上游
    // 标记——这类请求由 [`Simulation::detect`] 按形态识出来、一并走模拟接管。
    //
    // **自报的版本还得说得通**：不高于官方已发布的最新版（[`known_latest_release`]，从
    // `downloads.claude.ai/claude-code-releases/latest` 学来）。一个自称 `claude-cli/2.5.0`
    // 的客户端在官方只发到 2.1.260 的时候不是官方客户端——按非 CC 客户端处理（走模拟），
    // 也不再沿用它那个不存在的版本号去补 billing header、跑启动握手、发额度探测。
    let from_cc_client = trusted_cc_version(&client_ua).is_some();
    if !from_cc_client && let Some((a, b, c)) = cc_cli_version(&client_ua) {
        let (la, lb, lc) = known_latest_release();
        tracing::warn!(
            ua = %client_ua,
            claimed = %format!("{a}.{b}.{c}"),
            latest = %format!("{la}.{lb}.{lc}"),
            "client claims a Claude Code version newer than the latest official release; not treating it as an official client"
        );
    }

    // 请求的模型名：好几处都要用它——本地形态拦截按模型索引，选号的冷却也按
    // 「账号 + 模型」分格（fable 那类模型级 429 不该拖累整个账号），本地作答那条回的也是它。
    let req_model = request_model(body_json.as_ref());
    // 这条请求的会话 id（头优先、body 兜底）：选号失败与本地拒绝那几条日志的抑制键，
    // 在没有设备身份时要拿它来分桶；每会话限流在 2.2b / 2.2c 用的也是它。
    let session_id = match &session_from_header {
        Some(sid) => Some(sid.clone()),
        None => extract_session_id(body_json.as_ref()),
    };

    // 2.1) 这条路径是否消耗订阅额度——决定要不要卡设备身份、要不要改写出站体。
    //      判定吃 `uri.path()` 而非上面那个带查询串的 `path_and_query`：豁免要精确匹配。
    let billable = is_billable_messages(uri.path());

    // 2.1a) 探针类请求 → **本地回一条最小的正常回复**（200，见 [`probe_reply`]），不到上游，
    //       见 [`probe_signature`]。身份写错的不在这里拒，由 [`Simulation::detect`] 送进模拟
    //       重建身份。
    //       **位置有意排在下面 2.2 的设备身份闸之前**：探活多半根本不带 `metadata.user_id`
    //       （封号复盘里 Go-http-client 那批就是），而 `require_device_id` 默认开着、会先回一条
    //       403——那正是下游把整个 key 摘下去的信号，这一版要修的就是它。排在这里也意味着探针
    //       不占每会话 RPM 与并发名额（2.2b / 2.2c）：它根本不出站。
    //       0.3.101 之前回的是 403 permission_error：探活恰恰是下游中转用来判断「这个号还能
    //       不能用」的那条请求，luban 的 403 在它那侧与「号被封了」长得一样，整个 key 被摘下
    //       去、真流量跟着停——而这条请求根本没到上游、账号一点事没有。回 200 后探活看到的是
    //       「健康」，上游那边一条请求都没多。是 luban 就地答的、没到上游这件事标在三处：
    //       响应头 `x-luban-local: probe_reply` 与 `x-luban-probe-kind: <判据>`、Message id 的
    //       `msg_luban` 前缀、以及流水里的 `probe_reply` 标签（花费 0）。
    //       下游中转的探活脚本发一条无 tools 的单句小请求，每条在上游侧都是「一台设备开一个
    //       一次性会话只问一句话」——封号复盘里最显眼的判据。判据是形态与身份上的强特征，
    //       一条就够判，不做计数。**不限 UA**（0.3.99 起）：此前只判自报 claude-cli 的，
    //       Go-http-client 的探活反而走模拟、被装成官方形态发了出去。只判计费路径；
    //       `reject_probes` 关掉即放行。
    if billable
        && state.store.forward_flags().reject_probes
        && let Some(kind) = probe_signature(
            body_json.as_ref(),
            device_id.as_deref(),
            &inbound_beta_list(&headers),
            state.store.forward_flags().reject_probes_strict,
            || device_id.as_deref().is_some_and(|d| state.store.device_is_known(d)),
        )
    {
        // 抑制键按「类别 + 设备」分桶：探活脚本多半几十秒一条，同一台设备反复撞这里；
        // 类别分开是因为同一台设备先撞 ping、再撞身份句重复，是两件事。
        let who = device_id.as_deref().or(session_id.as_deref()).unwrap_or("-");
        if let Some(suppressed) =
            take_rejection_log_slot(&state.rejection_log, &format!("probe:{}:{who}", kind.tag()))
        {
            let device_short: String = who.chars().take(8).collect();
            tracing::warn!(
                %method, path = %path_and_query, ua = %client_ua,
                model = %req_model.as_deref().unwrap_or("-"), device = %device_short,
                kind = kind.tag(), from_cc_client, suppressed, reason = kind.message(),
                "not forwarded: request matches a probe / health-check signature; answered locally with a minimal 200"
            );
        }
        *log_state.local_replay.lock() = Some(REWRITE_PROBE_REPLY);
        return probe_reply(
            kind,
            req_model.as_deref(),
            body_json.as_ref().is_some_and(stream_requested),
        );
    }

    // 2.2) 无有效设备身份（无 metadata / 无法识别的 user_id 格式）→ 计费路径默认直接拒绝：
    //      这类请求既无法做身份伪装、也无从计入设备上限（会绕过 device_limit）。
    //      网页可关掉该校验（放行裸客户端），此时它们退化为不绑定、不占名额的负载均衡挑选。
    //      不带身份的**探针**到不了这里：它在 2.1a 就被就地答掉了——这道闸回的 403 与「号被封了」
    //      在下游那侧长得一样，探活撞上它整个 key 就被摘下去。
    if device_id.is_none() {
        if billable && state.store.require_device_id() {
            tracing::warn!(%method, path = %path_and_query, ua = %client_ua, "rejected: request has no usable device identity (metadata.user_id missing or unrecognized)");
            return error_response(
                StatusCode::FORBIDDEN,
                "permission_error",
                "missing a usable device identity (metadata.user_id)",
            );
        }
        tracing::debug!(%method, path = %path_and_query, billable, "allowing a request with no device identity");
    }

    // 2.2b) 每会话 RPM 上限（body 这一路）：头上没带会话 id，但 `metadata.user_id` 里有。
    //       只在头那路没判过时才判（`session_from_header.is_none()`），否则同一条请求会吃掉
    //       两个名额——官方两处同值，那等于把上限砍半。
    //       会话 id（头优先、body 兜底）在上面 2.1 就定下来了。
    if session_from_header.is_none()
        && let Some(sid) = session_id.as_deref()
        && let Some(retry) = state.store.take_session_rpm_slot(sid)
    {
        return session_rpm_rejection(
            &state.rejection_log,
            &method,
            &path_and_query,
            &client_ua,
            sid,
            retry,
            "body",
        );
    }

    // 2.2c) 每会话并发在途上限（body 这一路）：头那路没判过时补判。
    if session_from_header.is_none()
        && let Some(sid) = session_id.as_deref()
    {
        match try_acquire_session_concurrency(&state.session_concurrency, sid, concurrency_limit) {
            Ok(guard) => session_concurrency_guard = guard,
            Err(current) => {
                return session_concurrency_rejection(
                    &state.rejection_log,
                    &method,
                    &path_and_query,
                    &client_ua,
                    sid,
                    current,
                    concurrency_limit,
                    "body",
                );
            }
        }
    }

    // 这条请求声明的输出上限。只为日志：裸 429 那一档要拿它对上游那套「每分钟输出 token」
    // 限额，见 [`UpstreamLoad`]。算在这里是因为 `body_json` 只解析一次（见上面 2 那段），
    // 而这个值逐轮不变。
    let req_max_tokens = request_max_tokens(body_json.as_ref());

    // 2.3a) 4.6+ 全系列不支持 assistant message prefill（末尾 role=assistant 的轮次），
    //       上游会返回 400。策略由 `prefill_policy` 控制：
    //       - strip（默认）：主动剥掉末尾 assistant 轮后转发，省去白跑一趟。
    //       - reject：本地直接 400 拒绝，不往上游送。
    //       - off：不做任何处理，交给上游（被动重试兜底）。
    //       后面那条被动重试（[`is_prefill_not_supported_error`] → [`retry_without_prefill`]）
    //       仍保留作为兜底：万一新模型不在列表里、或者上游的拒绝消息换了措辞。
    let body = if req_model.as_deref().is_some_and(model_rejects_prefill)
        && has_trailing_assistant(body_json.as_ref())
    {
        match state.store.prefill_policy() {
            store::PrefillPolicy::Strip => match strip_assistant_prefill(&body) {
                Some(stripped) => {
                    tracing::info!(
                        model = %req_model.as_deref().unwrap_or("-"),
                        "proactively stripped trailing assistant prefill for a model that does not support it"
                    );
                    stripped
                }
                None => body,
            },
            store::PrefillPolicy::Reject => {
                tracing::info!(
                    model = %req_model.as_deref().unwrap_or("-"),
                    ua = %client_ua,
                    "rejected: assistant message prefill is not supported by this model (prefill_policy=reject)"
                );
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "This model does not support assistant message prefill. The conversation must end with a user message.",
                );
            }
            store::PrefillPolicy::Off => body,
        }
    } else {
        body
    };

    // 2.3) 上游已经拒过一次的「模型 + 请求里的某个取值」组合（`effort: 'xhigh'`、
    //      `role: 'system'` 之类）→ 本地直接拒，不往上游送。这是纯粹的请求形态错误：
    //      换哪个号发都是同一条 400，送上去只会白占一次请求配额，并在日志里留下一条与
    //      账号状态无关的 4xx。规则不是写死的，是上游那条 400 自己喂出来的，回给客户端的
    //      也是它当初那句原话，见 [`remember_shape_rejection`]。
    if let Some((field, value, message)) =
        known_shape_rejection(&state.shape_rejections, req_model.as_deref(), body_json.as_ref())
    {
        tracing::warn!(
            %method, path = %path_and_query, ua = %client_ua,
            model = %req_model.as_deref().unwrap_or("-"), %field, %value,
            "rejected locally: upstream has already rejected this request shape"
        );
        return error_response(StatusCode::BAD_REQUEST, "invalid_request_error", &message);
    }

    // 2.3a) OpenAI 格式转换残留 → 本地直接拒，不修补，见 [`find_openai_marker`]。
    //       `image_url` 一项无条件拒（Anthropic API 不认这个 type，送上去恒为 400）；其余
    //       残留（messages 里的 `role:"system"`、`call_` 前缀的工具调用 id、OpenAI 专属顶层
    //       字段……）由 `reject_openai_shape` 拨：开着一律拒，关着退回旧的修补路径
    //       （`hoist_system_role` 挪 system、[`normalize_tool_choice`] 翻译 tool_choice）。
    //       模拟路径不受影响：它只接管**本来就是 Anthropic 形态**的非 CC 请求。
    let reject_openai_shape = state.store.forward_flags().reject_openai_shape;
    if let Some(marker) = find_openai_marker(body_json.as_ref(), cc_shaped, reject_openai_shape) {
        tracing::warn!(
            %method, path = %path_and_query, ua = %client_ua,
            location = %marker.location, kind = %marker.kind,
            "rejected locally: request carries OpenAI-format residue, not accepted as an Anthropic Messages request"
        );
        return error_response(StatusCode::BAD_REQUEST, "invalid_request_error", marker.message());
    }

    // 2.3a2) 会话 id 头体不一致 → 本地直接拒，见 [`session_id_conflict`]。
    //        官方那两处逐字相同；两个都合法却不同的 uuid，luban 没有任何依据挑一个，而它
    //        正是会话链（`cc_prompt_id` / `cc_prev_req` / `diagnostics`）的键——挑错就是把
    //        两条链接到一起，事后再也看不出来。默认拒（`reject_session_conflict`）；关掉后
    //        退回「取头那个 + 打一条 warn」。
    if state.store.forward_flags().reject_session_conflict
        && let Some((header, body)) = session_id_conflict(&headers, body_json.as_ref())
    {
        tracing::warn!(
            %method, path = %path_and_query, ua = %client_ua,
            header = %header, body = %body,
            "rejected locally: the session id differs between X-Claude-Code-Session-Id and metadata.user_id"
        );
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!(
                "session id mismatch: X-Claude-Code-Session-Id is {header} but \
                 metadata.user_id carries {body}; send the same value in both \
                 (disable reject_session_conflict to fall back to the header)"
            ),
        );
    }

    // 2.3a4) 上游分类器拒答过的提示词（同一模型、system + messages + tools + tool_choice 逐字
    //        相同）→ **原样回放上游那次的响应**（200 + 同一段 `stop_reason: "refusal"` 的体，
    //        见 [`replay_refusal`]），不再送。不是 luban 自己造一条 403：客户端看到的与上游
    //        亲自再拒一次完全一样（0.3.98 之前回的是 403 permission_error，客户端按错误
    //        处理、看不到 stop_details）。学进来的只有带 `stop_details.category` 的分类器判决
    //        （见 [`UsageSniffer::classifier_refusal`]），那是确定性的：换个号、换个形态重发
    //        结果一样；见 [`known_refused_prompt`]。自己的开关 `reject_refusals`（0.3.93 之前
    //        借用 `reject_probes`，关探针就把这条一起放行了）。
    //
    //        **出站会带 `fallbacks` 的不拦**（客户端自带的，或 luban 按族开关要补的，见
    //        [`outbound_carries_fallbacks`]）：带 fallback 的请求上游拒答后会换模型重跑，那正是
    //        拒答该走的路。此前不看这一点，fallback 关着时学到的一条拒答，之后即便把 fallback
    //        打开也永远走不到上游——本地先 403 了。
    if billable
        && state.store.forward_flags().reject_refusals
        && !outbound_carries_fallbacks(
            body_json.as_ref(),
            req_model.as_deref(),
            state.store.forward_flags(),
            &inbound_beta_list(&headers),
            &state.deprecated_fields,
        )
        && let Some(refused) =
            known_refused_prompt(&state.empty_replies, req_model.as_deref(), body_json.as_ref())
    {
        let model = req_model.as_deref().unwrap_or("-");
        let who = device_id.as_deref().or(session_id.as_deref()).unwrap_or("-");
        let device_short: String = who.chars().take(8).collect();
        let wants_stream = body_json.as_ref().is_some_and(stream_requested);
        match replay_refusal(&refused.reply, wants_stream) {
            Some(resp) => {
                if let Some(suppressed) =
                    take_rejection_log_slot(&state.rejection_log, &format!("refusal:{model}:{who}"))
                {
                    tracing::warn!(
                        %method, path = %path_and_query, ua = %client_ua,
                        %model, device = %device_short, suppressed,
                        stream = wants_stream, replay_sse = refused.reply.sse,
                        verdict = %refused.verdict.chars().take(300).collect::<String>(),
                        "not forwarded: upstream has already refused this exact prompt; replaying upstream's refusal (200 + the same body) locally"
                    );
                }
                *log_state.local_replay.lock() = Some(REWRITE_REFUSAL_REPLAY);
                return resp;
            }
            // 学到的体按这次要的形态拼不出来（理论上不会：学的时候要求流完整收尾）——
            // 照常送上游，宁可多送一条，不回一段残缺的响应。
            None => tracing::warn!(
                %method, path = %path_and_query, ua = %client_ua,
                %model, device = %device_short,
                stream = wants_stream, replay_sse = refused.reply.sse,
                "the recorded upstream refusal could not be replayed in the shape this request asked for; forwarding upstream"
            ),
        }
    }

    // 2.3a4c) 按应用学到的拒答（只对**识别不了会话**的来访）：同一模型 + 同一份 system 被上游
    //         分类器拒得够多（至少 3 条且占三成以上，见 [`record_app_request`]）→ 之后这个应用
    //         的每条请求都回放最近那条拒答，见 [`known_app_refusal`]。
    //         封号复盘里 Go-http-client 那场风暴每条正文都不同、按提示词学的规则一条都命不中，
    //         而它只有 4 种 system——对不带身份的中转流量，system 就是「哪个应用」。带会话 id 或
    //         device_id 的来访不走这条：它们的 system 是客户端每轮都在变的官方形态，且真人对话
    //         偶发一次拒答不该连坐整个会话。与 2.3a4 共用 `reject_refusals` 开关与 fallbacks 例外。
    let session_less = device_id.is_none() && session_id.is_none();
    if billable
        && session_less
        && state.store.forward_flags().reject_refusals
        && !outbound_carries_fallbacks(
            body_json.as_ref(),
            req_model.as_deref(),
            state.store.forward_flags(),
            &inbound_beta_list(&headers),
            &state.deprecated_fields,
        )
        && let Some(refused) =
            known_app_refusal(&state.empty_replies, req_model.as_deref(), body_json.as_ref())
    {
        let model = req_model.as_deref().unwrap_or("-");
        let wants_stream = body_json.as_ref().is_some_and(stream_requested);
        match replay_refusal(&refused.reply, wants_stream) {
            Some(resp) => {
                if let Some(suppressed) = take_rejection_log_slot(
                    &state.rejection_log,
                    &format!("app-refusal:{model}:{client_ua}"),
                ) {
                    tracing::warn!(
                        %method, path = %path_and_query, ua = %client_ua,
                        %model, suppressed, stream = wants_stream, replay_sse = refused.reply.sse,
                        verdict = %refused.verdict.chars().take(300).collect::<String>(),
                        "not forwarded: upstream has already refused this session-less app (same model + system); replaying upstream's refusal locally"
                    );
                }
                *log_state.local_replay.lock() = Some(REWRITE_APP_REFUSAL_REPLAY);
                return resp;
            }
            None => tracing::warn!(
                %method, path = %path_and_query, ua = %client_ua, %model,
                "the recorded app-level refusal could not be replayed in the shape this request asked for; forwarding upstream"
            ),
        }
    }

    // 2.3a5) 上游对这一类请求（模型 + 无 tools 单条消息 + 这个 max_tokens）回过 200 却零输出
    //        → 本地 403，不再送。规则不是写死的，是上一条零输出的回复自己喂出来的，见
    //        [`known_empty_reply`]；回给客户端的文案带上上游当时的原话。自己的开关
    //        `reject_empty_replies`，不限 UA——模拟路径重建的是身份，改不了「问一句、上游一个字
    //        不回」这件事。
    if billable
        && state.store.forward_flags().reject_empty_replies
        && let Some((max_tokens, excerpt)) =
            known_empty_reply(&state.empty_replies, req_model.as_deref(), body_json.as_ref())
    {
        let model = req_model.as_deref().unwrap_or("-");
        let who = device_id.as_deref().or(session_id.as_deref()).unwrap_or("-");
        if let Some(suppressed) = take_rejection_log_slot(
            &state.rejection_log,
            &format!("empty-reply:{model}:{max_tokens}:{who}"),
        ) {
            let device_short: String = who.chars().take(8).collect();
            tracing::warn!(
                %method, path = %path_and_query, ua = %client_ua,
                %model, max_tokens, device = %device_short, suppressed,
                "rejected locally: upstream has already answered this request class with zero output tokens"
            );
        }
        return error_response(
            StatusCode::FORBIDDEN,
            "permission_error",
            format!(
                "not forwarded: upstream has already answered this request class (model {model}, \
                 tool-less single-message, max_tokens {max_tokens}) with 200 and zero output tokens; \
                 upstream reply was: {}",
                excerpt.chars().take(300).collect::<String>()
            ),
        );
    }

    // 2.3b) 上游曾以 `deprecated` 拒过的字段（`temperature`、`top_p` 之类）。
    //       策略由 `sampling_policy` 控制：strip（默认）= 剥掉后转发，reject = 本地 400，
    //       off = 不做静态预置处理（运行时学习仍兜底）。
    //       与 2.3 共享「从上游 400 里学」的范式，但行为相反：那条路是拒绝，这条路是修补。
    let sampling_policy = state.store.sampling_policy();
    // reject 策略下静态名单与学到的组合一视同仁：都是「这个模型不收这个参数」的既定事实。
    let body = if sampling_policy == store::PrefillPolicy::Reject
        && ((req_model.as_deref().is_some_and(model_rejects_sampling)
            && has_deprecated_sampling_field(body_json.as_ref()))
            || has_learned_deprecated_field(
                &state.deprecated_fields,
                req_model.as_deref(),
                body_json.as_ref(),
            )) {
        tracing::info!(
            model = %req_model.as_deref().unwrap_or("-"),
            ua = %client_ua,
            "rejected: sampling parameters (temperature/top_p/top_k) are deprecated for this model (sampling_policy=reject)"
        );
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Sampling parameters (temperature, top_p, top_k) are deprecated for this model.",
        );
    } else {
        maybe_strip_deprecated(
            &state.deprecated_fields,
            req_model.as_deref(),
            body_json.as_ref(),
            body,
            sampling_policy != store::PrefillPolicy::Off,
        )
    };

    // 2.4) 每设备 RPM 上限：这台机器最近 60 秒发得太多 → 直接 429 + `retry-after`。
    //      **不换号**：账号打满换个号还能发，设备打满换哪个号都是同一台机器在刷，换号只会
    //      白白改绑设备（还会连累 thinking 签名，见 [`store::RpmLimited::sticky`]）。故这道闸
    //      独立于选号，也因此排在形态拦截之后：一条发都发不出去的请求不该占掉设备的名额。
    //      没有设备身份的请求（网页关了校验的那些）不受此闸管——它们由裸请求速率上限兜着。
    //
    //      与会话闸（1.6 / 2.2b）是**同一件事的两个粒度**：那道贴合单个对话的真实节奏，这道
    //      兜「这台机器总量别失控」——会话 id 轮换免费，只有它拦不住换 id 的客户端。语义与
    //      两个阈值该怎么配见 [`store::SESSION_RPM_LIMIT`]。
    if let Some(dev) = device_id.as_deref()
        && let Some(retry) = state.store.take_device_rpm_slot(dev)
    {
        // 日志抑制：撞满的客户端多半每几十毫秒就再撞一次，一条一行会把日志刷没。
        // 憋掉的条数记在下一行的 `suppressed=` 上，见 [`take_rejection_log_slot`]。
        if let Some(suppressed) =
            take_rejection_log_slot(&state.rejection_log, &format!("device:{dev}"))
        {
            let device_short: String = dev.chars().take(8).collect();
            tracing::warn!(%method, path = %path_and_query, ua = %client_ua, device = %device_short, retry_after = retry, suppressed, "rejected: this device has reached its RPM limit");
        }
        return rate_limit_response(
            retry,
            format!("this device has reached its RPM limit; retry in {retry} seconds"),
        );
    }

    // 3) 按 device_id 粘性选出凭证的 access_token（必要时刷新）。
    // 首发与换号重试用同一份选号入参，只有「已试过哪些号」不同——写成函数而不是就地各构一份，
    // 免得两处的 device_id/model 哪天漂开。
    fn select<'a>(
        device_id: Option<&'a str>,
        billable: bool,
        model: Option<&'a str>,
        exclude: &'a [i64],
    ) -> store::Select<'a> {
        store::Select { device_id, rate_limited: billable, exclude, model, ..Default::default() }
    }
    let (token, cred) = match store::valid_access_token_for_device(
        &state.store,
        &state.clients,
        select(device_id.as_deref(), billable, req_model.as_deref(), &[]),
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
            } else if e.downcast_ref::<store::AllRateLimited>().is_some() {
                "all-cooling-down"
            } else if e.downcast_ref::<store::DeviceLimitReached>().is_some() {
                "device-limit"
            } else if e.downcast_ref::<store::ModelUnsupported>().is_some() {
                "model-unsupported"
            } else {
                "unavailable"
            };
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
                return rate_limit_response(secs, e.to_string());
            }
            // 所有号都被上游判过「套餐不含这个模型」→ 403 permission_error：等多久都没用，
            // 客户端该换模型。照上游拒绝无权限模型的口径回，别包装成 429 误导它退避重试。
            if e.downcast_ref::<store::ModelUnsupported>().is_some() {
                return error_response(StatusCode::FORBIDDEN, "permission_error", e.to_string());
            }
            // 设备数达硬上限 → 429（等多久取决于别人什么时候释放，给不出 retry-after，故这条
            // 不走 [`rate_limit_response`]）；其余（无凭证/刷新失败等）→ 503。
            let (status, etype) = if e.downcast_ref::<store::DeviceLimitReached>().is_some() {
                (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error")
            } else {
                (StatusCode::SERVICE_UNAVAILABLE, "api_error")
            };
            return error_response(status, etype, e.to_string());
        }
    };

    // 4) 目标 URL：上游 base + 原路径与查询串。
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, path_and_query);

    // 5) 组装转发头：复制安全头，注入鉴权与 beta。形态类改动逐项受网页开关控制，
    //    一条 SQL 读齐（默认全开 = 加入开关前的既有行为）。
    let flags = state.store.forward_flags();
    // 设备指纹用于派生伪装 device_id。归一化开着时只取平台（arch/os），关着时叠加客户端
    // 原始 device_id。头与体两侧都要用它（模拟模式的 session_id 也由它派生），故在装头之前先算好。
    let fp_device = if flags.normalize_device_fp { None } else { device_id.as_deref() };
    // **出站 UA 也进指纹**：一台设备只能有一个客户端版本，换版本就是换设备。判据与
    // [`Simulation::detect`] 同源（都走 [`simulates_cc`]），不然指纹会把一条请求算到另一台
    // 设备名下。理由与代价见 [`device_fingerprint`]。
    let simulating = simulates_cc(body_json.as_ref(), &headers, from_cc_client, flags);
    let device_fp = device_fingerprint(fp_device, &headers, outbound_ua(&client_ua, simulating));
    // 6) 转发前改写 body：system 形态对齐（拆/并成官方的 5 块 + 基座标 scope=global）
    //    + 身份伪装（metadata.user_id 的 account_uuid/device_id 换成该凭证自洽身份、
    //    billing header 补 cch）；模拟模式下另外补上官方 system 前缀与 metadata。
    {
        let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("-");
        tracing::debug!(
            ua = %h("user-agent"),
            x_app = %h("x-app"),
            arch = %h("x-stainless-arch"),
            os = %h("x-stainless-os"),
            runtime = %h("x-stainless-runtime"),
            pkg = %h("x-stainless-package-version"),
            "client identification headers"
        );
    }
    // 请求侧的速度档（顶层 `speed` 字段，配套 anthropic-beta: fast-mode-*）。
    // 仅作兜底：以上游 `usage.speed` 为准，那里才反映实际生效的档位。
    let req_speed = request_speed(body_json.as_ref());
    // 这条非流式请求要不要改成流式发、再聚合回整段 JSON（见
    // [`store::ForwardFlags::nonstream_as_sse`]）。
    //
    // 要求 body 能解析：解析不出来的话 [`rewrite_body`] 那边同样会原样返回，`stream` 根本
    // 改不成 true，此时若还按聚合走，就会拿一份非 SSE 的响应去喂聚合器。两处的判据必须同源。
    //
    // **官方本来就非流式的那两类不改**（安全分类、额度探测，见
    // [`CcRequestKind::keeps_nonstream`]）：这个开关的本意是「官方恒为流式，非流式请求
    // 一看就不是 CC」，对它们恰好相反——改成流式才是官方不产生的形态。
    let cc_kind = body_json
        .as_ref()
        .map(|v| CcRequestKind::of(v, &inbound_beta_list(&headers)))
        .unwrap_or(CcRequestKind::Main);
    let upgrade_stream = billable
        && flags.nonstream_as_sse
        && !cc_kind.keeps_nonstream()
        && body_json.as_ref().is_some_and(|v| !stream_requested(v));
    // 工具名混淆映射：从**客户端原始体**扫一次就够（后续改写不动工具名），请求侧与回程
    // 两侧共用同一份。见 [`ToolNameMap`]。
    let tool_names = (billable && flags.tool_name_mimic)
        .then(|| build_tool_name_map(body_json.as_ref()).map(std::sync::Arc::new))
        .flatten();
    if let Some(map) = &tool_names {
        tracing::debug!(count = map.forward.len(), "obfuscating tool names");
    }

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
    let mut tried: Vec<i64> = Vec::new();
    let (mut token, mut cred) = (token, cred);
    let mut retried = 0usize;
    let max_retry = if flags.rate_limit_retry { state.store.rate_limit_retry_max() } else { 0 };
    // 「套餐不含这个模型」引发的换号次数，与 429 那套 `retried`/`max_retry` **分开计**：那个
    // 开关管的是限流，关掉表示「429 原样透传」；而这一档是确定性失败，换号一定有意义，不受
    // 那个开关约束，见 [`LimitScope::Unsupported`]。
    let mut denial_swaps = 0usize;
    // 最后那一轮**上游原样给的**限流头，只在它回 429 时有值（每轮重置，故换号换到一发 200 时
    // 它是 `None`）。存在的理由是下面 transient 档会把我们自己算出来的退避写进 `retry-after`
    // 再交回客户端——那之后重解 `up.headers()` 就会把自己塞的那条当成上游给的读回来，
    // [`RateLimitInfo::no_limit_headers`] 从此恒为 false。留一份注入前的快照给循环之后用。
    // 不给初值：循环体在任何一条 `break` 之前都必经那次赋值，给了反而是个读不到的死值。
    let mut upstream_limit: Option<RateLimitInfo>;
    // 最后那一轮占住的「账号 + 模型」在飞格，见 [`UpstreamRouteGuard`]。同 `upstream_limit`：
    // 逐轮重新赋值（换号后是另一条路线，旧的那格在赋值时归还），不给初值是因为循环体在任何
    // 一条 `break` 之前都必经那次赋值。循环之后它会被交给 `ReqLog` 拿着，活到响应流结束。
    let mut route_load: UpstreamRouteGuard;
    let (upstream, resp, sent) = loop {
        let sim = Simulation::detect(
            body_json.as_ref(),
            &headers,
            from_cc_client,
            flags,
            &cred,
            &device_fp,
        );
        // 真实 CC（API-key 模式）来访的会话关联字段：它自己那条 billing header 里没有
        // `cc_prompt_id`/`cc_prev_req`，整条也没有 `diagnostics`，而订阅端官方每条主线程
        // 请求都有。见 [`client_session_link`]。
        let client_link =
            client_session_link(body_json.as_ref(), &headers, sim.as_ref(), flags, billable, &cred);
        // CC 形态的来访不走模拟，但它若不带 metadata.user_id，那份身份仍然是缺的。
        let bare_session = bare_session_id(
            &headers,
            flags,
            sim.as_ref(),
            billable,
            has_user_id,
            &cred,
            &device_fp,
        );
        // 这条请求的身份形态最终落在哪一路。**入站侧看不出来**：判据是 `system` 里那句
        // [`config::CC_SYSTEM_IDENTITY`]，不是 UA——一个自报 `claude-cli/...` 的客户端
        // （VSCode 扩展、agent-sdk）只要把 system 换成自己的，照样走模拟；反过来 `python-httpx`
        // 只要带上那句话就不走。所以「走没走模拟」只能在判完之后记，且**每轮都记**：429 换号
        // 重试后 session_id 由新账号派生，两轮不是同一个值。
        //
        // 只有我们真动了手脚的两路打 info（默认级别就能看见），原样转发那路留在 debug——
        // 那是绝大多数流量，每条刷一行没有意义。
        match (&sim, &bare_session) {
            (Some(s), _) => {
                // 来访体的结构事实（不含正文）：流水的 `shape` 列记的是出站体、模拟之后已是
                // 官方形态，来访原本几块 system、有没有 billing header、几个 tools 只有这里能看到。
                let facts = body_json.as_ref().map(inbound_facts);
                tracing::info!(
                    cred_id = cred.id, cred = %cred.label,
                    ua = %client_ua,
                    model = %req_model.as_deref().unwrap_or("-"),
                    reason = s.reason.tag(),
                    from_cc_client,
                    system_blocks = facts.as_ref().map_or(0, |f| f.system_blocks),
                    system_bytes = facts.as_ref().map_or(0, |f| f.system_bytes),
                    billing_header = facts.as_ref().is_some_and(|f| f.billing_header),
                    identity = facts.as_ref().is_some_and(|f| f.identity),
                    tools = facts.as_ref().map_or(0, |f| f.tools),
                    max_tokens = facts.as_ref().and_then(|f| f.max_tokens).unwrap_or(-1),
                    base_bytes = s.base.map(str::len).unwrap_or(0),
                    session_id = %s.session_id,
                    "identity path: SIMULATED — rebuilding this request into the official CC shape; reason names the first check it failed (not_cc_client / identity_malformed / not_cc_shaped / no_base_prompt / tools_not_cc)"
                )
            }
            (None, Some(sid)) => tracing::info!(
                cred_id = cred.id, cred = %cred.label,
                ua = %client_ua,
                model = %req_model.as_deref().unwrap_or("-"),
                session_id = %sid,
                "identity path: FILLED — CC-shaped request with no metadata.user_id, adding one"
            ),
            (None, None) => tracing::debug!(
                cred_id = cred.id,
                ua = %client_ua,
                model = %req_model.as_deref().unwrap_or("-"),
                cc_shaped,
                from_cc_client,
                has_user_id,
                simulate_cc = flags.simulate_cc,
                fill_metadata = flags.fill_metadata,
                spoof_identity = flags.spoof_identity,
                billable,
                "identity path: PASSTHROUGH — neither simulating nor filling identity"
            ),
        }
        // 出站两处要落的同一个会话 id。模拟路径不参与：那条整套头由 [`official_headers`]
        // 给出、体也重建过，两处都取 `sim.session_id`。见 [`outbound_session_id`]。
        let session_out = match sim {
            Some(_) => None,
            None => outbound_session_id(
                &headers,
                body_json.as_ref(),
                bare_session.as_deref(),
                &cred,
                flags.spoof_identity,
            ),
        };
        // 要不要补 `fallbacks`（拒答时上游换模型重跑），见 [`refusal_fallbacks_for`]。客户端
        // 自己带了数组形态的，luban 一个字不动、也就**不算 luban 补的**：`refusal_fallbacks`
        // 留 `None`，上游若以 400 拒了它，那是客户端的字段、原样回给客户端，不进「从上游学到
        // 的规则」、不剥掉重试（此前只看开关，客户端的目标被拒会被当成 luban 的学下来，
        // 该模型 7 天内不再补——污染的是全局规则）。头上的 beta 另算：体里只要有这个字段
        // （客户端带的或 luban 补的），头上就得有 `server-side-fallback`。
        let client_fallbacks = client_supplied_fallbacks(body_json.as_ref());
        let refusal_fallbacks = if client_fallbacks {
            None
        } else {
            refusal_fallbacks_for(
                req_model.as_deref(),
                flags,
                billable,
                cc_kind,
                &state.deprecated_fields,
            )
        };
        let out = build_forward_headers_for(
            &headers,
            &token,
            flags,
            sim.as_ref(),
            session_out.as_deref(),
            req_model.as_deref(),
            refusal_fallbacks.is_some() || (billable && client_fallbacks),
        );
        // 模拟路径的出站 URL 补 `?beta=true`（见 [`ensure_beta_query`]）。非计费路径不补：
        // `count_tokens` 官方带不带这个参数，抓包里没有样本，没有依据的形态就别猜着改。
        let target = if sim.is_some() && billable { ensure_beta_query(&url) } else { url.clone() };
        // 这一轮用的是 `cred` 这个号，出站客户端就取它的：配了专用代理的号必须走它自己的
        // 代理，否则真实出口 IP 会直接打到上游。取不出来（代理配错/建不出客户端）时直接
        // 标记禁用踢出调度池——不退回直连，也不留在池里每次白吃一发 503。
        let client = match state.clients.for_credential(&cred) {
            Ok(c) => c,
            Err(e) => {
                let reason = format!("[proxy] {e:#}");
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label, error = %reason,
                    "proxy unusable, disabling the credential"
                );
                let _ = state.store.record_ban(
                    cred.id,
                    &store::BanContext {
                        reason: reason.clone(),
                        source: "proxy",
                        error_message: Some(format!("{e:#}")),
                        request_id: Some(request_id.to_string()),
                        ..Default::default()
                    },
                );
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
                    format!("{e:#}"),
                );
            }
        };
        let upstream = Upstream {
            _state: std::marker::PhantomData,
            client,
            method: method.clone(),
            url: target,
            headers: out,
            flags,
            billable,
            sim,
            bare_session,
            session_out,
            force_stream: upgrade_stream,
            tool_names: tool_names.clone(),
            client_link,
            cc_kind,
            refusal_fallbacks,
        };
        // 改写后的出站体单独留一份：上游把请求判成第三方应用时要把它原样摘要打出来
        // （见 [`log_third_party_rejection`]）。`Bytes` 是引用计数，clone 不拷贝字节。
        let sent = upstream.shape(&body, &cred, &device_fp);
        // 新会话的启动握手：**在这条主请求发出之前**开跑（`cap/2.1.260-2` 的时序是
        // 17:10:17 policy_limits …→ 17:10:19.699 第一条 messages）。原先它排在遥测里，等回程
        // 之后再由 5s 一跳的发送循环取走——顺序整个反了。
        //
        // **不再只给模拟路径发。** 原先的理由是「真实 CC 自己会打这一串」，那是错的：
        // 要对齐形态的是**luban 这张凭证**在上游眼里的样子，而客户端自己那串（如果它真的
        // 登录过）走的是**它自己的账号**，luban 这张凭证一条都收不到。`cap/2.1.258-api`
        // 正好证到这一点——那台机器的 bootstrap/penguin 带的是它自己的
        // `sk-ant-oat01-…`，与经 luban 转发的 `/v1/messages` 完全是两个账号；而经 luban
        // 那一侧压根没有 policy_limits / settings / eval / mcp_servers / code_triggers /
        // metrics / 额度探测。于是「一个只发 messages、从不拉策略配置的 OAuth 账号」本身
        // 就是判据。
        //
        // 唯一会重复的情形是「客户端登录的正是 luban 代理的那个账号」（自代理），
        // 那种部署本来就不该存在。
        if billable
            && flags.api_telemetry
            && let Some(start) = session_start(
                upstream.sim.as_ref(),
                upstream.client_link.as_ref(),
                &trusted_cc_version(&client_ua)
                    .map(|(a, b, c)| format!("{a}.{b}.{c}"))
                    .unwrap_or_else(|| config::CC_VERSION_BASE.to_string()),
            )
        {
            spawn_session_handshake(
                &state,
                &cred,
                &token,
                &sent,
                start,
                &upstream.headers,
                req_model.as_deref(),
            )
            .await;
        }
        // 占住这条路线的在飞格并把这次发送记进窗口——**在 `send` 之前**，见
        // [`note_upstream_send`]。纯记录，不影响这条请求走向。
        route_load = note_upstream_send(
            &state.upstream_load,
            cred.id,
            req_model.as_deref().unwrap_or("-"),
            req_max_tokens.unwrap_or(0),
        );
        let mut resp = upstream.send(sent.clone()).await;

        // 只认「上游明确回 429」这一种：连不上/超时那类换个号一样连不上，重试只是浪费时间。
        let limited = match &resp {
            Ok(up) if up.status() == StatusCode::TOO_MANY_REQUESTS => {
                Some(RateLimitInfo::from_headers(up.headers()))
            }
            _ => None,
        };
        // 注入之前先留一份，见 `upstream_limit` 的声明。非 429 时写回 `None`：换号换到一发 200
        // 的那一轮，上一轮的 429 头不该再算数。
        upstream_limit = limited.clone();
        // 401 账号级错误（token revoked / invalid_grant 等）：停用当前号并换号重试。
        // 必须在 break 之前、loop 内部处理，才能 continue 回去用新号重发。
        // 成功换号则 continue；换不到号或判定不命中则直接 return 透传（body 已消费，
        // 不能再 break 出去走 4xx block，就地返回）。
        if limited.is_none()
            && max_retry > 0
            && resp.as_ref().is_ok_and(|up| up.status() == StatusCode::UNAUTHORIZED)
        {
            let up = resp.unwrap();
            let builder = resp_builder(&up);
            let up_request_id = header_opt(up.headers(), "request-id");
            // `up.bytes()` 下面就把 `up` 吃掉了，要用的响应头在这儿一次取齐。
            let up_org_id = header_opt(up.headers(), "anthropic-organization-id");
            let bytes = match up.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read the upstream 401 body");
                    // 同下面那条：这条路早退，`ReqLog` 建不起来，失败遥测就地补。
                    record_early_failure(
                        &state,
                        &cred,
                        &upstream,
                        &sent,
                        started,
                        flags,
                        billable,
                        up_request_id.as_deref(),
                        up_org_id,
                        crate::telemetry::CallFailure {
                            status: Some(StatusCode::UNAUTHORIZED.as_u16()),
                            error_type: None,
                            message: String::new(),
                            in_band: false,
                        },
                    );
                    log_early_upstream_failure(
                        &state,
                        log_state,
                        &cred,
                        &upstream,
                        &sent,
                        EarlyUpstreamFailure {
                            path: &path_and_query,
                            client_ua: &client_ua,
                            model: req_model.clone(),
                            device_id: early_logged_device(
                                &device_id, &upstream, flags, &cred, &device_fp,
                            ),
                            started,
                            request_id,
                            upstream_request_id: up_request_id.as_deref(),
                            status: StatusCode::UNAUTHORIZED,
                            error_type: None,
                            error_message: Some(format!(
                                "failed to read the upstream 401 body: {e}"
                            )),
                            third_party: false,
                            ratelimit: None,
                            tag: "upstream_401",
                        },
                    );
                    return builder.body(Body::empty()).unwrap_or_else(|e| {
                        error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
                    });
                }
            };
            let (etype, message) = parse_upstream_error(&bytes);
            // 下面两条不换号的出路（回 403 / 原样透传）要写流水，共用这一份。
            let early_401 = |status: StatusCode, error_type: Option<String>, message: String| {
                log_early_upstream_failure(
                    &state,
                    log_state,
                    &cred,
                    &upstream,
                    &sent,
                    EarlyUpstreamFailure {
                        path: &path_and_query,
                        client_ua: &client_ua,
                        model: req_model.clone(),
                        device_id: early_logged_device(
                            &device_id, &upstream, flags, &cred, &device_fp,
                        ),
                        started,
                        request_id,
                        upstream_request_id: up_request_id.as_deref(),
                        status,
                        error_type,
                        error_message: Some(message),
                        third_party: is_third_party_rejection(&bytes),
                        ratelimit: None,
                        tag: "upstream_401",
                    },
                );
            };
            {
                let (etype, message) = (etype.clone(), message.clone());
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    status = 401u16,
                    error_type = %etype.as_deref().unwrap_or("-"),
                    upstream_message = %message.chars().take(500).collect::<String>(),
                    "upstream returned 401"
                );
                // **在分叉之前记一次**：底下三条出路（换号 continue / 回 403 / 原样透传）
                // 全都绕开了 `ReqLog::drop`。记在这里，一条 401 恰好报一次，且报在**吃到
                // 这发 401 的那个号**上——换号之后的那一发由新号自己的 `ReqLog` 报。
                record_early_failure(
                    &state,
                    &cred,
                    &upstream,
                    &sent,
                    started,
                    flags,
                    billable,
                    up_request_id.as_deref(),
                    up_org_id,
                    crate::telemetry::CallFailure {
                        status: Some(StatusCode::UNAUTHORIZED.as_u16()),
                        error_type: etype,
                        message,
                        in_band: false,
                    },
                );
            }
            if let Some(reason) = detect_account_ban(StatusCode::UNAUTHORIZED, &bytes) {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    reason = %reason,
                    "401 account-level error, auto-disabling and attempting credential swap"
                );
                let ctx = ban_context(
                    &reason,
                    "forward_401",
                    StatusCode::UNAUTHORIZED,
                    &bytes,
                    request_id,
                    up_request_id.as_deref(),
                );
                let _ = state.store.record_ban(cred.id, &ctx);
                tried.push(cred.id);
                if retried < max_retry {
                    match store::valid_access_token_for_device(
                        &state.store,
                        &state.clients,
                        select(device_id.as_deref(), billable, req_model.as_deref(), &tried),
                    )
                    .await
                    {
                        Ok((next_token, next_cred)) => {
                            tracing::info!(
                                cred_id = cred.id, cred = %cred.label,
                                to_cred_id = next_cred.id,
                                to_cred = %next_cred.label,
                                attempt = retried + 1,
                                "401 credential swap: retrying with another credential"
                            );
                            (token, cred) = (next_token, next_cred);
                            retried += 1;
                            continue;
                        }
                        // 剩下的号全都被判过「套餐不含这个模型」：把上游那发 401 透传出去会让
                        // 客户端以为是自己的鉴权坏了；与首发选号那条路一样回 403 让它换模型。
                        Err(e) if e.downcast_ref::<store::ModelUnsupported>().is_some() => {
                            tracing::warn!(
                                cred_id = cred.id, cred = %cred.label,
                                error = %e,
                                "401 swap: no enabled account can use this model, answering 403"
                            );
                            early_401(
                                StatusCode::FORBIDDEN,
                                Some("permission_error".into()),
                                e.to_string(),
                            );
                            return error_response(
                                StatusCode::FORBIDDEN,
                                "permission_error",
                                e.to_string(),
                            );
                        }
                        Err(e) => tracing::warn!(
                            cred_id = cred.id, cred = %cred.label,
                            error = %e,
                            "401 but no credential to swap to, passing through as is"
                        ),
                    }
                }
            }
            // 没换号（判定不命中或换不到号）：body 已消费，就地透传。
            early_401(StatusCode::UNAUTHORIZED, etype, message);
            let bytes = match &tool_names {
                Some(map) => Bytes::from(map.restore(&bytes)),
                None => bytes,
            };
            return builder.body(Body::from(bytes)).unwrap_or_else(|e| {
                error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
            });
        }
        let Some(info) = limited else { break (upstream, resp, sent) };
        // 基础窗口真耗尽 → 停调度整个账号；超额池（7d_oi）满 → 只冷却这个模型、换号仍有意义；
        // 谁的额度都没满（容量/请求速率）→ 只冷却这个模型且**不换号**，见 [`LimitScope`]。
        let scope = rate_limit_scope_for(&info, req_model.as_deref(), is_max_plan(&cred));
        // 套餐不含这个模型：记准入、换号重发。学习不设条件——记录是对的就该记；换号有次数上限，
        // 免得一条请求把整池的 Pro 号挨个点一遍。换不到号时若是「全被判过」就回 403 让客户端
        // 换模型，其余原因（都在冷却等）保留上游那发 429 原样透传。
        if let LimitScope::Unsupported(model) = &scope {
            let reason = info.plan_denial_reason();
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                tier = %cred.tier.as_deref().unwrap_or("-"),
                model = %model,
                expires_at = info.unified_reset.unwrap_or(0),
                ratelimit = %info.raw,
                "upstream 429 with no quota window for this model: this account's plan does not include it, remembering that and switching accounts"
            );
            if let Err(e) = state.store.deny_model(cred.id, model, &reason, info.unified_reset) {
                tracing::error!(
                    cred_id = cred.id, cred = %cred.label,
                    error = %e,
                    "persisting the model denial failed (the swap still proceeds)"
                );
            }
            tried.push(cred.id);
            if denial_swaps >= MODEL_DENIAL_MAX_SWAPS {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    model = %model,
                    swaps = denial_swaps,
                    "model-denial swap cap reached, passing the upstream 429 through"
                );
                break (upstream, resp, sent);
            }
            match store::valid_access_token_for_device(
                &state.store,
                &state.clients,
                select(device_id.as_deref(), billable, req_model.as_deref(), &tried),
            )
            .await
            {
                Ok((next_token, next_cred)) => {
                    tracing::warn!(
                        cred_id = cred.id, cred = %cred.label,
                        to_cred_id = next_cred.id,
                        to_cred = %next_cred.label,
                        model = %model,
                        attempt = denial_swaps + 1,
                        "model not included in this account's plan: retrying with another account"
                    );
                    (token, cred) = (next_token, next_cred);
                    denial_swaps += 1;
                    continue;
                }
                Err(e) if e.downcast_ref::<store::ModelUnsupported>().is_some() => {
                    tracing::warn!(
                        cred_id = cred.id, cred = %cred.label,
                        model = %model,
                        error = %e,
                        "no enabled account can use this model, answering 403"
                    );
                    // 这条到过上游（吃到的是 429），客户端拿到的是 403：流水按客户端口径记
                    // 状态，额度窗口与上游 request-id 照上游那发记。
                    // 响应头先取齐，下面读体会把 `up` 吃掉。`limited` 有值说明 `resp` 是 `Ok`
                    // 且 429，这里的 `Err` 分支只是让类型闭合。
                    let (up_request_id, up_org_id, up_body) = match resp {
                        Ok(up) => (
                            header_opt(up.headers(), "request-id"),
                            header_opt(up.headers(), "anthropic-organization-id"),
                            up.bytes().await.ok(),
                        ),
                        Err(_) => (None, None, None),
                    };
                    let (up_etype, up_message) = up_body
                        .as_deref()
                        .map(parse_upstream_error)
                        .unwrap_or((None, String::new()));
                    // 同 401 与连接层失败那两条：`ReqLog` 建不起来，失败遥测就地补。报的是
                    // **这个号吃到的那发 429**（官方客户端对 429 发的正是 `tengu_api_error`），
                    // 不是 luban 回给客户端的 403——遥测描述的是上游调用本身。
                    record_early_failure(
                        &state,
                        &cred,
                        &upstream,
                        &sent,
                        started,
                        flags,
                        billable,
                        up_request_id.as_deref(),
                        up_org_id,
                        crate::telemetry::CallFailure {
                            status: Some(StatusCode::TOO_MANY_REQUESTS.as_u16()),
                            error_type: up_etype,
                            message: up_message,
                            in_band: false,
                        },
                    );
                    log_early_upstream_failure(
                        &state,
                        log_state,
                        &cred,
                        &upstream,
                        &sent,
                        EarlyUpstreamFailure {
                            path: &path_and_query,
                            client_ua: &client_ua,
                            model: req_model.clone(),
                            device_id: early_logged_device(
                                &device_id, &upstream, flags, &cred, &device_fp,
                            ),
                            started,
                            request_id,
                            upstream_request_id: up_request_id.as_deref(),
                            status: StatusCode::FORBIDDEN,
                            error_type: Some("permission_error".into()),
                            error_message: Some(e.to_string()),
                            third_party: false,
                            ratelimit: Some(&info),
                            tag: "model_unsupported",
                        },
                    );
                    return error_response(
                        StatusCode::FORBIDDEN,
                        "permission_error",
                        e.to_string(),
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        cred_id = cred.id, cred = %cred.label,
                        model = %model,
                        error = %e,
                        "model not included in this account's plan but no account to swap to, passing the 429 through"
                    );
                    break (upstream, resp, sent);
                }
            }
        }
        let mut cooldown = info.cooldown_for(&scope);
        // 瞬时限流那档的等待时长**每熬满一档翻一倍**，见 [`next_transient_backoff`]：这一档不换号、
        // 也不把号挪出调度池，客户端拿到的就是一发 429，那么「下次什么时候再来」就是我们唯一
        // 还能影响拥堵的东西。取两者较大值——上游给的 `retry-after` 是下限，连撞出来的退避
        // 只会把它往长了推，不会缩短。总开关关掉时（`max_retry == 0`）不参与：那条路要的是
        // 完全不干预、原样透传。
        // 连撞到 [`TRANSIENT_MAX_ATTEMPTS`] 档就不再当它是一阵拥堵，见下面 park 那一步。
        // 「档」不是「发」：一批并发只顶得动一档，走到头意味着这条路线连坏了 60 秒开外。
        let mut transient_exhausted = false;
        if max_retry > 0
            && let LimitScope::Transient(model) = &scope
        {
            let (wait, attempts) = next_transient_backoff(&state.transient_backoff, cred.id, model);
            cooldown = cooldown.max(wait);
            transient_exhausted = attempts >= TRANSIENT_MAX_ATTEMPTS;
        }
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            model = %req_model.as_deref().unwrap_or("-"),
            scope = scope.label(),
            cooldown_secs = cooldown.as_secs(),
            ratelimit = %info.raw,
            "upstream 429"
        );

        // 冷却与重试同受一个开关：关掉即完全退回「原样透传 429」的既有行为。
        if max_retry == 0 {
            break (upstream, resp, sent);
        }
        // 裸 429（一个限流头都没带）：完全透传，不打冷却、不改 retry-after、不剥 metadata。
        // 这类 429 来自上游服务端瞬态限流，不跟着账号走——我们干预没有意义，让官方 SDK
        // 按自己的退避策略重试才是正解。
        if info.no_limit_headers() {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                model = %req_model.as_deref().unwrap_or("-"),
                "upstream 429 with no rate-limit headers: passing through as-is, letting the client handle retry"
            );
            break (upstream, resp, sent);
        }
        park_rate_limited(&state.store, &cred, &scope, cooldown, transient_exhausted);
        // 谁的额度都没满（容量/请求速率限制）→ **就此打住，不换号**：这一发 429 不是这个号的
        // 问题，换到下一个号上重发只会撞同一堵墙，并把同一个模型的冷却一路盖到整池——一条客户端
        // 请求最多能盖 max_retry+1 个号，客户端再自己重试几轮，全部账号的卡片上就都挂着这个模型
        // 的冷却，而冷却是选号硬门禁，于是新请求一条都进不来（返回 `AllRateLimited`）。
        // 交回 429 + `retry-after` 让客户端退避才是这一档的正解，且那个秒数由我们**按连撞次数
        // 指数放大**后写回去——见下面那段与 [`next_transient_backoff`]。
        if !scope.worth_swapping() {
            // 把退避时长写进 `retry-after` 再交回客户端。**覆盖上游那份而不是只在缺失时补**：
            // 这一档上游给的 `retry-after` 本来就不可信（实测给过 63 小时，是按额度窗口算的，
            // 与「此刻拥堵」无关），[`RateLimitInfo::transient_cooldown`] 早就在夹它了；这里
            // 写回去的值已经把上游那份算进去过（取的较大值），故直接覆盖才是自洽的。
            //
            // 没有这一步，前面算出来的退避只活在我们自己的日志里——客户端看不见，照样秒重试。
            if let Ok(up) = &mut resp {
                up.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(cooldown.as_secs()));
            }
            // 吞够了单独记一行：这一发之后 gate 时间从短退避升级为完整冷却，后续请求从这一刻起
            // 会绕开这个号，日志上得看得出转折点在哪。
            if transient_exhausted {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    model = %req_model.as_deref().unwrap_or("-"),
                    attempts = TRANSIENT_MAX_ATTEMPTS,
                    cooldown_secs = cooldown.as_secs(),
                    "transient 429s all the way up the backoff ladder on this credential+model: taking this model out of the pool for a cooldown so later requests go elsewhere; this request still gets its 429 handed back"
                );
            }
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                model = %req_model.as_deref().unwrap_or("-"),
                retry_after_secs = cooldown.as_secs(),
                "upstream 429 is not account-specific (no quota window is full): passing it through with a backed-off retry-after instead of swapping credentials"
            );
            break (upstream, resp, sent);
        }
        tried.push(cred.id);
        if retried >= max_retry {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                retried,
                "upstream 429, credential-swap retry cap reached, passing the response through"
            );
            break (upstream, resp, sent);
        }

        // 换一个没试过的号。选号顺带**改绑**这台设备（绑定的号不在候选里时会重选并改绑），
        // 于是这台设备之后的请求直接落在新号上，不必每条都先撞一次 429。
        match store::valid_access_token_for_device(
            &state.store,
            &state.clients,
            select(device_id.as_deref(), billable, req_model.as_deref(), &tried),
        )
        .await
        {
            Ok((next_token, next_cred)) => {
                tracing::warn!(
                    cred_id = cred.id,
                    cred = %cred.label,
                    to_cred_id = next_cred.id,
                    to_cred = %next_cred.label,
                    cooldown_secs = cooldown.as_secs(),
                    attempt = retried + 1,
                    "upstream 429: credential put on cooldown, retrying with another one"
                );
                (token, cred) = (next_token, next_cred);
                retried += 1;
            }
            // 没有别的号可用（都试过/都停用了）：保留最初那条 429 原样透传，别把它变成 503。
            Err(e) => {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    error = %e,
                    "upstream 429 but no credential to swap to, passing through as is"
                );
                break (upstream, resp, sent);
            }
        }
    };
    // 请求日志里记哪个设备：客户端自己带了就记它的，裸客户端记出站那份**伪装** device_id。
    // 不记的话这段流量在日志里只留下 `device=-`，既看不出是谁、也无从聚合。见 [`sim_device_id`]。
    // 取最终那一轮的凭证与模拟参数——换过号的话，实际发出去的就是那份。
    let logged_device = device_id.clone().or_else(|| {
        sim_device_id(
            upstream.sim.as_ref(),
            upstream.bare_session.as_deref(),
            flags,
            &cred,
            &device_fp,
        )
    });

    match resp {
        Ok(up) => {
            let status = up.status();
            // 是否 SSE 流（决定用量嗅探逐行还是整段 JSON）；以及我们解不开的 content-encoding。
            //
            // 后者正常情况下恒为 None：上游客户端开了 gzip/br/zstd/deflate 解压，wreq
            // 收到时已解码，并把 `content-encoding`/`content-length` 一并摘掉。
            // 留着这个判断是兜底——若上游哪天用了我们没开的编码，tower-http 会原样放行并保留
            // 该头，那时响应体是我们读不懂的字节，嗅探与账号级错误判定都只能跳过。
            //
            // 曾经这是常态：v0.2.12 恢复转发 `accept-encoding` 却没开解压 feature，于是
            // **所有**响应（含 SSE）都成了压缩字节，用量/计价/封号判定整片失效。当时的 warn
            // 只在 4xx 上打，200 这条路径完全静默，症状是「统计悄悄归零且日志上看不出原因」。
            // 现在改成任何状态码都告警。
            let (is_stream, content_encoding) = resp_shape(&up);
            let compressed = content_encoding.is_some();
            if let Some(enc) = &content_encoding {
                tracing::warn!(
                    status = status.as_u16(),
                    encoding = %enc,
                    "upstream response uses an undecodable content-encoding: usage sniffing and account-level error detection are both skipped (that encoding must be enabled in wreq's features)"
                );
            }
            // 上游限流头（订阅账号 5h/7d 额度体现在此），随请求日志入库。
            //
            // 429 那条路**取循环里留下的快照，不重解 `up.headers()`**：transient 档已经把我们
            // 自己算出来的退避写进这份头的 `retry-after` 了，重解等于把自己塞的值当成上游给的
            // 读回来，`no_limit_headers()` 就此恒为 false，下面那个「裸 429 把响应体打出来」的
            // 分支永远不触发——而它正是为这一档写的。见 `upstream_limit` 的声明。
            let ratelimit =
                upstream_limit.unwrap_or_else(|| RateLimitInfo::from_headers(up.headers()));
            // 顺手看一眼额度：快用尽（默认 90%）就提前把这个号挪出调度池，别等下一条请求去撞
            // 429，见 [`park_if_quota_nearly_exhausted`]。本次响应照常回给客户端——它已经成了，
            // 停的是**之后**的调度。429 那条路不在这儿：上面已按账号/模型分档停过了，
            // 重复停只会多写一次库、多刷一行日志。
            if status != StatusCode::TOO_MANY_REQUESTS {
                park_if_quota_nearly_exhausted(&state.store, &cred, &ratelimit);
            }

            // 包裹响应流：首块到达记 TTFT，边转发边嗅探用量；
            // 流结束(或断开)时在 Drop 里记 total、输出一条日志并落库。
            // 从这儿起流水归 ReqLog；告诉外层别再按本地拒绝补一条。
            log_state.logged.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut rl = ReqLog {
                started,
                ttft_ms: None,
                method: method.to_string(),
                path: path_and_query,
                ua: client_ua,
                // 取最终那一轮的出站头——换过号的话，实际发出去的就是那份（同 logged_device）。
                ua_out: ua_of(&upstream.headers),
                cred_id: cred.id,
                cred_label: cred.label.clone(),
                device_id: logged_device,
                status: status.as_u16(),
                sse_aggregated: false,
                sniffer: UsageSniffer::new(is_stream, compressed),
                req_speed,
                req_model: req_model.clone(),
                ratelimit,
                stream_broke: None,
                request_id: request_id.to_string(),
                client_request_id: client_request_id.clone(),
                upstream_request_id: header_opt(up.headers(), "request-id"),
                forensics: capture_forensics(&upstream, &sent, &cred),
                // 只给计费路径备料：**含非 2xx**——失败的请求要报 `tengu_api_error`
                // （官方客户端对失败请求发的正是它），报不报由 Drop 里再判。
                telemetry: telemetry_capture(
                    &state,
                    &cred,
                    &upstream,
                    &sent,
                    started,
                    flags,
                    billable,
                    header_opt(up.headers(), "anthropic-organization-id"),
                ),
                // 两条路都要把回程记回去：模拟那条的会话 id 在 `sim` 里，真实 CC 那条在
                // `client_link` 里（键是客户端自己的会话 id）。
                cc_session: upstream
                    .sim
                    .as_ref()
                    .map(|s| s.session_id.clone())
                    .or_else(|| upstream.client_link.as_ref().map(|(sid, _)| sid.clone())),
                // 只给计费路径分类：count_tokens 之流没有「回复」可言。
                empty_reply_key: if billable {
                    empty_reply_class(req_model.as_deref(), body_json.as_ref())
                } else {
                    None
                },
                prompt_key: if billable {
                    req_model
                        .as_deref()
                        .zip(body_json.as_ref().and_then(prompt_digest))
                        .map(|(m, d)| (m.to_string(), d))
                } else {
                    None
                },
                app_key: if billable && session_less {
                    req_model
                        .as_deref()
                        .zip(body_json.as_ref().and_then(app_system_digest))
                        .map(|(m, d)| (m.to_string(), d))
                } else {
                    None
                },
                empty_replies: state.empty_replies.clone(),
                store: state.store.clone(),
                _in_flight: in_flight,
                _session_concurrency: session_concurrency_guard,
                _route_load: route_load,
            };

            // 400/401/403：先缓冲响应体做账号级错误判定，命中则自动停用该凭证并清空其
            // 设备绑定。401 账号级错误（token revoked 等）会换号重试而非直接透传。
            if matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) {
                let builder = resp_builder(&up);
                let err_bytes = match up.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to read the upstream error body");
                        return builder.body(Body::empty()).unwrap_or_else(|e| {
                            error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
                        });
                    }
                };
                rl.ttft_ms = Some(rl.started.elapsed().as_millis());
                rl.sniffer.feed(&err_bytes);
                if !compressed {
                    let (etype, message) = parse_upstream_error(&err_bytes);
                    tracing::warn!(
                        cred_id = cred.id, cred = %cred.label,
                        status = status.as_u16(),
                        error_type = %etype.as_deref().unwrap_or("-"),
                        upstream_message = %message.chars().take(500).collect::<String>(),
                        "upstream returned 4xx"
                    );
                    rl.forensics.error_type = etype;
                    rl.forensics.error_message = Some(message);
                    rl.forensics.third_party = is_third_party_rejection(&err_bytes);
                }
                if !compressed && status == StatusCode::BAD_REQUEST {
                    let mut learned = remember_shape_rejection(
                        &state.shape_rejections,
                        req_model.as_deref(),
                        body_json.as_ref(),
                        &err_bytes,
                    );
                    learned.extend(remember_deprecated_field(
                        &state.deprecated_fields,
                        req_model.as_deref(),
                        body_json.as_ref(),
                        &err_bytes,
                    ));
                    // 写穿落库：进程内表已经更新，落库失败只影响重启后要不要重学，不影响本次。
                    if let Err(e) = state.store.remember_rejections(&learned) {
                        tracing::warn!(error = %e, "persisting learned rejections failed (kept in memory)");
                    }
                }
                if !compressed && is_third_party_rejection(&err_bytes) {
                    log_third_party_rejection(&sent, &upstream.headers, &cred, status);
                    tracing::info!(
                        cred_id = cred.id, cred = %cred.label,
                        inbound_bytes = body.len(),
                        inbound_body = %String::from_utf8_lossy(&body),
                        "third-party rejection: dumping the INBOUND (client-original) request body for local replay"
                    );
                }
                // 「each thinking block must contain thinking」：出站前已经剥过空 thinking 块，
                // 还被拒就说明剥除条件与上游的真实判据有出入。把客户端原始请求体整体打出来
                // （与上面第三方拒绝那条同一取舍：可复现优先），再附一份出站体的结构摘要——
                // 摘要里 thinking 块带 len/sig_len，一眼能看出被拒的块有没有签名。
                if !compressed
                    && status == StatusCode::BAD_REQUEST
                    && is_empty_thinking_error(&err_bytes)
                {
                    let (_, message) = parse_upstream_error(&err_bytes);
                    let outbound = match serde_json::from_slice::<serde_json::Value>(&sent) {
                        Ok(v) => request_digest(&v).to_string(),
                        Err(_) => format!("<unparsable {} bytes>", sent.len()),
                    };
                    tracing::warn!(
                        cred_id = cred.id, cred = %cred.label,
                        upstream_message = %message,
                        outbound_digest = %outbound,
                        inbound_bytes = body.len(),
                        inbound_body = %String::from_utf8_lossy(&body),
                        "upstream rejected an empty thinking block; dumping the INBOUND (client-original) request body for local replay"
                    );
                }
                // 历史思考块验不过的那三条 400（签名 / 被改过 / `redacted_thinking` 的密文）：
                // 把上游点名的那个块在入站（客户端原件）与出站（luban 实际发出去的）两份体里
                // 对一遍，一行日志回答「是不是 luban 改的、改的是这个块还是它所在那一轮」。
                //
                // 三条共用这一段：它们的兜底各不相同，但问的是同一个问题，而
                // [`trace_thinking_block`] 本就不分块型（`thinking` 按 `signature` 配、
                // `redacted_thinking` 按 `data` 配）。`kind` 记是哪一条，见
                // [`thinking_block_error_kind`]。这条不打正文，故不受 `inbound_body` 那种体量之累。
                if !compressed
                    && status == StatusCode::BAD_REQUEST
                    && let Some(kind) = thinking_block_error_kind(&err_bytes)
                {
                    let (_, message) = parse_upstream_error(&err_bytes);
                    let trace = error_block_path(&message)
                        .and_then(|(mi, bi)| trace_thinking_block(&body, &sent, mi, bi));
                    match trace {
                        Some(t) => tracing::warn!(
                            cred_id = cred.id, cred = %cred.label,
                            kind,
                            upstream_message = %message,
                            outbound_at = %t.outbound_at,
                            inbound_at = %t.inbound_at,
                            payload_len = t.payload_len,
                            turn_identical = ?t.turn_identical,
                            inbound_turn = %t.inbound_turn,
                            outbound_turn = %t.outbound_turn,
                            "upstream rejected a historical thinking block; inbound_at=none means luban corrupted it, turn_identical=false means luban rewrote something else in that same assistant turn (tool-name mimicry), all matching means it arrived broken; inbound_at=ambiguous/unkeyed means the block could not be identified and nothing is being claimed"
                        ),
                        // 坐标解析不出来，或那个坐标上压根没有思考块：两者都说明这条错误的
                        // 形态与判据的假设对不上，原文打出来供修判据。
                        None => tracing::warn!(
                            cred_id = cred.id, cred = %cred.label,
                            kind,
                            upstream_message = %message,
                            "upstream rejected a historical thinking block, but the block it names could not be located in the outbound body"
                        ),
                    }
                }
                let banned =
                    (!compressed).then(|| detect_account_ban(status, &err_bytes)).flatten();
                if let Some(reason) = &banned {
                    tracing::warn!(
                        cred_id = cred.id, cred = %cred.label,
                        status = status.as_u16(),
                        reason = %reason,
                        "account-level error detected, auto-disabling the credential"
                    );
                    let ctx = ban_context(
                        reason,
                        "forward",
                        status,
                        &err_bytes,
                        request_id,
                        rl.upstream_request_id.as_deref(),
                    );
                    if let Err(e) = state.store.record_ban(cred.id, &ctx) {
                        tracing::warn!(error = %e, "failed to auto-disable the credential");
                    }
                }
                // thinking 签名降级重试。
                if status == StatusCode::BAD_REQUEST
                    && !compressed
                    && is_thinking_signature_error(&err_bytes)
                {
                    if !flags.thinking_signature_retry {
                        tracing::warn!(
                            cred_id = cred.id, cred = %cred.label,
                            "upstream rejected a thinking-block signature; demote-and-retry is off, passing through as is"
                        );
                    } else if let Some(up) = retry_demoted_thinking(
                        &upstream,
                        &cred,
                        &device_fp,
                        &body,
                        &mut rl,
                        "a thinking-block signature",
                    )
                    .await
                    {
                        return relay_upstream(up, rl, upgrade_stream, tool_names.clone()).await;
                    }
                }
                // thinking 块被修改降级重试（JSON 序列化改变了编码）。
                if status == StatusCode::BAD_REQUEST
                    && !compressed
                    && is_thinking_modified_error(&err_bytes)
                {
                    if !flags.thinking_modified_retry {
                        tracing::warn!(
                            cred_id = cred.id, cred = %cred.label,
                            "upstream rejected modified thinking blocks; demote-and-retry is off, passing through as is"
                        );
                    } else if let Some(up) = retry_demoted_thinking(
                        &upstream,
                        &cred,
                        &device_fp,
                        &body,
                        &mut rl,
                        "modified thinking blocks",
                    )
                    .await
                    {
                        return relay_upstream(up, rl, upgrade_stream, tool_names.clone()).await;
                    }
                }
                // `redacted_thinking` 密文验不过：与签名那条同一类事、同一个兜底。
                if status == StatusCode::BAD_REQUEST
                    && !compressed
                    && is_redacted_thinking_data_error(&err_bytes)
                {
                    if !flags.redacted_thinking_retry {
                        tracing::warn!(
                            cred_id = cred.id, cred = %cred.label,
                            "upstream rejected a redacted_thinking block's data; demote-and-retry is off, passing through as is"
                        );
                    } else if let Some(up) = retry_demoted_thinking(
                        &upstream,
                        &cred,
                        &device_fp,
                        &body,
                        &mut rl,
                        "a redacted_thinking block's data",
                    )
                    .await
                    {
                        return relay_upstream(up, rl, upgrade_stream, tool_names.clone()).await;
                    }
                }
                // assistant prefill 不支持时，剥掉末尾 assistant 轮后重试一次。
                if status == StatusCode::BAD_REQUEST
                    && !compressed
                    && is_prefill_not_supported_error(&err_bytes)
                    && let Some(up) =
                        retry_without_prefill(&upstream, &cred, &device_fp, &body, &mut rl).await
                {
                    return relay_upstream(up, rl, upgrade_stream, tool_names.clone()).await;
                }
                // luban 补的 `fallbacks` 被上游拒了（目标不在 allowed_fallback_models 之类）：
                // 记下来以后不补，这一发剥掉重试一次。客户端自己带的 fallbacks 不在此列——
                // 那是它的字段，被拒了照原样回给它：客户端带了数组时 `refusal_fallbacks` 就是
                // `None`（见上面装头处），这里判 `is_some()` 即「确是 luban 写进去的」。
                if status == StatusCode::BAD_REQUEST
                    && !compressed
                    && upstream.refusal_fallbacks.is_some()
                    && is_fallback_rejection(&err_bytes)
                {
                    if let Some(model) = req_model.as_deref()
                        && let Some(row) =
                            remember_fallback_rejection(&state.deprecated_fields, model, &err_bytes)
                        && let Err(e) = state.store.remember_rejections(&[row])
                    {
                        tracing::warn!(error = %e, "persisting the fallbacks rejection failed (kept in memory)");
                    }
                    if let Some(up) =
                        retry_without_fallbacks(&upstream, &cred, &device_fp, &body, &mut rl).await
                    {
                        return relay_upstream(up, rl, upgrade_stream, tool_names.clone()).await;
                    }
                }
                let err_bytes = match &tool_names {
                    Some(map) => Bytes::from(map.restore(&err_bytes)),
                    None => err_bytes,
                };
                return builder.body(Body::from(err_bytes)).unwrap_or_else(|e| {
                    error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
                });
            }

            // 429 且**一个限流头都没带**：这不是额度拒绝。上游的额度 429 必定带着
            // `anthropic-ratelimit-unified-*` 那一整套（见 [`rate_limit_scope`] 里两份实测
            // 样本），一条都没有的 429 来自更外层——网关/边缘的节流，或容量拒绝。
            //
            // 这一档在 [`rate_limit_scope`] 里只能落到 [`LimitScope::Transient`]（没有窗口
            // 可看），而 429 又不在上面那段 4xx 错误体日志的覆盖范围内（那里只收 400/401/403），
            // 于是服务端侧除了「撞了一发 429」之外**什么都不知道**。故这一档把能拿到的三类
            // 依据一次打全：
            //
            // 1. **响应体**——「速率限制还是容量拒绝」有时只写在这里（08d0b58 记下的那句
            //    「40,000 output tokens per minute」就是从body里读到的）；但它也可能只有一句
            //    `Error`（2026-08-20 实测），故光有它不够；
            // 2. **`request-id` / `x-should-retry`**——前者是与上游对话的唯一凭据，后者是上游
            //    自己对「这发能不能重试」的表态，见下面取头那一步；
            // 3. **我们这一侧的发送密度**——上游按组织/工作区的每分钟口径拒的，请求数、并发数、
            //    输出预算三者之一超了；它不说是哪一个，那就把三者的读数摆出来，见
            //    [`UpstreamLoad`]。
            //
            // 只在限流头全缺时打：正常的额度 429 头里已写明是哪个窗口满的、什么时候重置，
            // 上面那条 `upstream 429` 的 `ratelimit=` 已经带着全文，再刷一行没有意义。
            // 压缩体跳过，理由同上面那段：打出来只会是乱码字节。
            if status == StatusCode::TOO_MANY_REQUESTS
                && !compressed
                && rl.ratelimit.no_limit_headers()
            {
                let builder = resp_builder(&up);
                // 这两个头 [`RateLimitInfo`] 收不到（它的白名单只留 `anthropic-*` /
                // `*ratelimit*` / `retry-after`，而 `request-id` 连前缀都不带），可它们恰是
                // 这一档最缺的两句话：`request-id` 是上游侧唯一的抓手（对工单、跨系统核对都
                // 只认它，我们自己的日志里此前没有任何能与上游对上的标识），`x-should-retry`
                // 是上游**自己**对「这发能不能重试」的表态——它把「速率限制还是容量拒绝」
                // 这个我们一直只能猜的区分直接说了出来。`up.bytes()` 会吃掉 `up`，故先取走。
                let request_id = header_text(up.headers(), "request-id");
                let should_retry = header_text(up.headers(), "x-should-retry");
                let resp_headers = redact_headers(up.headers());
                return match up.bytes().await {
                    Ok(bytes) => {
                        rl.ttft_ms = Some(rl.started.elapsed().as_millis());
                        let (etype, message) = parse_upstream_error(&bytes);
                        rl.forensics.error_type = etype.clone();
                        rl.forensics.error_message = Some(message);
                        // 我们这一侧的发送密度，见 [`UpstreamLoad`]：这一档的成因（每分钟请求数
                        // / 并发连接数 / 输出 token 预算，三者之一）上游一个字都不说，只能拿
                        // 自己的读数去对它公布的限额。
                        let load = upstream_load_snapshot(
                            &state.upstream_load,
                            cred.id,
                            req_model.as_deref().unwrap_or("-"),
                        );
                        let out_headers = redact_headers(&upstream.headers);
                        tracing::warn!(
                            cred_id = cred.id, cred = %cred.label,
                            model = %req_model.as_deref().unwrap_or("-"),
                            error_type = %etype.as_deref().unwrap_or("-"),
                            request_id = %request_id,
                            should_retry = %should_retry,
                            max_tokens = req_max_tokens.unwrap_or(0),
                            stream = body_json.as_ref().is_some_and(stream_requested),
                            in_flight = state.in_flight.load(std::sync::atomic::Ordering::Relaxed),
                            cred_in_flight = load.cred_in_flight,
                            route_in_flight = load.route_in_flight,
                            sent_60s = load.sent,
                            max_tokens_60s = load.max_tokens,
                            inbound_body = %String::from_utf8_lossy(&body),
                            outbound_body = %String::from_utf8_lossy(&sent),
                            outbound_headers = %out_headers,
                            response_headers = %resp_headers,
                            response_body = %String::from_utf8_lossy(&bytes),
                            "upstream bare 429: full inbound/outbound dump for local debugging"
                        );
                        // 错误文本里可能回显假工具名，同 4xx 那一路顺手还原。
                        let bytes = match &tool_names {
                            Some(map) => Bytes::from(map.restore(&bytes)),
                            None => bytes,
                        };
                        builder.body(Body::from(bytes)).unwrap_or_else(|e| {
                            error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
                        })
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to read the upstream 429 body");
                        builder.body(Body::empty()).unwrap_or_else(|e| {
                            error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
                        })
                    }
                };
            }

            relay_upstream(up, rl, upgrade_stream, tool_names).await
        }
        Err(e) => {
            // wreq 顶层 Display 往往只有「error sending request」，真正原因在 source 链里。
            let detail = error_chain(&e);
            let kind = upstream_error_kind(&e);
            tracing::error!(
                %method,
                path = %path_and_query,
                kind,
                error = %detail,
                "upstream request failed"
            );
            // 这条路上 `ReqLog` 压根没建起来（那要有响应才行），失败遥测得就地补。
            // 官方客户端对连接层失败同样发 `tengu_api_error`，`errorType` 是
            // `connection_error`——SDK 那边这类没有状态码，见 [`crate::telemetry::error_kind`]。
            record_early_failure(
                &state,
                &cred,
                &upstream,
                &sent,
                started,
                flags,
                billable,
                None,
                // 请求压根没发出去，没有响应头可取组织 id；遥测那边按凭证缓存的那份还在。
                None,
                crate::telemetry::CallFailure {
                    status: None,
                    error_type: None,
                    message: detail.clone(),
                    in_band: false,
                },
            );
            log_early_upstream_failure(
                &state,
                log_state,
                &cred,
                &upstream,
                &sent,
                EarlyUpstreamFailure {
                    path: &path_and_query,
                    client_ua: &client_ua,
                    model: req_model.clone(),
                    device_id: logged_device,
                    started,
                    request_id,
                    upstream_request_id: None,
                    status: StatusCode::BAD_GATEWAY,
                    error_type: Some("api_error".into()),
                    error_message: Some(format!("upstream request failed [{kind}]: {detail}")),
                    third_party: false,
                    ratelimit: None,
                    tag: "connection_error",
                },
            );
            error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("upstream request failed [{kind}]: {detail}"),
            )
        }
    }
}

/// 早退路径上的流水 `device_id`，与正常路径 `logged_device` 同口径：来访自带的优先，没有就
/// 用模拟派生的那个（见 [`sim_device_id`]）。正常路径在 `match resp` 之前算一次；早退那几条
/// 在它之前就返回了，得各自算。
fn early_logged_device(
    device_id: &Option<String>,
    upstream: &Upstream<'_>,
    flags: store::ForwardFlags,
    cred: &crate::credentials::Credential,
    device_fp: &str,
) -> Option<String> {
    device_id.clone().or_else(|| {
        sim_device_id(
            upstream.sim.as_ref(),
            upstream.bare_session.as_deref(),
            flags,
            cred,
            device_fp,
        )
    })
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
