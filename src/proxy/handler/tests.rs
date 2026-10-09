//! 转发主流程的端到端用例：本地起一个模拟上游，经 [`crate::proxy::handle`] 发真请求。
//!
//! 守的是选号之后那一大段——401 / 403 换号、429 冷却与换号、400 学规则、连接失败——这些
//! 分支只有真打一发上游才走得到。用例只看客户端拿到什么、上游收到几发、账号状态与流水，
//! 不看日志与内部变量，拆分 `handle_inner` 时它们应当一条不改照样通过。
//!
//! 不出网：上游地址指到本地（[`AppState::upstream_base`]）；`api_telemetry` 关掉，免得新会话的
//! 启动握手打到真上游；凭证的 token 远未过期，选号不会去刷新。

use std::collections::VecDeque;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use parking_lot::Mutex;

use crate::store::{self, CredentialStore};
use crate::web::AppState;

/// 模拟上游的一发回复。
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn json_error(status: u16, etype: &str, message: &str) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(&serde_json::json!({
                "type": "error",
                "error": {"type": etype, "message": message}}))
            .unwrap(),
        }
    }

    fn with_headers(mut self, pairs: &[(&str, String)]) -> Self {
        self.headers.extend(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())));
        self
    }
}

/// 一段完整的流式成功回复，正文是 `text`。
fn sse_ok(text: &str) -> Reply {
    let events = [
        (
            "message_start",
            serde_json::json!({"type": "message_start", "message": {
                "id": "msg_mock", "type": "message", "role": "assistant",
                "model": "claude-sonnet-5", "content": [], "stop_reason": null,
                "usage": {"input_tokens": 12, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": text}}),
        ),
        ("content_block_stop", serde_json::json!({"type": "content_block_stop", "index": 0})),
        (
            "message_delta",
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 5}}),
        ),
        ("message_stop", serde_json::json!({"type": "message_stop"})),
    ];
    let body: String =
        events.iter().map(|(name, data)| format!("event: {name}\ndata: {data}\n\n")).collect();
    Reply {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: body.into_bytes(),
    }
}

/// 上游收到的一发请求。
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    token: String,
}

/// 本地模拟上游：按脚本依次回复（脚本用完回 500），记下每发请求。
struct MockUpstream {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl MockUpstream {
    async fn start(replies: Vec<Reply>) -> Self {
        let script = Arc::new(Mutex::new(VecDeque::from(replies)));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (script_h, seen_h) = (script.clone(), seen.clone());
        let app = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
            let (script, seen) = (script_h.clone(), seen_h.clone());
            async move {
                let token = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .unwrap_or_default()
                    .to_string();
                seen.lock().push(Seen { path: uri.to_string(), token });
                let reply = script.lock().pop_front().unwrap_or_else(|| {
                    Reply::json_error(500, "api_error", "mock upstream script exhausted")
                });
                let mut resp = axum::response::Response::new(axum::body::Body::from(reply.body));
                *resp.status_mut() = StatusCode::from_u16(reply.status).unwrap();
                for (k, v) in reply.headers {
                    resp.headers_mut().insert(
                        axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                        HeaderValue::from_str(&v).unwrap(),
                    );
                }
                resp
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base: format!("http://{addr}"), seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().clone()
    }
}

/// 一个装好 `n` 个号（token `tok-0`、`tok-1`…）的库与指向 `base` 的状态。
fn setup(n: usize, base: &str) -> (Arc<CredentialStore>, AppState, Vec<i64>) {
    setup_tier(n, base, "max")
}

fn setup_tier(n: usize, base: &str, tier: &str) -> (Arc<CredentialStore>, AppState, Vec<i64>) {
    let store = Arc::new(CredentialStore::open_in_memory().unwrap());
    // 新会话的启动握手会打真上游，测试里关掉；探针拦截与这里要测的分支无关，也关掉。
    store.set_setting(store::API_TELEMETRY, "0").unwrap();
    store.set_setting(store::REJECT_PROBES, "0").unwrap();
    let far_future = crate::credentials::now_secs() + 30 * 24 * 3600;
    let ids = (0..n)
        .map(|i| {
            store
                .insert(
                    &format!("acct-{i}"),
                    Some(tier),
                    &format!("tok-{i}"),
                    &format!("rt-{i}"),
                    far_future,
                    Some(&format!("00000000-0000-4000-8000-00000000000{i}")),
                    None,
                    1,
                )
                .unwrap()
                .id
        })
        .collect();
    let mut state = AppState::for_test(store.clone());
    state.upstream_base = base.into();
    (store, state, ids)
}

const SESSION: &str = "3f2b6c1e-8a4d-4c2b-9e7f-1a2b3c4d5e6f";

/// 一条官方客户端形态的流式请求：可信 UA、头体两处同一个会话 id、`metadata.user_id` 带设备。
fn cc_request(extra: serde_json::Value) -> (HeaderMap, Bytes) {
    let user_id = serde_json::json!({
        "device_id": "a".repeat(64),
        "account_uuid": "",
        "session_id": SESSION,
    })
    .to_string();
    let mut body = serde_json::json!({
        "model": "claude-sonnet-5",
        "max_tokens": 1024,
        "stream": true,
        "metadata": {"user_id": user_id},
        "messages": [{"role": "user", "content": "write a haiku about the sea"}],
    });
    if let (Some(obj), Some(more)) = (body.as_object_mut(), extra.as_object()) {
        obj.extend(more.clone());
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_str(&format!(
            "claude-cli/{} (external, cli)",
            crate::config::CC_VERSION_BASE
        ))
        .unwrap(),
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers.insert("x-claude-code-session-id", HeaderValue::from_static(SESSION));
    (headers, Bytes::from(serde_json::to_vec(&body).unwrap()))
}

async fn send(state: &AppState, req: (HeaderMap, Bytes)) -> (StatusCode, HeaderMap, Bytes) {
    let resp = crate::proxy::handle(
        axum::extract::State(state.clone()),
        axum::http::Method::POST,
        "/v1/messages".parse::<Uri>().unwrap(),
        req.0,
        req.1,
    )
    .await;
    let (status, headers) = (resp.status(), resp.headers().clone());
    let body = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
    (status, headers, body)
}

/// 流水是 `spawn_blocking` / 响应流结束时写的，等它落到 `n` 条（最多 3 秒）。
async fn usage_logs(store: &CredentialStore, n: usize) -> Vec<store::UsageLog> {
    let mut rows = Vec::new();
    for _ in 0..300 {
        rows = store.list_usage_logs(50).unwrap();
        if rows.len() >= n {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    rows
}

fn is_out_of_pool(store: &CredentialStore, id: i64) -> bool {
    store.get(id).unwrap().unwrap().disabled
}

/// 一发成功的流式回复原样转回客户端，上游收到的是号的 token，流水记到这个号上、带用量。
#[tokio::test]
async fn a_successful_stream_is_relayed_and_logged() {
    let mock = MockUpstream::start(vec![sse_ok("waves fold into foam")]).await;
    let (store, state, ids) = setup(1, &mock.base);

    let (status, headers, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap().contains("text/event-stream")
    );
    assert!(String::from_utf8_lossy(&body).contains("waves fold into foam"));
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].token, "tok-0");
    assert!(seen[0].path.starts_with("/v1/messages"), "{}", seen[0].path);

    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 200);
    assert_eq!(rows[0].cred_id, Some(ids[0]));
    assert!(rows[0].has_usage, "SSE 里的用量要嗅出来");
}

/// 401 账号级错误（token 作废）：停用这个号，换一个号重发，客户端拿到的是第二个号的 200。
#[tokio::test]
async fn an_account_level_401_disables_the_credential_and_swaps() {
    let mock = MockUpstream::start(vec![
        Reply::json_error(401, "authentication_error", "invalid bearer token"),
        sse_ok("second account answered"),
    ])
    .await;
    let (store, state, ids) = setup(2, &mock.base);

    let (status, _, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("second account answered"));
    let seen = mock.seen();
    assert_eq!(seen.len(), 2, "401 之后要换号重发一次");
    assert_ne!(seen[0].token, seen[1].token, "第二发必须换了号");
    let first = ids[if seen[0].token == "tok-0" { 0 } else { 1 }];
    let second = ids[if seen[1].token == "tok-0" { 0 } else { 1 }];
    assert!(is_out_of_pool(&store, first), "吃到账号级 401 的号要停用");
    assert!(store.get(first).unwrap().unwrap().ban_reason.is_some());
    assert!(!is_out_of_pool(&store, second));
}

/// 403 账号级错误（账号被停用）：同 401，停用并换号。
#[tokio::test]
async fn an_account_level_403_disables_the_credential_and_swaps() {
    let mock = MockUpstream::start(vec![
        Reply::json_error(403, "permission_error", "This account has been disabled"),
        sse_ok("second account answered"),
    ])
    .await;
    let (store, state, ids) = setup(2, &mock.base);

    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::OK);
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].token, seen[1].token);
    let first = ids[if seen[0].token == "tok-0" { 0 } else { 1 }];
    assert!(is_out_of_pool(&store, first));
}

/// 与账号无关的 403（请求本身没权限）：原样交回，不换号、不停用，流水照记。
#[tokio::test]
async fn a_request_level_403_is_passed_through_without_swapping() {
    let mock = MockUpstream::start(vec![Reply::json_error(
        403,
        "permission_error",
        "You do not have permission to use this feature",
    )])
    .await;
    let (store, state, ids) = setup(2, &mock.base);

    let (status, _, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("You do not have permission"));
    assert_eq!(mock.seen().len(), 1, "与账号无关，换号没有意义");
    assert!(ids.iter().all(|&id| !is_out_of_pool(&store, id)));
    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 403);
}

fn in_secs(secs: i64) -> String {
    (crate::credentials::now_secs() as i64 + secs).to_string()
}

/// 429 且 5h 窗口打满：这个号的额度真没了 → 暂停调度（到点自动恢复），换号重发。
#[tokio::test]
async fn an_exhausted_account_429_parks_the_credential_and_swaps() {
    let mock = MockUpstream::start(vec![
        Reply::json_error(429, "rate_limit_error", "rate limited").with_headers(&[
            ("anthropic-ratelimit-unified-status", "rejected".into()),
            ("anthropic-ratelimit-unified-5h-status", "rejected".into()),
            ("anthropic-ratelimit-unified-5h-utilization", "1.0".into()),
            ("anthropic-ratelimit-unified-5h-reset", in_secs(2 * 3600)),
        ]),
        sse_ok("second account answered"),
    ])
    .await;
    let (store, state, ids) = setup(2, &mock.base);

    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::OK);
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].token, seen[1].token);
    let first = store.get(ids[if seen[0].token == "tok-0" { 0 } else { 1 }]).unwrap().unwrap();
    assert!(first.disabled && first.resume_at.is_some(), "额度耗尽是暂停，到点自动恢复");
}

/// 一个限流头都没带的 429：不是额度问题，原样交回、不打冷却、不换号。
#[tokio::test]
async fn a_bare_429_is_passed_through_untouched() {
    let mock = MockUpstream::start(vec![Reply::json_error(429, "rate_limit_error", "Error")]).await;
    let (store, state, ids) = setup(2, &mock.base);

    let (status, headers, _) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.get(header::RETRY_AFTER).is_none(), "裸 429 不改写 retry-after");
    assert_eq!(mock.seen().len(), 1);
    assert!(ids.iter().all(|&id| !is_out_of_pool(&store, id)));
}

/// 带限流头、但没有一个窗口满的 429（容量 / 速率限制）：不换号，交回时 `retry-after` 换成
/// 我们算的退避。
#[tokio::test]
async fn a_transient_429_is_not_swapped_and_gets_a_backoff() {
    let mock = MockUpstream::start(vec![
        Reply::json_error(
            429,
            "rate_limit_error",
            "Number of requests has exceeded your rate limit",
        )
        .with_headers(&[
            ("anthropic-ratelimit-unified-status", "allowed".into()),
            ("anthropic-ratelimit-unified-5h-status", "allowed".into()),
            ("anthropic-ratelimit-unified-5h-utilization", "0.2".into()),
            ("anthropic-ratelimit-unified-5h-reset", in_secs(3 * 3600)),
        ]),
    ])
    .await;
    let (store, state, ids) = setup(2, &mock.base);

    let (status, headers, _) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(mock.seen().len(), 1, "不是这个号的问题，换号只会撞同一堵墙");
    let retry: u64 = headers.get(header::RETRY_AFTER).unwrap().to_str().unwrap().parse().unwrap();
    assert!(retry >= 1);
    assert!(ids.iter().all(|&id| !is_out_of_pool(&store, id)), "瞬时限流不停号");
}

/// 400 形态错误：原样交回并学成规则；同一形态再来一条，本地直接拒，不再打上游。
#[tokio::test]
async fn a_shape_400_is_learned_and_the_next_one_is_rejected_locally() {
    const EFFORT_400: &str = "This model does not support effort level 'xhigh'. \
                              Supported levels: high, low, max, medium.";
    let mock =
        MockUpstream::start(vec![Reply::json_error(400, "invalid_request_error", EFFORT_400)])
            .await;
    let (store, state, _) = setup(1, &mock.base);
    let req = || cc_request(serde_json::json!({"output_config": {"effort": "xhigh"}}));

    let (status, _, body) = send(&state, req()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("effort level 'xhigh'"));
    assert_eq!(mock.seen().len(), 1);
    assert!(
        store.learned_rejections().unwrap().iter().any(|r| r.field == "effort"),
        "学到的规则要落库"
    );

    let (status, _, body) = send(&state, req()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("effort level 'xhigh'"), "回的是上游原话");
    assert_eq!(mock.seen().len(), 1, "学到之后不再打上游");
}

/// 连不上上游：502，流水照记（标 `connection_error`）。
#[tokio::test]
async fn an_unreachable_upstream_is_a_502_and_logged() {
    // 先占一个端口再放掉，拿到一个此刻没人在听的地址。
    let addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let (store, state, ids) = setup(1, &format!("http://{addr}"));

    let (status, _, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8_lossy(&body).contains("upstream request failed"));
    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 502);
    assert_eq!(rows[0].cred_id, Some(ids[0]));
    assert!(!is_out_of_pool(&store, ids[0]), "连不上不是账号的问题");
}

/// 只有一个号时吃到账号级 401：停用它，没有号可换，401 原样交回。
#[tokio::test]
async fn an_account_level_401_with_nothing_to_swap_to_is_passed_through() {
    let mock = MockUpstream::start(vec![Reply::json_error(
        401,
        "authentication_error",
        "invalid bearer token",
    )])
    .await;
    let (store, state, ids) = setup(1, &mock.base);

    let (status, _, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(String::from_utf8_lossy(&body).contains("invalid bearer token"));
    assert_eq!(mock.seen().len(), 1);
    assert!(is_out_of_pool(&store, ids[0]));
    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 401);
}

/// 429 只说「组织关了超额」、一个额度窗口都没报，打的又是高档模型、号不是 Max：这个号的套餐
/// 不含这个模型 → 记下准入、换号重发（不受 429 换号开关约束）。
#[tokio::test]
async fn a_plan_denial_429_remembers_the_model_and_swaps() {
    let mock = MockUpstream::start(vec![
        Reply::json_error(429, "rate_limit_error", "rate limited").with_headers(&[
            ("anthropic-ratelimit-unified-status", "rejected".into()),
            ("anthropic-ratelimit-unified-overage-disabled-reason", "org_level_disabled".into()),
            ("anthropic-ratelimit-unified-reset", in_secs(30 * 24 * 3600)),
        ]),
        sse_ok("second account answered"),
    ])
    .await;
    let (store, state, ids) = setup_tier(2, &mock.base, "Pro");

    let (status, _, _) =
        send(&state, cc_request(serde_json::json!({"model": "claude-fable-5"}))).await;

    assert_eq!(status, StatusCode::OK);
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].token, seen[1].token);
    let first = ids[if seen[0].token == "tok-0" { 0 } else { 1 }];
    assert!(
        store.denied_models(first).unwrap().iter().any(|d| d.model.contains("fable")),
        "套餐不含这个模型要记下来"
    );
    assert!(!is_out_of_pool(&store, first), "不含某个模型不是停号的理由");
}

/// 429 换号开关关掉：账号级 401 不在循环里换号，交给后面的 4xx 段停用并原样透传。
#[tokio::test]
async fn with_retry_off_an_account_level_401_is_banned_but_not_swapped() {
    let mock = MockUpstream::start(vec![Reply::json_error(
        401,
        "authentication_error",
        "invalid bearer token",
    )])
    .await;
    let (store, state, ids) = setup(2, &mock.base);
    store.set_setting(store::RATE_LIMIT_RETRY, "0").unwrap();

    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let first = ids[if seen[0].token == "tok-0" { 0 } else { 1 }];
    assert!(is_out_of_pool(&store, first), "不换号，但账号级错误照样停用");
}

/// 裸 401（网关 / CDN 拦的，没有 `error.type`）：不是账号的问题，不停用、不换号，原样交回。
#[tokio::test]
async fn a_bare_401_is_passed_through_without_swapping() {
    let mock = MockUpstream::start(vec![Reply {
        status: 401,
        headers: vec![("content-type".into(), "text/plain".into())],
        body: b"Unauthorized".to_vec(),
    }])
    .await;
    let (store, state, ids) = setup(2, &mock.base);

    let (status, _, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(&body[..], b"Unauthorized");
    assert_eq!(mock.seen().len(), 1);
    assert!(ids.iter().all(|&id| !is_out_of_pool(&store, id)));
}

// ---------- 入口闸门：都在选号之前本地作答，一发上游都不该有 ----------

/// 本地拒绝：状态码、错误类型、正文片段，且上游一发都没收到。
async fn assert_local_reject(
    mock: &MockUpstream,
    state: &AppState,
    req: (HeaderMap, Bytes),
    want: StatusCode,
    etype: &str,
    needle: &str,
) {
    let (status, _, body) = send(state, req).await;
    assert_eq!(status, want, "{}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["type"], etype);
    assert!(v["error"]["message"].as_str().unwrap().contains(needle), "{v}");
    assert!(mock.seen().is_empty(), "本地拒绝不该打上游");
}

#[tokio::test]
async fn gate_invalid_api_key() {
    let mock = MockUpstream::start(vec![]).await;
    let (store, state, _) = setup(1, &mock.base);
    store.set_setting(store::CLIENT_API_KEY, "the-right-key").unwrap();
    let req = cc_request(serde_json::json!({}));
    assert_local_reject(
        &mock,
        &state,
        req,
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "invalid API key",
    )
    .await;
}

#[tokio::test]
async fn gate_client_version_below_minimum() {
    let mock = MockUpstream::start(vec![]).await;
    let (store, state, _) = setup(1, &mock.base);
    store.set_setting(store::MIN_CLIENT_VERSION, "9.0.0").unwrap();
    let req = cc_request(serde_json::json!({}));
    assert_local_reject(
        &mock,
        &state,
        req,
        StatusCode::FORBIDDEN,
        "permission_error",
        "upgrade to 9.0.0",
    )
    .await;
}

#[tokio::test]
async fn gate_missing_device_identity() {
    let mock = MockUpstream::start(vec![]).await;
    let (_, state, _) = setup(1, &mock.base);
    let (headers, body) = cc_request(serde_json::json!({}));
    let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    v.as_object_mut().unwrap().remove("metadata");
    let req = (headers, Bytes::from(serde_json::to_vec(&v).unwrap()));
    assert_local_reject(
        &mock,
        &state,
        req,
        StatusCode::FORBIDDEN,
        "permission_error",
        "device identity",
    )
    .await;
}

#[tokio::test]
async fn gate_session_id_mismatch() {
    let mock = MockUpstream::start(vec![]).await;
    let (_, state, _) = setup(1, &mock.base);
    let (mut headers, body) = cc_request(serde_json::json!({}));
    headers.insert(
        "x-claude-code-session-id",
        HeaderValue::from_static("9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d"),
    );
    assert_local_reject(
        &mock,
        &state,
        (headers, body),
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "session id mismatch",
    )
    .await;
}

#[tokio::test]
async fn gate_openai_residue() {
    let mock = MockUpstream::start(vec![]).await;
    let (_, state, _) = setup(1, &mock.base);
    let req = cc_request(serde_json::json!({"messages": [{"role": "user", "content": [
        {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}]}]}));
    let (status, _, _) = send(&state, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(mock.seen().is_empty());
}

/// `reject_learned_shapes` 关掉：上游的形态 400 不学；表里已有的规则也不拦，照常送上游。
#[tokio::test]
async fn learned_shapes_off_neither_learns_nor_blocks() {
    const EFFORT_400: &str = "This model does not support effort level 'xhigh'. \
                              Supported levels: high, low, max, medium.";
    let mock = MockUpstream::start(vec![
        Reply::json_error(400, "invalid_request_error", EFFORT_400),
        Reply::json_error(400, "invalid_request_error", EFFORT_400),
    ])
    .await;
    let (store, state, _) = setup(1, &mock.base);
    store.set_setting(store::REJECT_LEARNED_SHAPES, "false").unwrap();
    let req = || cc_request(serde_json::json!({"output_config": {"effort": "xhigh"}}));

    let (status, _, _) = send(&state, req()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(state.shape_rejections.read().is_empty(), "关着不该学");

    // 开着时学到的规则，关掉后不再拦。
    let body: serde_json::Value = serde_json::from_slice(&req().1).unwrap();
    crate::proxy::remember_shape_rejection(&state.shape_rejections, Some("claude-sonnet-5"), Some(&body), Some(&body), format!(r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"{EFFORT_400}"}}}}"#)
            .as_bytes());
    assert!(!state.shape_rejections.read().is_empty());
    let (status, _, _) = send(&state, req()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.seen().len(), 2, "关着不该本地拦");
}

/// 采样参数被废弃的 400 原样回给客户端，并学成规则；同样的请求再来时本地回同一句原话，
/// 不再发往上游。
#[tokio::test]
async fn deprecated_sampling_400_passes_through_then_is_rejected_locally() {
    const TEMP_400: &str = "`temperature` is deprecated for this model.";
    let mock =
        MockUpstream::start(vec![Reply::json_error(400, "invalid_request_error", TEMP_400)]).await;
    let (store, state, _) = setup(1, &mock.base);
    // 不注入 thinking：temperature 原样出站，上游才会点它的名。
    store.set_setting(store::INJECT_THINKING, "0").unwrap();
    let req = || cc_request(serde_json::json!({"temperature": 0.7}));

    let (status, _, body) = send(&state, req()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains(TEMP_400));

    let (status, _, body) = send(&state, req()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains(TEMP_400), "本地回上游原话");
    assert_eq!(mock.seen().len(), 1, "第二条不该再发往上游");
}

/// 规则按**出站体**判：已学到「该模型不收 temperature」，但这条请求走模拟路径、注入 thinking
/// 时会剥掉冲突的 `temperature: 0.7` 与 `top_p: 0.5`，出站里没有它们，不该在本地拦下。
#[tokio::test]
async fn sampling_rule_does_not_block_when_the_outbound_body_drops_the_param() {
    const TEMP_400: &str = "`temperature` is deprecated for this model.";
    const TOP_P_400: &str = "`top_p` is deprecated for this model.";
    let mock = MockUpstream::start(vec![sse_ok("ok")]).await;
    let (_, state, _) = setup(1, &mock.base);
    let learned_from = serde_json::json!({"model": "claude-sonnet-5", "temperature": 0.7, "top_p": 0.5,
        "messages": [{"role": "user", "content": "hi"}]});
    for msg in [TEMP_400, TOP_P_400] {
        crate::proxy::remember_shape_rejection(
            &state.shape_rejections,
            Some("claude-sonnet-5"),
            Some(&learned_from),
            Some(&learned_from),
            format!(
                r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"{msg}"}}}}"#
            )
            .as_bytes(),
        );
    }
    assert_eq!(state.shape_rejections.read().len(), 2);
    let (status, _, _) =
        send(&state, cc_request(serde_json::json!({"temperature": 0.7, "top_p": 0.5}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.seen().len(), 1, "出站不带这两个参数，照常发往上游");
}

/// prefill 不支持的 400 同理：原样回给客户端并学成规则，第二条本地拒。
#[tokio::test]
async fn unsupported_prefill_400_passes_through_then_is_rejected_locally() {
    const PREFILL_400: &str = "This model does not support assistant message prefill. \
                               The conversation must end with a user message.";
    let mock =
        MockUpstream::start(vec![Reply::json_error(400, "invalid_request_error", PREFILL_400)])
            .await;
    let (_, state, _) = setup(1, &mock.base);
    let req = || {
        cc_request(serde_json::json!({"messages": [
            {"role": "user", "content": "write a haiku about the sea"},
            {"role": "assistant", "content": "Waves"}]}))
    };
    let (status, _, _) = send(&state, req()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, body) = send(&state, req()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("does not support assistant message prefill"));
    assert_eq!(mock.seen().len(), 1, "第二条不该再发往上游");
}

/// 零输出拦截开关决定学不学：开着学到这一类，关着什么也不记。
#[tokio::test]
async fn empty_reply_learning_follows_its_switch() {
    let empty_sse = || {
        let events = [
            serde_json::json!({"type": "message_start", "message": {
                "id": "msg_mock", "type": "message", "role": "assistant",
                "model": "claude-sonnet-5", "content": [], "stop_reason": null,
                "usage": {"input_tokens": 12, "output_tokens": 0}}}),
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 0}}),
            serde_json::json!({"type": "message_stop"}),
        ];
        let body: String = events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect();
        Reply {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: body.into_bytes(),
        }
    };
    for on in [false, true] {
        let mock = MockUpstream::start(vec![empty_sse()]).await;
        let (store, state, _) = setup(1, &mock.base);
        store.set_setting(store::REJECT_EMPTY_REPLIES, if on { "true" } else { "false" }).unwrap();
        let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;
        assert_eq!(status, StatusCode::OK);
        usage_logs(&store, 1).await;
        let learned = !state.empty_replies.read().classes.is_empty();
        assert_eq!(learned, on, "reject_empty_replies={on}");
    }
}

/// 429 开关关掉：「套餐不含该模型」的 429 原样透传，不记准入、不换号。
#[tokio::test]
async fn plan_denial_with_rate_limit_retry_off_passes_through() {
    let mock = MockUpstream::start(vec![
        Reply::json_error(429, "rate_limit_error", "Error").with_headers(&[
            ("anthropic-ratelimit-unified-overage-disabled-reason", "org_level_disabled".into()),
            ("anthropic-ratelimit-unified-reset", in_secs(30 * 24 * 3600)),
        ]),
        sse_ok("should not be reached"),
    ])
    .await;
    let (store, state, ids) = setup_tier(2, &mock.base, "pro");
    store.set_setting(store::RATE_LIMIT_RETRY, "false").unwrap();

    let (status, _, _) =
        send(&state, cc_request(serde_json::json!({"model": "claude-fable-5"}))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(mock.seen().len(), 1, "关着不该换号");
    for id in ids {
        assert!(store.denied_models(id).unwrap().is_empty(), "关着不该记准入");
    }
}

/// 会话 RPM 上限 1：第一条放行，第二条本地 429 + `retry-after`，上游只收到一发。
#[tokio::test]
async fn gate_session_rpm_limit() {
    let mock = MockUpstream::start(vec![sse_ok("first"), sse_ok("second")]).await;
    let (store, state, _) = setup(1, &mock.base);
    store.set_setting(store::SESSION_RPM_LIMIT, "1").unwrap();

    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.get(header::RETRY_AFTER).is_some());
    assert_eq!(mock.seen().len(), 1);
}

/// 设备 RPM 上限 1：同上，按设备算。
#[tokio::test]
async fn gate_device_rpm_limit() {
    let mock = MockUpstream::start(vec![sse_ok("first"), sse_ok("second")]).await;
    let (store, state, _) = setup(1, &mock.base);
    store.set_setting(store::DEVICE_RPM_LIMIT, "1").unwrap();

    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.get(header::RETRY_AFTER).is_some());
    assert_eq!(mock.seen().len(), 1);
}

/// 没有可用的号：503，上游一发都没有。
#[tokio::test]
async fn no_credential_is_a_503() {
    let mock = MockUpstream::start(vec![]).await;
    let (_, state, _) = setup(0, &mock.base);
    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(mock.seen().is_empty());
}
