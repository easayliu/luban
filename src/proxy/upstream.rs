use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use futures_util::StreamExt;

use crate::config;
use crate::store;
use crate::web::AppState;

use super::body::{
    CcClient, ToolNameMap, cc_ua_entrypoint, declares_no_tools, injected_tools_of,
    restore_tool_names_stream, rewrite_body_out, trusted_cc_version,
};
use super::headers::{is_resp_forwardable, orig_header_case};
use super::logging::{ReqLog, ShapeBits, UsageSniffer, shape_summary_of};
use super::rate_limit::RateLimitInfo;
use super::session_link::{CcRequestKind, CcSessionLink};
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
        self.shape_with(body, cred, device_fp, self.refusal_fallbacks).0
    }

    /// [`Self::shape`] 外加一份**出站体的取证摘要**（[`shape_summary_of`]）。
    ///
    /// 转发主路径走这个。摘要在这里算而不是等落库时再从字节解析，是因为改写刚好把出站体的
    /// `Value` 建在手里（见 [`rewrite_body_out`]）——借它走一趟只要 0.6ms，而从字节重新解析
    /// 一份 1.8MB 的会话要 6.7ms。两者的产物逐字相同。
    ///
    /// `inbound` 是来访体已经解析好的那份（[`handle_inner`] 里的 `body_json`）：改写没动过
    /// 体的时候出站字节与来访逐字节相同，摘要拿它算即可，同样不必重新解析。
    ///
    /// **`Value` 不外泄**：只返回摘要那三个小 String。解析态的 `Value` 通常是字节数的几倍，
    /// 把它交出去让调用方一路拿着穿过上游那次往返，是拿内存峰值换 CPU——那笔账不划算。
    pub(super) fn shape_outbound(
        &self,
        body: &Bytes,
        cred: &crate::credentials::Credential,
        device_fp: &str,
        inbound: Option<&serde_json::Value>,
    ) -> (Bytes, ShapeBits) {
        let (sent, outbound) = self.shape_with(body, cred, device_fp, self.refusal_fallbacks);
        let mut bits = match outbound.as_ref().or(inbound) {
            Some(v) => shape_summary_of(v),
            None => ShapeBits::default(),
        };
        // 注入了哪些工具按**出站**对来访算：改写没动体（`outbound` 为 `None`）就是一个没注。
        if let (Some(sim), Some(inb), Some(out)) = (self.sim.as_ref(), inbound, outbound.as_ref()) {
            bits.injected_tools = injected_tools_of(inb, out, sim.profile);
            bits.tools_filled = !bits.injected_tools.is_empty() && declares_no_tools(inb);
        }
        (sent, bits)
    }

    /// [`Self::shape`] 指定要不要补 `fallbacks`：重试路径（[`retry_without_fallbacks`]）
    /// 传 `None`，其余一律传 `self.refusal_fallbacks`。
    fn shape_with(
        &self,
        body: &Bytes,
        cred: &crate::credentials::Credential,
        device_fp: &str,
        fallbacks: Option<&'static str>,
    ) -> (Bytes, Option<serde_json::Value>) {
        if self.billable {
            // body 侧要不要补 `thinking.display:"updates"`，看**实际发出的头**里有没有那项 beta
            // （[`merge_beta_for`] 只给 2.1.251+ 世代的 fable 补；agent-sdk / VSCode 扩展那类客户端
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
            // 给真实 CC 补 billing header 时写的是**它自报的**版本与 entrypoint，不是 luban
            // 自己那份：见 [`billing_header_text`]。模拟路径不看这个值（那条路的版本在 profile 里）。
            let ua = self.headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
            let client_version =
                ua.and_then(trusted_cc_version).map(|(a, b, c)| format!("{a}.{b}.{c}"));
            let client = client_version.as_deref().map(|version| CcClient {
                version,
                entrypoint: ua.and_then(cc_ua_entrypoint).unwrap_or("cli"),
            });
            // `sim_billing_only` 的两处置空（`tool_names` / `refusal_fallbacks`）不在这里做，而是
            // 在 [`AttemptPlan`] 生成时统一门控——那样请求头的 beta、请求体、重试与错误学习用的是
            // 同一份状态（见 [`crate::proxy::handler::attempt`]）。这里原样透传即可。
            rewrite_body_out(
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
                client,
                self.client_link.as_ref().map(|(_, l)| l),
                self.cc_kind,
                fallbacks,
            )
        } else {
            (body.clone(), None)
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

/// 把已经读完体的上游响应原样拼回一个 [`wreq::Response`]：状态码、协议版本、全部响应头
/// （**不筛**，筛是 [`resp_builder`] 回给客户端时的事）、体。
///
/// 用在「先读体判一判、判不中再交给原来那条路」的地方（转发循环里的 403 换号）：下游只读
/// status / headers / bytes，拼回来的与原件没有区别。丢掉的只有 wreq 自己挂的 extensions
/// （请求 URL 之类），转发路径不读它们。
pub(super) fn rebuild_response(
    status: StatusCode,
    version: axum::http::Version,
    headers: HeaderMap,
    body: Bytes,
) -> wreq::Response {
    let mut res = axum::http::Response::new(body);
    *res.status_mut() = status;
    *res.version_mut() = version;
    *res.headers_mut() = headers;
    wreq::Response::from(res)
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
    let (sse, _) = resp_shape(&up);
    // 末尾接一个哨兵：上游读到头时记下 `upstream_done`。客户端先断开的话 axum 丢掉响应体，
    // 哨兵永远走不到——收尾时据此分出「客户端取消」与「上游半截 EOF」，见 [`ReqLog::upstream_done`]。
    let stream = up.bytes_stream().map(Some).chain(futures_util::stream::iter([None])).filter_map(
        move |chunk| {
            let Some(chunk) = chunk else {
                rl.upstream_done = true;
                return std::future::ready(None);
            };
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
            std::future::ready(Some(chunk))
        },
    );
    // 用量嗅探喂的是**还原前**的字节（`usage` 里没有工具名，两者等价），还原只包在最外层。
    let body = match tool_names {
        Some(map) => Body::from_stream(restore_tool_names_stream(stream, map, sse)),
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
                    Some(map) => map.restore_json_body(&body),
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
            // 报错正文里可能回显假工具名，同别的错误一路全文还原。
            match serde_json::to_vec(&payload).map(|b| match tool_names {
                Some(map) => map.restore(&b),
                None => b,
            }) {
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
    let retried = upstream.shape_with(client_body, cred, device_fp, None).0;
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
    rl.note_retry(
        "no_fallbacks",
        up.headers(),
        &retried,
        upstream.sim.as_ref().and_then(|s| s.take_thread()),
    );
    Some(up)
}

/// 模拟路径的一条 `thread: continue` 被上游拒了（400 / 404：线程过期、`previous_message_id`
/// 对不上……）：作废它接的那条线程，同一个号、同一份客户端体**改发 `create`**（完整上下文）
/// 再发一次。本地线程状态最长留 [`super::session_link`] 里那三个小时，上游那边可能早就忘了；
/// 官方客户端遇到这种事当场重建线程，用户看不到失败，这里也不该把这一发原样甩给客户端。
///
/// 成了按重试那次记账，标签 `thread_create`；没成（发不出去或也被拒）返回 `None`，调用方照旧
/// 处理原来那条响应。
pub(super) async fn retry_thread_as_create(
    upstream: &Upstream<'_>,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    client_body: &Bytes,
    rl: &mut ReqLog,
) -> Option<wreq::Response> {
    let failed = rl.cc_thread.take()?;
    CcSessionLink::drop_thread(&failed);
    let retried = upstream.shape_with(client_body, cred, device_fp, upstream.refusal_fallbacks).0;
    let up = match upstream.send(retried.clone()).await {
        Ok(up) => up,
        Err(e) => {
            tracing::warn!(error = %error_chain(&e), "the thread create retry could not be sent, passing the failed continue through");
            return None;
        }
    };
    let status = up.status();
    if !status.is_success() {
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            status = status.as_u16(),
            "the thread create retry was rejected too, passing the failed continue through"
        );
        return None;
    }
    tracing::info!(
        cred_id = cred.id, cred = %cred.label,
        "upstream rejected a message-thread continue; resent the turn as create"
    );
    let (is_stream, encoding) = resp_shape(&up);
    rl.status = status.as_u16();
    rl.ttft_ms = None;
    rl.sniffer = UsageSniffer::new(is_stream, encoding.is_some());
    rl.ratelimit = RateLimitInfo::from_headers(up.headers());
    rl.note_retry(
        "thread_create",
        up.headers(),
        &retried,
        upstream.sim.as_ref().and_then(|s| s.take_thread()),
    );
    Some(up)
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
mod tests;
