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

    // 2) 提取 device_id（在请求体 metadata.user_id 里，值本身是一段 JSON 字符串）。
    let device_id = extract_device_id(&body);

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
    let mut out = HeaderMap::new();
    for (k, v) in headers.iter() {
        if is_forwardable(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    // anthropic-version 缺省补齐。
    if !out.contains_key("anthropic-version") {
        out.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    // anthropic-beta 合并，确保带上 oauth。
    let beta = merge_beta(headers.get("anthropic-beta"));
    if let Ok(v) = HeaderValue::from_str(&beta) {
        out.insert("anthropic-beta", v);
    }
    // 注入 OAuth 鉴权（覆盖来访的任何鉴权头）。
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", token)) {
        out.insert(header::AUTHORIZATION, v);
    }

    // 6) 发起上游请求并流式回传。
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
            // 解析并打印上游限流头（订阅账号 5h/7d 额度体现在此），随后随日志入库。
            let ratelimit = RateLimitInfo::from_headers(up.headers());
            tracing::info!(
                cred = format!("#{} {}", cred.id, cred.label),
                rep = %ratelimit.representative.as_deref().unwrap_or("-"),
                u5h = %opt_str(ratelimit.five_h_utilization),
                u7d = %opt_str(ratelimit.seven_d_utilization),
                headers = %ratelimit.raw,
                "上游限流头"
            );

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
                sniffer: UsageSniffer::new(is_stream),
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
                        if let Some(reason) = detect_account_ban(status, &bytes) {
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
    /// 上游返回的订阅账号限流快照。
    ratelimit: RateLimitInfo,
    store: std::sync::Arc<store::CredentialStore>,
}

impl Drop for ReqLog {
    fn drop(&mut self) {
        self.sniffer.finish();
        let has_usage = self.sniffer.has_usage();
        let cost_usd = crate::pricing::estimate_usd(
            self.sniffer.model.as_deref(),
            self.sniffer.input_tokens,
            self.sniffer.output_tokens,
            self.sniffer.cache_creation_tokens,
            self.sniffer.cache_creation_5m,
            self.sniffer.cache_creation_1h,
            self.sniffer.cache_read_tokens,
        );
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
}

impl UsageSniffer {
    fn new(is_stream: bool) -> Self {
        Self {
            is_stream,
            ..Default::default()
        }
    }

    /// 喂入一块响应字节。
    fn feed(&mut self, chunk: &[u8]) {
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

/// 从请求体提取 device_id：`metadata.user_id` 是一段 JSON 字符串，内含 `device_id`。
/// 解析失败或字段缺失/为空时返回 `None`（退化为纯优先级选择、不做粘性绑定）。
fn extract_device_id(body: &Bytes) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let user_id = json.get("metadata")?.get("user_id")?.as_str()?;
    let inner: serde_json::Value = serde_json::from_str(user_id).ok()?;
    let dev = inner.get("device_id")?.as_str()?;
    (!dev.is_empty()).then(|| dev.to_string())
}

/// 400 场景下的账号级错误特征词：命中其一才判定为「该账号被上游封禁/停用/授权失效」，
/// 以区别于常规的客户端请求错误（invalid_request_error，如模型名错、body 超长）——避免
/// 客户端一条坏请求重试时把所有账号逐个误禁。命中后原文（截断）存作 `ban_reason`。
const BAN_KEYWORDS: &[&str] = &[
    "disabled", "suspended", "banned", "terminated", "deactivated", "violat", "invalid_grant",
    "oauth",
];

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
/// - 401 authentication_error / 403 permission_error：账号级鉴权/权限失效，一律停用。
/// - 400：仅当错误类型/消息指向账号级问题（命中 [`BAN_KEYWORDS`]）时停用；
///   普通 invalid_request_error（客户端请求错误）不停用，原样透传。
fn detect_account_ban(status: StatusCode, body: &[u8]) -> Option<String> {
    let (etype, message) = parse_upstream_error(body);
    let reason = || {
        let head = match &etype {
            Some(t) => format!("[{}] {t}: {message}", status.as_u16()),
            None => format!("[{}] {message}", status.as_u16()),
        };
        head.chars().take(200).collect::<String>()
    };
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Some(reason()),
        StatusCode::BAD_REQUEST => {
            let hay = format!("{} {}", etype.as_deref().unwrap_or(""), message).to_lowercase();
            BAN_KEYWORDS.iter().any(|k| hay.contains(k)).then(reason)
        }
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

/// 合并来访的 anthropic-beta 值，保证含 `oauth-2025-04-20`。
fn merge_beta(incoming: Option<&HeaderValue>) -> String {
    let mut parts: Vec<String> = incoming
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    if !parts.iter().any(|p| p == config::OAUTH_BETA_HEADER) {
        parts.push(config::OAUTH_BETA_HEADER.to_string());
    }
    parts.join(",")
}

/// 把 `Option<f64>` 渲染成日志友好字符串（None → `-`）。
fn opt_str(v: Option<f64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".into())
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

/// 请求头是否可转发：跳过鉴权、Host、逐跳头、以及我们显式设置的头。
fn is_forwardable(name: &HeaderName) -> bool {
    let n = name.as_str().to_ascii_lowercase();
    !matches!(
        n.as_str(),
        "host"
            | "authorization"
            | "x-api-key"
            | "content-length"
            | "accept-encoding"
            | "connection"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "anthropic-version"
            | "anthropic-beta"
    )
}

/// 响应头是否可回传：跳过由框架管理的分帧类头。
fn is_resp_forwardable(name: &HeaderName) -> bool {
    let n = name.as_str().to_ascii_lowercase();
    !matches!(
        n.as_str(),
        "content-length" | "transfer-encoding" | "connection" | "content-encoding"
    )
}
