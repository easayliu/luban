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
            let mut builder = Response::builder().status(status);
            for (k, v) in up.headers().iter() {
                if is_resp_forwardable(k) {
                    builder = builder.header(k, v);
                }
            }

            // 包裹响应流：首块到达记 TTFT，流结束(或断开)时在 Drop 里记 total 并输出一条日志。
            let mut rl = ReqLog {
                started,
                ttft_ms: None,
                method: method.to_string(),
                path: path_and_query,
                cred: format!("#{} {}", cred.id, cred.label),
                device: device_id.map(|d| d.chars().take(8).collect()),
                status: status.as_u16(),
            };
            let stream = up.bytes_stream().map(move |chunk| {
                if rl.ttft_ms.is_none() {
                    rl.ttft_ms = Some(rl.started.elapsed().as_millis());
                }
                chunk
            });

            builder
                .body(Body::from_stream(stream))
                .unwrap_or_else(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())
        }
        Err(e) => {
            tracing::error!(%method, path = %path_and_query, error = %e, "上游请求失败");
            (StatusCode::BAD_GATEWAY, format!("上游请求失败: {}", e)).into_response()
        }
    }
}

/// 随响应流一起存活；流结束/断开时在 Drop 里输出一条转发日志（含 TTFT 与总耗时）。
struct ReqLog {
    started: std::time::Instant,
    ttft_ms: Option<u128>,
    method: String,
    path: String,
    cred: String,
    /// device_id 前 8 位（脱敏），便于观察粘性绑定命中情况。
    device: Option<String>,
    status: u16,
}

impl Drop for ReqLog {
    fn drop(&mut self) {
        tracing::info!(
            method = %self.method,
            path = %self.path,
            cred = %self.cred,
            device = ?self.device,
            status = self.status,
            ttft_ms = ?self.ttft_ms,
            total_ms = self.started.elapsed().as_millis(),
            "转发"
        );
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
