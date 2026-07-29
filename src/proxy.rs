//! 转发代理：Claude Code → luban → 官方 Anthropic API。
//!
//! 透传请求体，仅替换鉴权：校验来访 API Key 后，注入选中凭证的 OAuth access_token
//! 与 `anthropic-beta: oauth-2025-04-20`，响应流式原样回传。

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use rand::RngExt;

use crate::config;
use crate::store;
use crate::web::AppState;

/// 转发 `/v1/*` 到官方 API。
pub async fn handle(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = std::time::Instant::now();
    let path_and_query =
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path()).to_string();

    // 1) 校验来访 API Key（未配置则放行）。生效 key：环境覆盖优先，否则用库中配置。
    if let Some(expected) = effective_client_key(&state) {
        if !client_authorized(&headers, &expected) {
            tracing::warn!(%method, path = %path_and_query, "拒绝：无效的接入 API Key");
            return (StatusCode::UNAUTHORIZED, "无效的 API Key").into_response();
        }
    }

    // 2) 提取 device_id（在请求体 metadata.user_id 里；兼容 CC 内嵌 JSON 与扁平串两种格式）。
    let device_id = extract_device_id(&body);

    // 2.1) 无有效设备身份（无 metadata / 无法识别的 user_id 格式）→ 默认直接拒绝：
    //      这类请求既无法做身份伪装、也无从计入设备上限（会绕过 device_limit）。
    //      网页可关掉该校验（放行裸客户端），此时它们退化为不绑定、不占名额的负载均衡挑选。
    if device_id.is_none() {
        if state.store.require_device_id() {
            tracing::warn!(%method, path = %path_and_query, "拒绝：请求无有效设备身份（metadata.user_id 缺失或格式无法识别）");
            return (StatusCode::FORBIDDEN, "缺少有效的设备身份（metadata.user_id）")
                .into_response();
        }
        tracing::debug!(%method, path = %path_and_query, "放行无设备身份的请求（设备身份校验已关闭）");
    }

    // 3) 按 device_id 粘性选出凭证的 access_token（必要时刷新）。
    let (token, cred) =
        match store::valid_access_token_for_device(&state.store, &state.http, device_id.as_deref())
            .await
        {
            Ok(t) => t,
            Err(e) => {
                // 设备数达硬上限 → 429；其余（无凭证/刷新失败等）→ 503。
                let status = if e.downcast_ref::<store::DeviceLimitReached>().is_some() {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                tracing::warn!(%method, path = %path_and_query, error = %e, "拒绝转发");
                return (status, e.to_string()).into_response();
            }
        };

    // 4) 目标 URL：上游 base + 原路径与查询串。
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, path_and_query);

    // 5) 组装转发头：复制安全头，注入鉴权与 beta。形态类改动逐项受网页开关控制，
    //    一条 SQL 读齐（默认全开 = 加入开关前的既有行为）。
    let flags = state.store.forward_flags();
    let out = build_forward_headers(&headers, &token, flags);

    // 6) 转发前改写 body：system 形态对齐（拆成官方的 4 块 + 断点全上 1h + 基座标 scope=global）
    //    + 身份伪装（metadata.user_id 的 account_uuid/device_id 换成该凭证自洽身份、
    //    billing header 补 cch）。
    //    设备指纹叠加客户端原始 device_id 与平台 arch/os，使不同设备得到不同伪装 device_id。
    let device_fp = device_fingerprint(device_id.as_deref(), &headers);
    {
        let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("-");
        tracing::debug!(
            ua = %h("user-agent"),
            x_app = %h("x-app"),
            arch = %h("x-stainless-arch"),
            os = %h("x-stainless-os"),
            runtime = %h("x-stainless-runtime"),
            pkg = %h("x-stainless-package-version"),
            "客户端识别头"
        );
    }
    // 请求侧的速度档（顶层 `speed` 字段，配套 anthropic-beta: fast-mode-*）。
    // 仅作兜底：以上游 `usage.speed` 为准，那里才反映实际生效的档位。
    let req_speed = request_speed(&body);
    let body = rewrite_body(&body, &cred, &device_fp, flags);

    // 7) 发起上游请求并流式回传。头名的拼写与顺序由 orig_header_case 决定（关掉即退回
    //    「全小写 + Host/User-Agent/Content-Length 钉在队尾」，也就是换 wreq 之前的形态）。
    let req = state.http.request(method.clone(), &url).headers(out).body(body);
    let req = if flags.orig_header_case { req.orig_headers(orig_header_case()) } else { req };
    let resp = req.send().await;

    match resp {
        Ok(up) => {
            let status = up.status();
            // 判断响应是否为 SSE 流（决定用量嗅探采用逐行还是整段 JSON 模式）。
            let is_stream = up
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.contains("text/event-stream"))
                .unwrap_or(false);
            // 正常情况下这里恒为 false：上游客户端开了 gzip/br/zstd/deflate 解压，wreq
            // 收到时已解码，并把 `content-encoding`/`content-length` 一并摘掉。
            // 留着这个判断是兜底——若上游哪天用了我们没开的编码，tower-http 会原样放行并保留
            // 该头，那时响应体是我们读不懂的字节，嗅探与账号级错误判定都只能跳过。
            //
            // 曾经这是常态：v0.2.12 恢复转发 `accept-encoding` 却没开解压 feature，于是
            // **所有**响应（含 SSE）都成了压缩字节，用量/计价/封号判定整片失效。当时的 warn
            // 只在 4xx 上打，200 这条路径完全静默，症状是「统计悄悄归零且日志上看不出原因」。
            // 现在改成任何状态码都告警。
            let content_encoding = up
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("identity"))
                .map(str::to_string);
            let compressed = content_encoding.is_some();
            if let Some(enc) = &content_encoding {
                tracing::warn!(
                    status = status.as_u16(),
                    encoding = %enc,
                    "上游响应带无法解码的 content-encoding：用量嗅探与账号级错误判定都会被跳过（该编码需在 wreq feature 里开启）"
                );
            }
            // 解析上游限流头（订阅账号 5h/7d 额度体现在此），随请求日志入库。
            let ratelimit = RateLimitInfo::from_headers(up.headers());

            let mut builder = Response::builder().status(status);
            for (k, v) in up.headers().iter() {
                if is_resp_forwardable(k) {
                    builder = builder.header(k, v);
                }
            }

            // 包裹响应流：首块到达记 TTFT，边转发边嗅探用量；
            // 流结束(或断开)时在 Drop 里记 total、输出一条日志并落库。
            let mut rl = ReqLog {
                started,
                ttft_ms: None,
                method: method.to_string(),
                path: path_and_query,
                cred_id: cred.id,
                cred_label: cred.label.clone(),
                device_id,
                status: status.as_u16(),
                sniffer: UsageSniffer::new(is_stream, compressed),
                req_speed,
                ratelimit,
                store: state.store.clone(),
            };

            // 400/401/403：先缓冲响应体做账号级错误判定，命中则自动停用该凭证并清空其
            // 设备绑定，让下一次请求立即改选其它凭证；命中与否响应体都原样透传。
            if matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) {
                return match up.bytes().await {
                    Ok(bytes) => {
                        rl.ttft_ms = Some(rl.started.elapsed().as_millis());
                        rl.sniffer.feed(&bytes);
                        // 无条件把上游的错误文本打出来。此前只有被判成账号级错误时才有日志，
                        // 普通 400（`invalid_request_error`，多半是请求形态被上游拒了）只会留下
                        // ReqLog 里那条 `status=400` 而不带任何原因——body 虽原样透传给了客户端，
                        // 但服务端侧查不出所以然。压缩体跳过：打出来只会是乱码字节。
                        if !compressed {
                            let (etype, message) = parse_upstream_error(&bytes);
                            tracing::warn!(
                                cred = format!("#{} {}", cred.id, cred.label),
                                status = status.as_u16(),
                                error_type = %etype.as_deref().unwrap_or("-"),
                                message = %message.chars().take(500).collect::<String>(),
                                "上游返回 4xx"
                            );
                        }
                        // 压缩体读不出内容，宁可漏判也不误判（乱码可能碰巧命中特征词）。
                        if let Some(reason) =
                            (!compressed).then(|| detect_account_ban(status, &bytes)).flatten()
                        {
                            tracing::warn!(
                                cred = format!("#{} {}", cred.id, cred.label),
                                status = status.as_u16(),
                                reason = %reason,
                                "检测到账号级错误，自动停用该凭证"
                            );
                            if let Err(e) = state.store.mark_banned(cred.id, &reason) {
                                tracing::warn!(error = %e, "自动停用凭证失败");
                            }
                        }
                        builder.body(Body::from(bytes)).unwrap_or_else(|e| {
                            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
                        })
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "读取上游错误响应体失败");
                        builder.body(Body::empty()).unwrap_or_else(|e| {
                            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
                        })
                    }
                };
            }

            let stream = up.bytes_stream().map(move |chunk| {
                if rl.ttft_ms.is_none() {
                    rl.ttft_ms = Some(rl.started.elapsed().as_millis());
                }
                if let Ok(bytes) = &chunk {
                    rl.sniffer.feed(bytes);
                }
                chunk
            });

            builder
                .body(Body::from_stream(stream))
                .unwrap_or_else(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())
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
                "上游请求失败"
            );
            (StatusCode::BAD_GATEWAY, format!("上游请求失败[{kind}]: {detail}")).into_response()
        }
    }
}

/// 展开 error 的 source 链，拼成「顶层 -> 次层 -> …」，暴露底层真实原因。
fn error_chain(e: &dyn std::error::Error) -> String {
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
fn upstream_error_kind(e: &wreq::Error) -> &'static str {
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

/// 随响应流一起存活；流结束/断开时在 Drop 里输出一条转发日志（含 TTFT、总耗时与用量）并落库。
struct ReqLog {
    started: std::time::Instant,
    ttft_ms: Option<u128>,
    method: String,
    path: String,
    cred_id: i64,
    cred_label: String,
    /// 完整 device_id；日志里只展示前 8 位（脱敏）。
    device_id: Option<String>,
    status: u16,
    /// 增量嗅探到的响应用量。
    sniffer: UsageSniffer,
    /// 请求体里声明的速度档；仅在响应未回报 `usage.speed` 时兜底。
    req_speed: Option<String>,
    /// 上游返回的订阅账号限流快照。
    ratelimit: RateLimitInfo,
    store: std::sync::Arc<store::CredentialStore>,
}

impl Drop for ReqLog {
    fn drop(&mut self) {
        self.sniffer.finish();
        let has_usage = self.sniffer.has_usage();
        // 速度档以上游回报为准（fast 被限流时会回落），响应没带才退回请求声明。
        let speed = self.sniffer.speed.clone().or_else(|| self.req_speed.clone());
        let cost_usd = crate::pricing::estimate_usd(crate::pricing::Usage {
            model: self.sniffer.model.as_deref(),
            speed: speed.as_deref(),
            input_tokens: self.sniffer.input_tokens,
            output_tokens: self.sniffer.output_tokens,
            cache_creation_total: self.sniffer.cache_creation_tokens,
            cache_5m_tokens: self.sniffer.cache_creation_5m,
            cache_1h_tokens: self.sniffer.cache_creation_1h,
            cache_read_tokens: self.sniffer.cache_read_tokens,
        });
        let total_ms = self.started.elapsed().as_millis();
        let device_short: String = self
            .device_id
            .as_ref()
            .map(|d| d.chars().take(8).collect())
            .unwrap_or_else(|| "-".into());
        let ttft = self.ttft_ms.map(|v| v as i64);
        let total = i64::try_from(total_ms).ok();

        tracing::info!(
            method = %self.method,
            path = %self.path,
            cred = format!("#{} {}", self.cred_id, self.cred_label),
            device = %device_short,
            status = self.status,
            model = %self.sniffer.model.as_deref().unwrap_or("-"),
            speed = %speed.as_deref().unwrap_or("-"),
            has_usage,
            input_tokens = self.sniffer.input_tokens.unwrap_or(0),
            output_tokens = self.sniffer.output_tokens.unwrap_or(0),
            cache_creation_tokens = self.sniffer.cache_creation_tokens.unwrap_or(0),
            cache_read_tokens = self.sniffer.cache_read_tokens.unwrap_or(0),
            ttft_ms = self.ttft_ms.map(|v| v as u64).unwrap_or(0),
            total_ms,
            cost_usd = cost_usd.map(|c| format!("{c:.5}")).unwrap_or_else(|| "-".into()),
            "转发"
        );

        let rec = store::UsageRecord {
            cred_id: Some(self.cred_id),
            cred_label: self.cred_label.clone(),
            device_id: self.device_id.clone(),
            model: self.sniffer.model.clone(),
            path: self.path.clone(),
            status: self.status,
            has_usage,
            input_tokens: self.sniffer.input_tokens,
            output_tokens: self.sniffer.output_tokens,
            cache_creation_tokens: self.sniffer.cache_creation_tokens,
            cache_5m_tokens: self.sniffer.cache_creation_5m,
            cache_1h_tokens: self.sniffer.cache_creation_1h,
            cache_read_tokens: self.sniffer.cache_read_tokens,
            ttft_ms: ttft,
            total_ms: total,
            unified_status: self.ratelimit.unified_status.clone(),
            rl_5h_status: self.ratelimit.five_h_status.clone(),
            rl_5h_reset: self.ratelimit.five_h_reset,
            rl_5h_utilization: self.ratelimit.five_h_utilization,
            rl_7d_status: self.ratelimit.seven_d_status.clone(),
            rl_7d_reset: self.ratelimit.seven_d_reset,
            rl_7d_utilization: self.ratelimit.seven_d_utilization,
            rl_representative: self.ratelimit.representative.clone(),
            ratelimit_raw: (!self.ratelimit.raw.is_empty()).then(|| self.ratelimit.raw.clone()),
            cost_usd,
        };
        if let Err(e) = self.store.insert_usage_log(&rec) {
            tracing::warn!(error = %e, "写入用量日志失败");
        }
    }
}

/// 从上游响应中增量嗅探 token 用量。
///
/// - SSE 流：逐行解析 `data:` 事件——`message_start` 带 input/cache 与 model，
///   `message_delta` 带最终 output_tokens。后见到的非空值覆盖旧值。
/// - 非流式 JSON：累积整段响应体，在 [`Self::finish`] 时解析顶层 `usage`。
#[derive(Default)]
struct UsageSniffer {
    is_stream: bool,
    /// 响应体带我们解不开的 `content-encoding`，只能一律不解析（`feed` 直接丢弃）。
    /// 正常路径下恒为 false——wreq 已解码，见 [`handle`] 里 `compressed` 的说明。
    opaque: bool,
    /// SSE 模式下未处理完的行尾；非流式模式下累积的整段响应体。
    buf: Vec<u8>,
    model: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    /// 缓存写细分：5 分钟 / 1 小时档（上游 `usage.cache_creation` 下）。
    cache_creation_5m: Option<i64>,
    cache_creation_1h: Option<i64>,
    cache_read_tokens: Option<i64>,
    /// 上游回报的实际速度档（`usage.speed`，如 `"fast"`）。fast 有独立限流，
    /// 被限流时会回落到标准档，故以响应为准、请求体只作兜底。
    speed: Option<String>,
}

impl UsageSniffer {
    fn new(is_stream: bool, opaque: bool) -> Self {
        Self { is_stream, opaque, ..Default::default() }
    }

    /// 喂入一块响应字节。
    fn feed(&mut self, chunk: &[u8]) {
        if self.opaque {
            return;
        }
        if self.is_stream {
            self.buf.extend_from_slice(chunk);
            // 逐个完整行处理，保留最后不完整的一段在 buf 里。
            while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                self.parse_line(&line[..line.len() - 1]);
            }
            // 防御：异常超长行避免无界增长。
            if self.buf.len() > 1_000_000 {
                self.buf.clear();
            }
        } else if self.buf.len() < 1_000_000 {
            // 非流式：累积整段响应体（JSON 消息响应通常很小）。
            self.buf.extend_from_slice(chunk);
        }
    }

    /// 解析一行 SSE 数据行（`data: {...}`）或裸 JSON 行。
    fn parse_line(&mut self, line: &[u8]) {
        let s = match std::str::from_utf8(line) {
            Ok(s) => s.trim(),
            Err(_) => return,
        };
        let json_str = s.strip_prefix("data:").map(str::trim).unwrap_or(s);
        if !json_str.starts_with('{') {
            return;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
            self.merge(&v);
        }
    }

    /// 合并一段 JSON 里的用量字段（顶层或 `message.` 下）。
    fn merge(&mut self, v: &serde_json::Value) {
        if let Some(m) = v
            .get("model")
            .and_then(|m| m.as_str())
            .or_else(|| v.get("message").and_then(|m| m.get("model")).and_then(|m| m.as_str()))
        {
            self.model = Some(m.to_string());
        }
        let usage = v.get("usage").or_else(|| v.get("message").and_then(|m| m.get("usage")));
        if let Some(u) = usage {
            if let Some(x) = u.get("input_tokens").and_then(|x| x.as_i64()) {
                self.input_tokens = Some(x);
            }
            if let Some(x) = u.get("output_tokens").and_then(|x| x.as_i64()) {
                self.output_tokens = Some(x);
            }
            if let Some(x) = u.get("cache_creation_input_tokens").and_then(|x| x.as_i64()) {
                self.cache_creation_tokens = Some(x);
            }
            if let Some(x) = u.get("cache_read_input_tokens").and_then(|x| x.as_i64()) {
                self.cache_read_tokens = Some(x);
            }
            if let Some(s) = u.get("speed").and_then(|s| s.as_str()) {
                self.speed = Some(s.to_string());
            }
            // 缓存写细分（5m / 1h）：`usage.cache_creation.ephemeral_*_input_tokens`。
            if let Some(cc) = u.get("cache_creation") {
                if let Some(x) = cc.get("ephemeral_5m_input_tokens").and_then(|x| x.as_i64()) {
                    self.cache_creation_5m = Some(x);
                }
                if let Some(x) = cc.get("ephemeral_1h_input_tokens").and_then(|x| x.as_i64()) {
                    self.cache_creation_1h = Some(x);
                }
            }
        }
    }

    /// 收尾：非流式模式在此解析累积的整段 JSON。
    fn finish(&mut self) {
        if !self.is_stream && !self.buf.is_empty() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&self.buf) {
                self.merge(&v);
            }
        }
    }

    /// 是否解析到任一用量字段。
    fn has_usage(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_creation_tokens.is_some()
            || self.cache_read_tokens.is_some()
    }
}

/// 从请求体提取「客户端设备标识」，用于粘性选择与设备指纹派生。
/// 兼容两种 `metadata.user_id` 格式：
/// - CC 内嵌 JSON（`{"device_id":...}`）：取 `device_id`。
/// - 扁平串 `user_<hash>_account_<acct>_session_<sess>`（如 Windows 客户端）：取 `<hash>`。
///
/// 解析失败或标识为空时返回 `None`（退化为纯优先级选择、不做粘性绑定）。
fn extract_device_id(body: &Bytes) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let user_id = json.get("metadata")?.get("user_id")?.as_str()?;
    // CC 内嵌 JSON 优先。
    if let Ok(inner) = serde_json::from_str::<serde_json::Value>(user_id) {
        if let Some(dev) = inner.get("device_id").and_then(|d| d.as_str()) {
            if !dev.is_empty() {
                return Some(dev.to_string());
            }
        }
    }
    // 退化：扁平串格式，取 device 段。
    let flat = parse_flat_user_id(user_id)?;
    (!flat.device.is_empty()).then_some(flat.device)
}

/// 扁平 `metadata.user_id` 中我们需要的两段：`user_<device>_account_<..>_session_<session>`。
/// account 段被凭证真实值覆盖，故不保留。
struct FlatUserId {
    device: String,
    session: String,
}

/// 解析扁平 user_id；不匹配该形态时返回 `None`。
/// 按标记切分，允许 account 段为空（`account__session`）。
fn parse_flat_user_id(s: &str) -> Option<FlatUserId> {
    let rest = s.strip_prefix("user_")?;
    let (device, rest) = rest.split_once("_account_")?;
    let (_account, session) = rest.split_once("_session_")?;
    Some(FlatUserId { device: device.to_string(), session: session.to_string() })
}

/// 400 场景下的账号级错误特征词：命中其一才判定为「该账号被上游封禁/停用/授权失效」，
/// 以区别于常规的客户端请求错误（invalid_request_error，如模型名错、body 超长）——避免
/// 客户端一条坏请求重试时把所有账号逐个误禁。命中后原文（截断）存作 `ban_reason`。
const BAN_KEYWORDS: &[&str] = &[
    "disabled",
    "suspended",
    "banned",
    "terminated",
    "deactivated",
    "violat",
    "invalid_grant",
    "oauth",
];

/// 反向豁免：命中其一则**一定不是**账号级问题，无论状态码与特征词如何都不停用。
/// 用于挡住「特征词碰巧出现在非账号报错里」的误杀，见 [`detect_account_ban`]。
const NOT_ACCOUNT_PHRASES: &[&str] =
    &["not supported for this endpoint", "does not support", "unsupported model"];

/// 从上游错误响应体解析 `(error.type, error.message)`；解析失败时 message 退化为整段原文。
fn parse_upstream_error(body: &[u8]) -> (Option<String>, String) {
    let text = String::from_utf8_lossy(body);
    let v = serde_json::from_slice::<serde_json::Value>(body).ok();
    let field = |name: &str| {
        v.as_ref().and_then(|v| v.get("error")?.get(name)?.as_str().map(str::to_string))
    };
    (field("type"), field("message").unwrap_or_else(|| text.to_string()))
}

/// 依据状态码与响应体判定是否应自动停用该凭证，命中则返回写入 `ban_reason` 的原因
/// （`[状态码] 类型: 消息`，截断至 200 字符）。
///
/// 三档都要求响应体确实是 Anthropic 的错误 JSON（能取到 `error.type`）或命中
/// [`BAN_KEYWORDS`]，避免把「非账号问题的 4xx」当成封号，把健康账号打成停用：
/// - 401：`authentication_error` 才停用。裸 401（CDN/网关拦截，无 `error.type`）不停用。
/// - 403：**仅**命中 [`BAN_KEYWORDS`] 时停用。普通 `permission_error`（如 Pro 账号请求
///   Opus、beta 未开通、区域限制）是能力/权限问题而非封号，原样透传即可。
/// - 400：同 403，仅命中特征词时停用；普通 `invalid_request_error` 是客户端请求错误。
fn detect_account_ban(status: StatusCode, body: &[u8]) -> Option<String> {
    let (etype, message) = parse_upstream_error(body);
    let reason = || {
        let head = match &etype {
            Some(t) => format!("[{}] {t}: {message}", status.as_u16()),
            None => format!("[{}] {message}", status.as_u16()),
        };
        head.chars().take(200).collect::<String>()
    };
    let hay = format!("{} {}", etype.as_deref().unwrap_or(""), message).to_lowercase();
    // 先排除「端点/能力不支持」这类与账号状态无关的报错——它们可能带上 oauth 等特征词
    // （如 401 `OAuth authentication is currently not supported for this endpoint`），
    // 但账号本身是好的，停用了反而白扣一个号。
    if NOT_ACCOUNT_PHRASES.iter().any(|p| hay.contains(p)) {
        return None;
    }
    let hits_keyword = || BAN_KEYWORDS.iter().any(|k| hay.contains(k));
    match status {
        StatusCode::UNAUTHORIZED => {
            (etype.as_deref() == Some("authentication_error") || hits_keyword()).then(reason)
        }
        StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST => hits_keyword().then(reason),
        _ => None,
    }
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
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if v == expected {
            return true;
        }
    }
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if v.strip_prefix("Bearer ").map(str::trim) == Some(expected) {
            return true;
        }
    }
    false
}

/// 合并来访的 `anthropic-beta`：**客户端自有的那串一字不动**，只把 API-key 模式的客户端不会
/// 自带的那四项补进去，各自插到官方位置上。这里是「注入哪几项」的唯一真源。
///
/// 只追加不落位会得到官方客户端不会产生的排列（缺失项全堆在末尾），集合对了顺序错，一次精确
/// 字符串匹配即可判定中间有代理。但**落位不能靠一张全局顺序表**——haiku 的客户端把
/// `claude-code-20250219` 排在队尾，opus/sonnet 排在队首，任何单一总序都同时满足不了
/// （见 [`config::cc_beta_order_is_not_a_table`]）。四对 raw 抓包里唯一稳定的是「客户端自有串
/// 的相对顺序在订阅模式下逐字不变」，故这里保留原串，按经验规则插入：
///
/// - [`config::OAUTH_BETA_HEADER`]：OAuth 鉴权必需。客户端串以
///   [`config::CC_BETA_CLAUDE_CODE`] 开头就插它后面，否则插最前（haiku 即后者）。
/// - [`config::CC_BETA_ADVANCED_TOOL_USE`]：有 [`config::CC_BETA_EFFORT`] 就插它前面，
///   没有就跟在客户端自有串之后（haiku）。
/// - [`config::CC_BETA_PROMPT_CACHING_SCOPE`]：四份抓包里客户端都自带，真缺时补在末尾。
/// - [`config::CC_BETA_EXTENDED_CACHE_TTL`]：官方恒为最后一项，故**最后**追加——这也是本
///   函数里插入顺序有讲究的唯一一处。
///
/// 三对抓包（opus-5 / sonnet-5 / haiku-4.5）用这套规则都能**逐字节**还原官方串；fable-5 那对
/// 两侧会话配置不同（`context-1m` / `server-side-fallback`），能验的是落位，也一致。
/// 回归测试见 [`tests::merged_beta_matches_official_order`]。
fn merge_beta(incoming: Option<&HeaderValue>) -> String {
    let mut parts: Vec<String> = incoming
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let has = |parts: &[String], beta: &str| parts.iter().any(|p| p == beta);

    if !has(&parts, config::OAUTH_BETA_HEADER) {
        let at = usize::from(parts.first().is_some_and(|p| p == config::CC_BETA_CLAUDE_CODE));
        parts.insert(at, config::OAUTH_BETA_HEADER.to_string());
    }
    if !has(&parts, config::CC_BETA_ADVANCED_TOOL_USE) {
        let at = parts.iter().position(|p| p == config::CC_BETA_EFFORT).unwrap_or(parts.len());
        parts.insert(at, config::CC_BETA_ADVANCED_TOOL_USE.to_string());
    }
    if !has(&parts, config::CC_BETA_PROMPT_CACHING_SCOPE) {
        parts.push(config::CC_BETA_PROMPT_CACHING_SCOPE.to_string());
    }
    if !has(&parts, config::CC_BETA_EXTENDED_CACHE_TTL) {
        parts.push(config::CC_BETA_EXTENDED_CACHE_TTL.to_string());
    }
    parts.join(",")
}

/// 组装发往上游的请求头：原样转发可转发头，再对需要 luban 决定取值的头**原位覆盖**。
///
/// **头序**：`HeaderMap` 按插入序迭代，hyper 也按这个顺序写到线上，所以来访客户端的头序
/// 默认是保住的。但「先在 [`is_forwardable`] 里剥离、之后再 `insert`」会把那些头从原位摘走、
/// 追加到队尾（`anthropic-beta`/`anthropic-version`/`authorization` 都是），得到官方客户端
/// 不会产生的排列——和 [`merge_beta`] 要解决的问题同类，只是从「值内顺序」变成「头之间顺序」。
/// 故这里让它们照常转发，再用 `insert` 覆盖：`insert` 命中已有 key 时原位替换值，位置不动。
///
/// 只在客户端没带时才补的头（`accept-encoding`、`x-client-request-id`）没有原位可循，
/// 追加在末尾；官方客户端这两个头都带，走的是原位覆盖那条路。
///
/// **开关**（[`store::ForwardFlags`]，默认全开）：`merge_beta` 关掉即原样转发客户端那串
/// `anthropic-beta`（含不再塞 `oauth-2025-04-20`）；`fill_client_headers` 关掉即不补任何
/// 客户端没带的头。唯一无条件执行的是注入 `Authorization`——实测那是上游唯一必需的改动。
///
/// 注意 `fill_client_headers` 关掉后，若客户端自己也没带 `accept-encoding`，兜底会落到
/// [`crate::web::upstream_client`] 的 `default_headers`（同为官方取值），不会退化成
/// tower-http 那个非官方的 `zstd,gzip,deflate,br`。
///
/// 无法对齐的部分（头名大小写、hyper 自己追加的 `user-agent`/`host`/`content-length`）
/// 见 [`crate::config::known_fingerprint_gaps`]。
fn build_forward_headers(
    headers: &HeaderMap,
    token: &str,
    flags: store::ForwardFlags,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    // `append` 而非 `insert`：同名多值头要全部保留，`insert` 会只剩最后一个。
    for (k, v) in headers.iter() {
        if is_forwardable(k, v) {
            out.append(k.clone(), v.clone());
        }
    }
    // anthropic-version 缺省补齐。
    if flags.fill_client_headers && !out.contains_key("anthropic-version") {
        out.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    // anthropic-beta 合并，确保带上 oauth，并按官方客户端顺序重排。
    if flags.merge_beta {
        match HeaderValue::from_str(&merge_beta(headers.get("anthropic-beta"))) {
            Ok(v) => {
                out.insert("anthropic-beta", v);
            }
            // merge_beta 只产出 ASCII，理论上不可达；真发生时保留来访原值，别把这个头发空。
            Err(e) => tracing::warn!(error = %e, "构造 anthropic-beta 失败，保留来访原值"),
        }
    }
    if flags.fill_client_headers {
        // accept-encoding：客户端没带时补上官方客户端的取值（缺失本身就是特征）。
        if !out.contains_key(header::ACCEPT_ENCODING) {
            out.insert(
                header::ACCEPT_ENCODING,
                HeaderValue::from_static(config::CC_ACCEPT_ENCODING),
            );
        }
        // x-client-request-id：官方客户端每请求一个 uuid v4；API-key 模式的 CC 不发，补齐。
        if !out.contains_key("x-client-request-id")
            && let Ok(v) = HeaderValue::from_str(&uuid_v4())
        {
            out.insert("x-client-request-id", v);
        }
    }
    // 注入 OAuth 鉴权，原位覆盖来访的任何鉴权头。
    match HeaderValue::from_str(&format!("Bearer {token}")) {
        Ok(v) => {
            out.insert(header::AUTHORIZATION, v);
        }
        // 这个头现在是**照常转发再覆盖**的，覆盖失败就必须摘掉：
        // 留在原地等于把来访者的接入 key 漏给上游。
        Err(e) => {
            tracing::error!(error = %e, "构造 Authorization 失败，移除该头避免泄漏接入 key");
            out.remove(header::AUTHORIZATION);
        }
    }
    out
}

/// 按官方拼写与顺序构造 `OrigHeaderMap`（`wreq` 据它决定线上头名的大小写**与顺序**）。
///
/// 整张 [`config::CC_HEADER_ORDER`] 无条件塞进去，不按本次请求裁剪——预检实测：表里有、
/// 本次没带的头**不会**凭空发出。反之表外的头照发，但一律小写且排在所有表内头之后，
/// 所以自定义头不会因缺表项而丢失，只是拿不到官方位置。
///
/// 这也是 `Host`/`User-Agent`/`Content-Length` 唯一的归位途径：它们由 HTTP 客户端自己追加，
/// 不在我们的 `HeaderMap` 里，但只要列进这张表就会落到官方位置，而不是被钉在队尾。
pub(crate) fn orig_header_case() -> wreq::header::OrigHeaderMap {
    let mut orig = wreq::header::OrigHeaderMap::new();
    for name in config::CC_HEADER_ORDER {
        // 返回值是 `HeaderMap::append` 的语义（false = 新键），不是成功与否，别拿来判错。
        orig.insert(*name);
    }
    orig
}

/// 生成一个随机 uuid v4（小写带连字符），用于补齐 `x-client-request-id`。
fn uuid_v4() -> String {
    let mut b: [u8; 16] = rand::rng().random();
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!("{}-{}-{}-{}-{}", h(&b[0..4]), h(&b[4..6]), h(&b[6..8]), h(&b[8..10]), h(&b[10..16]))
}

/// 读取请求体里声明的速度档（顶层 `speed` 字段，如 `"fast"`；配套 header
/// `anthropic-beta: fast-mode-*`）。解析失败或没有该字段时返回 `None`。
///
/// 仅作兜底：fast 有独立于标准档的限流，被限流时上游会回落到标准速度，
/// 只看请求会把这类流量按 fast 价（两倍）高估——以响应 `usage.speed` 为准。
fn request_speed(body: &Bytes) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    Some(v.get("speed")?.as_str()?.to_string())
}

/// 转发前改写请求体，三项各自受 [`store::ForwardFlags`] 里的开关控制（默认全开；全关即
/// 请求体逐字节原样转发）：
///
/// 1. **system 形态**（`system_shape`）：把 API-key 模式的 3 块改写成订阅模式的 4 块，
///    见 [`align_system_shape`]。含拆块、断点全上 `ttl:1h`、基座标 `scope:"global"`。
/// 2. **身份伪装**（`spoof_identity`）：把 `metadata.user_id` 里的 `account_uuid`/`device_id`
///    换成该凭证自洽的身份（真实 account_uuid + 由其稳定派生的 device_id），避免
///    「真账号 + 陌生设备」的矛盾。
/// 3. **cch**（`billing_cch`）：给 `x-anthropic-billing-header` 补订阅模式独有的 `cch`。
///
/// **key 顺序**：改写要把 body 重新序列化，serde_json 默认的 `Map = BTreeMap` 会把**整个
/// body**（含 tools/messages/content/cache_control 里每一个对象）的 key 按字母序重排，得到
/// 官方客户端不会产生的排列——集合对了顺序错，一次精确比对即可判定中间有代理。故本 crate
/// 开了 serde_json 的 `preserve_order`（见 Cargo.toml），解析出的顺序原样写回，
/// 新增字段追加在末尾。回归测试见 [`tests::preserves_key_order`]。
///
/// 解析失败或结构异常时原样返回——绝不因改写失败而阻断转发。
fn rewrite_body(
    body: &Bytes,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    flags: store::ForwardFlags,
) -> Bytes {
    // `ttl` 要上游认，前提是 `anthropic-beta` 里有 `extended-cache-ttl-2025-04-11`，而那串
    // 是 `merge_beta` 补的（API-key 模式的客户端自己不发，cap/raw/00002 证实）。两个开关必须
    // 同时开，否则就是「body 里写了 1h、头上没声明」的自相矛盾。
    let shape = flags.system_shape && flags.merge_beta;
    // 三项全关：连解析都不必做，原样返回。
    if !shape && !flags.spoof_identity && !flags.billing_cch {
        return body.clone();
    }
    let mut v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.clone(),
    };
    let shaped = shape && align_system_shape(&mut v);
    let cch_added = flags.billing_cch && ensure_billing_cch(&mut v);
    tracing::debug!(
        metadata = %v.get("metadata").map(|m| m.to_string()).unwrap_or_else(|| "<无 metadata>".into()),
        "入站 metadata"
    );
    let spoofed = flags.spoof_identity && spoof_identity(&mut v, cred, device_fp);
    tracing::debug!(
        shaped,
        spoofed,
        cch_added,
        device_fp = %device_fp,
        spoof_device = %cred.spoof_device_id(device_fp).as_deref().unwrap_or("-"),
        "改写 body"
    );
    if !shaped && !spoofed && !cch_added {
        return body.clone();
    }
    match serde_json::to_vec(&v) {
        Ok(bytes) => Bytes::from(bytes),
        Err(_) => body.clone(),
    }
}

/// 构造设备指纹：客户端原始 `device_id` + 平台 `arch`/`os`，用于派生每设备唯一的伪装
/// device_id。刻意只取**稳定的硬件/系统身份**，排除会随客户端升级变动的字段
/// （runtime 版本、UA 版本号），以免每次升级都刷新 device_id。
fn device_fingerprint(client_device_id: Option<&str>, headers: &HeaderMap) -> String {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    format!("{}|{}|{}", client_device_id.unwrap_or(""), h("x-stainless-arch"), h("x-stainless-os"),)
}

/// 把 `metadata.user_id` 里的 `account_uuid`/`device_id` 换成凭证自洽身份，**保持原格式**：
/// - CC 内嵌 JSON：**字符串级定点替换**这两个字段的值，字段顺序与其余内容原样不动。
///   真实 CC 发的是紧凑 JSON `{"device_id":..,"account_uuid":..,"session_id":..}`。外层 body
///   已靠 serde_json 的 `preserve_order` 保住顺序，但这层仍绕开 serde：内层是**字符串里的
///   JSON**，重新序列化会连空白、转义写法一起归一化，只有定点替换才逐字节不变。
/// - 扁平串 `user_<hash>_account_<acct>_session_<sess>`（如 Windows）：换掉 device 段与
///   account 段，保留 session 段，仍以扁平串回写——不把 Windows 请求伪装成 CC 的 JSON 形态。
///
/// 凭证无 `account_uuid`（如旧库未回填）或 user_id 结构无法识别时不改动，返回 `false`。
fn spoof_identity(
    v: &mut serde_json::Value,
    cred: &crate::credentials::Credential,
    device_fp: &str,
) -> bool {
    let account_uuid = match cred.account_uuid.as_deref() {
        Some(u) if !u.trim().is_empty() => u,
        _ => return false,
    };
    let device_id = match cred.spoof_device_id(device_fp) {
        Some(d) => d,
        None => return false,
    };
    let user_id = match v.get_mut("metadata").and_then(|m| m.get_mut("user_id")) {
        Some(u) => u,
        None => return false,
    };
    let inner_str = match user_id.as_str() {
        Some(s) => s.to_string(),
        None => return false,
    };

    // 格式一：CC 内嵌 JSON——先确认是 JSON 对象，再对原始字符串做定点值替换，
    // 保持字段顺序与其余内容（session_id 等）逐字节不变。
    if serde_json::from_str::<serde_json::Value>(&inner_str)
        .ok()
        .as_ref()
        .and_then(|v| v.as_object())
        .is_some()
    {
        let mut s = inner_str;
        let mut changed = false;
        if let Some(next) = replace_json_str_field(&s, "account_uuid", account_uuid) {
            s = next;
            changed = true;
        }
        if let Some(next) = replace_json_str_field(&s, "device_id", &device_id) {
            s = next;
            changed = true;
        }
        if changed {
            *user_id = serde_json::Value::String(s);
        }
        return changed;
    }

    // 格式二：扁平串——保持格式，只换 device 与 account，保留 session。
    if let Some(flat) = parse_flat_user_id(&inner_str) {
        let rebuilt =
            format!("user_{}_account_{}_session_{}", device_id, account_uuid, flat.session);
        *user_id = serde_json::Value::String(rebuilt);
        return true;
    }

    false
}

/// 在紧凑 JSON 字符串里，把 `"key":"<旧值>"` 的值原地替换成 `new_val`，字段位置与其余
/// 内容逐字节不变。仅处理**字符串型且值内无转义引号**的字段——`device_id`(hex)、
/// `account_uuid`(UUID，可能为空串)均满足，`new_val` 同为 hex/UUID，无需 JSON 转义。
/// 找不到该字段（或其不是 `"key":"` 形态）时返回 `None`，**不新增字段**，以免改变结构。
fn replace_json_str_field(s: &str, key: &str, new_val: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let val_start = s.find(&needle)? + needle.len();
    // 值到下一个引号为止（值内无转义引号，故直接找 '"'）。
    let val_end = val_start + s[val_start..].find('"')?;
    let mut out = String::with_capacity(s.len() - (val_end - val_start) + new_val.len());
    out.push_str(&s[..val_start]);
    out.push_str(new_val);
    out.push_str(&s[val_end..]);
    Some(out)
}

/// 给 `system[0]` 的 `x-anthropic-billing-header` 补上 `cch=<值>`，对齐订阅客户端。
///
/// 官方客户端只在订阅(OAuth)模式下发这个字段，API-key 模式（接入 luban 的形态）不发，
/// 于是「OAuth token + 无 cch」是个确定性判据。抓包实测补齐后与真实客户端形态一致：
/// `…cc_version=2.1.218.0b9; cc_entrypoint=cli; cch=00000;`
///
/// 只在该块确实是 billing header、且尚无 `cch=` 时改写；其余情况返回 `false` 不动结构。
///
/// 注意：`system[0]` 位于第一个缓存断点之前，属于被缓存的前缀——改写它会让**部署后的第一次
/// 请求**缓存未命中一次。因此 `cch` 的取值必须对同一前缀保持稳定，见 [`cch_value`]。
fn ensure_billing_cch(v: &mut serde_json::Value) -> bool {
    let blk = match v.get_mut("system").and_then(|s| s.as_array_mut()).and_then(|a| a.first_mut()) {
        Some(b) => b,
        None => return false,
    };
    let text = match blk.get("text").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    if !text.starts_with("x-anthropic-billing-header:") || text.contains("cch=") {
        return false;
    }
    let mut s = text.trim_end().to_string();
    if !s.ends_with(';') {
        s.push(';');
    }
    s.push_str(&format!(" cch={};", cch_value()));
    match blk.get_mut("text") {
        Some(t) => {
            *t = serde_json::Value::String(s);
            true
        }
        None => false,
    }
}

/// `cch` 的取值。当前是常量 [`config::BILLING_CCH`]。
///
/// 真实算法无法从抓包反推，所以这里只能给占位值。**代价**：它跨账号恒定，所有经由 luban
/// 的请求都带同一个真实客户端从不产生的 `cch`，上游一按此聚类就把所有账号串成一串。
///
/// 想改成每账号不同，把本函数换成从「已在缓存前缀内的内容」派生即可，例如
/// `sha256(account_uuid ‖ system[1..] 文本)` 取前 5 位小写 hex：前缀不变则取值不变，
/// 不会打爆 prompt cache，同时每个账号各自不同。（若上游会校验 cch 与内容的对应关系，
/// 两种做法都是错值——那种情况下正确的选择是根本不补，见 config 里的说明。）
fn cch_value() -> &'static str {
    config::BILLING_CCH
}

/// 把 API-key 模式的 3 块 `system` 改写成订阅模式的 4 块，并把全部缓存断点对齐到官方形态。
///
/// 两种形态的差别只有「切法」，文本本身逐字节相同（`cap/raw` 那对同机同版本抓包验证过）：
///
/// ```text
/// 官方直连(00006)                     API-key 模式(00002)
/// [0] billing header      无断点      [0] billing header      无断点
/// [1] 身份句 57B          无断点      [1] 身份句 57B          {ephemeral}   ← 多余断点
/// [2] 基座 1210B  {type,ttl:1h,scope:global}
/// [3] 其余        {type,ttl:1h}       [2] 基座‖"\n\n"‖其余    {ephemeral}
/// ```
///
/// 故改写是四件事，缺一都会得到真实客户端不产生的中间态，因此**同受一个开关控制**：
/// 1. 在 [`config::CC_SYSTEM_BASE_ANCHOR`] 前的 `\n\n` 处把合并块切成基座 + 其余；
/// 2. 基座标 `{type:ephemeral, ttl:1h, scope:global}`，其余标 `{type:ephemeral, ttl:1h}`；
/// 3. 去掉身份句上那个断点——它的缓存前缀只有 127 字节（约 35 token），远低于最小可缓存长度，
///    本就是空转，官方也不发；
/// 4. body 里剩下的 ephemeral 断点（尾部那条 role=system 的消息等）补 `ttl:1h`，官方 3/3 全带。
///
/// **`scope:global` 只标基座**。之前是「标 text 最长的那块」，在三块形态下必然选中合并块，
/// 而合并块含 `# Environment` 的 cwd/git、技能清单这些本机内容——跨账号不可能撞上，标了换不来
/// 复用，还发出 `{type,scope}`（global 却无 ttl）这种官方不产生的组合。拆开之后基座是纯静态的，
/// 全网同一份，这个标记才真正有意义。
///
/// **代价**：1h 缓存写单价是 5m 的 2 倍。这是形态对齐的一部分（官方就是全 1h），不是优化。
///
/// 保守起见只处理「确实是 API-key 三块形态」：`system` 长度不为 3、锚点匹配不到、或锚点前不是
/// `\n\n`，一律不动结构返回 `false`。客户端本来就是 4 块（订阅形态）时同样不动。
fn align_system_shape(v: &mut serde_json::Value) -> bool {
    let sys = match v.get_mut("system").and_then(|s| s.as_array_mut()) {
        Some(s) if s.len() == 3 => s,
        _ => return false,
    };
    // 合并块必须本来就是个带断点的文本块，否则不是我们认识的形态。
    if sys[2].get("cache_control").is_none() {
        return false;
    }
    let text = match sys[2].get("text").and_then(|t| t.as_str()) {
        Some(t) => t.to_string(),
        None => return false,
    };
    // 逐个模型族的锚点找，取**最早**命中的那个：基座是前缀，切得越靠前越不会把基座切碎。
    // 锚点前必须紧跟 `\n\n`——那两个字节是两块的分隔符，切开后两边都不保留它。
    // 用字节比较：`find` 给的是字节偏移，`p - 2` 未必落在字符边界上，直接切片会 panic。
    let at = config::CC_SYSTEM_BASE_ANCHORS
        .iter()
        .filter_map(|anchor| text.find(anchor))
        .filter(|&p| p >= 2 && &text.as_bytes()[p - 2..p] == b"\n\n")
        .min();
    let Some(at) = at else { return false };

    if let Some(obj) = sys[1].as_object_mut() {
        obj.remove("cache_control");
    }
    sys[2] = text_block(&text[..at - 2], cache_control(true));
    sys.push(text_block(&text[at..], cache_control(false)));
    fill_cache_ttl(v);
    true
}

/// 构造一个 `system` 文本块，key 序与官方一致：`type` → `text` → `cache_control`。
fn text_block(text: &str, cache_control: serde_json::Value) -> serde_json::Value {
    let mut blk = serde_json::Map::new();
    blk.insert("type".into(), "text".into());
    blk.insert("text".into(), text.into());
    blk.insert("cache_control".into(), cache_control);
    serde_json::Value::Object(blk)
}

/// 构造 `cache_control`，key 序与官方一致：`type` → `ttl` → `scope`。
fn cache_control(global: bool) -> serde_json::Value {
    let mut cc = serde_json::Map::new();
    cc.insert("type".into(), "ephemeral".into());
    cc.insert("ttl".into(), config::CC_CACHE_TTL.into());
    if global {
        cc.insert("scope".into(), "global".into());
    }
    serde_json::Value::Object(cc)
}

/// 递归给 body 里所有 `cache_control: {"type":"ephemeral"}` 补上 `ttl`（已有则不动）。
/// 追加在 `type` 之后，得到官方的 `{"type":"ephemeral","ttl":"1h"}` 键序。
fn fill_cache_ttl(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(cc) = map.get_mut("cache_control").and_then(|c| c.as_object_mut())
                && cc.get("type").and_then(|t| t.as_str()) == Some("ephemeral")
                && !cc.contains_key("ttl")
            {
                cc.insert("ttl".into(), config::CC_CACHE_TTL.into());
            }
            for (_, child) in map.iter_mut() {
                fill_cache_ttl(child);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(fill_cache_ttl),
        _ => {}
    }
}

/// 上游订阅账号限流快照，从 `anthropic-ratelimit-unified-*` 响应头解析。
///
/// 5h/7d 两个窗口各有 status/reset(unix 秒)/utilization(0~1)；`representative` 指明
/// 当前起约束作用的窗口（如 `five_hour`）。`raw` 保留全部匹配头，字段变化时兜底回看。
#[derive(Default, Clone)]
struct RateLimitInfo {
    unified_status: Option<String>,
    five_h_status: Option<String>,
    five_h_reset: Option<i64>,
    five_h_utilization: Option<f64>,
    seven_d_status: Option<String>,
    seven_d_reset: Option<i64>,
    seven_d_utilization: Option<f64>,
    representative: Option<String>,
    /// 全部匹配到的限流/anthropic- 头，`k=v` 以 `, ` 连接。
    raw: String,
}

impl RateLimitInfo {
    fn from_headers(headers: &HeaderMap) -> Self {
        let mut info = RateLimitInfo::default();
        let mut pairs: Vec<String> = Vec::new();
        for (k, v) in headers.iter() {
            let name = k.as_str().to_ascii_lowercase();
            if !(name.contains("ratelimit")
                || name == "retry-after"
                || name.starts_with("anthropic-"))
            {
                continue;
            }
            let val = v.to_str().unwrap_or("<non-utf8>");
            pairs.push(format!("{name}={val}"));
            match name.as_str() {
                "anthropic-ratelimit-unified-status" => info.unified_status = Some(val.to_string()),
                "anthropic-ratelimit-unified-5h-status" => {
                    info.five_h_status = Some(val.to_string())
                }
                "anthropic-ratelimit-unified-5h-reset" => info.five_h_reset = val.parse().ok(),
                "anthropic-ratelimit-unified-5h-utilization" => {
                    info.five_h_utilization = val.parse().ok()
                }
                "anthropic-ratelimit-unified-7d-status" => {
                    info.seven_d_status = Some(val.to_string())
                }
                "anthropic-ratelimit-unified-7d-reset" => info.seven_d_reset = val.parse().ok(),
                "anthropic-ratelimit-unified-7d-utilization" => {
                    info.seven_d_utilization = val.parse().ok()
                }
                "anthropic-ratelimit-unified-representative-claim" => {
                    info.representative = Some(val.to_string())
                }
                _ => {}
            }
        }
        info.raw = pairs.join(", ");
        info
    }
}

/// 请求头是否可转发：跳过接入 key、Host、逐跳头。
///
/// `accept-encoding` 刻意**保留转发**：官方客户端必带 `gzip, deflate, br, zstd`，剥掉它等于
/// 发出一个「自称 claude-cli 却不声明压缩支持」的请求。上游会照单压缩（连 140 字节的错误体
/// 都压，SSE 也不例外），故上游客户端开了对应的解压 feature，wreq 收到时已解码；
/// 回给客户端的是未压缩内容（见 [`is_resp_forwardable`]）。
///
/// `authorization`/`anthropic-beta`/`anthropic-version` 也**保留转发**——它们的值随后会被
/// [`build_forward_headers`] 原位覆盖。在这里剥离会让它们被追加到头列表末尾，破坏来访头序。
///
/// `connection` 只在值为 `keep-alive` 时转发：官方客户端显式发这个头（抓包 040 可见），
/// 而 hyper 认为 HTTP/1.1 隐含 keep-alive、默认不发，剥掉就是个稳定差异。其余取值
/// （`close` 会打掉连接池、`upgrade` 更不能转）照旧当逐跳头丢弃。
fn is_forwardable(name: &HeaderName, value: &HeaderValue) -> bool {
    // HeaderName 构造时即归一化为小写，无需再转。
    let n = name.as_str();
    if n == "connection" {
        return value.as_bytes().eq_ignore_ascii_case(b"keep-alive");
    }
    !matches!(
        n,
        "host"
            | "x-api-key"
            | "content-length"
            | "expect"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// 响应头是否可回传：跳过由框架管理的分帧类头。
///
/// `content-encoding` 保留转发，但正常情况下它**根本不会出现**——wreq 解码后会把它连同
/// `content-length` 一起摘掉，我们回给客户端的是未压缩内容。只有上游用了我们没开的编码时
/// 它才会残留，那时压缩体确实是原样透传的，这个头必须跟着走，否则客户端会把压缩字节当明文解析。
///
/// 客户端向 luban 声明了 `accept-encoding` 却收到未压缩内容，这在 HTTP 里完全合法
/// （accept-encoding 是偏好不是要求）。代价是 luban→客户端这一腿不再压缩。
fn is_resp_forwardable(name: &HeaderName) -> bool {
    let n = name.as_str().to_ascii_lowercase();
    !matches!(n.as_str(), "content-length" | "transfer-encoding" | "connection")
}

#[cfg(test)]
mod tests {
    use super::{
        Bytes, HeaderValue, StatusCode, UsageSniffer, build_forward_headers, config,
        detect_account_ban, ensure_billing_cch, header, merge_beta, replace_json_str_field,
        request_speed, store, uuid_v4,
    };

    /// 形态开关全开（= 默认，也是加入开关机制之前的既有行为）。
    fn all_on() -> store::ForwardFlags {
        store::ForwardFlags::default()
    }

    /// 三个模型族的 `anthropic-beta`，逐字取自 `cap/raw` 的原始报文头
    /// （claude-cli/2.1.220，每对都是同机、经 luban 与直连相隔几十秒）：
    /// `(模型, 客户端自己发的, 官方订阅客户端发的)`。
    ///
    /// haiku 那对是关键反例：它的客户端把 `claude-code-20250219` 排在**队尾**、`oauth` 在
    /// 队首，与 opus/sonnet 正好相反，任何单一顺序表都同时对不上两边。
    const BETA_PAIRS: &[(&str, &str, &str)] = &[
        (
            "opus-5 (00002/00006)",
            "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,effort-2025-11-24,fallback-credit-2026-06-01",
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
             interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,fallback-credit-2026-06-01,\
             extended-cache-ttl-2025-04-11",
        ),
        (
            "sonnet-5 (00012/00009)",
            "claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             effort-2025-11-24",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,\
             effort-2025-11-24,extended-cache-ttl-2025-04-11",
        ),
        (
            "haiku-4.5 (00026/00031)",
            "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,claude-code-20250219",
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,claude-code-20250219,\
             advanced-tool-use-2025-11-20,extended-cache-ttl-2025-04-11",
        ),
    ];

    /// 补齐 + 落位后应与官方客户端的 beta 串**逐字节一致**，三个模型族都要过。
    #[test]
    fn merged_beta_matches_official_order() {
        for (model, client, official) in BETA_PAIRS {
            let v = HeaderValue::from_str(client).unwrap();
            assert_eq!(&merge_beta(Some(&v)), official, "{model} 的 beta 串没对齐");
        }
    }

    /// 客户端自有的那串**一字不动**：这是三对抓包里唯一稳定的不变量，重排它就等于自造判据。
    #[test]
    fn merged_beta_preserves_client_order() {
        for (model, client, _) in BETA_PAIRS {
            let v = HeaderValue::from_str(client).unwrap();
            let out = merge_beta(Some(&v));
            let kept: Vec<&str> =
                out.split(',').filter(|b| client.split(',').any(|c| c.trim() == *b)).collect();
            let sent: Vec<&str> = client.split(',').map(str::trim).collect();
            assert_eq!(kept, sent, "{model} 的客户端自有串被重排了: {out}");
        }
    }

    /// 未知 beta 不被丢弃，也不被挪位——它在客户端串里什么位置就还在什么位置。
    #[test]
    fn merged_beta_keeps_unknown_betas_in_place() {
        let (_, client, official) = BETA_PAIRS[1];
        let v = HeaderValue::from_str(&format!("{client},some-future-beta-2027-01-01")).unwrap();
        let out = merge_beta(Some(&v));
        // 客户端把它放在自有串末尾，官方串里它就该在 effort 之后、extended-cache-ttl 之前。
        assert_eq!(
            out,
            official.replace(
                ",extended-cache-ttl-2025-04-11",
                ",some-future-beta-2027-01-01,extended-cache-ttl-2025-04-11"
            )
        );
    }

    /// 无来访 beta 是退化情形（真实客户端必带），仍要给出确定输出：四个注入项按落位规则排。
    #[test]
    fn merged_beta_from_empty_is_deterministic() {
        assert_eq!(
            merge_beta(None),
            "oauth-2025-04-20,advanced-tool-use-2025-11-20,prompt-caching-scope-2026-01-05,\
             extended-cache-ttl-2025-04-11"
        );
    }

    /// 抓包 040 里的真实 account_uuid。
    const ACCOUNT_UUID: &str = "27aa7c53-0d20-42d2-806a-60c710529405";

    /// 来访客户端的头（API-key 模式的 CC，取自抓包 041）。构造成 `HeaderMap` 时保持插入序，
    /// 与 axum 从线上解析出来的顺序一致。
    fn incoming_headers() -> super::HeaderMap {
        let mut h = super::HeaderMap::new();
        for (k, v) in [
            ("accept", "application/json"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
            ("authorization", "Bearer luban-CLIENT-KEY"),
            ("connection", "keep-alive"),
            ("content-type", "application/json"),
            ("proxy-connection", "Keep-Alive"),
            ("x-claude-code-session-id", "6eb83bfc-fdf8-4c43-ba4a-6ff95c60a0de"),
            ("x-stainless-arch", "arm64"),
            ("x-stainless-os", "MacOS"),
            ("anthropic-beta", "claude-code-20250219,effort-2025-11-24"),
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("anthropic-version", "2023-06-01"),
            ("x-app", "cli"),
        ] {
            h.insert(super::HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        h
    }

    /// 按顺序取出 `HeaderMap` 里的头名。
    fn names(h: &super::HeaderMap) -> Vec<String> {
        h.iter().map(|(k, _)| k.as_str().to_string()).collect()
    }

    /// 需要 luban 改写取值的头必须**留在来访客户端给它们的位置上**。
    ///
    /// 剥离后再 `insert` 会把它们追加到队尾，得到官方客户端不会产生的头序——和 `merge_beta`
    /// 处理的是同一类问题（那个管值内顺序，这个管头之间的顺序）。
    #[test]
    fn forward_headers_keep_client_order() {
        let out = build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", all_on());

        assert_eq!(
            names(&out),
            vec![
                "accept",
                "accept-encoding",
                "authorization", // 原位，值被换成 OAuth token
                "connection",    // keep-alive 保留转发
                "content-type",
                // proxy-connection 被剥离
                "x-claude-code-session-id",
                "x-stainless-arch",
                "x-stainless-os",
                "anthropic-beta", // 原位，值被合并重排
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version", // 原位
                "x-app",
                "x-client-request-id", // 客户端没带，无原位可循，追加末尾
            ],
            "转发头序被打乱"
        );

        // 值确实被覆盖了，不是原样透传。
        assert_eq!(out["authorization"], "Bearer sk-ant-oat01-REAL");
        assert!(
            out["anthropic-beta"].to_str().unwrap().contains(config::OAUTH_BETA_HEADER),
            "anthropic-beta 未合并 oauth"
        );
    }

    /// 形态开关全关 = 只注入鉴权，其余头逐项原样：beta 不重排、不塞 oauth，
    /// 也不补任何客户端没带的头。实测上游只强制 `Authorization`，故这条路径必须真能走通。
    #[test]
    fn all_flags_off_only_injects_auth() {
        let flags = store::ForwardFlags {
            spoof_identity: false,
            billing_cch: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
        };
        let out = build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", flags);

        // 头序与来访一致，且末尾不再追加 x-client-request-id。
        assert_eq!(
            names(&out),
            vec![
                "accept",
                "accept-encoding",
                "authorization",
                "connection",
                "content-type",
                "x-claude-code-session-id",
                "x-stainless-arch",
                "x-stainless-os",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version",
                "x-app",
            ],
            "全关时不应补头"
        );
        // 唯一必需的改动仍然生效。
        assert_eq!(out["authorization"], "Bearer sk-ant-oat01-REAL");
        // beta 原样转发：既不重排也不塞 oauth。
        assert_eq!(out["anthropic-beta"], "claude-code-20250219,effort-2025-11-24");
        assert!(
            !out["anthropic-beta"].to_str().unwrap().contains(config::OAUTH_BETA_HEADER),
            "merge_beta 关闭后不应塞 oauth beta"
        );
    }

    /// 客户端什么都没带时，`fill_client_headers` 决定补不补——关掉就真的一个都不补，
    /// 只留鉴权（`accept-encoding` 由上游 client 的 default_headers 兜底，不在这一层）。
    #[test]
    fn fill_off_adds_nothing_for_bare_client() {
        let mut bare = super::HeaderMap::new();
        bare.insert(
            super::HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );

        let on = build_forward_headers(&bare, "tok", all_on());
        assert_eq!(
            names(&on),
            vec![
                "content-type",
                "anthropic-version",
                "anthropic-beta",
                "accept-encoding",
                "x-client-request-id",
                "authorization"
            ],
            "开启时应补齐这四个头"
        );

        let flags = store::ForwardFlags { fill_client_headers: false, ..all_on() };
        let off = build_forward_headers(&bare, "tok", flags);
        assert_eq!(
            names(&off),
            vec!["content-type", "anthropic-beta", "authorization"],
            "关闭后只该有客户端原有的头 + beta + 鉴权"
        );
        assert!(!off.contains_key("x-client-request-id"));
        assert!(!off.contains_key(header::ACCEPT_ENCODING));
        assert!(!off.contains_key("anthropic-version"));
    }

    /// 覆盖失败时必须把 `authorization` 摘掉，不能把来访者的接入 key 漏给上游。
    /// （这条只有在「照常转发再覆盖」的写法下才存在，剥离式写法天然没有这个洞。）
    #[test]
    fn never_leaks_client_key_upstream() {
        // token 里塞进换行——`HeaderValue::from_str` 会拒绝，走到移除分支。
        let out = build_forward_headers(&incoming_headers(), "bad\ntoken", all_on());
        assert!(!out.contains_key("authorization"), "构造失败时应移除该头: {out:?}");
        // 任何路径下都不得把接入 key 转发出去。
        for (_, v) in out.iter() {
            assert!(
                !v.to_str().unwrap_or("").contains("luban-CLIENT-KEY"),
                "接入 key 泄漏到上游: {out:?}"
            );
        }
        assert!(!out.contains_key("x-api-key"), "x-api-key 不应转发");
    }

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

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
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
        let up = crate::web::upstream_client()
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

    /// 上游用了我们没开的编码时，只能跳过嗅探——但不得崩、不得把压缩字节当明文解析。
    #[test]
    fn unknown_encoding_is_skipped_not_misparsed() {
        let mut s = UsageSniffer::new(true, true);
        s.feed(&gzip(b"data: {\"usage\":{\"input_tokens\":999}}\n"));
        s.finish();
        assert!(!s.has_usage(), "解不开的响应体不应被当明文解析出用量");
        assert_eq!(s.model, None);
    }

    /// 起个裸 TCP「上游」，用 [`crate::web::upstream_client`] 那份**真配置**打一发，
    /// 返回请求的原始字节（不做大小写归一化——这里正是要看拼写的）。
    async fn capture_wire(orig_case: bool) -> String {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
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
            (&stream).write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").unwrap();
            raw
        });

        let req = crate::web::upstream_client()
            .unwrap()
            .post(format!("http://{addr}/v1/messages?beta=true"))
            .headers(build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", all_on()))
            .body(r#"{"model":"claude-sonnet-5"}"#);
        let req = if orig_case { req.orig_headers(super::orig_header_case()) } else { req };
        let _ = req.send().await;

        server.join().unwrap()
    }

    /// 取出线上的头名，保留原始拼写与顺序。
    fn wire_names(raw: &str) -> Vec<&str> {
        raw.lines()
            .skip(1) // 请求行
            .filter(|l| !l.is_empty())
            .map(|l| l.split(':').next().unwrap())
            .collect()
    }

    /// 线上字节的看门狗：头名的**拼写与顺序**都要与官方客户端一致
    /// （基准是 `cap/raw/00006` 的原始报文头，claude-cli/2.1.220 直连）。
    ///
    /// 这条同时钉住三件事，任一退化都会失败：
    /// 1. 分裂大小写——标准头首字母大写、`anthropic-*`/`x-app`/`x-client-request-id` 全小写、
    ///    `X-Stainless-OS` 的 `OS` 全大写（机械 title-case 会写成 `X-Stainless-Os`）。
    /// 2. `User-Agent` 落在 `Content-Type` 与 `X-Claude-Code-Session-Id` 之间，而
    ///    `Connection`/`Host`/`Accept-Encoding`/`Content-Length` 是队尾四个——这是官方线序，
    ///    不是字母序（曾照 `cap/040.json` 的字母序排过，那是抓包工具重排的产物）。
    /// 3. 显式的 `Connection: keep-alive` 确实发出（客户端库默认认为 HTTP/1.1 隐含、不发）。
    #[tokio::test]
    async fn wire_bytes_match_official_header_form() {
        let raw = capture_wire(true).await;
        assert_eq!(
            wire_names(&raw),
            &[
                "Accept",
                "Authorization",
                "Content-Type",
                "User-Agent", // 来访没带，由 upstream_client 兜底，靠 CC_HEADER_ORDER 归位
                "X-Claude-Code-Session-Id",
                "X-Stainless-Arch",
                "X-Stainless-OS",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version",
                "x-app",
                "x-client-request-id",
                // 队尾四个：客户端库自己追加的那些，官方也在这个位置。
                "Connection",
                "Host",
                "Accept-Encoding",
                "Content-Length",
            ],
            "线上头名的拼写或顺序与官方形态不符:\n{raw}"
        );
        assert!(raw.contains("Connection: keep-alive"), "显式的 Connection 头被吞掉了:\n{raw}");
    }

    /// `orig_header_case` 关掉后的形态。**注意它并不等于换 wreq 之前那份**：
    ///
    /// - 头名退回全小写（这点与 reqwest 时代一致）；
    /// - 但 `default_headers` 里的 `user-agent`/`accept-encoding` 被**前置到队首**，
    ///   来访客户端的头序因此被打散（reqwest 是把它们并进原位/队尾的）；
    /// - `host`/`content-length` 仍在队尾。
    ///
    /// 也就是说这条 off 路径比开着**更不像**官方客户端，它的用途只是出问题时能二分
    /// （「是 OrigHeaderMap 引入的问题，还是别处」），不是一个可用的形态选择。
    #[tokio::test]
    async fn wire_bytes_fall_back_to_lowercase_when_off() {
        let raw = capture_wire(false).await;
        let wire = wire_names(&raw);

        let tail = wire.len() - 2;
        assert_eq!(
            &wire[..tail],
            &[
                // default_headers 被前置，不在来访客户端给它们的位置上
                "user-agent",
                "accept-encoding",
                // 以下按来访头序
                "accept",
                "authorization",
                "connection",
                "content-type",
                "x-claude-code-session-id",
                "x-stainless-arch",
                "x-stainless-os",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version",
                "x-app",
                "x-client-request-id",
            ],
            "关掉开关后的形态与实测不符:\n{raw}"
        );
        // 位置不可控，只断言这两个确实在末尾。
        let mut appended = wire[tail..].to_vec();
        appended.sort_unstable();
        assert_eq!(appended, ["content-length", "host"], "\n{raw}");
    }

    fn test_cred() -> crate::credentials::Credential {
        crate::credentials::Credential {
            id: 1,
            label: "t".into(),
            tier: None,
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: u64::MAX,
            priority: 0,
            disabled: false,
            device_limit: 0,
            ban_reason: None,
            account_uuid: Some(ACCOUNT_UUID.into()),
            created_at: 0,
            updated_at: 0,
        }
    }

    /// API-key 模式客户端的三块 `system`，形状取自 `cap/raw/00002`（内容缩短）：
    /// billing header 无断点、身份句带 5m 断点、合并块 = 基座 ‖ `\n\n` ‖ 锚点开头的其余部分。
    /// 尾部那条 role=system 的消息也带一个 5m 断点，和真实客户端一样。
    const API_SHAPE_BODY: &str = concat!(
        r#"{"model":"claude-opus-5","messages":[{"role":"system","content":[{"type":"text","#,
        r#""text":"deferred tools","cache_control":{"type":"ephemeral"}}]}],"#,
        r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"},"#,
        r#"{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude.","#,
        r#""cache_control":{"type":"ephemeral"}},"#,
        r#"{"type":"text","text":"\nBASE — 基座\n\nWrite code that reads like the surrounding "#,
        r#"code: match its comment density, naming, and idiom.\n\nREST","#,
        r#""cache_control":{"type":"ephemeral"}}],"#,
        r#""metadata":{"user_id":"{\"device_id\":\"dddd\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}"}}"#
    );

    /// 三块改写成官方的四块，且逐字段与 `cap/raw/00006` 的形态一致：
    /// 身份句不再带断点、基座 `{type,ttl:1h,scope:global}`、其余 `{type,ttl:1h}`，
    /// 消息里的断点也补上 `ttl`。切开处那个 `\n\n` 两边都不保留。
    #[test]
    fn aligns_system_to_official_four_blocks() {
        let out = super::rewrite_body(&Bytes::from(API_SHAPE_BODY), &test_cred(), "fp", all_on());
        let s = String::from_utf8(out.to_vec()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "应拆成四块: {s}");
        assert!(sys[0].get("cache_control").is_none(), "billing header 不该有断点: {s}");
        assert!(sys[1].get("cache_control").is_none(), "身份句上的断点应去掉: {s}");
        assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "基座切错: {s}");
        assert!(
            sys[3]["text"].as_str().unwrap().starts_with("Write code that reads like"),
            "其余部分应从锚点开始: {s}"
        );
        assert!(sys[3]["text"].as_str().unwrap().ends_with("\n\nREST"), "其余部分被截断: {s}");

        // 键序也要对：type → text → cache_control，cache_control 内 type → ttl → scope。
        assert!(
            s.contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#),
            "基座的 cache_control 形态不对: {s}"
        );
        assert_eq!(
            s.matches(r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#).count(),
            2,
            "其余块与消息断点都应是 1h: {s}"
        );
        assert!(!s.contains(r#"{"type":"ephemeral"}"#), "还有没补 ttl 的断点: {s}");
    }

    /// 一份 body 里可能**同时**含多条锚点，此时必须切在最早的那个上。
    ///
    /// 实例是 fable-5（`cap/raw/00035` 直连 ↔ `00037` 经 luban）：它自己的锚点
    /// `# Communicating with the user` 在合并块偏移 1212，而 opus 那句
    /// `Write code that reads like…` 也在正文里、偏移 3284。按表序先到先得会切在 3282，
    /// 基座凭空多出 2072 字节；取最早命中才得到官方那 1210B 的基座。
    #[test]
    fn splits_at_earliest_anchor_when_several_match() {
        let raw = Bytes::from(API_SHAPE_BODY.replace(
            r#"\nBASE — 基座\n\nWrite code that reads like the surrounding code: match its comment density, naming, and idiom.\n\nREST"#,
            r#"\nBASE — 基座\n\n# Communicating with the user\n\nWrite code that reads like the surrounding code: match its comment density, naming, and idiom.\n\nREST"#,
        ));
        let out = super::rewrite_body(&raw, &test_cred(), "fp", all_on());
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "应拆成四块: {v}");
        assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "该切在最早的锚点上: {v}");
        assert!(
            sys[3]["text"].as_str().unwrap().starts_with("# Communicating with the user"),
            "其余部分应从最早那个锚点开始: {v}"
        );
    }

    /// 锚点是**按模型族**的：sonnet-5 的基座后面跟的不是 opus 那句，而是 `# Text output …`
    /// （`cap/raw/00009` 直连 10676B 基座 ↔ `00012` 经 luban 合并块偏移 10678）。
    /// haiku-4.5 与 sonnet-5 共用基座，命中的也是这一条。
    #[test]
    fn aligns_sonnet_shape_by_its_own_anchor() {
        let raw = Bytes::from(API_SHAPE_BODY.replace(
            "Write code that reads like the surrounding code: match its comment density, naming, and idiom.",
            "# Text output (does not apply to tool calls)",
        ));
        let out = super::rewrite_body(&raw, &test_cred(), "fp", all_on());
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "sonnet 锚点应能切块: {v}");
        assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "基座切错: {v}");
        assert!(
            sys[3]["text"].as_str().unwrap().starts_with("# Text output"),
            "其余部分应从 sonnet 锚点开始: {v}"
        );
    }

    /// 锚点匹配不到（未知模型族/新版本改了措辞）时**不动结构**，退回三块原样转发——
    /// 宁可不拆，也不切在错误的位置上。其余两项改写照常。
    #[test]
    fn leaves_system_alone_when_anchor_missing() {
        let raw = Bytes::from(API_SHAPE_BODY.replace("Write code that reads like", "改了措辞的"));
        let out = super::rewrite_body(&raw, &test_cred(), "fp", all_on());
        let s = String::from_utf8(out.to_vec()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();

        assert_eq!(v["system"].as_array().unwrap().len(), 3, "不该拆块: {s}");
        assert!(!s.contains("\"ttl\""), "不拆块时不应注入 ttl: {s}");
        assert!(!s.contains("\"scope\""), "不拆块时不应标 scope: {s}");
        assert!(s.contains("cch=00000"), "其余改写仍应生效: {s}");
    }

    /// 客户端本来就是订阅形态（四块）时不动 `system`——它已经是目标形态了。
    #[test]
    fn leaves_official_four_block_shape_alone() {
        let raw = Bytes::from(
            r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli; cch=0848d;"},
                          {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
                          {"type":"text","text":"base","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}},
                          {"type":"text","text":"Write code that reads like the surrounding code: match its comment density, naming, and idiom.","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#,
        );
        let out = super::rewrite_body(&raw, &test_cred(), "fp", all_on());
        assert_eq!(out, raw, "四块形态应原样返回");
    }

    /// 三项 body 改写全关 = **逐字节原样透传**：不重新序列化，故连缩进、换行、转义写法
    /// 这些 serde 会归一化掉的细节都保持不变（重新序列化本身就是个形态 tell）。
    #[test]
    fn body_flags_off_passes_through_byte_for_byte() {
        // 刻意带上多余空白与换行：一旦走了 serde 往返，这些都会被抹平。
        let raw = Bytes::from(format!(" {}\n", API_SHAPE_BODY));
        let flags = store::ForwardFlags {
            spoof_identity: false,
            billing_cch: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
        };
        let out = super::rewrite_body(&raw, &test_cred(), "fp", flags);
        assert_eq!(out, raw, "全关时必须原样返回");

        // 逐项开一个，就只有那一项生效，其余仍不动。
        let only_cch = store::ForwardFlags { billing_cch: true, ..flags };
        let s = String::from_utf8(super::rewrite_body(&raw, &test_cred(), "fp", only_cch).to_vec())
            .unwrap();
        assert!(s.contains("cch=00000"), "只开 cch 时应补 cch: {s}");
        assert!(!s.contains(r#""ttl""#), "system_shape 关着不应拆块/上 ttl: {s}");
        assert!(s.contains(r#"\"account_uuid\":\"\""#), "spoof 关着应保留空 uuid: {s}");

        // system 形态依赖 merge_beta 补的 extended-cache-ttl beta：只开 system_shape 不生效。
        let shape_only = store::ForwardFlags { system_shape: true, ..flags };
        let out = super::rewrite_body(&raw, &test_cred(), "fp", shape_only);
        assert_eq!(out, raw, "merge_beta 关着时不应写出 ttl");

        let with_beta = store::ForwardFlags { merge_beta: true, ..shape_only };
        let s =
            String::from_utf8(super::rewrite_body(&raw, &test_cred(), "fp", with_beta).to_vec())
                .unwrap();
        assert!(s.contains(r#""scope":"global""#), "两个开关都开时应对齐形态: {s}");
        assert!(!s.contains("cch="), "billing_cch 关着不应补 cch: {s}");
    }

    /// 改写后 body 的 key 顺序必须与入站逐字节一致，只允许新增字段追加在末尾。
    ///
    /// serde_json 默认 `Map = BTreeMap`，会把整个 body（含嵌套对象）的 key 按字母序重排，
    /// 得到官方客户端不会产生的排列。靠 `preserve_order` feature 兜住，本测试是它的看门狗：
    /// 一旦该 feature 被摘掉，这里立刻失败。
    #[test]
    fn preserves_key_order() {
        // 客户端的真实字段次序，取自 cap/raw/00002 的原始报文体：顶层是
        // model→messages→system→tools→metadata→max_tokens→…→stream，system 块是 type→text，
        // cache_control 是 type→ttl→scope，metadata.user_id 内层是
        // device_id→account_uuid→session_id。字母序全都不是这样。
        //
        // （cap/*.json 里看到的字母序是抓包工具重新序列化的产物，不是线上的样子。）
        let raw = concat!(
            r#"{"model":"claude-opus-5","messages":[],"#,
            r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"},"#,
            r#"{"type":"text","text":"ident","cache_control":{"type":"ephemeral"}},"#,
            r#"{"type":"text","text":"base\n\nWrite code that reads like the surrounding code: "#,
            r#"match its comment density, naming, and idiom.","cache_control":{"type":"ephemeral"}}],"#,
            r#""tools":[],"#,
            r#""metadata":{"user_id":"{\"device_id\":\"dddd\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}"},"#,
            r#""max_tokens":64000,"stream":true}"#
        );
        let out = super::rewrite_body(&Bytes::from(raw), &test_cred(), "fp", all_on());
        let s = String::from_utf8(out.to_vec()).unwrap();

        // 三项改写都生效了（否则会走 body.clone() 早退，测试空过）。
        assert!(s.contains("cch=00000"), "应补 cch: {s}");
        assert!(s.contains(r#""scope":"global""#), "应对齐 system 形态: {s}");
        assert!(s.contains(&format!(r#"\"account_uuid\":\"{}\""#, ACCOUNT_UUID)), "应填 uuid: {s}");

        // 顶层顺序不变，未被字母序重排（重排后 max_tokens/messages 会跑到 model 前）。
        let mut at = 0;
        for k in ["model", "messages", "system", "tools", "metadata", "max_tokens", "stream"] {
            let needle = format!("\"{k}\":");
            let pos = s[at..].find(&needle).unwrap_or_else(|| panic!("顶层 key {k} 顺序错乱: {s}"));
            at += pos + needle.len();
        }

        // 嵌套对象同样不重排：system 块是 type→text（字母序会变成 text→type），
        // 拆块后新建的两块也按这个键序写回，cache_control 内是 type→ttl→scope。
        assert!(s.contains(r#"{"type":"text","text":"base""#), "system 块 key 被重排: {s}");
        assert!(
            s.contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#),
            "cache_control key 被重排: {s}"
        );

        // 内层 user_id 仍走定点替换，device_id→account_uuid→session_id 原序。
        assert!(
            s.contains(r#"\"device_id\":\""#)
                && s.find(r#"\"device_id\":\""#) < s.find(r#"\"account_uuid\":\""#),
            "内层 user_id key 被重排: {s}"
        );
    }

    fn body_with_system0(text: &str) -> serde_json::Value {
        serde_json::json!({"system": [{"type": "text", "text": text}]})
    }

    /// 补出的 billing header 与订阅模式的真实形态一致（抓包 040 的 `; cch=…;` 形态）。
    #[test]
    fn adds_cch_in_official_shape() {
        let mut v = body_with_system0(
            "x-anthropic-billing-header: cc_version=2.1.218.0b9; cc_entrypoint=cli;",
        );
        assert!(ensure_billing_cch(&mut v));
        assert_eq!(
            v["system"][0]["text"],
            serde_json::json!(
                "x-anthropic-billing-header: cc_version=2.1.218.0b9; cc_entrypoint=cli; cch=00000;"
            )
        );
    }

    /// 已带 cch（订阅模式客户端）不重复追加；非 billing 块不动。
    #[test]
    fn cch_is_idempotent_and_scoped() {
        let mut has = body_with_system0(
            "x-anthropic-billing-header: cc_version=2.1.218.2d7; cc_entrypoint=cli; cch=0848d;",
        );
        assert!(!ensure_billing_cch(&mut has));

        let mut other =
            body_with_system0("You are Claude Code, Anthropic's official CLI for Claude.");
        assert!(!ensure_billing_cch(&mut other));

        let mut empty = serde_json::json!({"messages": []});
        assert!(!ensure_billing_cch(&mut empty));
    }

    fn err_body(etype: &str, msg: &str) -> Vec<u8> {
        serde_json::json!({"type": "error", "error": {"type": etype, "message": msg}})
            .to_string()
            .into_bytes()
    }

    /// 账号级错误照旧停用。
    #[test]
    fn bans_on_real_account_errors() {
        let cases = [
            (StatusCode::UNAUTHORIZED, err_body("authentication_error", "invalid bearer token")),
            (StatusCode::FORBIDDEN, err_body("permission_error", "This account has been disabled")),
            (
                StatusCode::BAD_REQUEST,
                err_body("invalid_request_error", "Your account was suspended"),
            ),
        ];
        for (status, body) in cases {
            assert!(
                detect_account_ban(status, &body).is_some(),
                "应判定为账号级错误: {status} {}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    /// 非账号问题的 4xx 不得停用——这类误杀会把健康账号一个个扣掉。
    #[test]
    fn does_not_ban_on_non_account_errors() {
        let cases = [
            // Pro 账号请求 Opus / beta 未开通：能力问题，不是封号。
            (
                StatusCode::FORBIDDEN,
                err_body("permission_error", "Your account does not have access to claude-opus-5"),
            ),
            // 裸 401（CDN/网关拦截，非 Anthropic 错误 JSON）。
            (StatusCode::UNAUTHORIZED, b"<html>401 Unauthorized</html>".to_vec()),
            // 客户端请求错误。
            (
                StatusCode::BAD_REQUEST,
                err_body("invalid_request_error", "max_tokens: must be <= 64000"),
            ),
            // 特征词碰巧出现在「端点不支持」里：账号是好的。
            (
                StatusCode::UNAUTHORIZED,
                err_body(
                    "authentication_error",
                    "OAuth authentication is currently not supported for this endpoint",
                ),
            ),
        ];
        for (status, body) in cases {
            assert!(
                detect_account_ban(status, &body).is_none(),
                "不应停用: {status} {}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    /// 补齐的 x-client-request-id 是标准 uuid v4 形态。
    #[test]
    fn generates_uuid_v4() {
        let u = uuid_v4();
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "非法字符: {u}");
        assert_eq!(parts[2].as_bytes()[0], b'4', "version 位应为 4: {u}");
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'), "variant 位不对: {u}");
        assert_ne!(u, uuid_v4(), "每请求应不同");
    }

    // 真实 CC 抓包形态：字段顺序 device_id → account_uuid → session_id。
    const CC: &str = r#"{"device_id":"dddd","account_uuid":"aaaa","session_id":"ssss"}"#;

    #[test]
    fn replaces_value_and_preserves_order() {
        let s = replace_json_str_field(CC, "account_uuid", "NEW").unwrap();
        let s = replace_json_str_field(&s, "device_id", "DEV").unwrap();
        assert_eq!(s, r#"{"device_id":"DEV","account_uuid":"NEW","session_id":"ssss"}"#);
    }

    #[test]
    fn fills_empty_account_uuid() {
        let empty = r#"{"device_id":"dddd","account_uuid":"","session_id":"ssss"}"#;
        let s = replace_json_str_field(empty, "account_uuid", "FILLED").unwrap();
        assert_eq!(s, r#"{"device_id":"dddd","account_uuid":"FILLED","session_id":"ssss"}"#);
    }

    #[test]
    fn missing_field_returns_none_no_insert() {
        assert!(replace_json_str_field(CC, "not_here", "X").is_none());
    }

    /// 请求体顶层 `speed` 字段能被读出；缺字段/非法 JSON 返回 None（不阻断转发）。
    #[test]
    fn reads_speed_from_request_body() {
        let with = Bytes::from(r#"{"model":"claude-opus-5","speed":"fast","messages":[]}"#);
        assert_eq!(request_speed(&with).as_deref(), Some("fast"));
        let without = Bytes::from(r#"{"model":"claude-opus-5","messages":[]}"#);
        assert_eq!(request_speed(&without), None);
        assert_eq!(request_speed(&Bytes::from("not json")), None);
    }

    /// 上游 SSE 的 `usage.speed` 会被嗅探到——这是计费的权威来源（fast 被限流会回落）。
    #[test]
    fn sniffs_speed_from_response_usage() {
        let mut s = UsageSniffer::new(true, false);
        s.feed(
            b"data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\
              \"usage\":{\"input_tokens\":10,\"speed\":\"fast\"}}}\n",
        );
        s.finish();
        assert_eq!(s.speed.as_deref(), Some("fast"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(s.input_tokens, Some(10));

        // 非流式 JSON 响应同样能取到。
        let mut s2 = UsageSniffer::new(false, false);
        s2.feed(br#"{"model":"claude-opus-5","usage":{"output_tokens":5,"speed":"standard"}}"#);
        s2.finish();
        assert_eq!(s2.speed.as_deref(), Some("standard"));
    }
}
