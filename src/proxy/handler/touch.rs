//! `POST /v1/messages/cache_touch`：缓存保活的续命请求（[`crate::proxy::cache_keepalive`]）。
//!
//! 与 `/v1/messages` 那条主流程完全分开走：它不是一条对话请求，体里只有上一条回复的 `request-id`，
//! 没有设备身份、没有模型、不计费，也不该占会话的 RPM 与并发名额（空闲的会话每 30 秒一发）。
//! 唯一要紧的是**打回发出那条回复的号**——所以不选号、不换号重试，记录里是哪个号就用哪个号、
//! 用那条请求出站时的身份头，照官方 touch 的形态发（`cap/auto-2.1.300-20261015-full/00139`）。
//!
//! 成功的 touch 不写流水（一个开着没说话的会话一小时 120 条）；本地拒绝的照常由 [`super::super::handle`]
//! 补流水，上游回了错的打一行日志。

// 闸门失败时的 `Err` 就是要回给客户端的那条响应：只在拒绝时出现，装箱不划算。
#![allow(clippy::result_large_err)]
use super::*;
use crate::proxy::cache_keepalive::{self, Touch, TouchRoute};
use crate::proxy::headers::{orig_header_case, uuid_v4};

/// 体的上限：官方那份 45 字节，留足余量也用不了 4KB。
const TOUCH_BODY_MAX: usize = 4 * 1024;

/// 一发 touch 等多久：官方 10 秒没回就当超时放弃这一发（`touch_timeout`）。
const TOUCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) async fn touch(
    state: &AppState,
    method: Method,
    uri: &Uri,
    headers: HeaderMap,
    body: axum::body::Body,
    request_id: &str,
    log_state: &RequestLogState,
) -> Response {
    let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path()).to_string();
    let client_ua = ua_of(&headers);
    let key_access = match admit::client_gates(state, &method, &path, &client_ua, &headers).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    *log_state.key_id.lock() = key_access.key_id;
    let Some(body) = read_touch_body(body).await else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "failed to read the cache_touch request body",
        );
    };
    let Some(touched) = touched_request_id(&body) else {
        tracing::warn!(%method, path = %path, ua = %client_ua, "rejected: cache_touch body is not {{\"request_id\":\"req_…\"}}");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "request_id: expected the request-id of an earlier /v1/messages response",
        );
    };
    let route = match cache_keepalive::route(&touched, key_access.key_id) {
        Touch::Forward(route) => route,
        // 同一条距上次转发不到 10 秒（客户端异常重发、几个客户端共用一把 Key）：就地当成功回，不把
        // 官方不会有的频率带到号上。上一发刚续过，缓存还在。
        Touch::TooSoon => {
            tracing::debug!(ua = %client_ua, request = %touched, "cache_touch too soon after the last one, answered locally");
            return axum::response::IntoResponse::into_response(StatusCode::NO_CONTENT);
        }
        // 没记过（luban 重启过、记录过期、或根本不是经 luban 发出的那条）：本地回 404，客户端连着三次
        // 404 就不再续这一条，等下一条主线程回复重新上膛。
        Touch::Unknown => {
            *log_state.local_reject.lock() = Some("touch-unknown");
            tracing::debug!(ua = %client_ua, request = %touched, "cache_touch for a reply luban has no keepalive record of");
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found_error",
                "no cache keepalive record for this request_id",
            );
        }
    };
    let cred = match state.store.get(route.cred_id).await {
        Ok(Some(c)) if !c.disabled => c,
        Ok(_) => {
            // 那个号停了或删了：缓存在它名下，换号续不了，当作没有这条。
            cache_keepalive::forget(&touched);
            *log_state.local_reject.lock() = Some("touch-cred-gone");
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found_error",
                "the account that served this request is no longer available",
            );
        }
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), cred_id = route.cred_id, "cache_touch: failed to load the credential");
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "failed to load the account",
            );
        }
    };
    let token = match store::access_token_of(&state.store, &state.clients, &cred).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(cred_id = cred.id, cred = %cred.label, error = %format!("{e:#}"), "cache_touch: getting an access_token failed");
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                format!("token refresh for credential #{} failed; please retry shortly", cred.id),
            );
        }
    };
    let client = match attempt::client_for(state, &cred, request_id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let out = touch_headers(&route, &token, headers.get("x-client-request-id"));
    let url = format!("{}{}", state.upstream_base, config::CC_CACHE_TOUCH_PATH);
    // 官方是 `JSON.stringify({request_id})`：没有空格，键只有这一个。
    let out_body = serde_json::json!({ "request_id": touched }).to_string();
    let req = client.post(&url).headers(out).body(out_body);
    let req = if state.store.forward_flags().orig_header_case {
        req.orig_headers(orig_header_case())
    } else {
        req
    };
    let started = std::time::Instant::now();
    let sent = tokio::time::timeout(TOUCH_TIMEOUT, async {
        let up = req.send().await?;
        let builder = resp_builder(&up);
        let status = up.status();
        let bytes = up.bytes().await?;
        Ok::<_, wreq::Error>((status, builder, bytes))
    })
    .await;
    match sent {
        Ok(Ok((status, builder, bytes))) => {
            cache_keepalive::settle(&touched, status.as_u16());
            // 到过上游的不按本地拒绝补流水（见模块说明）。
            log_state.logged.store(true, std::sync::atomic::Ordering::Relaxed);
            if status.is_success() {
                tracing::debug!(cred_id = cred.id, request = %touched, ms = started.elapsed().as_millis() as u64, "cache_touch ok");
            } else {
                tracing::info!(
                    cred_id = cred.id, cred = %cred.label, ua = %client_ua, request = %touched,
                    status = status.as_u16(),
                    body = %String::from_utf8_lossy(&bytes[..bytes.len().min(512)]),
                    "cache_touch: upstream did not accept the touch"
                );
            }
            builder.body(Body::from(bytes)).unwrap_or_else(|e| {
                tracing::error!(error = %e, "cache_touch: building the response failed");
                error_response(StatusCode::BAD_GATEWAY, "api_error", "bad upstream response")
            })
        }
        Ok(Err(e)) => {
            tracing::warn!(cred_id = cred.id, cred = %cred.label, error = %error_chain(&e), "cache_touch: upstream request failed");
            error_response(StatusCode::BAD_GATEWAY, "api_error", "upstream request failed")
        }
        Err(_) => {
            tracing::warn!(cred_id = cred.id, cred = %cred.label, "cache_touch: upstream timed out");
            error_response(StatusCode::GATEWAY_TIMEOUT, "api_error", "upstream timed out")
        }
    }
}

/// 读体：超过 [`TOUCH_BODY_MAX`] 或读失败就是 `None`。
async fn read_touch_body(body: axum::body::Body) -> Option<Bytes> {
    use futures_util::StreamExt;
    let mut stream = body.into_data_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if buf.len() + chunk.len() > TOUCH_BODY_MAX {
            return None;
        }
        buf.extend_from_slice(&chunk);
    }
    Some(Bytes::from(buf))
}

/// 体里那个 `request_id`：JSON 对象里唯一要的字段，形态得是上游的 `req_…`。
fn touched_request_id(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let id = v.get("request_id")?.as_str()?;
    cache_keepalive::is_request_id(id).then(|| id.to_string())
}

/// touch 的出站头，照官方线序（[`config::CC_HEADER_ORDER`] 管落位）：身份那几项取自那条回复的
/// 出站请求（[`TouchRoute::headers`]），其余是 touch 自己的固定值——`Accept: application/json`、
/// 两项 beta、请求类别 `auxiliary`、`X-Stainless-Retry-Count: 0`（官方 `maxRetries: 0`）。
/// `x-client-request-id` 沿用来访那个（合法 uuid 时），没有就生成一个。
fn touch_headers(
    route: &TouchRoute,
    token: &str,
    client_request_id: Option<&HeaderValue>,
) -> HeaderMap {
    let carried =
        |name: &str| route.headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.clone());
    let mut out = HeaderMap::new();
    let mut put = |name: &'static str, v: Option<HeaderValue>| {
        if let Some(v) = v {
            out.insert(name, v);
        }
    };
    put("accept", Some(HeaderValue::from_static("application/json")));
    put("authorization", HeaderValue::from_str(&format!("Bearer {token}")).ok());
    put("content-type", Some(HeaderValue::from_static("application/json")));
    for name in [
        "user-agent",
        "x-claude-code-session-id",
        "x-stainless-arch",
        "x-stainless-lang",
        "x-stainless-os",
        "x-stainless-package-version",
    ] {
        put(name, carried(name));
    }
    put("x-stainless-retry-count", Some(HeaderValue::from_static("0")));
    for name in ["x-stainless-runtime", "x-stainless-runtime-version"] {
        put(name, carried(name));
    }
    put("anthropic-beta", HeaderValue::from_str(&cache_keepalive::touch_beta()).ok());
    put(
        "anthropic-dangerous-direct-browser-access",
        carried("anthropic-dangerous-direct-browser-access")
            .or(Some(HeaderValue::from_static("true"))),
    );
    put(
        "anthropic-version",
        carried("anthropic-version").or(Some(HeaderValue::from_static("2023-06-01"))),
    );
    put("x-app", carried("x-app"));
    put("x-claude-code-request-class", Some(HeaderValue::from_static("auxiliary")));
    let rid = client_request_id
        .filter(|v| v.to_str().is_ok_and(crate::proxy::simulation::looks_like_uuid))
        .cloned()
        .or_else(|| HeaderValue::from_str(&uuid_v4()).ok());
    put("x-client-request-id", rid);
    put("connection", Some(HeaderValue::from_static("keep-alive")));
    put("accept-encoding", Some(HeaderValue::from_static(config::CC_ACCEPT_ENCODING)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_body_must_carry_a_request_id() {
        assert_eq!(
            touched_request_id(br#"{"request_id":"req_011Cg34k1PDydFY8V38T9315"}"#).as_deref(),
            Some("req_011Cg34k1PDydFY8V38T9315")
        );
        for bad in [
            &br#"{"request_id":"msg_011C"}"#[..],
            br#"{"request_id":1}"#,
            br#"{}"#,
            b"req_011C",
            b"",
        ] {
            assert!(touched_request_id(bad).is_none(), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// 出站头与官方 touch 逐项一致（`cap/auto-2.1.300-20261015-full/00139`），线序由
    /// `orig_header_case` 按 [`config::CC_HEADER_ORDER`] 归位，这里只核插入序与取值。
    #[test]
    fn touch_headers_match_the_official_touch() {
        let v = |s: &str| HeaderValue::from_str(s).unwrap();
        let route = TouchRoute {
            cred_id: 1,
            headers: vec![
                ("user-agent", v("claude-cli/2.1.300 (external, cli)")),
                ("x-claude-code-session-id", v("3730aa95-50eb-4b01-a271-045e2e06e647")),
                ("x-stainless-arch", v("arm64")),
                ("x-stainless-lang", v("js")),
                ("x-stainless-os", v("MacOS")),
                ("x-stainless-package-version", v("0.128.0")),
                ("x-stainless-runtime", v("node")),
                ("x-stainless-runtime-version", v("v26.3.0")),
                ("anthropic-dangerous-direct-browser-access", v("true")),
                ("anthropic-version", v("2023-06-01")),
                ("x-app", v("cli")),
            ],
        };
        let rid = v("33ea8e48-c900-4e1b-8cc1-e982e3d7e155");
        let out = touch_headers(&route, "tok", Some(&rid));
        let got: Vec<(String, String)> = out
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap().to_string()))
            .collect();
        let want: Vec<(String, String)> = [
            ("accept", "application/json"),
            ("authorization", "Bearer tok"),
            ("content-type", "application/json"),
            ("user-agent", "claude-cli/2.1.300 (external, cli)"),
            ("x-claude-code-session-id", "3730aa95-50eb-4b01-a271-045e2e06e647"),
            ("x-stainless-arch", "arm64"),
            ("x-stainless-lang", "js"),
            ("x-stainless-os", "MacOS"),
            ("x-stainless-package-version", "0.128.0"),
            ("x-stainless-retry-count", "0"),
            ("x-stainless-runtime", "node"),
            ("x-stainless-runtime-version", "v26.3.0"),
            ("anthropic-beta", "oauth-2025-04-20,cache-keepalive-2026-09-03"),
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("anthropic-version", "2023-06-01"),
            ("x-app", "cli"),
            ("x-claude-code-request-class", "auxiliary"),
            ("x-client-request-id", "33ea8e48-c900-4e1b-8cc1-e982e3d7e155"),
            ("connection", "keep-alive"),
            ("accept-encoding", config::CC_ACCEPT_ENCODING),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(got, want);
        // 来访那个不是 uuid：换一个新的。
        let out = touch_headers(&route, "tok", Some(&v("sess-42")));
        let rid = out.get("x-client-request-id").unwrap().to_str().unwrap();
        assert!(crate::proxy::simulation::looks_like_uuid(rid) && rid != "sess-42");
    }
}
