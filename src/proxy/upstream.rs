use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use futures_util::StreamExt;

use crate::config;
use crate::store;
use crate::web::AppState;

use super::ban::parse_upstream_error;
use super::body::{ToolNameMap, restore_tool_names_stream, rewrite_body, trusted_cc_version};
use super::headers::{is_resp_forwardable, orig_header_case};
use super::logging::{ReqLog, UsageSniffer};
use super::rate_limit::RateLimitInfo;
use super::session_link::{CcRequestKind, CcSessionLink};
use super::thinking::preserve_thinking_encoding;
use super::{Simulation, error_response, header_opt};

/// 一次转发要发往上游的全部固定入参（方法/URL/已装好的转发头/开关），只有请求体每次不同。
///
/// 存在的理由是**重试**：签名降级重试必须和首发除了 body 之外逐字节一致，否则「重试成功了」
/// 有可能只是因为顺手换了别的东西，排查时会被带偏。把这些一次装好、两次共用，就不存在
/// 「重建时漏了一项」的可能。
pub(super) struct Upstream<'a> {
    /// 本次请求该用的出站客户端——**由选中的那个凭证决定**，配了专用代理的号走它自己的
    /// 那一份。首发与换号重试各自重建 [`Upstream`]，所以换号时这里也跟着换，不会出现
    /// 「用 A 号的代理发 B 号的 token」。见 [`crate::clients::ClientPool`]。
    pub(super) client: wreq::Client,
    /// 只为把生命周期钉在 [`AppState`] 上（客户端已在 `client` 里取好）。
    pub(super) _state: std::marker::PhantomData<&'a AppState>,
    pub(super) method: Method,
    pub(super) url: String,
    /// [`build_forward_headers`] 的产物，逐次 clone 后发出。
    pub(super) headers: HeaderMap,
    pub(super) flags: store::ForwardFlags,
    /// 见 [`is_billable_messages`]。为假时出站体一律原样透传，见 [`Self::shape`]。
    pub(super) billable: bool,
    /// 非 CC 客户端的模拟参数；`None` 即来访本来就是 CC 形态。见 [`Simulation`]。
    pub(super) sim: Option<Simulation>,
    /// **CC 形态但不带 `metadata.user_id`** 的来访要补的那份身份用的 session_id
    /// （`sim` 为 `Some` 时恒为 `None`——那条路的会话 id 在 [`Simulation::session_id`] 里）。
    ///
    /// 这条路**只服务第三方 CC 兼容客户端**：系统提示词学了官方的，metadata 却不发。官方
    /// 每条请求都带那个字段，缺了就是一处白给的判据，所以替它补上（[`ensure_cc_metadata`]）。
    /// 真实 CC 客户端（UA 自报 `claude-cli/`）不在此列——它没带就是它的真实形态，见
    /// [`bare_session_id`] 的前提。
    /// 取值优先用来访自己带的 `X-Claude-Code-Session-Id`：官方头体两处逐字相同，另派生一个
    /// 只会让它们对不上，那比两处都缺更显眼；没带才派生，并由 [`build_forward_headers`]
    /// 把同一个值补进头里。
    pub(super) bare_session: Option<String>,
    /// 这条请求出站时两处（`X-Claude-Code-Session-Id` 头与 `metadata.user_id`）都要落的
    /// **同一个**会话 id；模拟路径为 `None`（那条的在 [`Simulation::session_id`]）。
    /// 取值与理由见 [`outbound_session_id`]。
    pub(super) session_out: Option<String>,
    /// 来访是非流式、要改写成 `stream:true` 发出（回程再聚合成整段 JSON）。
    /// 见 [`store::ForwardFlags::nonstream_as_sse`]。
    pub(super) force_stream: bool,
    /// 工具名混淆映射；`None` 即没有要混淆的工具（真 CC／全在白名单里／`tools` 为空），
    /// 此时请求与回程两侧都零开销。见 [`ToolNameMap`]。
    pub(super) tool_names: Option<std::sync::Arc<ToolNameMap>>,
    /// **真实 CC 来访**（非模拟）要补的会话关联字段：`(会话 id, 链)`。
    /// 判据与取值见 [`client_session_link`]；模拟那条路的链在 [`Simulation::link`] 里。
    pub(super) client_link: Option<(String, CcSessionLink)>,
    /// 这条来访属于哪一类官方 profile。除了会话链，它还管住「别把官方非流式请求改成
    /// 流式」与「别给额度探测补 system」两条，见 [`CcRequestKind`]。
    pub(super) cc_kind: CcRequestKind,
    /// 出站体要补的 `fallbacks` 字面量（[`refusal_fallbacks_for`]）；`None` 即不补。
    /// 有值时出站头已带 `server-side-fallback` beta（[`build_forward_headers_for`]）。
    pub(super) refusal_fallbacks: Option<&'static str>,
}

impl Upstream<'_> {
    /// 出站体改写的唯一入口：非计费路径（count_tokens 等）原样透传。
    ///
    /// 首发与「thinking 签名降级重试」两条路都必须走这里，否则同一条 count_tokens 会出现
    /// 首发透传、重试却被 shape 过的分裂形态。
    ///
    /// **模拟模式下 count_tokens 会低估**：出站头已经是官方那套，体却没补 system 前缀，
    /// 于是客户端数出来的 token 比它真发时少一个基座（opus 族约 300、sonnet 族约 2700）。
    /// 宁可低估也不在这条路径上改体：`count_tokens` 的请求体没有 `metadata`，改了既伪装不成
    /// 也只是多担一份上游挑刺的风险，而这条路径既不产生 usage 也不消耗额度。
    pub(super) fn shape(
        &self,
        body: &Bytes,
        cred: &crate::credentials::Credential,
        device_fp: &str,
    ) -> Bytes {
        self.shape_with(body, cred, device_fp, self.refusal_fallbacks)
    }

    /// [`Self::shape`] 指定要不要补 `fallbacks`：重试路径（[`retry_without_fallbacks`]）
    /// 传 `None`，其余一律传 `self.refusal_fallbacks`。
    fn shape_with(
        &self,
        body: &Bytes,
        cred: &crate::credentials::Credential,
        device_fp: &str,
        fallbacks: Option<&'static str>,
    ) -> Bytes {
        if self.billable {
            // body 侧要不要补 `thinking.display:"updates"`，看**实际发出的头**里有没有那项 beta
            // （[`merge_beta`] 只给 2.1.251+ 世代的 fable 补；agent-sdk / VSCode 扩展那类客户端
            // 的串没有 `advisor-tool`，头上不补，体里就也不能写，否则上游 400：
            // `thinking.adaptive.display: Input should be 'summarized', 'omitted'`）。
            let has_beta = |name: &str| {
                self.headers
                    .get("anthropic-beta")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|s| s.split(',').any(|b| b.trim() == name))
            };
            let display_beta = has_beta(config::CC_BETA_THINKING_DISPLAY_UPDATES);
            // 同理，工具声明上的 `eager_input_streaming` 只在出站头带了 `advanced-tool-use`
            // 时才补：带 eager 的官方请求头上都有它，见 [`config::CcEagerTools`]。
            let adv_beta = has_beta(config::CC_BETA_ADVANCED_TOOL_USE);
            // 给真实 CC 补 billing header 时写的是**它自报的**版本，不是 luban 自己那个：
            // 见 [`billing_header_text`]。模拟路径不看这个值（那条路的版本在 profile 里）。
            let client_version = self
                .headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .and_then(trusted_cc_version)
                .map(|(a, b, c)| format!("{a}.{b}.{c}"));
            rewrite_body(
                body,
                cred,
                device_fp,
                self.flags,
                self.sim.as_ref(),
                self.bare_session.as_deref(),
                self.session_out.as_deref(),
                self.force_stream,
                self.tool_names.as_deref(),
                display_beta,
                adv_beta,
                client_version.as_deref(),
                self.client_link.as_ref().map(|(_, l)| l),
                self.cc_kind,
                fallbacks,
            )
        } else {
            body.clone()
        }
    }

    /// 发一次。头名的拼写与顺序由 `orig_header_case` 决定（关掉即退回「全小写 +
    /// Host/User-Agent/Content-Length 钉在队尾」，也就是换 wreq 之前的形态）。
    pub(super) async fn send(&self, body: Bytes) -> Result<wreq::Response, wreq::Error> {
        let req = self
            .client
            .request(self.method.clone(), &self.url)
            .headers(self.headers.clone())
            .body(body);
        let req =
            if self.flags.orig_header_case { req.orig_headers(orig_header_case()) } else { req };
        req.send().await
    }
}

/// 从上游响应里取出决定「响应体怎么读」的两项：是否 SSE 流、以及我们解不开的
/// `content-encoding`（正常恒为 `None`，见 [`handle`] 里的说明）。
pub(super) fn resp_shape(up: &wreq::Response) -> (bool, Option<String>) {
    let is_stream = up
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    let encoding = up
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("identity"))
        .map(str::to_string);
    (is_stream, encoding)
}

/// 拼出回给客户端的响应骨架：上游状态码 + 放行的上游响应头（见 [`is_resp_forwardable`]）。
pub(super) fn resp_builder(up: &wreq::Response) -> axum::http::response::Builder {
    resp_builder_as(up, None)
}

/// 同 [`resp_builder`]，但把 `content-type` 换成 `ct`。
///
/// **必须在这一层换而不是事后 `.header()` 追加**：`Builder::header` 是**追加**语义，
/// 那样会得到两个 `content-type`（`text/event-stream` 在前），客户端按哪个都可能。
/// 聚合路径回的是整段 JSON，上游那份 SSE 的 `content-type` 必须原地替掉。
pub(super) fn resp_builder_as(
    up: &wreq::Response,
    ct: Option<&str>,
) -> axum::http::response::Builder {
    let mut builder = Response::builder().status(up.status());
    for (k, v) in up.headers().iter() {
        if !is_resp_forwardable(k) {
            continue;
        }
        if ct.is_some() && k == header::CONTENT_TYPE {
            continue;
        }
        builder = builder.header(k, v);
    }
    match ct {
        Some(ct) => builder.header(header::CONTENT_TYPE, ct),
        None => builder,
    }
}

/// 回程总入口：按来访形态决定原样流式回传，还是把上游的 SSE 聚合成整段 JSON。
///
/// `upgrade_stream` 为真即「来访是非流式、我们替它改成了流式」（见
/// [`store::ForwardFlags::nonstream_as_sse`]）。此时**只有上游真回了 SSE 才聚合**——
/// 上游若因为别的原因回了整段 JSON（形态没被接受、或哪天默认变了），原样透传才是对的，
/// 拿聚合器去解一份非 SSE 的 body 只会得到一个空 Message。
pub(super) async fn relay_upstream(
    up: wreq::Response,
    mut rl: ReqLog,
    upgrade_stream: bool,
    tool_names: Option<std::sync::Arc<ToolNameMap>>,
) -> Response {
    // 重试路径（thinking 降级、剥 prefill）会换一发上游响应再进来：记录以最终那一发为准。
    if let Some(rid) = header_opt(up.headers(), "request-id") {
        rl.upstream_request_id = Some(rid);
    }
    let (is_stream, _) = resp_shape(&up);
    if upgrade_stream && is_stream {
        aggregate_sse(up, rl, tool_names.as_deref()).await
    } else {
        stream_upstream(up, rl, tool_names)
    }
}

/// 把上游响应包成流式回传：首块到达记 TTFT，边转发边嗅探用量；
/// 流结束（或客户端断开）时 `rl` 在 Drop 里记 total、输出一条日志并落库。
fn stream_upstream(
    up: wreq::Response,
    mut rl: ReqLog,
    tool_names: Option<std::sync::Arc<ToolNameMap>>,
) -> Response {
    let builder = resp_builder(&up);
    let stream = up.bytes_stream().map(move |chunk| {
        if rl.ttft_ms.is_none() {
            rl.ttft_ms = Some(rl.started.elapsed().as_millis());
        }
        match &chunk {
            Ok(bytes) => rl.sniffer.feed(bytes),
            // 上游把流掐了。错误照旧原样交给 axum（客户端拿到的行为不变），但要留个痕
            // 给收尾时的日志——否则这条请求在服务端侧只剩一行 `forwarded status=200`。
            Err(e) => {
                rl.stream_broke = Some(format!("[{}] {e}", upstream_error_kind(e)));
            }
        }
        chunk
    });
    // 用量嗅探喂的是**还原前**的字节（`usage` 里没有工具名，两者等价），还原只包在最外层。
    let body = match tool_names {
        Some(map) => Body::from_stream(restore_tool_names_stream(stream, map)),
        None => Body::from_stream(stream),
    };
    builder
        .body(body)
        .unwrap_or_else(|e| error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string()))
}

/// 收齐上游 SSE、聚合成一条整段 JSON 的 Message 回给客户端（来访本来发的就是非流式）。
///
/// 用量嗅探照旧按 SSE 逐行走——**比非流式那条路更准**：整段 JSON 模式有 1MB 的累积上限，
/// 超了就整条丢用量，逐行模式没有这个限制。TTFT 记的是上游首字节，客户端感知不到（它只会
/// 在末尾一次性收到整段），故日志里这两列会不一致，`sse_aggregated=true` 用来标出这类记录。
pub(super) async fn aggregate_sse(
    up: wreq::Response,
    mut rl: ReqLog,
    tool_names: Option<&ToolNameMap>,
) -> Response {
    let builder = resp_builder_as(&up, Some("application/json"));
    rl.sse_aggregated = true;
    let mut agg = SseAggregator::default();
    let mut stream = up.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "upstream SSE broke while aggregating; failing the request");
                rl.status = StatusCode::BAD_GATEWAY.as_u16();
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("upstream stream failed: {e}"),
                );
            }
        };
        if rl.ttft_ms.is_none() {
            rl.ttft_ms = Some(rl.started.elapsed().as_millis());
        }
        rl.sniffer.feed(&bytes);
        agg.feed(&bytes);
    }
    match agg.finish() {
        // 正常收尾：整段 Message，`content-type` 已换成 application/json。
        Aggregated::Message(msg) => match serde_json::to_vec(&msg) {
            // 聚合完再还原：整段都在内存里，不必操心分块边界。
            Ok(body) => builder
                .body(Body::from(match tool_names {
                    Some(map) => map.restore(&body),
                    None => body,
                }))
                .unwrap_or_else(|e| {
                    error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
                }),
            Err(e) => {
                rl.status = StatusCode::BAD_GATEWAY.as_u16();
                error_response(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("failed to serialize the aggregated message: {e}"),
                )
            }
        },
        // 流中 `event: error`：错误 JSON 原样当响应体，**状态码按 `error.type` 映射**
        // （见 [`error_status`]）。这条错误是裹在 200 里来的，照搬 200 等于把一次失败记成
        // 成功——客户端要靠状态码分支，日志与统计也要靠它，所以翻译成非流式那边该有的那个。
        Aggregated::UpstreamError(payload) => {
            let status = error_status(&payload);
            tracing::warn!(
                status = status.as_u16(),
                error = %payload.get("error").map(|e| e.to_string()).unwrap_or_else(|| payload.to_string()),
                "upstream sent an error event mid-stream; mapping it to a status code"
            );
            rl.status = status.as_u16();
            // 同一份 error 事件也进了 sniffer（两者都在 feed 同一条流）。这条路已经就地
            // 告警并把状态码换给了客户端，留着它只会让 `ReqLog::drop` 再报一次同样的事。
            rl.sniffer.stream_error = None;
            match serde_json::to_vec(&payload) {
                Ok(body) => builder.status(status).body(Body::from(body)).unwrap_or_else(|e| {
                    error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
                }),
                Err(e) => {
                    rl.status = StatusCode::BAD_GATEWAY.as_u16();
                    error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
                }
            }
        }
        // 没收到 `message_stop` 就断了 → 502。**不能把攒了一半的内容当完整响应回去**：
        // 客户端拿到的会是一条看着正常、实则被截断的 Message，比一个明确的错误糟得多。
        Aggregated::Incomplete(why) => {
            tracing::warn!(reason = why, "upstream SSE ended without message_stop; returning 502");
            rl.status = StatusCode::BAD_GATEWAY.as_u16();
            error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("incomplete upstream stream: {why}"),
            )
        }
    }
}

/// 上游流中 `event: error` 的 `error.type` → HTTP 状态码。
///
/// 取值表照抄非流式那条路上同一个错误会用的状态码（见 Anthropic 的 errors 文档），
/// 这样开不开「非流式请求流式化」，客户端看到的状态码都一样。
///
/// **认不出来的类型一律 500**，不是 200：它确实是个错误，回 200 会让客户端与统计都把它
/// 当成功。500 是最不误导的兜底——客户端会当服务端故障重试，而不是把错误体当成模型输出。
pub(super) fn error_status(payload: &serde_json::Value) -> StatusCode {
    let kind =
        payload.get("error").and_then(|e| e.get("type")).and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "invalid_request_error" => StatusCode::BAD_REQUEST,
        "authentication_error" => StatusCode::UNAUTHORIZED,
        // 计费问题上游同样回 403（额度/欠费与权限不足共用一个状态码）。
        "permission_error" | "billing_error" => StatusCode::FORBIDDEN,
        "not_found_error" => StatusCode::NOT_FOUND,
        "request_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "timeout_error" => StatusCode::REQUEST_TIMEOUT,
        "rate_limit_error" => StatusCode::TOO_MANY_REQUESTS,
        // 529 不在 `StatusCode` 的常量表里，只能按数字构造（`http` 允许 100~999）。
        "overloaded_error" => {
            StatusCode::from_u16(529).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        }
        "api_error" => StatusCode::INTERNAL_SERVER_ERROR,
        other => {
            tracing::warn!(
                error_type = other,
                "unrecognized upstream error type in a mid-stream error event; falling back to 500"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// [`SseAggregator::finish`] 的三种结局。
pub(super) enum Aggregated {
    /// 收到了 `message_stop`，攒出一条完整 Message。
    Message(serde_json::Value),
    /// 流中来了 `event: error`，带上那份错误 JSON 原样回给客户端。
    UpstreamError(serde_json::Value),
    /// 流断在半路（没有 `message_start` 或没有 `message_stop`）。
    Incomplete(&'static str),
}

/// 把 `/v1/messages` 的 SSE 事件流攒回一条整段 Message，规则与官方各语言 SDK 一致：
///
/// | 事件 | 动作 |
/// |---|---|
/// | `message_start` | 取 `.message` 当骨架（`content` 清空重攒） |
/// | `content_block_start` | `content[index] = .content_block`（原样收下） |
/// | `content_block_delta` | 按 `delta.type` 追加到该块的对应字段 |
/// | `content_block_stop` | `input_json_delta` 攒的串在这里解析成 `.input` |
/// | `message_delta` | `.delta` 合进顶层、`.usage` 合进 `usage` |
/// | `message_stop` | 收尾 |
/// | `ping` / 未知事件 | 忽略 |
///
/// **未知的块类型自动透传**（`content_block_start` 整个收下，不挑字段），所以上游新增块类型
/// 时这里不用改。**未知的 `delta.type` 会丢内容**，故打一条 warn 而不是静默——那是唯一需要
/// 跟着上游演进的地方。
#[derive(Default)]
pub(super) struct SseAggregator {
    /// 未处理完的行尾（`feed` 按行切，最后一段不完整的留着等下一块）。
    buf: Vec<u8>,
    /// `message_start` 给的骨架；没收到它就说明流从一开始就不对。
    msg: Option<serde_json::Value>,
    /// 各 `tool_use` 块正在累积的 `partial_json`，键是块下标。
    partial_json: std::collections::HashMap<usize, String>,
    /// 已经 warn 过的未知 `delta.type`，同一条流里只报一次。
    warned: Vec<String>,
    /// 收到过 `message_stop`。
    done: bool,
    /// 流中的 `event: error` 负载（整个 data 对象）。
    error: Option<serde_json::Value>,
}

impl SseAggregator {
    /// 喂入一块响应字节，按整行处理，不完整的行尾留在 `buf` 里。
    pub(super) fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            self.parse_line(&line[..line.len() - 1]);
        }
        // 防御：异常超长行避免无界增长（与 [`UsageSniffer::feed`] 同口径）。
        if self.buf.len() > 1_000_000 {
            self.buf.clear();
        }
    }

    /// 解析一行。只认 `data:` 行——事件类型在 payload 自己的 `type` 字段里，
    /// `event:` 行没有额外信息，忽略即可。
    fn parse_line(&mut self, line: &[u8]) {
        let Ok(s) = std::str::from_utf8(line) else { return };
        let Some(json) = s.trim().strip_prefix("data:") else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json.trim()) else { return };
        self.apply(&v);
    }

    /// 处理一个已解析的事件。
    fn apply(&mut self, v: &serde_json::Value) {
        match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            "message_start" => {
                let Some(mut msg) = v.get("message").cloned() else { return };
                // 骨架里的 `content` 一律清空：官方那份是 `[]`，内容全靠后面的块事件攒。
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert("content".into(), serde_json::Value::Array(Vec::new()));
                }
                self.msg = Some(msg);
            }
            "content_block_start" => {
                let (Some(idx), Some(block)) = (event_index(v), v.get("content_block").cloned())
                else {
                    return;
                };
                if let Some(slot) = self.block_mut(idx) {
                    *slot = block;
                }
            }
            "content_block_delta" => {
                let (Some(idx), Some(delta)) = (event_index(v), v.get("delta").cloned()) else {
                    return;
                };
                self.apply_delta(idx, &delta);
            }
            "content_block_stop" => {
                let Some(idx) = event_index(v) else { return };
                // 攒完的 `partial_json` 在这里落成 `input`。空串意味着这个块没有增量，
                // 保留 `content_block_start` 给的那份（官方那份是 `{}`）。
                let Some(raw) = self.partial_json.remove(&idx).filter(|s| !s.is_empty()) else {
                    return;
                };
                match serde_json::from_str::<serde_json::Value>(&raw) {
                    Ok(input) => {
                        if let Some(block) = self.block_mut(idx)
                            && let Some(obj) = block.as_object_mut()
                        {
                            obj.insert("input".into(), input);
                        }
                    }
                    Err(e) => tracing::warn!(
                        index = idx,
                        error = %e,
                        "failed to parse the accumulated tool_use input_json; keeping the block's original input"
                    ),
                }
            }
            "message_delta" => {
                let Some(msg) = self.msg.as_mut().and_then(|m| m.as_object_mut()) else { return };
                // `delta` 里是顶层字段（stop_reason/stop_sequence/…）：逐个合进去，
                // 不认识的字段照样合——那是上游新增的顶层信息，丢了才是错。
                if let Some(delta) = v.get("delta").and_then(|d| d.as_object()) {
                    for (k, val) in delta {
                        msg.insert(k.clone(), val.clone());
                    }
                }
                // `usage` 是**增量覆盖**：这里给的是最终 output_tokens 等，逐键盖上去，
                // message_start 那份里没被提到的键（cache_read 等）保留。
                if let Some(usage) = v.get("usage").and_then(|u| u.as_object()) {
                    let slot = msg
                        .entry("usage")
                        .or_insert_with(|| serde_json::Value::Object(Default::default()));
                    if let Some(obj) = slot.as_object_mut() {
                        for (k, val) in usage {
                            obj.insert(k.clone(), val.clone());
                        }
                    }
                }
            }
            "message_stop" => self.done = true,
            // 上游明确报错：整份 data 收下（形状与非流式的错误响应体一致：`{type, error}`）。
            "error" => self.error = Some(v.clone()),
            // `ping` 与将来新增的事件：没有内容要攒，忽略。
            _ => {}
        }
    }

    /// 按 `delta.type` 把增量追加到对应字段。
    fn apply_delta(&mut self, idx: usize, delta: &serde_json::Value) {
        let kind = delta.get("type").and_then(|t| t.as_str()).unwrap_or_default();
        // 文本类三种：同样是「取一个字符串字段追加到块的同名目标字段」。
        let text = |field: &str| delta.get(field).and_then(|t| t.as_str()).unwrap_or_default();
        match kind {
            "text_delta" => self.append_str(idx, "text", text("text")),
            "thinking_delta" => self.append_str(idx, "thinking", text("thinking")),
            "signature_delta" => self.append_str(idx, "signature", text("signature")),
            // tool_use 的入参是分片的 JSON 串，攒到 content_block_stop 再整体解析。
            "input_json_delta" => {
                self.partial_json.entry(idx).or_default().push_str(text("partial_json"));
            }
            "citations_delta" => {
                let Some(citation) = delta.get("citation").cloned() else { return };
                if let Some(block) = self.block_mut(idx)
                    && let Some(obj) = block.as_object_mut()
                {
                    match obj.get_mut("citations").and_then(|c| c.as_array_mut()) {
                        Some(list) => list.push(citation),
                        None => {
                            obj.insert(
                                "citations".into(),
                                serde_json::Value::Array(vec![citation]),
                            );
                        }
                    }
                }
            }
            // 认不出来的增量类型 = 这块内容会丢。绝不静默：它是本聚合器唯一需要跟着上游
            // 演进的地方，日志里没有信号的话，症状会是「响应少了一段」而查不出所以然。
            other => {
                if !self.warned.iter().any(|w| w == other) {
                    self.warned.push(other.to_string());
                    tracing::warn!(
                        delta_type = other,
                        "unknown SSE delta type while aggregating; its content is dropped from the aggregated response"
                    );
                }
            }
        }
    }

    /// 把 `s` 追加到第 `idx` 块的 `field` 字段（字段不存在就新建）。
    fn append_str(&mut self, idx: usize, field: &str, s: &str) {
        if s.is_empty() {
            return;
        }
        let Some(block) = self.block_mut(idx) else { return };
        let Some(obj) = block.as_object_mut() else { return };
        match obj.get_mut(field).and_then(|t| t.as_str()).map(|t| format!("{t}{s}")) {
            Some(joined) => {
                obj.insert(field.into(), serde_json::Value::String(joined));
            }
            None => {
                obj.insert(field.into(), serde_json::Value::String(s.to_string()));
            }
        }
    }

    /// 取第 `idx` 块的可变引用，必要时用 `null` 把数组补长——块事件理论上顺序到达，
    /// 但下标是上游给的，按它填才不会因为一次乱序把内容写错位置。
    fn block_mut(&mut self, idx: usize) -> Option<&mut serde_json::Value> {
        let content = self.msg.as_mut()?.as_object_mut()?.get_mut("content")?.as_array_mut()?;
        while content.len() <= idx {
            content.push(serde_json::Value::Null);
        }
        content.get_mut(idx)
    }

    /// 收尾判定，见 [`Aggregated`]。
    pub(super) fn finish(self) -> Aggregated {
        if let Some(err) = self.error {
            return Aggregated::UpstreamError(err);
        }
        match self.msg {
            None => Aggregated::Incomplete("no message_start event"),
            Some(_) if !self.done => Aggregated::Incomplete("no message_stop event"),
            Some(msg) => Aggregated::Message(msg),
        }
    }
}

/// 取事件里的 `index`（块事件用它定位是第几块）。
fn event_index(v: &serde_json::Value) -> Option<usize> {
    v.get("index")?.as_u64().map(|i| i as usize)
}

/// 上游拒了 luban 补的 `fallbacks`（见 [`is_fallback_rejection`]）：同一个号、同一份客户端
/// 体，**不补 `fallbacks`** 再发一次。头上的 `server-side-fallback` beta 留着——「有 beta
/// 没字段」本身就是官方形态（[`config::known_fingerprint_gaps`] 第 7 条）。
///
/// 成了按重试那次记账，标签 `no_fallbacks`；没成把原来那条 400 原样透传。
pub(super) async fn retry_without_fallbacks(
    upstream: &Upstream<'_>,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    client_body: &Bytes,
    rl: &mut ReqLog,
) -> Option<wreq::Response> {
    let retried = upstream.shape_with(client_body, cred, device_fp, None);
    let up = match upstream.send(retried.clone()).await {
        Ok(up) => up,
        Err(e) => {
            tracing::warn!(error = %error_chain(&e), "the retry without fallbacks could not be sent, passing the original 400 through");
            return None;
        }
    };
    let status = up.status();
    if !status.is_success() {
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            status = status.as_u16(),
            "the retry without fallbacks was rejected too, passing the original 400 through"
        );
        return None;
    }
    let (is_stream, encoding) = resp_shape(&up);
    rl.status = status.as_u16();
    rl.ttft_ms = None;
    rl.sniffer = UsageSniffer::new(is_stream, encoding.is_some());
    rl.ratelimit = RateLimitInfo::from_headers(up.headers());
    rl.note_retry("no_fallbacks", up.headers(), &retried);
    Some(up)
}

/// `messages` 末尾是不是 `assistant` 轮——用已解析的 `body_json` 判，零开销。
pub(super) fn has_trailing_assistant(body: Option<&serde_json::Value>) -> bool {
    body.and_then(|v| v.get("messages"))
        .and_then(|m| m.as_array())
        .and_then(|a| a.last())
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        == Some("assistant")
}

/// 模型是否不支持 assistant message prefill（4.6+ 全系列均不支持）。
///
/// 用于在转发前主动剥掉末尾 assistant 轮，省去被上游 400 后再重试的往返。
/// 客户端可能带日期后缀（如 `claude-opus-4-6-20251114`），故用前缀匹配。
pub(super) fn model_rejects_prefill(model: &str) -> bool {
    [
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-4-6",
        "claude-sonnet-5",
        "claude-opus-5",
        "claude-fable-5",
        "claude-mythos-5",
    ]
    .iter()
    .any(|p| model.starts_with(p))
}

/// 上游那条 400 是不是「该模型不支持 assistant message prefill」，形如
/// `This model does not support assistant message prefill.`
///
/// 只按 message 文本判、不卡 `error.type`：同样归在 `invalid_request_error` 名下。
pub(super) fn is_prefill_not_supported_error(body: &[u8]) -> bool {
    let (_, message) = parse_upstream_error(body);
    let hay = message.to_lowercase();
    hay.contains("does not support") && hay.contains("prefill")
}

/// 剥掉 `messages` 末尾连续的 `assistant` 轮——也就是客户端发的 prefill。
///
/// 返回 `None` 表示末轮不是 assistant（不该走到这）或者剥完之后一条消息都不剩。
pub(super) fn strip_assistant_prefill(body: &Bytes) -> Option<Bytes> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let msgs = v.get_mut("messages")?.as_array_mut()?;
    let before = msgs.len();
    while msgs.last().and_then(|m| m.get("role")).and_then(|r| r.as_str()) == Some("assistant") {
        msgs.pop();
    }
    if msgs.is_empty() || msgs.len() == before {
        return None;
    }
    serde_json::to_vec(&v).ok().map(|bytes| Bytes::from(preserve_thinking_encoding(body, bytes)))
}

/// 展开 error 的 source 链，拼成「顶层 -> 次层 -> …」，暴露底层真实原因。
pub(super) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(inner) = src {
        let msg = inner.to_string();
        // 避免与上层完全重复的冗余拼接。
        if !s.ends_with(&msg) {
            s.push_str(" -> ");
            s.push_str(&msg);
        }
        src = inner.source();
    }
    s
}

/// 粗分上游 HTTP 客户端的错误类别，便于一眼定位（超时 / 连接 / DNS-TLS 等）。
pub(super) fn upstream_error_kind(e: &wreq::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_request() {
        "request"
    } else if e.is_body() {
        "body"
    } else if e.is_decode() {
        "decode"
    } else {
        "other"
    }
}

/// 在途请求计数的 RAII 句柄：构造时 +1，Drop 时 -1。
///
/// 挂在 [`ReqLog`] 上（而不是在 `handle` 末尾手工减一），于是它随**响应流**一起存活：
/// 一条流式回复要几十秒才走完，那整段时间它都确实占着一条上游连接，正是「并发」要数的东西。
/// 中途被拒的请求（限流、形态错误）不会走到 `ReqLog`，它们的句柄在 `handle` 返回时就 drop 了，
/// 也符合直觉——那些请求根本没发出去。
pub struct InFlightGuard(std::sync::Arc<std::sync::atomic::AtomicI64>);

impl InFlightGuard {
    pub(super) fn new(counter: std::sync::Arc<std::sync::atomic::AtomicI64>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 每会话并发在途上限：限制单个 session 同时在飞的请求数。
///
/// Claude Desktop 启动时会并行发 20+ 条 `max_tokens=1` 的 cache 预热请求，在一秒内全部打到
/// 上游，触发组织级的瞬时速率限制（裸 429）；随后代理侧的 strip-metadata 重试和换号逻辑
/// 会把 damage 扩散到整个凭证池，导致后续真正的请求也全部 429。
///
/// 并发上限把这种脉冲拉平：超过上限的请求直接返回 429 + 短 `retry-after`，客户端自行退避后
/// 重发，对上游的瞬时压力从 20+ 条削减到 3~5 条。
pub type SessionConcurrency = std::sync::Arc<parking_lot::Mutex<SessionConcurrencyTable>>;

/// 并发表本体。键是 session id，值是此刻在飞的请求数。归零即删键。
#[derive(Default)]
pub struct SessionConcurrencyTable {
    in_flight: std::collections::HashMap<String, u32>,
}

/// 占住一个 session 的并发格，Drop 时归还。挂在 [`ReqLog`] 上活到响应流结束。
pub struct SessionConcurrencyGuard {
    table: SessionConcurrency,
    session_id: String,
}

impl SessionConcurrencyGuard {
    pub(super) fn dummy(table: SessionConcurrency) -> Self {
        Self { table, session_id: String::new() }
    }
}

impl Drop for SessionConcurrencyGuard {
    fn drop(&mut self) {
        if self.session_id.is_empty() {
            return;
        }
        let mut t = self.table.lock();
        if let Some(n) = t.in_flight.get_mut(&self.session_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                t.in_flight.remove(&self.session_id);
            }
        }
    }
}

/// 尝试占一个 session 的并发格。成功返回 `Ok(guard)`，格子满了返回 `Err(当前在飞数)`。
/// `limit <= 0` 时不限（总返回 Ok）。
pub(super) fn try_acquire_session_concurrency(
    table: &SessionConcurrency,
    session_id: &str,
    limit: i64,
) -> Result<SessionConcurrencyGuard, u32> {
    if limit <= 0 {
        return Ok(SessionConcurrencyGuard {
            table: table.clone(),
            session_id: session_id.to_string(),
        });
    }
    let mut t = table.lock();
    let current = t.in_flight.get(session_id).copied().unwrap_or(0);
    if current >= limit as u32 {
        return Err(current);
    }
    *t.in_flight.entry(session_id.to_string()).or_default() += 1;
    drop(t);
    Ok(SessionConcurrencyGuard { table: table.clone(), session_id: session_id.to_string() })
}

/// 「账号 + 模型」维度的上游负载表：每条路线此刻有几条请求在上游那边跑着，以及每个账号在最近
/// [`UPSTREAM_SEND_WINDOW`] 内发出去了几条、一共声明了多少输出预算。
///
/// 存在的理由只有一个：**裸 429（一个限流头都不带的那一档）的成因只能从我们自己这一侧的发送
/// 密度反推**。那种 429 上游既不给 `anthropic-ratelimit-*`，错误文案也可能只有一句 `Error`
/// （2026-08-20 实测），而它按的是组织/工作区的**每分钟**口径——请求数、并发连接数、输出
/// token 预算，三者之一超了。上游不说是哪一个，那就把三者在我们这边的读数一并打出来（见
/// `carried no rate-limit headers at all` 那行日志），对着组织的限额就能对上号：08d0b58 记下的
/// 那句实测文案是「40,000 output tokens per minute」，只要 `max_tokens_60s` 越过 40000，
/// 这发 429 就已经解释完了，不必再猜是速率还是容量。
///
/// 三项**只为日志服务，不参与任何判定**：这一档的行为（不换号、按连撞档位退避）一个字节没动。
pub type UpstreamLoad = std::sync::Arc<parking_lot::Mutex<UpstreamLoadTable>>;

/// 发送记录的统计窗口。取 60 秒是因为上游那套限额本身就是「每分钟」的口径——窗口对不齐，
/// 读数与限额就没法直接比大小，而这条日志的全部用处就在这个可比性上。
pub(super) const UPSTREAM_SEND_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// [`UpstreamLoad`] 的表体。
#[derive(Default)]
pub struct UpstreamLoadTable {
    /// `(账号, 模型)` → 此刻在上游那边跑着的请求数。**归零即删键**，故不像
    /// [`TransientStreaks`] 那样需要清扫：模型名同样来自来访请求体（乱编就能造键），
    /// 但这里的键活不过它那几条请求。
    pub(super) in_flight: std::collections::HashMap<(i64, String), u32>,
    /// 账号 → 最近发出去的那些请求的 `(发送时刻, 声明的 max_tokens)`，按时刻升序。
    ///
    /// 键是账号 id（来自我们自己的库，有界），且每次触碰都会把滚出窗口的条目丢掉、空了就删键，
    /// 于是这张表同样自清。
    ///
    /// 记**声明的** `max_tokens` 而不是实际产出：上游那档输出限额是按请求声明的上限**预扣**的
    /// （官方文档口径），等产出算完早就拒了——这也正是「一个 token 都还没产出就撞 429」
    /// （`input_tokens=0`、`ttft_ms=218`）的解释。
    pub(super) sent:
        std::collections::HashMap<i64, std::collections::VecDeque<(std::time::Instant, i64)>>,
}

/// 一条已发往上游的请求在 [`UpstreamLoad`] 里占的那一格在飞数，Drop 时归还。
///
/// 和 [`InFlightGuard`] 一样挂在 [`ReqLog`] 上活到**响应流结束**，理由同上：流式回复那几十秒
/// 里连接是真占着的，而并发连接数正是这一档 429 的候选成因之一。换号重试时每轮重新占一格，
/// 旧的那格在赋值时就归还了。
pub struct UpstreamRouteGuard {
    load: UpstreamLoad,
    key: (i64, String),
}

impl Drop for UpstreamRouteGuard {
    fn drop(&mut self) {
        let mut table = self.load.lock();
        if let Some(n) = table.in_flight.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                table.in_flight.remove(&self.key);
            }
        }
    }
}

/// 记一条「这就发出去了」：占住这条路线的在飞格，并把发送时刻与声明的输出上限压进窗口，
/// 返回归还在飞格的句柄。
///
/// **必须在 `send` 之前调用**：上游是按它收到请求的那一刻计数的，我们这边晚记一步，读数就会
/// 在最要紧的那一瞬（一批并发同时在飞）系统性偏小。
pub(super) fn note_upstream_send(
    load: &UpstreamLoad,
    cred_id: i64,
    model: &str,
    max_tokens: i64,
) -> UpstreamRouteGuard {
    let key = (cred_id, model.to_string());
    {
        let mut table = load.lock();
        *table.in_flight.entry(key.clone()).or_default() += 1;
        let q = table.sent.entry(cred_id).or_default();
        prune_send_window(q);
        q.push_back((std::time::Instant::now(), max_tokens));
    }
    UpstreamRouteGuard { load: load.clone(), key }
}

/// 丢掉队首所有已滚出 [`UPSTREAM_SEND_WINDOW`] 的条目（队列按时刻升序，遇到第一条还在窗口内的
/// 即可停）。
fn prune_send_window(q: &mut std::collections::VecDeque<(std::time::Instant, i64)>) {
    while q.front().is_some_and(|(t, _)| t.elapsed() >= UPSTREAM_SEND_WINDOW) {
        q.pop_front();
    }
}

/// 一发裸 429 落地时，我们这一侧的发送密度读数，见 [`UpstreamLoad`]。
pub(super) struct UpstreamLoadSnapshot {
    /// 这条「账号 + 模型」路线此刻的在飞数，**含发起这次查询的这条请求自己**（它的格子还没归还）。
    pub(super) route_in_flight: u32,
    /// 这个账号全部模型合计的在飞数。组织/工作区那套限额不分模型，故这一项才是与限额同口径的
    /// 那个；分模型那项留着是为了看清「是不是全压在一个模型上」。
    pub(super) cred_in_flight: u32,
    /// 这个账号在窗口内发出去的请求数（含这一条）。
    pub(super) sent: usize,
    /// 同一批请求声明的 `max_tokens` 之和，没声明的按 0 计。
    pub(super) max_tokens: i64,
}

/// 取一份 [`UpstreamLoadSnapshot`]；顺手把这个账号已滚出窗口的发送记录丢掉（空了就删键）。
pub(super) fn upstream_load_snapshot(
    load: &UpstreamLoad,
    cred_id: i64,
    model: &str,
) -> UpstreamLoadSnapshot {
    let mut table = load.lock();
    let route_in_flight = table.in_flight.get(&(cred_id, model.to_string())).copied().unwrap_or(0);
    let cred_in_flight =
        table.in_flight.iter().filter(|((id, _), _)| *id == cred_id).map(|(_, n)| *n).sum();
    let (mut sent, mut max_tokens) = (0usize, 0i64);
    if let Some(q) = table.sent.get_mut(&cred_id) {
        prune_send_window(q);
        sent = q.len();
        max_tokens = q.iter().map(|(_, m)| *m).sum();
    }
    // 窗口内一条不剩就把键删了，见 `sent` 的说明（`get_mut` 那句借用还在，故挪到这里做）。
    if sent == 0 {
        table.sent.remove(&cred_id);
    }
    UpstreamLoadSnapshot { route_in_flight, cred_in_flight, sent, max_tokens }
}

#[cfg(test)]
mod tests {
    use crate::proxy::test_support::{gzip, rl_headers};
    use crate::proxy::{Bytes, HeaderValue, UsageSniffer, config, header, store};

    /// 起一个本地 HTTP 服务，用给定的响应字节应答，并把收到的请求头原样返回。
    fn serve_once(response: Vec<u8>) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut r = BufReader::new(&stream);
            let mut raw = String::new();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let end = line == "\r\n";
                raw.push_str(&line);
                if end {
                    break;
                }
            }
            (&stream).write_all(&response).unwrap();
            raw
        });
        (addr, h)
    }

    /// 上游客户端必须**透明解压**，否则用量嗅探拿到的是压缩字节、什么都解析不出来。
    ///
    /// 这正是线上花费统计消失的成因：v0.2.12 恢复转发 `accept-encoding` 让上游开始压缩响应，
    /// 但 reqwest 没开解压 feature，于是 `UsageSniffer` 被整个跳过——model、token、cost 全空。
    /// 本测试同时盯住两件事：解压 feature 在不在，以及请求侧声明的取值是否仍是官方那个。
    #[tokio::test]
    async fn upstream_client_decodes_gzip_and_keeps_official_accept_encoding() {
        // 一段真实形态的 SSE，压成 gzip 后由服务端返回。
        const SSE: &str = "event: message_start\n\
            data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-5\",\
            \"usage\":{\"input_tokens\":123,\"cache_read_input_tokens\":456}}}\n\n";
        let body = gzip(SSE.as_bytes());
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
             content-encoding: gzip\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        resp.extend_from_slice(&body);

        let (addr, server) = serve_once(resp);
        let up = crate::clients::upstream_client(None)
            .unwrap()
            .post(format!("http://{addr}/v1/messages"))
            .send()
            .await
            .unwrap();

        // wreq 解码后会把 content-encoding / content-length 一并摘掉。
        assert!(
            up.headers().get(header::CONTENT_ENCODING).is_none(),
            "解码后不该再有 content-encoding：{:?}",
            up.headers()
        );
        let bytes = up.bytes().await.unwrap();
        assert_eq!(&bytes[..], SSE.as_bytes(), "响应体应已是明文");

        // 明文喂给嗅探器就能拿到 model 与用量——这是花费统计的全部输入。
        let mut sniffer = UsageSniffer::new(true, false);
        sniffer.feed(&bytes);
        sniffer.finish();
        assert_eq!(sniffer.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(sniffer.input_tokens, Some(123));
        assert_eq!(sniffer.cache_read_tokens, Some(456));
        assert!(sniffer.has_usage());

        // 请求侧仍是官方取值，不是解压中间件那个 `zstd,gzip,deflate,br`。
        // 这条请求没经过 build_forward_headers，走的正是 default_headers 兜底那条路
        // ——和 luban 自身的刷新/profile 请求同一条。
        let raw = server.join().unwrap().to_ascii_lowercase();
        assert!(
            raw.contains(&format!("accept-encoding: {}\r\n", config::CC_ACCEPT_ENCODING)),
            "accept-encoding 应为官方取值:\n{raw}"
        );
    }

    /// 回复里的 `fallback` 块：嗅探器记下作答模型；Drop 时打标签 `served_by_fallback`。
    /// 输出前就被拒的（refusal + 零输出）花费记 0；流到一半被掐的照常计价。
    #[test]
    fn fallback_block_is_noted_and_pre_output_refusals_cost_nothing() {
        let mut s = crate::proxy::UsageSniffer::new(false, false);
        s.feed(br#"{"id":"msg_1","model":"claude-opus-4-8","content":[{"type":"fallback","from":{"model":"claude-opus-5"},"to":{"model":"claude-opus-4-8"}},{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#);
        s.finish();
        assert_eq!(s.fallback_to.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"), "计价按作答的模型");
        assert!(!s.refused());

        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        let log = |body: &[u8]| {
            let mut sniffer = crate::proxy::UsageSniffer::new(false, false);
            sniffer.feed(body);
            drop(crate::proxy::ReqLog {
                started: std::time::Instant::now(),
                ttft_ms: None,
                method: "POST".into(),
                path: "/v1/messages".into(),
                ua: "-".into(),
                ua_out: "-".into(),
                cred_id: cred.id,
                cred_label: cred.label.clone(),
                device_id: None,
                status: 200,
                sse_aggregated: false,
                sniffer,
                req_speed: None,
                req_model: Some("claude-opus-5".into()),
                ratelimit: rl_headers(&[]),
                stream_broke: None,
                request_id: "lb-test".into(),
                client_request_id: None,
                upstream_request_id: None,
                forensics: Default::default(),
                telemetry: None,
                cc_session: None,
                empty_reply_key: None,
                prompt_key: None,
                app_key: None,
                empty_replies: Default::default(),
                injected_tools: Vec::new(),
                store: store.clone(),
                _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
                _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(
                    Default::default(),
                ),
                _route_load: crate::proxy::note_upstream_send(&Default::default(), 0, "-", 0),
            })
        };
        // 1) 输出前被拒：花费 0，标签 refusal。
        log(br#"{"id":"msg_r","model":"claude-opus-5","content":[],"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"},"usage":{"input_tokens":1200,"output_tokens":0}}"#);
        // 2) fallback 作答：正常计价（按 4.8），标签 served_by_fallback。
        log(br#"{"id":"msg_f","model":"claude-opus-4-8","content":[{"type":"fallback","from":{"model":"claude-opus-5"},"to":{"model":"claude-opus-4-8"}},{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1000,"output_tokens":100}}"#);
        // 3) 流到一半被掐（refusal 但有输出）：照常计价。
        log(br#"{"id":"msg_m","model":"claude-opus-5","content":[{"type":"text","text":"part"}],"stop_reason":"refusal","usage":{"input_tokens":1000,"output_tokens":40}}"#);
        let logs = store.list_usage_logs(10).unwrap();
        assert_eq!(logs.len(), 3);
        assert_eq!(logs[2].cost_usd, Some(0.0), "输出前被拒不计费");
        assert_eq!(logs[2].forensics.rewrites.as_deref(), Some("refusal"));
        assert_eq!(logs[1].forensics.rewrites.as_deref(), Some("served_by_fallback"));
        assert_eq!(logs[1].model.as_deref(), Some("claude-opus-4-8"));
        assert!(logs[1].cost_usd.unwrap() > 0.0);
        assert!(logs[0].cost_usd.unwrap() > 0.0, "半截拒答按已产出计价");
        assert_eq!(logs[0].forensics.rewrites.as_deref(), Some("refusal"));
        // 4) 流到一半被掐、但 usage 里没有 output_tokens：正文已经流出来了，不能当「输出前」
        //    记 0——官方口径是已流出的输出与输入都计费。
        log(br#"{"id":"msg_n","model":"claude-opus-5","content":[{"type":"text","text":"part"}],"stop_reason":"refusal","usage":{"input_tokens":1000}}"#);
        // 5) 输出前被拒、usage 里同样没有 output_tokens：仍是 0。
        log(br#"{"id":"msg_z","model":"claude-opus-5","content":[],"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"},"usage":{"input_tokens":1000}}"#);
        // 6) 半截被掐、正文只有不认识的块类型（服务端工具结果）、usage 缺 output_tokens：
        //    见过内容块就不是「输出前」，照常计价。
        log(br#"{"id":"msg_u","model":"claude-opus-5","content":[{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[]}],"stop_reason":"refusal","usage":{"input_tokens":1000}}"#);
        // 7) 主模型与 fallback 都在输出前被拒：正文里只有 `fallback` 切换标记，它不是产出，
        //    仍按「输出前」记 0。
        log(br#"{"id":"msg_ff","model":"claude-opus-4-8","content":[{"type":"fallback","from":{"model":"claude-opus-5"},"to":{"model":"claude-opus-4-8"}}],"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"},"usage":{"input_tokens":1000,"output_tokens":0}}"#);
        let logs = store.list_usage_logs(10).unwrap();
        assert_eq!(logs.len(), 7);
        assert_eq!(logs[0].cost_usd, Some(0.0), "只有 fallback 切换标记：仍是输出前被拒");
        assert!(
            logs[1].cost_usd.unwrap() > 0.0,
            "不认识的内容块也算已输出：usage 缺 output_tokens 时不能记 0"
        );
        assert_eq!(logs[2].cost_usd, Some(0.0), "输出前被拒、usage 缺 output_tokens：仍记 0");
        assert!(
            logs[3].cost_usd.unwrap() > 0.0,
            "半截拒答、usage 缺 output_tokens：按输入计价，不能记 0"
        );
    }

    /// 上游回 200 却零输出：收尾时响应体开头落进流水的 `response_excerpt`、标签 `empty_reply`，
    /// 这一类（模型 + max_tokens）学进记忆表并写穿落库；半截流（没等到 `message_stop`）与
    /// 正常回复都不算。
    #[test]
    fn a_zero_output_reply_is_captured_and_its_request_class_learned_on_drop() {
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        let mem = crate::proxy::EmptyReplyMemory::default();
        let log = |is_stream: bool,
                   body: &[u8],
                   key: Option<(&str, i64)>,
                   prompt: Option<(&str, &str)>| {
            let mut sniffer = crate::proxy::UsageSniffer::new(is_stream, false);
            sniffer.feed(body);
            drop(crate::proxy::ReqLog {
                started: std::time::Instant::now(),
                ttft_ms: None,
                method: "POST".into(),
                path: "/v1/messages".into(),
                ua: "Go-http-client/1.1".into(),
                ua_out: config::CC_USER_AGENT.into(),
                cred_id: cred.id,
                cred_label: cred.label.clone(),
                device_id: None,
                status: 200,
                sse_aggregated: false,
                sniffer,
                req_speed: None,
                req_model: Some("claude-fable-5".into()),
                ratelimit: rl_headers(&[]),
                stream_broke: None,
                request_id: "lb-test".into(),
                client_request_id: None,
                upstream_request_id: None,
                forensics: Default::default(),
                telemetry: None,
                cc_session: None,
                empty_reply_key: key.map(|(m, n)| (m.to_string(), n)),
                prompt_key: prompt.map(|(m, d)| (m.to_string(), d.to_string())),
                // 与提示词哈希同源的 system 哈希：按应用学的那条与按提示词学的那条一起验。
                app_key: prompt.map(|(m, d)| (m.to_string(), format!("app-{d}"))),
                empty_replies: mem.clone(),
                injected_tools: Vec::new(),
                store: store.clone(),
                _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
                _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(
                    Default::default(),
                ),
                _route_load: crate::proxy::note_upstream_send(&Default::default(), 0, "-", 0),
            })
        };
        let empty = br#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-fable-5","content":[],"stop_reason":"end_turn","usage":{"input_tokens":425,"output_tokens":0}}"#;
        // 1) 非流式零输出：学到 + 落库 + 流水带原文；不是拒答，不按提示词学。
        log(false, empty, Some(("claude-fable-5", 16)), Some(("claude-fable-5", "aa")));
        assert_eq!(
            mem.read().classes.get(&("claude-fable-5".to_string(), 16)).map(String::as_str),
            Some(std::str::from_utf8(empty).unwrap())
        );
        assert!(mem.read().prompts.is_empty(), "零输出不是拒答，不按提示词学");
        let rows = store.learned_rejections().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (
                rows[0].kind.as_str(),
                rows[0].model.as_str(),
                rows[0].field.as_str(),
                rows[0].value.as_str()
            ),
            ("empty_reply", "claude-fable-5", "max_tokens", "16")
        );
        assert!(rows[0].message.starts_with(r#"{"id":"msg_1""#), "上游原话落库供人看");
        // 2) 正常回复（输出 1）：什么都不记。
        log(
            false,
            br#"{"id":"msg_2","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":427,"output_tokens":1}}"#,
            Some(("claude-fable-5-1", 1)),
            Some(("claude-fable-5-1", "bb")),
        );
        assert!(!mem.read().classes.contains_key(&("claude-fable-5-1".to_string(), 1)));
        assert!(mem.read().prompts.is_empty());
        // 3) 流式：`message_start` 报 0 但没等到 `message_stop`——半截流，不算零输出。
        log(
            true,
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":425,\"output_tokens\":0}}}\n\n",
            Some(("claude-fable-5", 4)),
            None,
        );
        assert!(!mem.read().classes.contains_key(&("claude-fable-5".to_string(), 4)), "半截流不学");
        // 4) 流式且完整收尾、最终 output_tokens = 0：学。
        log(
            true,
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":425,\"output_tokens\":0}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":0}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            Some(("claude-fable-5", 4)),
            None,
        );
        assert!(mem.read().classes.contains_key(&("claude-fable-5".to_string(), 4)));
        // 5) 不属于任何一类（带 tools 的请求）却回了零输出：流水照记原文，记忆表不动。
        log(false, empty, None, None);
        assert_eq!(mem.read().classes.len(), 2);
        // 6) 拒答（`stop_reason: "refusal"`，实测 opus-5 + max_tokens 65536 的 cyber 拒答）：
        //    按提示词哈希学，**不**按请求类学——否则一条内容连坐同形态的所有正常请求。
        let refusal = br#"{"model":"claude-opus-5","id":"msg_2","type":"message","role":"assistant","content":[],"stop_reason":"refusal","stop_sequence":null,"stop_details":{"type":"refusal","category":"cyber","explanation":"blocked"},"usage":{"input_tokens":1200,"output_tokens":0}}"#;
        log(false, refusal, Some(("claude-opus-5", 65536)), Some(("claude-opus-5", "deadbeef")));
        assert!(
            !mem.read().classes.contains_key(&("claude-opus-5".to_string(), 65536)),
            "拒答不能学成请求类"
        );
        assert!(
            mem.read()
                .prompts
                .get(&("claude-opus-5".to_string(), "deadbeef".to_string()))
                .unwrap()
                .verdict
                .contains(r#""category":"cyber""#)
        );
        // 按应用学的那条（测试里 app_key 与提示词哈希同源）：一条拒答只记计数，不到门槛不学。
        assert!(mem.read().apps.is_empty(), "一条拒答不够按应用学");
        assert_eq!(
            mem.read().app_counters.get(&("claude-opus-5".to_string(), "app-deadbeef".to_string())),
            Some(&crate::proxy::AppCounter { total: 1, refused: 1 })
        );
        let rows = store.learned_rejections().unwrap();
        assert!(rows.iter().all(|r| r.kind != "app_refusal"));
        let refused_row = rows.iter().find(|r| r.kind == "refusal").expect("拒答规则落库");
        assert_eq!(
            (refused_row.field.as_str(), refused_row.value.as_str()),
            ("prompt_sha", "deadbeef")
        );
        assert!(rows.iter().all(|r| !(r.kind == "empty_reply" && r.model == "claude-opus-5")));
        // 7) 拒答带半截正文（流式、output_tokens > 0）同样算：判据是 stop_reason，不是 0；
        //    流式的 `stop_details` 在 `message_delta.delta` 里，类别从那里取。
        log(
            true,
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"type\":\"refusal\",\"category\":\"cyber\",\"recommended_model\":null}},\"usage\":{\"output_tokens\":37}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            Some(("claude-opus-5", 65536)),
            Some(("claude-opus-5", "cafe")),
        );
        assert!(
            mem.read().prompts.contains_key(&("claude-opus-5".to_string(), "cafe".to_string()))
        );
        assert!(!mem.read().classes.contains_key(&("claude-opus-5".to_string(), 65536)));

        // 流水（倒序）：7、6、5、4、1 带原文，2、3 不带；标签按种类分。
        let logs = store.list_usage_logs(10).unwrap();
        assert_eq!(logs.len(), 7);
        let excerpted: Vec<bool> =
            logs.iter().map(|l| l.forensics.response_excerpt.is_some()).collect();
        assert_eq!(excerpted, vec![true, true, true, true, false, false, true]);
        assert_eq!(logs[0].forensics.rewrites.as_deref(), Some("refusal"));
        assert_eq!(logs[1].forensics.rewrites.as_deref(), Some("refusal"));
        assert_eq!(logs[2].forensics.rewrites.as_deref(), Some("empty_reply"));
        assert_eq!(logs[6].forensics.rewrites.as_deref(), Some("empty_reply"));
        assert_eq!(
            logs[6].forensics.response_excerpt.as_deref(),
            Some(std::str::from_utf8(empty).unwrap())
        );
        assert_eq!(logs[5].forensics.rewrites, None);
        assert!(
            logs[3]
                .forensics
                .response_excerpt
                .as_deref()
                .unwrap()
                .starts_with("event: message_start")
        );
        // 学到的规则文案 = 「[类别] stop_details=<原样 JSON>」——带上游的解释字段，不带
        // 响应体开头那段 usage 样板；流式的判决在 `message_delta` 里，文案照样取到它而不是
        // `message_start`。
        let deadbeef_entry = mem
            .read()
            .prompts
            .get(&("claude-opus-5".to_string(), "deadbeef".to_string()))
            .cloned()
            .unwrap();
        // 回放体 = 上游那次的原样响应（非流式整段 JSON），一个字节不差。
        assert_eq!(
            deadbeef_entry.reply,
            store::LearnedReply { sse: false, body: std::str::from_utf8(refusal).unwrap().into() }
        );
        let deadbeef = deadbeef_entry.verdict;
        assert!(deadbeef.starts_with("[cyber] stop_details={"), "{deadbeef}");
        assert!(deadbeef.contains(r#""explanation":"blocked""#), "{deadbeef}");
        assert!(!deadbeef.contains("usage"), "文案不该是响应体开头：{deadbeef}");
        let cafe_entry = mem
            .read()
            .prompts
            .get(&("claude-opus-5".to_string(), "cafe".to_string()))
            .cloned()
            .unwrap();
        // 流式学到的回放体是整条 SSE 原文，形态标 sse。
        assert!(cafe_entry.reply.sse);
        assert!(
            cafe_entry.reply.body.starts_with("event: message_start\n"),
            "{}",
            cafe_entry.reply.body
        );
        assert!(
            cafe_entry
                .reply
                .body
                .ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
        );
        let cafe = cafe_entry.verdict;
        assert!(
            cafe.starts_with(r#"[cyber] stop_details={"type":"refusal","category":"cyber""#),
            "{cafe}"
        );
        assert!(!cafe.contains("message_start"), "流式文案不该是流开头：{cafe}");
        assert!(
            store
                .learned_rejections()
                .unwrap()
                .iter()
                .any(|r| r.kind == "refusal" && r.value == "cafe" && r.message == cafe),
            "落库的文案与进程内一致"
        );

        // 8) category 为空的拒答是模型自己拒的（带采样，重发可能就答）：标签、原文照记，
        //    **不学**。
        let model_refusal = br#"{"model":"claude-opus-5","id":"msg_3","type":"message","role":"assistant","content":[],"stop_reason":"refusal","stop_details":{"type":"refusal","category":null,"explanation":null},"usage":{"input_tokens":1200,"output_tokens":0}}"#;
        log(false, model_refusal, Some(("claude-opus-5", 65536)), Some(("claude-opus-5", "f00d")));
        assert!(
            !mem.read().prompts.contains_key(&("claude-opus-5".to_string(), "f00d".to_string())),
            "模型自己的拒绝不能锁提示词"
        );
        // 9) 带 recommended_model 的拒答：fallback 模型限流没跑成，直接重试可能就成——不学。
        let unserved = br#"{"model":"claude-opus-5","id":"msg_4","type":"message","role":"assistant","content":[],"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","recommended_model":"claude-opus-4-8"},"usage":{"input_tokens":1200,"output_tokens":0}}"#;
        log(false, unserved, Some(("claude-opus-5", 65536)), Some(("claude-opus-5", "beef")));
        assert!(
            !mem.read().prompts.contains_key(&("claude-opus-5".to_string(), "beef".to_string())),
            "fallback 没跑成的拒答不能锁提示词"
        );
        assert!(!mem.read().classes.contains_key(&("claude-opus-5".to_string(), 65536)));
        // 10) 分类器判决齐全、但回复里已经有输出内容块（流到一半才被掐）：没有可回放的确定性
        //     响应——流水照记 refusal 标签，**不学**。
        log(
            true,
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"half\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"type\":\"refusal\",\"category\":\"cyber\"}},\"usage\":{\"output_tokens\":37}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            None,
            Some(("claude-opus-5", "m1d")),
        );
        assert!(
            !mem.read().prompts.contains_key(&("claude-opus-5".to_string(), "m1d".to_string())),
            "流到一半才被掐的拒答没有可回放的响应，不学"
        );
        assert!(
            !mem.read().apps.contains_key(&("claude-opus-5".to_string(), "app-m1d".to_string())),
            "按应用学的同样要求体可回放"
        );
        // 流到一半才被掐的：不算可学的拒答，只给应用的总数记一笔（分母）。
        assert_eq!(
            mem.read().app_counters.get(&("claude-opus-5".to_string(), "app-m1d".to_string())),
            Some(&crate::proxy::AppCounter { total: 1, refused: 0 })
        );
        let logs = store.list_usage_logs(10).unwrap();
        assert_eq!(logs.len(), 10);
        assert_eq!(logs[0].forensics.rewrites.as_deref(), Some("refusal"));
        assert_eq!(logs[1].forensics.rewrites.as_deref(), Some("refusal"));
        assert_eq!(logs[2].forensics.rewrites.as_deref(), Some("refusal"));
        assert!(logs[0].forensics.response_excerpt.is_some());
        assert!(logs[1].forensics.response_excerpt.is_some());
        let rows = store.learned_rejections().unwrap();
        assert!(
            rows.iter()
                .all(|r| r.kind != "refusal" || matches!(r.value.as_str(), "deadbeef" | "cafe")),
            "落库的拒答规则只有分类器判决那两条"
        );
    }

    /// 两份 UA 各存各的：入站记来访那份、出站记实际发出去那份，`-` 占位一律还原成 NULL
    /// （存进去就成了一个真实存在的 UA，按 UA 分组时会凭空多出一类）。
    #[test]
    fn client_ua_lands_in_the_usage_log() {
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        let log = |ua: &str, ua_out: &str| {
            drop(crate::proxy::ReqLog {
                started: std::time::Instant::now(),
                ttft_ms: None,
                method: "POST".into(),
                path: "/v1/messages?beta=true".into(),
                ua: ua.into(),
                ua_out: ua_out.into(),
                cred_id: cred.id,
                cred_label: cred.label.clone(),
                device_id: None,
                status: 200,
                sse_aggregated: false,
                sniffer: crate::proxy::UsageSniffer::new(false, false),
                req_speed: None,
                req_model: None,
                ratelimit: rl_headers(&[]),
                stream_broke: None,
                request_id: "lb-test".into(),
                client_request_id: None,
                upstream_request_id: None,
                forensics: Default::default(),
                telemetry: None,
                cc_session: None,
                empty_reply_key: None,
                prompt_key: None,
                app_key: None,
                empty_replies: Default::default(),
                injected_tools: Vec::new(),
                store: store.clone(),
                _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
                _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(
                    Default::default(),
                ),
                _route_load: crate::proxy::note_upstream_send(&Default::default(), 0, "-", 0),
            })
        };
        // 非模拟路径：来访那份原样转发，两列相同。
        log(config::CC_USER_AGENT, config::CC_USER_AGENT);
        // 模拟路径：来访是第三方客户端，出站换成官方那串——正是分两列才看得见的东西。
        log("python-httpx/0.27.0", config::CC_USER_AGENT);
        // 两边都没有（裸请求且开关关到不补头）。
        log("-", "-");

        // 倒序：后写的那条在前。
        let logs = store.list_usage_logs(10).unwrap();
        assert_eq!(logs.len(), 3);
        assert_eq!(logs[0].ua, None, "没带 UA 的请求不该存成 `-`");
        assert_eq!(logs[0].ua_out, None);
        assert_eq!(logs[1].ua.as_deref(), Some("python-httpx/0.27.0"), "来访那份是第三方客户端");
        assert_eq!(logs[1].ua_out.as_deref(), Some(config::CC_USER_AGENT), "出站换成了官方那串");
        assert_eq!(logs[2].ua.as_deref(), Some(config::CC_USER_AGENT));
        assert_eq!(logs[2].ua_out.as_deref(), Some(config::CC_USER_AGENT));
    }

    /// 透传流路径（`sse_aggregated=false`，绝大多数请求走这条）上，上游在 200 的流中途
    /// 模型调了模拟路径注进去、客户端没声明的官方工具：`rewrites` 列打 `injected_tool_called`
    /// 标签。客户端会收到一个自己不认识的 tool_use，这是注入策略的已知代价；此前只在文档里
    /// 写着「概率低」，没有任何一处量过——流水的 `shape` 只记 tool_use 个数，遥测那张表又把
    /// 名字归了类。调的是客户端自己的工具（假名 `mcp__luban__*`）或没注过的名字，不打标签。
    #[test]
    fn a_call_to_an_injected_tool_is_tagged_in_the_flow_log() {
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        let build = |injected: Vec<&'static str>| crate::proxy::ReqLog {
            started: std::time::Instant::now(),
            ttft_ms: None,
            method: "POST".into(),
            path: "/v1/messages?beta=true".into(),
            ua: "Go-http-client/1.1".into(),
            ua_out: config::CC_USER_AGENT.into(),
            cred_id: cred.id,
            cred_label: cred.label.clone(),
            device_id: None,
            status: 200,
            sse_aggregated: false,
            sniffer: crate::proxy::UsageSniffer::new(true, false),
            req_speed: None,
            req_model: Some("claude-opus-5".into()),
            ratelimit: rl_headers(&[]),
            stream_broke: None,
            request_id: "lb-test".into(),
            client_request_id: None,
            upstream_request_id: None,
            forensics: Default::default(),
            telemetry: None,
            cc_session: None,
            empty_reply_key: None,
            prompt_key: None,
            app_key: None,
            empty_replies: Default::default(),
            injected_tools: injected,
            store: store.clone(),
            _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
            _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(Default::default()),
            _route_load: crate::proxy::note_upstream_send(&Default::default(), 0, "-", 0),
        };
        let reply = |name: &str| -> Vec<u8> {
            const SSE: &str = "event: message_start
data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":2}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t0\",\"name\":\"NAME\",\"input\":{}}}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}

event: message_stop
data: {\"type\":\"message_stop\"}

";
            SSE.replace("NAME", name).into_bytes()
        };
        // 调了注入的 Bash → 打标签。
        let mut rl = build(vec!["Bash", "Edit", "Read", "Write"]);
        rl.sniffer.feed(&reply("Bash"));
        drop(rl);
        // 调的是客户端自己的工具（假名）→ 不打。
        let mut rl = build(vec!["Bash", "Edit", "Read", "Write"]);
        rl.sniffer.feed(&reply("mcp__luban__query_bas00"));
        drop(rl);
        // 非模拟路径（没注过）调 Bash → 不打：那是客户端自己声明的 Bash。
        let mut rl = build(Vec::new());
        rl.sniffer.feed(&reply("Bash"));
        drop(rl);

        let logs = store.list_usage_logs(10).unwrap();
        assert_eq!(logs.len(), 3);
        // list 按时间倒序：最后写入的在前。
        let tags: Vec<Option<&str>> =
            logs.iter().rev().map(|l| l.forensics.rewrites.as_deref()).collect();
        assert_eq!(tags, [Some("injected_tool_called"), None, None], "{tags:?}");
        assert_eq!(logs[2].status, 200, "标签不改状态码与记账");
    }

    /// 改口报错：客户端已经收到 200 头，改不动，但**记账**要按真实结果走。
    ///
    /// 这条曾是纯盲区。线上实例的原始形态是：`message_start` 与 `message_delta` 都到了，
    /// 随后上游发 `event: error`，我们原样透传，客户端报错，而服务端只留下一行
    /// `forwarded status=200 has_usage=true`，还照常算了花费——唯一的线索是
    /// `output_tokens` 小得离谱（实测那次是 2）。
    #[test]
    fn mid_stream_error_is_billed_as_the_mapped_status() {
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        let mut rl = crate::proxy::ReqLog {
            started: std::time::Instant::now(),
            ttft_ms: None,
            method: "POST".into(),
            path: "/v1/messages?beta=true".into(),
            ua: "-".into(),
            ua_out: "-".into(),
            cred_id: cred.id,
            cred_label: cred.label.clone(),
            device_id: None,
            status: 200,
            sse_aggregated: false,
            sniffer: crate::proxy::UsageSniffer::new(true, false),
            req_speed: None,
            req_model: None,
            ratelimit: rl_headers(&[]),
            stream_broke: None,
            request_id: "lb-test".into(),
            client_request_id: None,
            upstream_request_id: None,
            forensics: Default::default(),
            telemetry: None,
            cc_session: None,
            empty_reply_key: None,
            prompt_key: None,
            app_key: None,
            empty_replies: Default::default(),
            injected_tools: Vec::new(),
            store: store.clone(),
            _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
            _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(Default::default()),
            _route_load: crate::proxy::note_upstream_send(&Default::default(), 0, "-", 0),
        };
        rl.sniffer.feed(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":2,\"cache_read_input_tokens\":47030}}}\n\n",
        );
        rl.sniffer.feed(
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
        drop(rl);

        let logs = store.list_usage_logs(10).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status, 529, "流中途报错不能记成 200——失败会从成功率里消失");
        assert_eq!(logs[0].model.as_deref(), Some("claude-opus-5"), "用量照旧嗅探，不受影响");
        assert_eq!(logs[0].cache_read_tokens, Some(47030));
    }

    /// 内部重试成功后，**请求侧**的遥测也要换成重试实际发出去的那份 body。
    ///
    /// 此前只换了响应侧（状态码、用量、model、stop_reason、限流），请求侧还挂着首发那份
    /// ——于是同一条 `tengu_api_success` 里，`requestId`/token 来自重试那一发，而
    /// `messageCount`/`inputTextCharLength`/`toolsCount` 算的是**一条被上游拒了的请求**。
    /// prefill 那条尤其明显：[`crate::proxy::strip_assistant_prefill`] 直接弹掉末尾的 assistant 轮。
    #[test]
    fn a_successful_retry_reports_the_body_it_actually_sent() {
        use base64::Engine as _;
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        let sink = crate::telemetry::Telemetry::default();
        // 首发：3 条消息，末条是 assistant prefill。重试：剥掉它，只剩 2 条。
        let body = |msgs: &str| -> Bytes {
            Bytes::from(format!(
                r#"{{"model":"claude-opus-5","messages":[{msgs}],"system":[{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}}],"metadata":{{"user_id":"{{\"device_id\":\"dd\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}}"}},"max_tokens":64000}}"#
            ))
        };
        let first = body(
            r#"{"role":"user","content":"hi"},{"role":"assistant","content":"a"},{"role":"assistant","content":"prefill"}"#,
        );
        let retried = body(r#"{"role":"user","content":"hi"},{"role":"assistant","content":"a"}"#);

        let mut rl = crate::proxy::ReqLog {
            started: std::time::Instant::now(),
            ttft_ms: Some(10),
            method: "POST".into(),
            path: "/v1/messages".into(),
            ua: config::CC_USER_AGENT.into(),
            ua_out: config::CC_USER_AGENT.into(),
            cred_id: cred.id,
            cred_label: cred.label.clone(),
            device_id: None,
            status: 200,
            sse_aggregated: false,
            sniffer: crate::proxy::UsageSniffer::new(false, false),
            req_speed: None,
            req_model: None,
            ratelimit: rl_headers(&[]),
            stream_broke: None,
            request_id: "lb-test".into(),
            client_request_id: None,
            upstream_request_id: Some("req_first".into()),
            forensics: store::Forensics {
                shape: crate::proxy::shape_summary(&first).0,
                ..Default::default()
            },
            telemetry: Some(crate::telemetry::Capture {
                sink: sink.clone(),
                account_uuid: cred.account_uuid.clone(),
                org_type: None,
                body: first.clone(),
                betas: None,
                session_header: None,
                client_request_id: None,
                organization_id: None,
                started_at: std::time::SystemTime::now(),
            }),
            cc_session: None,
            empty_reply_key: None,
            prompt_key: None,
            app_key: None,
            empty_replies: Default::default(),
            injected_tools: Vec::new(),
            store: store.clone(),
            _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
            _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(Default::default()),
            _route_load: crate::proxy::note_upstream_send(&Default::default(), 0, "-", 0),
        };
        // 上游回包（非流式）：有 usage，收尾按成功走。
        rl.sniffer.feed(
            br#"{"id":"msg_1","model":"claude-opus-5","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":5,"output_tokens":1}}"#,
        );
        // 重试成功那一步：调用方换了响应侧，`note_retry` 负责把请求侧一并换过来。
        let mut h = crate::proxy::HeaderMap::new();
        h.insert("request-id", HeaderValue::from_static("req_retry"));
        rl.note_retry("no_prefill", &h, &retried);
        assert_eq!(rl.upstream_request_id.as_deref(), Some("req_retry"));
        drop(rl);

        // 取走这一批（把「到期」时间推远，攒批规则就不拦着了）。
        let flushes =
            sink.take_due(std::time::Instant::now() + std::time::Duration::from_secs(3_600));
        let events: Vec<serde_json::Value> = flushes.into_iter().flat_map(|f| f.events).collect();
        let meta = |name: &str| -> serde_json::Value {
            let e = events
                .iter()
                .find(|e| e["event_data"]["event_name"] == name)
                .unwrap_or_else(|| panic!("{name} 应当在这一批里"));
            let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
            let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
            serde_json::from_slice(&raw).unwrap()
        };
        // 报的是重试那份：2 条消息，不是首发的 3 条。
        assert_eq!(meta("tengu_api_success")["messageCount"], 2, "请求侧要跟着重试那一发走");
        assert_eq!(meta("tengu_api_query")["messagesLength"], 2);
        assert_eq!(meta("tengu_api_success")["requestId"], "req_retry", "响应侧本来就是重试那发");
        assert_eq!(
            meta("tengu_api_success")["requestBodyChars"],
            retried.len(),
            "体长度也得是实际发出去那份"
        );

        // 取证的形态摘要同理——它要回答的是「发出去的到底长什么样」。
        let logged = &store.list_usage_logs(1).unwrap()[0];
        let shape: serde_json::Value =
            serde_json::from_str(logged.forensics.shape.as_deref().unwrap()).unwrap();
        assert_eq!(shape["messages"]["count"], 2, "shape 列也要是重试那份");
        assert_eq!(logged.forensics.rewrites.as_deref(), Some("no_prefill"));
    }

    /// 在途计数：句柄在则计数在，句柄没了计数就得跟着回去。挂在 `ReqLog` 上的那份要活到
    /// 响应流结束，所以这里钉住的是 Drop 语义本身——漏了它，并发数会只涨不落。
    #[test]
    fn in_flight_guard_counts_up_and_back_down() {
        use std::sync::atomic::Ordering::Relaxed;
        let counter: std::sync::Arc<std::sync::atomic::AtomicI64> = Default::default();
        assert_eq!(counter.load(Relaxed), 0);

        let a = crate::proxy::InFlightGuard::new(counter.clone());
        let b = crate::proxy::InFlightGuard::new(counter.clone());
        assert_eq!(counter.load(Relaxed), 2, "两条并发请求各占一格");

        drop(a);
        assert_eq!(counter.load(Relaxed), 1, "一条走完只减自己那格");
        drop(b);
        assert_eq!(counter.load(Relaxed), 0, "全部走完必须回到 0");
    }

    /// 上游负载表的三项读数：路线在飞、账号在飞（跨模型合计）、窗口内的发送数与输出预算之和。
    ///
    /// 钉住它是因为这三项是裸 429 唯一的解释来源（上游那一档一个限流头都不给），读数错了
    /// 排查就会被带向错误的方向：在飞数只涨不落会把「一条一条发」误判成并发触限，
    /// `max_tokens` 漏加会让「输出预算超了」这条真正的成因看不出来。
    #[test]
    fn upstream_load_counts_in_flight_and_the_send_window() {
        let load: crate::proxy::UpstreamLoad = Default::default();
        let snap = |model: &str| crate::proxy::upstream_load_snapshot(&load, 1, model);

        // 同一个号的两条路线：一条 sonnet 两发、一条 opus 一发。
        let a = crate::proxy::note_upstream_send(&load, 1, "claude-sonnet-5", 32000);
        let b = crate::proxy::note_upstream_send(&load, 1, "claude-sonnet-5", 8000);
        let c = crate::proxy::note_upstream_send(&load, 1, "claude-opus-5", 1024);
        // 别的号不该混进来（限额按组织算，但表是按号分的）。
        let _other = crate::proxy::note_upstream_send(&load, 2, "claude-sonnet-5", 64000);

        let s = snap("claude-sonnet-5");
        assert_eq!(s.route_in_flight, 2, "这条路线两发在飞，含发起查询的那条自己");
        assert_eq!(s.cred_in_flight, 3, "账号维度要跨模型合计——限额不分模型");
        assert_eq!(s.sent, 3, "窗口内这个号一共发了三条");
        assert_eq!(s.max_tokens, 32000 + 8000 + 1024, "声明的输出上限逐条累加");
        assert_eq!(snap("claude-opus-5").route_in_flight, 1, "另一条路线各算各的");

        drop(a);
        assert_eq!(snap("claude-sonnet-5").route_in_flight, 1, "走完一条只归还自己那格");
        drop(b);
        drop(c);
        let s = snap("claude-sonnet-5");
        assert_eq!(s.route_in_flight, 0, "全部走完必须回到 0");
        assert_eq!(s.cred_in_flight, 0);
        assert_eq!(s.sent, 3, "在飞归零不影响发送窗口：那是「最近一分钟发过什么」，不是「还在飞」");

        // 归零即删键，故不需要清扫（模型名来自来访请求体，乱编就能造键）。
        assert!(
            !load.lock().in_flight.keys().any(|(id, _)| *id == 1),
            "这个号的在飞格全归还后不该留下空键"
        );
        // 未声明 max_tokens 的按 0 计，不影响其余条目。
        let _d = crate::proxy::note_upstream_send(&load, 3, "-", 0);
        let s3 = crate::proxy::upstream_load_snapshot(&load, 3, "-");
        assert_eq!((s3.sent, s3.max_tokens), (1, 0));
    }

    /// 窗口外的发送记录不算数，且清空后连键一起删掉——不然一个久不用的号会永远留着一条空队列。
    #[test]
    fn upstream_send_window_drops_stale_entries() {
        let load: crate::proxy::UpstreamLoad = Default::default();
        let guard = crate::proxy::note_upstream_send(&load, 7, "claude-sonnet-5", 4096);
        // 把那条记录的时刻推到窗口之外（真等 60 秒不是测试该干的事）。
        {
            let mut table = load.lock();
            let q = table.sent.get_mut(&7).unwrap();
            q[0].0 = q[0]
                .0
                .checked_sub(crate::proxy::UPSTREAM_SEND_WINDOW)
                .expect("Instant 是自启动起算的单调时钟，机器开机不足一分钟时减不出来");
        }
        let s = crate::proxy::upstream_load_snapshot(&load, 7, "claude-sonnet-5");
        assert_eq!((s.sent, s.max_tokens), (0, 0), "滚出窗口的不该再算进来");
        assert_eq!(s.route_in_flight, 1, "但它还在飞——两件事，两条时间线");
        assert!(!load.lock().sent.contains_key(&7), "窗口空了就把键删掉");
        drop(guard);
    }

    /// 把一串 SSE 文本喂给聚合器；`chunk` 是每次喂的字节数，用来构造跨块断行。
    fn aggregate(sse: &str, chunk: usize) -> crate::proxy::Aggregated {
        let mut agg = crate::proxy::SseAggregator::default();
        for part in sse.as_bytes().chunks(chunk.max(1)) {
            agg.feed(part);
        }
        agg.finish()
    }

    fn aggregated_message(sse: &str, chunk: usize) -> serde_json::Value {
        match aggregate(sse, chunk) {
            crate::proxy::Aggregated::Message(v) => v,
            crate::proxy::Aggregated::UpstreamError(e) => panic!("不该判成上游错误: {e}"),
            crate::proxy::Aggregated::Incomplete(why) => panic!("不该判成不完整: {why}"),
        }
    }

    /// 一条典型的文本流：文本增量拼接、`message_delta` 的 stop_reason 与 usage 合进顶层。
    ///
    /// **逐字节喂一遍**：真实网络下 SSE 的分块与行边界毫无关系，聚合器必须自己攒行。
    #[test]
    fn aggregates_a_text_stream() {
        let sse = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5","content":[],"stop_reason":null,"usage":{"input_tokens":10,"cache_read_input_tokens":5,"output_tokens":1}}}"#,
            "\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"，世界"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":42}}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );

        for chunk in [1usize, 7, 4096] {
            let v = aggregated_message(sse, chunk);
            assert_eq!(v["id"], "msg_1", "chunk={chunk}");
            assert_eq!(v["type"], "message");
            assert_eq!(v["content"][0]["type"], "text");
            assert_eq!(v["content"][0]["text"], "你好，世界", "文本增量要按序拼接");
            assert_eq!(v["stop_reason"], "end_turn", "message_delta 的字段合进顶层");
            assert_eq!(v["usage"]["output_tokens"], 42, "usage 逐键覆盖");
            assert_eq!(v["usage"]["input_tokens"], 10, "message_start 里没被覆盖的键要留着");
            assert_eq!(v["usage"]["cache_read_input_tokens"], 5);
        }
    }

    /// tool_use 的入参是分片 JSON 串，攒到 `content_block_stop` 整体解析；
    /// thinking 块的正文与签名各自拼接；未知块类型原样透传。
    #[test]
    fn aggregates_tool_use_thinking_and_unknown_blocks() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_2","content":[],"usage":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"先想一下"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"上海\"}"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":1}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"some_future_block","payload":{"k":1}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":2}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let v = aggregated_message(sse, 5);

        assert_eq!(v["content"][0]["thinking"], "先想一下");
        assert_eq!(v["content"][0]["signature"], "sig-abc", "签名同样是分片拼接");
        assert_eq!(v["content"][1]["name"], "get_weather");
        assert_eq!(
            v["content"][1]["input"],
            serde_json::json!({"city": "上海"}),
            "分片 JSON 要在 content_block_stop 时整体解析成 input"
        );
        assert_eq!(
            v["content"][2],
            serde_json::json!({"type":"some_future_block","payload":{"k":1}}),
            "认不出来的块类型原样收下——上游新增块类型时这里不该跟着改"
        );
    }

    /// 认不出来的 `delta.type` 不能把整条响应带崩：那一块的内容丢掉，其余照常攒完。
    /// （丢内容这件事本身在 [`crate::proxy::SseAggregator::apply_delta`] 里另打 warn。）
    #[test]
    fn unknown_delta_type_does_not_break_aggregation() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_3","content":[],"usage":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"future_delta","whatever":"x"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let v = aggregated_message(sse, 64);
        assert_eq!(v["content"][0]["text"], "ok", "认识的增量照样要攒上");
    }

    /// 流中 `event: error`：整份 error 负载原样交出去（回程拿它当响应体；状态码另按
    /// [`crate::proxy::error_status`] 映射，见 `mid_stream_error_maps_to_the_non_streaming_status`）。
    #[test]
    fn mid_stream_error_payload_is_surfaced_as_is() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_4","content":[],"usage":{}}}"#,
            "\n\n",
            "event: error\n",
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            "\n\n",
        );
        match aggregate(sse, 3) {
            crate::proxy::Aggregated::UpstreamError(e) => {
                assert_eq!(e["error"]["type"], "overloaded_error");
                assert_eq!(e["type"], "error", "整份 data 原样带走，形状与非流式错误体一致");
            }
            other => panic!(
                "应判成上游错误，实际: {}",
                match other {
                    crate::proxy::Aggregated::Message(_) => "Message",
                    crate::proxy::Aggregated::Incomplete(_) => "Incomplete",
                    crate::proxy::Aggregated::UpstreamError(_) => unreachable!(),
                }
            ),
        }
    }

    /// 流中错误的状态码映射：与非流式那条路上同一个错误该有的状态码一致——开不开这个功能，
    /// 客户端看到的状态码都一样。认不出来的类型兜底 500，**不能是 200**：那会把一次失败
    /// 记成成功，客户端与统计两边都被带偏。
    #[test]
    fn mid_stream_error_maps_to_the_non_streaming_status() {
        let status = |kind: &str| {
            crate::proxy::error_status(&serde_json::json!({"type":"error","error":{"type":kind}}))
                .as_u16()
        };
        assert_eq!(status("invalid_request_error"), 400);
        assert_eq!(status("authentication_error"), 401);
        assert_eq!(status("permission_error"), 403);
        assert_eq!(status("billing_error"), 403);
        assert_eq!(status("not_found_error"), 404);
        assert_eq!(status("request_too_large"), 413);
        assert_eq!(status("timeout_error"), 408);
        assert_eq!(status("rate_limit_error"), 429);
        assert_eq!(status("api_error"), 500);
        assert_eq!(status("overloaded_error"), 529, "529 不在常量表里，按数字构造");
        assert_eq!(status("something_new_2027"), 500, "认不出来的一律 500");
        // 连 `error` 字段都没有的畸形负载同样按 500，绝不退回 200。
        assert_eq!(crate::proxy::error_status(&serde_json::json!({"type":"error"})).as_u16(), 500);
    }

    /// 端到端：上游在流中报 overloaded → 客户端拿到 529 + 那份错误 JSON 原文。
    #[tokio::test]
    async fn mid_stream_error_reaches_the_client_with_a_mapped_status() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_err","content":[],"usage":{}}}"#,
            "\n\n",
            "event: error\n",
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            "\n\n",
        );
        let (status, ctype, body) = relay_sse(sse).await;

        assert_eq!(status.as_u16(), 529, "上游那个 200 不能照搬——它其实是一次失败");
        assert_eq!(ctype.as_deref(), Some("application/json"));
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["type"], "overloaded_error", "错误原文原样交给客户端");
        assert_eq!(v["error"]["message"], "Overloaded");
    }

    /// 没收到 `message_stop` 就断了 → 判不完整（回程 502）。
    ///
    /// **绝不能把攒了一半的内容当完整响应回去**：客户端拿到的会是一条看着正常、实则被截断的
    /// Message，比一个明确的错误糟得多——它会被当成模型的真实输出写进会话历史。
    #[test]
    fn truncated_stream_is_incomplete_not_a_partial_message() {
        let cut = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_5","content":[],"usage":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"半句"}}"#,
            "\n\n",
        );
        assert!(matches!(aggregate(cut, 9), crate::proxy::Aggregated::Incomplete(_)));
        // 一个事件都没来（比如连上就断）同样是不完整，不是空 Message。
        assert!(matches!(aggregate("", 1), crate::proxy::Aggregated::Incomplete(_)));
    }

    /// 端到端走一遍聚合回程：起一个吐 SSE 的本地上游，`aggregate_sse` 必须回一条
    /// `content-type: application/json` 的整段 Message——客户端本来就是按非流式发的，
    /// 它认的是这个形态。
    #[tokio::test]
    async fn aggregated_response_is_a_single_json_message() {
        let sse = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_e2e","type":"message","role":"assistant","model":"claude-sonnet-5","content":[],"usage":{"input_tokens":9}}}"#,
            "\n\n",
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pong"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let (status, ctype, body) = relay_sse(sse).await;

        assert_eq!(status, crate::proxy::StatusCode::OK);
        assert_eq!(
            ctype.as_deref(),
            Some("application/json"),
            "上游那份 text/event-stream 必须被替掉"
        );
        let v: serde_json::Value =
            serde_json::from_slice(&body).expect("回给客户端的必须是整段 JSON");
        assert_eq!(v["id"], "msg_e2e");
        assert_eq!(v["content"][0]["text"], "pong");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["output_tokens"], 3);
        assert_eq!(v["usage"]["input_tokens"], 9);
    }

    /// 流断在半路 → 502，且**不带**攒了一半的内容：截断的 Message 会被客户端当成模型的
    /// 真实输出写进会话历史，比一个明确的错误糟得多。
    #[tokio::test]
    async fn truncated_upstream_stream_yields_502() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_cut","content":[],"usage":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"半句"}}"#,
            "\n\n",
        );
        let (status, _, body) = relay_sse(sse).await;

        assert_eq!(status, crate::proxy::StatusCode::BAD_GATEWAY);
        assert!(!String::from_utf8_lossy(&body).contains("半句"), "截断的内容不该回给客户端");
    }

    /// 起一个吐 `sse` 的本地上游，取回响应交给 [`crate::proxy::aggregate_sse`]，
    /// 返回 (状态码, content-type, 响应体)。
    async fn relay_sse(sse: &str) -> (crate::proxy::StatusCode, Option<String>, Bytes) {
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
            sse.len()
        )
        .into_bytes();
        resp.extend_from_slice(sse.as_bytes());
        let (addr, server) = serve_once(resp);
        let up = crate::clients::upstream_client(None)
            .unwrap()
            .post(format!("http://{addr}/v1/messages"))
            .send()
            .await
            .unwrap();

        let out = crate::proxy::aggregate_sse(up, req_log(), None).await;
        server.join().unwrap();
        let status = out.status();
        let ctype =
            out.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(String::from);
        let body = axum::body::to_bytes(out.into_body(), usize::MAX).await.unwrap();
        (status, ctype, body)
    }

    /// 聚合路径要一份 `ReqLog`（它在 Drop 里落日志与用量）；这里给一份最小可用的。
    fn req_log() -> crate::proxy::ReqLog {
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        crate::proxy::ReqLog {
            started: std::time::Instant::now(),
            ttft_ms: None,
            method: "POST".into(),
            path: "/v1/messages".into(),
            ua: "-".into(),
            ua_out: "-".into(),
            cred_id: cred.id,
            cred_label: cred.label,
            device_id: None,
            status: 200,
            sse_aggregated: false,
            sniffer: crate::proxy::UsageSniffer::new(true, false),
            req_speed: None,
            req_model: None,
            ratelimit: rl_headers(&[]),
            stream_broke: None,
            request_id: "lb-test".into(),
            client_request_id: None,
            upstream_request_id: None,
            forensics: Default::default(),
            telemetry: None,
            cc_session: None,
            empty_reply_key: None,
            prompt_key: None,
            app_key: None,
            empty_replies: Default::default(),
            injected_tools: Vec::new(),
            store,
            _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
            _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(Default::default()),
            _route_load: crate::proxy::note_upstream_send(&Default::default(), 0, "-", 0),
        }
    }
}
