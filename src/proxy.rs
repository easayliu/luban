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
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path()).to_string();

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
            return (StatusCode::FORBIDDEN, "缺少有效的设备身份（metadata.user_id）").into_response();
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

    // 5) 组装转发头：复制安全头，注入鉴权与 beta。
    let out = build_forward_headers(&headers, &token);

    // 6) 转发前改写 body：最大 system 块标 scope=global + 身份伪装（metadata.user_id 的
    //    account_uuid/device_id 换成该凭证自洽身份、billing header 补 cch）。缓存 TTL 不动。
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
    let body = rewrite_body(&body, &cred, &device_fp, state.store.spoof_identity_enabled());

    // 7) 发起上游请求并流式回传。
    let resp = state
        .http
        .request(method.clone(), &url)
        .headers(out)
        .body(body)
        .send()
        .await;

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
            // 正常情况下这里恒为 false：上游客户端开了 gzip/br/zstd/deflate 解压，reqwest
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
                    "上游响应带无法解码的 content-encoding：用量嗅探与账号级错误判定都会被跳过（该编码需在 reqwest feature 里开启）"
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
                        builder
                            .body(Body::from(bytes))
                            .unwrap_or_else(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "读取上游错误响应体失败");
                        builder
                            .body(Body::empty())
                            .unwrap_or_else(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())
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
            // reqwest 顶层 Display 往往只有「error sending request」，真正原因在 source 链里。
            let detail = error_chain(&e);
            let kind = reqwest_error_kind(&e);
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

/// 粗分 reqwest 错误类别，便于一眼定位（超时 / 连接 / DNS-TLS 等）。
fn reqwest_error_kind(e: &reqwest::Error) -> &'static str {
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
    /// 正常路径下恒为 false——reqwest 已解码，见 [`handle`] 里 `compressed` 的说明。
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
        Self {
            is_stream,
            opaque,
            ..Default::default()
        }
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
    Some(FlatUserId {
        device: device.to_string(),
        session: session.to_string(),
    })
}

/// 400 场景下的账号级错误特征词：命中其一才判定为「该账号被上游封禁/停用/授权失效」，
/// 以区别于常规的客户端请求错误（invalid_request_error，如模型名错、body 超长）——避免
/// 客户端一条坏请求重试时把所有账号逐个误禁。命中后原文（截断）存作 `ban_reason`。
const BAN_KEYWORDS: &[&str] = &[
    "disabled", "suspended", "banned", "terminated", "deactivated", "violat", "invalid_grant",
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
        v.as_ref()
            .and_then(|v| v.get("error")?.get(name)?.as_str().map(str::to_string))
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
    state
        .store
        .get_setting(store::CLIENT_API_KEY)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
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

/// 合并来访的 anthropic-beta 值，补齐 [`config::INJECT_BETAS`]（对齐官方订阅客户端），
/// 并按 [`config::CC_BETA_ORDER`] 重排。
///
/// 只追加不重排会得到官方客户端不会产生的排列（缺失项全堆在末尾），集合对了顺序错，
/// 一次精确字符串匹配即可判定中间有代理。表外的未知 beta 保持相对顺序附在末尾。
fn merge_beta(incoming: Option<&HeaderValue>) -> String {
    let mut parts: Vec<String> = incoming
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    for beta in config::INJECT_BETAS {
        if !parts.iter().any(|p| p == beta) {
            parts.push((*beta).to_string());
        }
    }
    // 稳定排序：已知 beta 按官方位次，未知的排在最后并保留原有相对顺序。
    let rank = |p: &String| {
        config::CC_BETA_ORDER
            .iter()
            .position(|k| k == p)
            .unwrap_or(config::CC_BETA_ORDER.len())
    };
    parts.sort_by_key(rank);
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
/// 无法对齐的部分（头名大小写、hyper 自己追加的 `user-agent`/`host`/`content-length`）
/// 见 [`crate::config::known_fingerprint_gaps`]。
fn build_forward_headers(headers: &HeaderMap, token: &str) -> HeaderMap {
    let mut out = HeaderMap::new();
    // `append` 而非 `insert`：同名多值头要全部保留，`insert` 会只剩最后一个。
    for (k, v) in headers.iter() {
        if is_forwardable(k, v) {
            out.append(k.clone(), v.clone());
        }
    }
    // anthropic-version 缺省补齐。
    if !out.contains_key("anthropic-version") {
        out.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    // anthropic-beta 合并，确保带上 oauth，并按官方客户端顺序重排。
    match HeaderValue::from_str(&merge_beta(headers.get("anthropic-beta"))) {
        Ok(v) => {
            out.insert("anthropic-beta", v);
        }
        // merge_beta 只产出 ASCII，理论上不可达；真发生时保留来访原值，别把这个头发空。
        Err(e) => tracing::warn!(error = %e, "构造 anthropic-beta 失败，保留来访原值"),
    }
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

/// 生成一个随机 uuid v4（小写带连字符），用于补齐 `x-client-request-id`。
fn uuid_v4() -> String {
    let mut b: [u8; 16] = rand::rng().random();
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(&b[0..4]),
        h(&b[4..6]),
        h(&b[6..8]),
        h(&b[8..10]),
        h(&b[10..16])
    )
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

/// 转发前改写请求体：
///
/// 1. **缓存 scope**：`system` 里文本最长的静态块标记 `scope: "global"`，提升跨会话缓存复用。
/// 2. **身份伪装**：把 `metadata.user_id` 里的 `account_uuid`/`device_id` 换成该凭证自洽的
///    身份（真实 account_uuid + 由其稳定派生的 device_id），避免「真账号 + 陌生设备」的矛盾；
///    并给 `x-anthropic-billing-header` 补订阅模式独有的 `cch`。
///
/// **不动缓存 TTL**：客户端声明 5m 就按 5m 转发。曾把所有 ephemeral 断点无条件升成 1h，
/// 但 1h 缓存写单价是 2 倍、且会让上游看到「1h 缓存写占比异常」，收益不值这个代价。
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
    spoof_enabled: bool,
) -> Bytes {
    let mut v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.clone(),
    };
    let global_idx = mark_largest_system_global(&mut v);
    let cch_added = spoof_enabled && ensure_billing_cch(&mut v);
    tracing::debug!(
        metadata = %v.get("metadata").map(|m| m.to_string()).unwrap_or_else(|| "<无 metadata>".into()),
        "入站 metadata"
    );
    let spoofed = spoof_enabled && spoof_identity(&mut v, cred, device_fp);
    tracing::debug!(
        scope_global_at = global_idx.map(|i| i as i64).unwrap_or(-1),
        spoofed,
        cch_added,
        device_fp = %device_fp,
        spoof_device = %cred.spoof_device_id(device_fp).as_deref().unwrap_or("-"),
        "改写 body"
    );
    if global_idx.is_none() && !spoofed && !cch_added {
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
    format!(
        "{}|{}|{}",
        client_device_id.unwrap_or(""),
        h("x-stainless-arch"),
        h("x-stainless-os"),
    )
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

/// 给 `system` 数组里「带 cache_control 且 text 最长」的块补 `scope: "global"`。
/// 对应订阅客户端把体积最大的静态系统提示词标为全局缓存的做法。
/// 返回被标记的块下标；无可标记或已是 global 时返回 `None`。
fn mark_largest_system_global(v: &mut serde_json::Value) -> Option<usize> {
    let sys = v.get_mut("system").and_then(|s| s.as_array_mut())?;
    let mut best: Option<(usize, usize)> = None; // (下标, text 长度)
    for (i, blk) in sys.iter().enumerate() {
        if blk.get("cache_control").is_none() {
            continue;
        }
        let len = blk.get("text").and_then(|t| t.as_str()).map(str::len).unwrap_or(0);
        if best.map_or(true, |(_, bl)| len > bl) {
            best = Some((i, len));
        }
    }
    let (idx, _) = best?;
    let cc = sys[idx].get_mut("cache_control").and_then(|c| c.as_object_mut())?;
    if cc.get("scope").and_then(|s| s.as_str()) == Some("global") {
        return None;
    }
    cc.insert("scope".into(), serde_json::Value::String("global".into()));
    Some(idx)
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
            if !(name.contains("ratelimit") || name == "retry-after" || name.starts_with("anthropic-"))
            {
                continue;
            }
            let val = v.to_str().unwrap_or("<non-utf8>");
            pairs.push(format!("{name}={val}"));
            match name.as_str() {
                "anthropic-ratelimit-unified-status" => info.unified_status = Some(val.to_string()),
                "anthropic-ratelimit-unified-5h-status" => info.five_h_status = Some(val.to_string()),
                "anthropic-ratelimit-unified-5h-reset" => info.five_h_reset = val.parse().ok(),
                "anthropic-ratelimit-unified-5h-utilization" => {
                    info.five_h_utilization = val.parse().ok()
                }
                "anthropic-ratelimit-unified-7d-status" => info.seven_d_status = Some(val.to_string()),
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
/// 都压，SSE 也不例外），故上游客户端开了对应的解压 feature，reqwest 收到时已解码；
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
/// `content-encoding` 保留转发，但正常情况下它**根本不会出现**——reqwest 解码后会把它连同
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
        request_speed, uuid_v4,
    };

    /// API-key 模式的 CC 实际发出的 beta 串（抓包 041，经 luban 转发那一条）。
    const CLIENT_BETA: &str = "claude-code-20250219,interleaved-thinking-2025-05-14,\
        redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,\
        prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,effort-2025-11-24";

    /// 官方订阅客户端直连 API 时的 beta 串（抓包 040，同一台机器同一版本）。
    const OFFICIAL_BETA: &str = "claude-code-20250219,oauth-2025-04-20,\
        interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
        thinking-token-count-2026-05-13,context-management-2025-06-27,\
        prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
        advanced-tool-use-2025-11-20,effort-2025-11-24,extended-cache-ttl-2025-04-11";

    /// 补齐 + 重排后应与官方客户端的 beta 串**逐字节一致**，而不是把缺失项堆在末尾。
    #[test]
    fn merged_beta_matches_official_order() {
        let v = HeaderValue::from_static(CLIENT_BETA);
        assert_eq!(merge_beta(Some(&v)), OFFICIAL_BETA);
    }

    /// 表外的未知 beta 保留在末尾，不因排序被丢弃或插到中间。
    #[test]
    fn merged_beta_keeps_unknown_betas_last() {
        let raw = format!("{CLIENT_BETA},some-future-beta-2027-01-01");
        let v = HeaderValue::from_str(&raw).unwrap();
        let out = merge_beta(Some(&v));
        assert_eq!(out, format!("{OFFICIAL_BETA},some-future-beta-2027-01-01"));
    }

    /// 无来访 beta 时也要输出官方位次（oauth 在最前档、extended-cache-ttl 最后）。
    #[test]
    fn merged_beta_from_empty_is_ordered() {
        assert_eq!(
            merge_beta(None),
            "oauth-2025-04-20,prompt-caching-scope-2026-01-05,advanced-tool-use-2025-11-20,\
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
            h.insert(
                super::HeaderName::from_static(k),
                HeaderValue::from_static(v),
            );
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
        let out = build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL");

        assert_eq!(
            names(&out),
            vec![
                "accept",
                "accept-encoding",
                "authorization",  // 原位，值被换成 OAuth token
                "connection",     // keep-alive 保留转发
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

    /// 覆盖失败时必须把 `authorization` 摘掉，不能把来访者的接入 key 漏给上游。
    /// （这条只有在「照常转发再覆盖」的写法下才存在，剥离式写法天然没有这个洞。）
    #[test]
    fn never_leaks_client_key_upstream() {
        // token 里塞进换行——`HeaderValue::from_str` 会拒绝，走到移除分支。
        let out = build_forward_headers(&incoming_headers(), "bad\ntoken");
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

        // reqwest 解码后会把 content-encoding / content-length 一并摘掉。
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

    /// 线上字节的看门狗：`HeaderMap` 的顺序要真的落到线上，且 hyper 要肯发
    /// 显式的 `Connection: keep-alive`（它默认认为 HTTP/1.1 隐含 keep-alive、不发这个头）。
    ///
    /// 用的是 [`crate::web::upstream_client`] 那份**真配置**，不是测试里另抄一份。
    #[tokio::test]
    async fn wire_bytes_preserve_header_order() {
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

        let _ = crate::web::upstream_client()
            .unwrap()
            .post(format!("http://{addr}/v1/messages?beta=true"))
            .headers(build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL"))
            .body(r#"{"model":"claude-sonnet-5"}"#)
            .send()
            .await;

        let raw = server.join().unwrap();
        let wire: Vec<&str> = raw
            .lines()
            .skip(1) // 请求行
            .filter(|l| !l.is_empty())
            .map(|l| l.split(':').next().unwrap())
            .collect();

        // 来访头序原样落到线上（hyper 追加的三个在末尾，见 known_fingerprint_gaps）。
        let tail = wire.len() - 3;
        assert_eq!(
            &wire[..tail],
            &[
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
                "x-client-request-id",
            ],
            "线上头序与来访不符:\n{raw}"
        );
        assert!(
            raw.contains("connection: keep-alive"),
            "hyper 吞掉了显式的 Connection 头:\n{raw}"
        );
        // hyper 自己追加的三个：位置无法控制，只断言它们确实在末尾，别的没变。
        let mut appended = wire[tail..].to_vec();
        appended.sort_unstable();
        assert_eq!(appended, ["content-length", "host", "user-agent"], "\n{raw}");
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

    /// 转发时**不得**改写缓存 TTL：客户端声明 5m（不带 ttl）就按原样发。
    /// 曾无条件升成 1h，代价是 2 倍缓存写单价 + 上游可见的「1h 写占比异常」，已去掉。
    #[test]
    fn does_not_rewrite_cache_ttl() {
        let raw = Bytes::from(
            r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"},
                          {"type":"text","text":"big","cache_control":{"type":"ephemeral"}}],
                "messages":[{"role":"user","content":[{"type":"text","text":"hi",
                          "cache_control":{"type":"ephemeral"}}]}]}"#,
        );
        let out = super::rewrite_body(&raw, &test_cred(), "fp", true);
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(!s.contains("\"ttl\""), "不应注入 ttl: {s}");
        // 同时确认另外两项改写仍生效（否则这个测试会因为整体没改写而空过）。
        assert!(s.contains("cch=00000"), "应补 cch: {s}");
        assert!(s.contains("\"scope\":\"global\""), "应标 scope: {s}");
    }

    /// 改写后 body 的 key 顺序必须与入站逐字节一致，只允许新增字段追加在末尾。
    ///
    /// serde_json 默认 `Map = BTreeMap`，会把整个 body（含嵌套对象）的 key 按字母序重排，
    /// 得到官方客户端不会产生的排列。靠 `preserve_order` feature 兜住，本测试是它的看门狗：
    /// 一旦该 feature 被摘掉，这里立刻失败。
    #[test]
    fn preserves_key_order() {
        // 抓包 040/041 的真实字段次序：model 在 max_tokens 前、system 块是 type→text、
        // metadata.user_id 内层是 device_id→account_uuid→session_id。字母序全都不是这样。
        let raw = concat!(
            r#"{"model":"claude-sonnet-5","max_tokens":64000,"#,
            r#""metadata":{"user_id":"{\"device_id\":\"dddd\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}"},"#,
            r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"},"#,
            r#"{"type":"text","text":"big","cache_control":{"type":"ephemeral"}}],"#,
            r#""stream":true,"tools":[]}"#
        );
        let out = super::rewrite_body(&Bytes::from(raw), &test_cred(), "fp", true);
        let s = String::from_utf8(out.to_vec()).unwrap();

        // 三项改写都生效了（否则会走 body.clone() 早退，测试空过）。
        assert!(s.contains("cch=00000"), "应补 cch: {s}");
        assert!(s.contains(r#""scope":"global""#), "应标 scope: {s}");
        assert!(s.contains(&format!(r#"\"account_uuid\":\"{}\""#, ACCOUNT_UUID)), "应填 uuid: {s}");

        // 顶层顺序不变，未被字母序重排（重排后 max_tokens 会跑到 model 前）。
        let mut at = 0;
        for k in ["model", "max_tokens", "metadata", "system", "stream", "tools"] {
            let needle = format!("\"{k}\":");
            let pos = s[at..].find(&needle).unwrap_or_else(|| panic!("顶层 key {k} 顺序错乱: {s}"));
            at += pos + needle.len();
        }

        // 嵌套对象同样不重排：system 块是 type→text（字母序会变成 text→type），
        // cache_control 新增的 scope 追加在 type 之后。
        assert!(s.contains(r#"{"type":"text","text":"big""#), "system 块 key 被重排: {s}");
        assert!(s.contains(r#""cache_control":{"type":"ephemeral","scope":"global"}"#), "cache_control key 被重排: {s}");

        // 内层 user_id 仍走定点替换，device_id→account_uuid→session_id 原序。
        assert!(
            s.contains(r#"\"device_id\":\""#) && s.find(r#"\"device_id\":\""#) < s.find(r#"\"account_uuid\":\""#),
            "内层 user_id key 被重排: {s}"
        );
    }

    fn body_with_system0(text: &str) -> serde_json::Value {
        serde_json::json!({"system": [{"type": "text", "text": text}]})
    }

    /// 补出的 billing header 与订阅模式的真实形态一致（抓包 040 的 `; cch=…;` 形态）。
    #[test]
    fn adds_cch_in_official_shape() {
        let mut v =
            body_with_system0("x-anthropic-billing-header: cc_version=2.1.218.0b9; cc_entrypoint=cli;");
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

        let mut other = body_with_system0("You are Claude Code, Anthropic's official CLI for Claude.");
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
        assert_eq!(
            s,
            r#"{"device_id":"DEV","account_uuid":"NEW","session_id":"ssss"}"#
        );
    }

    #[test]
    fn fills_empty_account_uuid() {
        let empty = r#"{"device_id":"dddd","account_uuid":"","session_id":"ssss"}"#;
        let s = replace_json_str_field(empty, "account_uuid", "FILLED").unwrap();
        assert_eq!(
            s,
            r#"{"device_id":"dddd","account_uuid":"FILLED","session_id":"ssss"}"#
        );
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
