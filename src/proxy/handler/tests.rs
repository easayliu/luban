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
    headers: HeaderMap,
    body: Bytes,
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
        let app = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let (script, seen) = (script_h.clone(), seen_h.clone());
            async move {
                let token = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .unwrap_or_default()
                    .to_string();
                seen.lock().push(Seen { path: uri.to_string(), token, headers, body });
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
async fn setup(
    pool: sqlx::PgPool,
    n: usize,
    base: &str,
) -> (Arc<CredentialStore>, AppState, Vec<i64>) {
    setup_tier(pool.clone(), n, base, "max").await
}

async fn setup_tier(
    pool: sqlx::PgPool,
    n: usize,
    base: &str,
    tier: &str,
) -> (Arc<CredentialStore>, AppState, Vec<i64>) {
    let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
    // 新会话的启动握手会打真上游，测试里关掉；探针拦截与这里要测的分支无关，也关掉。
    store.set_setting(store::API_TELEMETRY, "0").await.unwrap();
    store.set_setting(store::REJECT_PROBES, "0").await.unwrap();
    store.set_setting(store::PROBE_AS_PREWARM, "0").await.unwrap();
    let far_future = crate::credentials::now_secs() + 30 * 24 * 3600;
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let cred = store
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
            .await
            .unwrap();
        ids.push(cred.id);
    }
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
    send_to(state, "/v1/messages", req).await
}

async fn send_to(
    state: &AppState,
    path: &str,
    req: (HeaderMap, Bytes),
) -> (StatusCode, HeaderMap, Bytes) {
    send_body_to(state, path, req.0, axum::body::Body::from(req.1)).await
}

async fn send_body(
    state: &AppState,
    headers: HeaderMap,
    body: axum::body::Body,
) -> (StatusCode, HeaderMap, Bytes) {
    send_body_to(state, "/v1/messages", headers, body).await
}

async fn send_body_to(
    state: &AppState,
    path: &str,
    mut headers: HeaderMap,
    body: axum::body::Body,
) -> (StatusCode, HeaderMap, Bytes) {
    // 用例自己带了 Key（换 Key、错 Key）就用它的，否则带上测试状态的那把。
    if !headers.contains_key("x-api-key") {
        headers.insert("x-api-key", HeaderValue::from_static(AppState::TEST_CLIENT_KEY));
    }
    let resp = crate::proxy::handle(
        axum::extract::State(state.clone()),
        axum::http::Method::POST,
        path.parse::<Uri>().unwrap(),
        headers,
        body,
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
        rows = store.list_usage_logs(50).await.unwrap();
        if rows.len() >= n {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    rows
}

async fn is_out_of_pool(store: &CredentialStore, id: i64) -> bool {
    store.get(id).await.unwrap().unwrap().disabled
}

/// `ids` 里的号一个都没停。
async fn none_out_of_pool(store: &CredentialStore, ids: &[i64]) -> bool {
    for &id in ids {
        if is_out_of_pool(store, id).await {
            return false;
        }
    }
    true
}

/// 一发成功的流式回复原样转回客户端，上游收到的是号的 token，流水记到这个号上、带用量。
#[sqlx::test]
async fn a_successful_stream_is_relayed_and_logged(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![sse_ok("waves fold into foam")]).await;
    let (store, state, ids) = setup(pool.clone(), 1, &mock.base).await;

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
#[sqlx::test]
async fn an_account_level_401_disables_the_credential_and_swaps(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        Reply::json_error(401, "authentication_error", "invalid bearer token"),
        sse_ok("second account answered"),
    ])
    .await;
    let (store, state, ids) = setup(pool.clone(), 2, &mock.base).await;

    let (status, _, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("second account answered"));
    let seen = mock.seen();
    assert_eq!(seen.len(), 2, "401 之后要换号重发一次");
    assert_ne!(seen[0].token, seen[1].token, "第二发必须换了号");
    let first = ids[if seen[0].token == "tok-0" { 0 } else { 1 }];
    let second = ids[if seen[1].token == "tok-0" { 0 } else { 1 }];
    assert!(is_out_of_pool(&store, first).await, "吃到账号级 401 的号要停用");
    assert!(store.get(first).await.unwrap().unwrap().ban_reason.is_some());
    assert!(!is_out_of_pool(&store, second).await);
}

/// 403 账号级错误（账号被停用）：同 401，停用并换号。
#[sqlx::test]
async fn an_account_level_403_disables_the_credential_and_swaps(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        Reply::json_error(403, "permission_error", "This account has been disabled"),
        sse_ok("second account answered"),
    ])
    .await;
    let (store, state, ids) = setup(pool.clone(), 2, &mock.base).await;

    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::OK);
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].token, seen[1].token);
    let first = ids[if seen[0].token == "tok-0" { 0 } else { 1 }];
    assert!(is_out_of_pool(&store, first).await);
}

/// 与账号无关的 403（请求本身没权限）：原样交回，不换号、不停用，流水照记。
#[sqlx::test]
async fn a_request_level_403_is_passed_through_without_swapping(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![Reply::json_error(
        403,
        "permission_error",
        "You do not have permission to use this feature",
    )])
    .await;
    let (store, state, ids) = setup(pool.clone(), 2, &mock.base).await;

    let (status, _, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("You do not have permission"));
    assert_eq!(mock.seen().len(), 1, "与账号无关，换号没有意义");
    assert!(none_out_of_pool(&store, &ids).await);
    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 403);
}

fn in_secs(secs: i64) -> String {
    (crate::credentials::now_secs() as i64 + secs).to_string()
}

/// 429 且 5h 窗口打满：这个号的额度真没了 → 暂停调度（到点自动恢复），换号重发。
#[sqlx::test]
async fn an_exhausted_account_429_parks_the_credential_and_swaps(pool: sqlx::PgPool) {
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
    let (store, state, ids) = setup(pool.clone(), 2, &mock.base).await;

    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::OK);
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].token, seen[1].token);
    let first =
        store.get(ids[if seen[0].token == "tok-0" { 0 } else { 1 }]).await.unwrap().unwrap();
    assert!(first.disabled && first.resume_at.is_some(), "额度耗尽是暂停，到点自动恢复");
}

/// 一个限流头都没带的 429：不是额度问题，原样交回、不打冷却、不换号。
#[sqlx::test]
async fn a_bare_429_is_passed_through_untouched(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![Reply::json_error(429, "rate_limit_error", "Error")]).await;
    let (store, state, ids) = setup(pool.clone(), 2, &mock.base).await;

    let (status, headers, _) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.get(header::RETRY_AFTER).is_none(), "裸 429 不改写 retry-after");
    assert_eq!(mock.seen().len(), 1);
    assert!(none_out_of_pool(&store, &ids).await);
}

/// 带限流头、但没有一个窗口满的 429（容量 / 速率限制）：不换号，交回时 `retry-after` 换成
/// 我们算的退避。
#[sqlx::test]
async fn a_transient_429_is_not_swapped_and_gets_a_backoff(pool: sqlx::PgPool) {
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
    let (store, state, ids) = setup(pool.clone(), 2, &mock.base).await;

    let (status, headers, _) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(mock.seen().len(), 1, "不是这个号的问题，换号只会撞同一堵墙");
    let retry: u64 = headers.get(header::RETRY_AFTER).unwrap().to_str().unwrap().parse().unwrap();
    assert!(retry >= 1);
    assert!(none_out_of_pool(&store, &ids).await, "瞬时限流不停号");
}

/// 400 形态错误：原样交回并学成规则；同一形态再来一条，本地直接拒，不再打上游。
#[sqlx::test]
async fn a_shape_400_is_learned_and_the_next_one_is_rejected_locally(pool: sqlx::PgPool) {
    const EFFORT_400: &str = "This model does not support effort level 'xhigh'. \
                              Supported levels: high, low, max, medium.";
    let mock =
        MockUpstream::start(vec![Reply::json_error(400, "invalid_request_error", EFFORT_400)])
            .await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    let req = || cc_request(serde_json::json!({"output_config": {"effort": "xhigh"}}));

    let (status, _, body) = send(&state, req()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("effort level 'xhigh'"));
    assert_eq!(mock.seen().len(), 1);
    assert!(
        store.learned_rejections().await.unwrap().iter().any(|r| r.field == "effort"),
        "学到的规则要落库"
    );

    let (status, _, body) = send(&state, req()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("effort level 'xhigh'"), "回的是上游原话");
    assert_eq!(mock.seen().len(), 1, "学到之后不再打上游");
}

/// 连不上上游：502，流水照记（标 `connection_error`）。
#[sqlx::test]
async fn an_unreachable_upstream_is_a_502_and_logged(pool: sqlx::PgPool) {
    // 先占一个端口再放掉，拿到一个此刻没人在听的地址。
    let addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let (store, state, ids) = setup(pool.clone(), 1, &format!("http://{addr}")).await;

    let (status, _, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8_lossy(&body).contains("upstream request failed"));
    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 502);
    assert_eq!(rows[0].cred_id, Some(ids[0]));
    assert!(!is_out_of_pool(&store, ids[0]).await, "连不上不是账号的问题");
}

/// 只有一个号时吃到账号级 401：停用它，没有号可换，401 原样交回。
#[sqlx::test]
async fn an_account_level_401_with_nothing_to_swap_to_is_passed_through(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![Reply::json_error(
        401,
        "authentication_error",
        "invalid bearer token",
    )])
    .await;
    let (store, state, ids) = setup(pool.clone(), 1, &mock.base).await;

    let (status, _, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(String::from_utf8_lossy(&body).contains("invalid bearer token"));
    assert_eq!(mock.seen().len(), 1);
    assert!(is_out_of_pool(&store, ids[0]).await);
    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 401);
}

/// 429 只说「组织关了超额」、一个额度窗口都没报，打的又是高档模型、号不是 Max：这个号的套餐
/// 不含这个模型 → 记下准入、换号重发（不受 429 换号开关约束）。
#[sqlx::test]
async fn a_plan_denial_429_remembers_the_model_and_swaps(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        Reply::json_error(429, "rate_limit_error", "rate limited").with_headers(&[
            ("anthropic-ratelimit-unified-status", "rejected".into()),
            ("anthropic-ratelimit-unified-overage-disabled-reason", "org_level_disabled".into()),
            ("anthropic-ratelimit-unified-reset", in_secs(30 * 24 * 3600)),
        ]),
        sse_ok("second account answered"),
    ])
    .await;
    let (store, state, ids) = setup_tier(pool.clone(), 2, &mock.base, "Pro").await;

    let (status, _, _) =
        send(&state, cc_request(serde_json::json!({"model": "claude-fable-5"}))).await;

    assert_eq!(status, StatusCode::OK);
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].token, seen[1].token);
    let first = ids[if seen[0].token == "tok-0" { 0 } else { 1 }];
    assert!(
        store.denied_models(first).await.unwrap().iter().any(|d| d.model.contains("fable")),
        "套餐不含这个模型要记下来"
    );
    assert!(!is_out_of_pool(&store, first).await, "不含某个模型不是停号的理由");
}

/// 429 换号开关关掉：账号级 401 不在循环里换号，交给后面的 4xx 段停用并原样透传。
#[sqlx::test]
async fn with_retry_off_an_account_level_401_is_banned_but_not_swapped(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![Reply::json_error(
        401,
        "authentication_error",
        "invalid bearer token",
    )])
    .await;
    let (store, state, ids) = setup(pool.clone(), 2, &mock.base).await;
    store.set_setting(store::RATE_LIMIT_RETRY, "0").await.unwrap();

    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let first = ids[if seen[0].token == "tok-0" { 0 } else { 1 }];
    assert!(is_out_of_pool(&store, first).await, "不换号，但账号级错误照样停用");
}

/// 裸 401（网关 / CDN 拦的，没有 `error.type`）：不是账号的问题，不停用、不换号，原样交回。
#[sqlx::test]
async fn a_bare_401_is_passed_through_without_swapping(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![Reply {
        status: 401,
        headers: vec![("content-type".into(), "text/plain".into())],
        body: b"Unauthorized".to_vec(),
    }])
    .await;
    let (store, state, ids) = setup(pool.clone(), 2, &mock.base).await;

    let (status, _, body) = send(&state, cc_request(serde_json::json!({}))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(&body[..], b"Unauthorized");
    assert_eq!(mock.seen().len(), 1);
    assert!(none_out_of_pool(&store, &ids).await);
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

#[sqlx::test]
async fn gate_invalid_api_key(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.create_api_key("k", "the-right-key", &[]).await.unwrap().unwrap();
    let mut req = cc_request(serde_json::json!({}));
    req.0.insert("x-api-key", HeaderValue::from_static("a-wrong-key"));
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

/// 环境变量与库里都没有任何接入 Key：不带 Key 的请求照样 401，不退回「没配就放行」。
#[sqlx::test]
async fn gate_rejects_when_no_key_is_configured(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![]).await;
    let (_store, mut state, _) = setup(pool.clone(), 1, &mock.base).await;
    state.client_key = None;
    let mut req = cc_request(serde_json::json!({}));
    req.0.insert("x-api-key", HeaderValue::from_static(""));
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

#[sqlx::test]
async fn gate_client_version_below_minimum(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::MIN_CLIENT_VERSION, "9.0.0").await.unwrap();
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

#[sqlx::test]
async fn gate_missing_device_identity(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![]).await;
    let (_, state, _) = setup(pool.clone(), 1, &mock.base).await;
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

#[sqlx::test]
async fn gate_session_id_mismatch(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![]).await;
    let (_, state, _) = setup(pool.clone(), 1, &mock.base).await;
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

#[sqlx::test]
async fn gate_openai_residue(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![]).await;
    let (_, state, _) = setup(pool.clone(), 1, &mock.base).await;
    let req = cc_request(serde_json::json!({"messages": [{"role": "user", "content": [
        {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}]}]}));
    let (status, _, _) = send(&state, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(mock.seen().is_empty());
}

/// `reject_learned_shapes` 关掉：上游的形态 400 不学；表里已有的规则也不拦，照常送上游。
#[sqlx::test]
async fn learned_shapes_off_neither_learns_nor_blocks(pool: sqlx::PgPool) {
    const EFFORT_400: &str = "This model does not support effort level 'xhigh'. \
                              Supported levels: high, low, max, medium.";
    let mock = MockUpstream::start(vec![
        Reply::json_error(400, "invalid_request_error", EFFORT_400),
        Reply::json_error(400, "invalid_request_error", EFFORT_400),
    ])
    .await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::REJECT_LEARNED_SHAPES, "false").await.unwrap();
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
#[sqlx::test]
async fn deprecated_sampling_400_passes_through_then_is_rejected_locally(pool: sqlx::PgPool) {
    const TEMP_400: &str = "`temperature` is deprecated for this model.";
    let mock =
        MockUpstream::start(vec![Reply::json_error(400, "invalid_request_error", TEMP_400)]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    // 不注入 thinking：temperature 原样出站，上游才会点它的名。
    store.set_setting(store::INJECT_THINKING, "0").await.unwrap();
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
#[sqlx::test]
async fn sampling_rule_does_not_block_when_the_outbound_body_drops_the_param(pool: sqlx::PgPool) {
    const TEMP_400: &str = "`temperature` is deprecated for this model.";
    const TOP_P_400: &str = "`top_p` is deprecated for this model.";
    let mock = MockUpstream::start(vec![sse_ok("ok")]).await;
    let (_, state, _) = setup(pool.clone(), 1, &mock.base).await;
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
#[sqlx::test]
async fn unsupported_prefill_400_passes_through_then_is_rejected_locally(pool: sqlx::PgPool) {
    const PREFILL_400: &str = "This model does not support assistant message prefill. \
                               The conversation must end with a user message.";
    let mock =
        MockUpstream::start(vec![Reply::json_error(400, "invalid_request_error", PREFILL_400)])
            .await;
    let (_, state, _) = setup(pool.clone(), 1, &mock.base).await;
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
#[sqlx::test]
async fn empty_reply_learning_follows_its_switch(pool: sqlx::PgPool) {
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
        let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
        store
            .set_setting(store::REJECT_EMPTY_REPLIES, if on { "true" } else { "false" })
            .await
            .unwrap();
        let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;
        assert_eq!(status, StatusCode::OK);
        usage_logs(&store, 1).await;
        let learned = !state.empty_replies.read().classes.is_empty();
        assert_eq!(learned, on, "reject_empty_replies={on}");
        // 两轮共用同一个测试库：清掉这一轮的号与流水，下一轮从空库开始。
        store.drain_writes(std::time::Duration::from_secs(3)).await;
        store.clear().await.unwrap();
    }
}

/// 429 开关关掉：「套餐不含该模型」的 429 原样透传，不记准入、不换号。
#[sqlx::test]
async fn plan_denial_with_rate_limit_retry_off_passes_through(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        Reply::json_error(429, "rate_limit_error", "Error").with_headers(&[
            ("anthropic-ratelimit-unified-overage-disabled-reason", "org_level_disabled".into()),
            ("anthropic-ratelimit-unified-reset", in_secs(30 * 24 * 3600)),
        ]),
        sse_ok("should not be reached"),
    ])
    .await;
    let (store, state, ids) = setup_tier(pool.clone(), 2, &mock.base, "pro").await;
    store.set_setting(store::RATE_LIMIT_RETRY, "false").await.unwrap();

    let (status, _, _) =
        send(&state, cc_request(serde_json::json!({"model": "claude-fable-5"}))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(mock.seen().len(), 1, "关着不该换号");
    for id in ids {
        assert!(store.denied_models(id).await.unwrap().is_empty(), "关着不该记准入");
    }
}

/// 会话 RPM 上限 1：第一条放行，第二条本地 429 + `retry-after`，上游只收到一发。
#[sqlx::test]
async fn gate_session_rpm_limit(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![sse_ok("first"), sse_ok("second")]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::SESSION_RPM_LIMIT, "1").await.unwrap();

    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.get(header::RETRY_AFTER).is_some());
    assert_eq!(mock.seen().len(), 1);
}

/// 设备 RPM 上限 1：同上，按设备算。
#[sqlx::test]
async fn gate_device_rpm_limit(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![sse_ok("first"), sse_ok("second")]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::DEVICE_RPM_LIMIT, "1").await.unwrap();

    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.get(header::RETRY_AFTER).is_some());
    assert_eq!(mock.seen().len(), 1);
}

/// 没有可用的号：503，上游一发都没有。
#[sqlx::test]
async fn no_credential_is_a_503(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![]).await;
    let (_, state, _) = setup(pool.clone(), 0, &mock.base).await;
    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(mock.seen().is_empty());
}

/// 接入 Key 绑定了分组：转发只落在这些分组的号上（上游收到的是组里那个号的 token）；
/// 分组里没有号时本地拒掉，一发上游都不打。
#[sqlx::test]
async fn api_key_groups_route_to_their_accounts(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![sse_ok("ok")]).await;
    let (store, state, ids) = setup(pool.clone(), 2, &mock.base).await;
    let vip = store.create_group("vip", "").await.unwrap().unwrap();
    let empty = store.create_group("empty", "").await.unwrap().unwrap();
    store.set_credential_groups(&[ids[1]], &[vip]).await.unwrap().unwrap();
    store.create_api_key("vip", "key-vip", &[vip]).await.unwrap().unwrap();
    store.create_api_key("empty", "key-empty", &[empty]).await.unwrap().unwrap();

    let mut req = cc_request(serde_json::json!({}));
    req.0.insert("x-api-key", HeaderValue::from_static("key-vip"));
    let (status, _, body) = send(&state, req).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(mock.seen().iter().map(|s| s.token.as_str()).collect::<Vec<_>>(), vec!["tok-1"]);

    let mut req = cc_request(serde_json::json!({}));
    req.0.insert("x-api-key", HeaderValue::from_static("key-empty"));
    let (status, _, body) = send(&state, req).await;
    assert_ne!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(mock.seen().len(), 1, "分组里没有号，不该再打上游");
}

/// 唯一一把接入 Key 删掉之后，不带 Key、带旧 Key 都被拒（不退回「没配 Key 就放行」）。
#[sqlx::test]
async fn deleting_the_last_key_does_not_open_the_proxy(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![]).await;
    let (store, mut state, _) = setup(pool.clone(), 1, &mock.base).await;
    state.client_key = None;
    let id = store.create_api_key("k", "old-key", &[]).await.unwrap().unwrap();
    store.delete_api_key(id).await.unwrap();
    // 空串即「不带 Key」（`presented_key` 滤掉空值），也挡住 `send_to` 补测试 Key。
    for key in ["", "old-key"] {
        let mut req = cc_request(serde_json::json!({}));
        req.0.insert("x-api-key", HeaderValue::from_static(key));
        let (status, _, _) = send(&state, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert!(mock.seen().is_empty());
}

/// 第三方客户端（非 CC 的 UA、不带官方提示词）的一条请求，`metadata` 由调用方给。
fn third_party_request(metadata: Option<serde_json::Value>) -> (HeaderMap, Bytes) {
    let mut body = serde_json::json!({
        "model": "claude-sonnet-5",
        "max_tokens": 1024,
        "stream": true,
        "system": "You are a terse assistant.",
        "messages": [{"role": "user", "content": "write a haiku about the sea"}],
    });
    if let Some(m) = metadata {
        body["metadata"] = m;
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::USER_AGENT, HeaderValue::from_static("curl/8.7.1"));
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    (headers, Bytes::from(serde_json::to_vec(&body).unwrap()))
}

/// billing-only 下流水的伪装设备按**出站体**记：补了 `user_id` 的记 `sim:` 派生设备；客户端自带
/// 非 CC 格式的 `user_id`（补那步不覆盖，出站体里没有派生设备）、非计费的 `count_tokens`（原样
/// 透传）都不记——否则上游没见过的设备会进流水与设备费用。
#[sqlx::test]
async fn billing_only_logs_the_simulated_device_only_when_it_was_sent(pool: sqlx::PgPool) {
    let count_ok = Reply {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: br#"{"input_tokens":12}"#.to_vec(),
    };
    let mock = MockUpstream::start(vec![sse_ok("a"), sse_ok("b"), count_ok]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::SIM_BILLING_ONLY, "1").await.unwrap();
    // 放行不带设备身份的裸客户端（默认拒）：这里要测的正是它们。
    store.set_setting(store::REQUIRE_DEVICE_ID, "0").await.unwrap();

    let (status, _, body) = send(&state, third_party_request(None)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let rows = usage_logs(&store, 1).await;
    let dev = rows[0].device_id.as_deref().unwrap_or_default();
    assert!(dev.starts_with("sim:"), "补了 user_id，记派生设备: {dev:?}");

    let plain = serde_json::json!({"user_id": "client-uid-123"});
    let (status, _, _) = send(&state, third_party_request(Some(plain))).await;
    assert_eq!(status, StatusCode::OK);
    let rows = usage_logs(&store, 2).await;
    let row = rows.iter().max_by_key(|r| r.id).unwrap();
    assert_eq!(row.device_id, None, "出站是客户端原样的 user_id，没有派生设备");

    let (status, _, body) =
        send_to(&state, "/v1/messages/count_tokens", third_party_request(None)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(mock.seen().len(), 3);
    let rows = usage_logs(&store, 3).await;
    assert_eq!(rows.len(), 3);
    let row = rows.iter().max_by_key(|r| r.id).unwrap();
    assert!(row.path.contains("count_tokens"), "{}", row.path);
    assert_eq!(row.device_id, None, "count_tokens 原样透传，不记派生设备");
}

/// 不带 Key 的请求在读体之前就被拒：体是一条发完第一块就挂住的流，读它的话这条永远回不来。
#[sqlx::test]
async fn unauthenticated_requests_are_rejected_before_the_body_is_read(pool: sqlx::PgPool) {
    use futures_util::StreamExt;
    let mock = MockUpstream::start(vec![]).await;
    let (_store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    let first = futures_util::stream::once(async {
        Ok::<_, std::io::Error>(Bytes::from_static(b"{\"model\":"))
    });
    let body = axum::body::Body::from_stream(first.chain(futures_util::stream::pending()));
    let mut headers = cc_request(serde_json::json!({})).0;
    headers.insert("x-api-key", HeaderValue::from_static("a-wrong-key"));
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::proxy::handle(
            axum::extract::State(state.clone()),
            axum::http::Method::POST,
            "/v1/messages".parse::<Uri>().unwrap(),
            headers,
            body,
        ),
    )
    .await
    .expect("鉴权失败的请求不该等体读完");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(mock.seen().is_empty());
}

/// 带了 Key 的超限体回 413 `request_too_large`，不打上游。
#[sqlx::test]
async fn oversized_bodies_get_413(pool: sqlx::PgPool) {
    static MB: [u8; 1024 * 1024] = [b' '; 1024 * 1024];
    let mock = MockUpstream::start(vec![]).await;
    let (_store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    let chunks = crate::proxy::MAX_BODY_BYTES / MB.len() + 1;
    let stream = futures_util::stream::iter(
        (0..chunks).map(|_| Ok::<_, std::io::Error>(Bytes::from_static(&MB))),
    );
    let (status, _, body) = send_body(
        &state,
        cc_request(serde_json::json!({})).0,
        axum::body::Body::from_stream(stream),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["type"], "request_too_large");
    assert!(mock.seen().is_empty());
}

/// 一条下游中转的探活：`Go-http-client`、不带身份、无 tools、一句话、`max_tokens` 8（判据 ping）。
fn go_probe(stream: bool) -> (HeaderMap, Bytes) {
    let body = serde_json::json!({
        "model": "claude-opus-5",
        "max_tokens": 8,
        "stream": stream,
        "messages": [{"role": "user", "content": "ping"}],
    });
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::USER_AGENT, HeaderValue::from_static("Go-http-client/1.1"));
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    (headers, Bytes::from(serde_json::to_vec(&body).unwrap()))
}

/// 上游对 1 token 预热的整段 JSON 回复，正文是 `content`。
fn prewarm_reply(content: serde_json::Value) -> Reply {
    Reply {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: serde_json::to_vec(&serde_json::json!({
            "id": "msg_01upstream", "type": "message", "role": "assistant",
            "model": "claude-opus-5", "content": content,
            "stop_reason": "max_tokens", "stop_sequence": null,
            "usage": {"input_tokens": 33, "output_tokens": 1}}))
        .unwrap(),
    }
}

/// 探针拒绝关、`probe_as_prewarm` 开：探活改写成官方 `/model` 预热发出——出站体是预热（`Hi`、
/// `max_tokens` 1、billing header + 身份句、不带 tools 与 stream），头是官方 CLI 的，身份是这个号
/// 自己的（头体同一个会话 id）。不带身份的探活不被设备身份闸拦下。上游回空正文时补一句「OK」，
/// 其余照上游；流水标 `probe_prewarm:ping` 与 `probe_reply_filled`。
#[sqlx::test]
async fn a_probe_is_sent_as_a_prewarm_and_an_empty_reply_gets_ok(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![prewarm_reply(serde_json::json!([]))]).await;
    let (store, state, ids) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::PROBE_AS_PREWARM, "1").await.unwrap();

    let (status, headers, body) = send(&state, go_probe(false)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(headers.get("x-luban-probe-kind").is_none(), "判据只记在流水里");
    let reply: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(reply["content"], serde_json::json!([{"type": "text", "text": "OK"}]));
    assert_eq!(reply["id"], "msg_01upstream");
    assert_eq!(reply["stop_reason"], "end_turn", "来访要 8 个 token，回一个字就停不是截断");
    assert_eq!(reply["usage"]["input_tokens"], 33);

    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].token, "tok-0");
    assert!(seen[0].path.contains("beta=true"), "{}", seen[0].path);
    let ua = seen[0].headers.get(header::USER_AGENT).unwrap().to_str().unwrap();
    assert!(ua.starts_with("claude-cli/"), "{ua}");
    let out: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(out["model"], "claude-opus-5");
    assert_eq!(out["max_tokens"], 1);
    assert!(out.get("stream").is_none() && out.get("tools").is_none(), "{out}");
    let system = out["system"].as_array().unwrap();
    assert_eq!(system.len(), 2);
    assert!(system[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"));
    assert_eq!(system[1]["text"], crate::config::CC_SYSTEM_IDENTITY);
    assert_eq!(out["messages"][0]["content"][0]["text"], "Hi");
    let user_id: serde_json::Value =
        serde_json::from_str(out["metadata"]["user_id"].as_str().unwrap()).unwrap();
    let header_sid =
        seen[0].headers.get("x-claude-code-session-id").unwrap().to_str().unwrap().to_string();
    assert_eq!(user_id["session_id"], header_sid, "头体同一个会话 id");

    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cred_id, Some(ids[0]));
    assert_eq!(
        rows[0].forensics.rewrites.as_deref(),
        Some("probe_prewarm:ping,probe_reply_filled")
    );
}

/// 来访要流式就把上游那段整段 JSON 展成 SSE；旧一代模型回的那一个字原样交回，不补「OK」。
/// 出站 URL 固定是 `/v1/messages?beta=true`：来访的路径（前缀匹配算计费的 `/v1/messages/…`）
/// 与查询串（`beta=false` 之类）都不跟着出站。
#[sqlx::test]
async fn a_streamed_probe_gets_the_prewarm_reply_as_sse(pool: sqlx::PgPool) {
    let hi = serde_json::json!([{"type": "text", "text": "Hi"}]);
    let mock = MockUpstream::start(vec![prewarm_reply(hi)]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::PROBE_AS_PREWARM, "1").await.unwrap();

    let (status, headers, body) =
        send_to(&state, "/v1/messages/batches?beta=false&x=1", go_probe(true)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let ct = headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/event-stream"), "{ct}");
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.starts_with("event: message_start"), "{text}");
    assert!(text.contains(r#""text":"Hi""#) && !text.contains(r#""text":"OK""#), "{text}");
    assert!(text.trim_end().ends_with(r#"data: {"type":"message_stop"}"#), "{text}");
    assert_eq!(mock.seen()[0].path, "/v1/messages?beta=true");
    let out: serde_json::Value = serde_json::from_slice(&mock.seen()[0].body).unwrap();
    assert!(out.get("stream").is_none(), "预热照官方非流式发");

    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows[0].forensics.rewrites.as_deref(), Some("probe_prewarm:ping"));
}

/// 改写成预热的探针不学零输出：`reject_empty_replies` 开着、上游对预热回 0 输出的空正文（比实测的
/// 1 个 token 更极端），也不学进规则；下一条同样的探活照常改写成预热发到上游，不被本地 403。
#[sqlx::test]
async fn a_probe_sent_as_a_prewarm_never_learns_an_empty_reply(pool: sqlx::PgPool) {
    let zero = || {
        let mut r = prewarm_reply(serde_json::json!([]));
        let mut v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        v["usage"]["output_tokens"] = 0.into();
        r.body = serde_json::to_vec(&v).unwrap();
        r
    };
    let mock = MockUpstream::start(vec![zero(), zero()]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::PROBE_AS_PREWARM, "1").await.unwrap();
    store.set_setting(store::REJECT_EMPTY_REPLIES, "true").await.unwrap();
    store.set_setting(store::REJECT_REFUSALS, "true").await.unwrap();

    for n in 1..=2 {
        let (status, _, body) = send(&state, go_probe(false)).await;
        assert_eq!(status, StatusCode::OK, "第 {n} 条: {}", String::from_utf8_lossy(&body));
        let reply: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(reply["content"][0]["text"], "OK");
        usage_logs(&store, n).await;
        store.drain_writes(std::time::Duration::from_secs(3)).await;
        assert!(state.empty_replies.read().classes.is_empty(), "第 {n} 条后不该学到零输出");
    }
    assert_eq!(mock.seen().len(), 2, "第二条照常到上游");
}

/// 缓存保活：主线程回复带 `anthropic-cache-keepalive: active` 时原样交回客户端，之后的 `cache_touch`
/// 打回发出那条回复的号、带那条请求出站的会话 id 与 UA，头与体照官方 touch；没记过的 request_id 本地
/// 回 404、不打上游，也不按计费请求占名额。
#[sqlx::test]
async fn cache_touch_goes_back_to_the_account_that_served_the_reply(pool: sqlx::PgPool) {
    let touched = Reply {
        status: 204,
        headers: vec![("request-id".into(), "req_touch".into())],
        body: vec![],
    };
    let mock = MockUpstream::start(vec![
        sse_ok("hi").with_headers(&[
            ("request-id", "req_011CmockKeepA1".into()),
            ("anthropic-cache-keepalive", "active".into()),
        ]),
        touched,
    ])
    .await;
    let (_store, state, _ids) = setup(pool.clone(), 2, &mock.base).await;

    let (status, headers, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("anthropic-cache-keepalive").unwrap(), "active", "回复头原样交回");

    let touch = |id: &str| {
        let (mut h, _) = cc_request(serde_json::json!({}));
        h.insert("x-claude-code-request-class", HeaderValue::from_static("auxiliary"));
        (h, Bytes::from(format!(r#"{{"request_id":"{id}"}}"#)))
    };
    let (status, _, body) =
        send_to(&state, crate::config::CC_CACHE_TOUCH_PATH, touch("req_011CmockKeepA1")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{}", String::from_utf8_lossy(&body));

    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    let (main, t) = (&seen[0], &seen[1]);
    assert_eq!(t.path, crate::config::CC_CACHE_TOUCH_PATH, "不带 ?beta=true");
    assert_eq!(t.token, main.token, "同一个号");
    let h = |s: &Seen, k: &str| s.headers.get(k).map(|v| v.to_str().unwrap().to_string());
    for k in ["x-claude-code-session-id", "user-agent", "x-app", "x-stainless-package-version"] {
        assert_eq!(h(t, k), h(main, k), "{k}");
    }
    assert_eq!(
        h(t, "anthropic-beta").as_deref(),
        Some("oauth-2025-04-20,cache-keepalive-2026-09-03")
    );
    assert_eq!(h(t, "x-claude-code-request-class").as_deref(), Some("auxiliary"));
    assert_eq!(h(t, "x-stainless-retry-count").as_deref(), Some("0"));
    assert_eq!(h(t, "accept").as_deref(), Some("application/json"));
    assert!(h(t, "x-api-key").is_none(), "接入 Key 不出站");
    assert_eq!(&t.body[..], br#"{"request_id":"req_011CmockKeepA1"}"#);

    // 同一条 10 秒内再来：就地 204，不再打上游。
    let (status, _, _) =
        send_to(&state, crate::config::CC_CACHE_TOUCH_PATH, touch("req_011CmockKeepA1")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(mock.seen().len(), 2, "太密的 touch 不转");

    // 没记过的：本地 404，不打上游。
    let (status, _, _) =
        send_to(&state, crate::config::CC_CACHE_TOUCH_PATH, touch("req_011CmockUnknown")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // 体不对：400。
    let (h0, _) = touch("x");
    let (status, _, _) =
        send_to(&state, crate::config::CC_CACHE_TOUCH_PATH, (h0, Bytes::from_static(b"{}"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.seen().len(), 2);
}

/// 测试请求里那个历史 thinking 块的签名（`cc_request` 用例共用）。
const HISTORY_SIGNATURE: &str = "c2lnbmF0dXJl";

/// 给 `cc_request` 那条会话铺一条「另一个号成功应答过」的流水：先用 `sse_ok` 跑一轮拿到它的
/// 会话键，再按同一键插一条 `cred_id` 不同的 200。此后这条会话在
/// [`CredentialStore::session_served_by_another_credential`] 眼里就是换过号的——签发者登记一个都
/// 没有时，历史思考块验不过才凭它降级重试。调用方的 mock 队列开头要多放一条 `sse_ok`。
async fn seed_session_served_by_other_cred(store: &CredentialStore, state: &AppState) {
    let (status, _, _) = send(state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let rows = usage_logs(store, 1).await;
    let key = rows[0].forensics.session_key.clone().expect("CC 来访按会话占名额，流水带会话键");
    let mut rec = store::UsageRecord {
        cred_id: Some(999_999),
        cred_label: "other".into(),
        status: 200,
        ..Default::default()
    };
    rec.forensics.session_key = Some(key);
    store.insert_usage_log(&rec).await.unwrap();
}

/// 历史思考块的签发者一个都不认识、会话没经过别的号、但**当前号此前就在这条会话上应答过**
/// （渠道测试工具的形态：首轮经这里拿到签名，改坏后回传）：上游那条「thinking 签名无效」400
/// **原样透传**、不降级重试——从这里发出的签名都登记过，认不出的不可能是本号的真签名，API Key
/// 直连也是 400。`redacted_thinking` 密文那条同样。
#[sqlx::test]
async fn broken_thinking_blocks_pass_through_when_this_credential_served_the_session(
    pool: sqlx::PgPool,
) {
    let mock = MockUpstream::start(vec![
        sse_ok("first turn"),
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.1.content.0: Invalid `signature` in `thinking` block",
        ),
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.1.content.0: Invalid `data` in `redacted_thinking` block",
        ),
    ])
    .await;
    let (store, state, _ids) = setup(pool.clone(), 1, &mock.base).await;
    // 首轮：当前号在这条会话上应答过（流水里一条本号的 200）。
    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    usage_logs(&store, 1).await;
    let req = cc_request(serde_json::json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "let me think", "signature": "TAMPERED"},
            {"type": "text", "text": "hello"}]},
        {"role": "user", "content": "again"}]}));
    let (status, _, body) = send(&state, req.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("Invalid `signature`"));
    assert_eq!(mock.seen().len(), 2, "没有降级重试");

    let (status, _, body) = send(&state, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("redacted_thinking"));
    assert_eq!(mock.seen().len(), 3, "没有降级重试");
    let rows = usage_logs(&store, 3).await;
    assert!(
        rows.iter().all(|r| r.forensics.rewrites.as_deref().is_none_or(|t| !t.contains("demoted")))
    );
}

/// 签发者一个都不认识、对这条会话也一无所知（全新会话键）：保留「历史未知」，按能恢复处理、
/// 降级重试。这正是审查指出的场景——不带会话 id 的来访 system 一变前缀键就变、落到新号，
/// 功能上线前的历史没有登记，不能把它当成「确认没换过」拦下。
#[sqlx::test]
async fn unknown_history_on_a_fresh_session_still_retries(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.1.content.0: Invalid `signature` in `thinking` block",
        ),
        sse_ok("hi"),
    ])
    .await;
    let (_store, state, _ids) = setup(pool.clone(), 1, &mock.base).await;
    let req = cc_request(serde_json::json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "let me think", "signature": "UNKNOWN_SIG"},
            {"type": "text", "text": "hello"}]},
        {"role": "user", "content": "again"}]}));
    let (status, _, _) = send(&state, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.seen().len(), 2, "降级重试了一次");
}

/// 部分签名认识（当前号签发的）、部分不认识：不能因为命中的只有本号就关掉恢复——认不出的那块
/// 可能是旧号签发而没登记的，保留「历史未知」，全新会话上照常重试。
#[sqlx::test]
async fn partially_known_history_keeps_the_recovery_path(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.1.content.0: Invalid `signature` in `thinking` block",
        ),
        sse_ok("hi"),
    ])
    .await;
    let (store, state, ids) = setup(pool.clone(), 1, &mock.base).await;
    store
        .remember_thinking_blocks(
            Some(ids[0]),
            &[store::thinking_block_fingerprint(HISTORY_SIGNATURE)],
        )
        .await
        .unwrap();
    let req = cc_request(serde_json::json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "old", "signature": "UNKNOWN_FROM_A"},
            {"type": "text", "text": "hello"}]},
        {"role": "user", "content": "more"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "new", "signature": HISTORY_SIGNATURE},
            {"type": "text", "text": "world"}]},
        {"role": "user", "content": "again"}]}));
    let (status, _, _) = send(&state, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.seen().len(), 2, "降级重试了一次");
}

/// 历史思考块**全部有记录且全是当前号签发的**还验不过：块是客户端改坏的，原样透传 400，不降级
/// 重试——哪怕这条会话另有别的号的痕迹也不重试，签发者是主判据。
#[sqlx::test]
async fn broken_thinking_blocks_pass_through_when_history_was_issued_here(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        sse_ok("seed"),
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.1.content.0: Invalid `signature` in `thinking` block",
        ),
    ])
    .await;
    let (store, state, ids) = setup(pool.clone(), 1, &mock.base).await;
    seed_session_served_by_other_cred(&store, &state).await;
    store
        .remember_thinking_blocks(
            Some(ids[0]),
            &[store::thinking_block_fingerprint(HISTORY_SIGNATURE)],
        )
        .await
        .unwrap();
    let req = cc_request(serde_json::json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "let me think", "signature": HISTORY_SIGNATURE},
            {"type": "text", "text": "hello"}]},
        {"role": "user", "content": "again"}]}));
    let (status, _, body) = send(&state, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
    assert_eq!(mock.seen().len(), 2, "没有降级重试");
}

/// 签发者登记一个都不认识时退回会话痕迹：会话经过了别的号（[`seed_session_served_by_other_cred`]）
/// 就降级重试。
#[sqlx::test]
async fn demote_retry_falls_back_to_session_history_when_issuers_are_unknown(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        sse_ok("seed"),
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.1.content.0: Invalid `signature` in `thinking` block",
        ),
        sse_ok("hi"),
    ])
    .await;
    let (store, state, _ids) = setup(pool.clone(), 1, &mock.base).await;
    seed_session_served_by_other_cred(&store, &state).await;
    let req = cc_request(serde_json::json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "let me think", "signature": HISTORY_SIGNATURE},
            {"type": "text", "text": "hello"}]},
        {"role": "user", "content": "again"}]}));
    let (status, _, _) = send(&state, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.seen().len(), 3, "降级重试了一次");
}

/// 救活缓存只认救活过的那段历史：会话救活过一次之后，客户端丢掉旧历史、只带着一个**全新的**
/// 无效签名来（与救活过的历史无关），不能借会话之光放行——本号已在此会话应答过，透传 400。
#[sqlx::test]
async fn recovery_cache_does_not_cover_unrelated_new_signatures(pool: sqlx::PgPool) {
    let sig_err = || {
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.2.content.0: Invalid `signature` in `thinking` block",
        )
    };
    let mock = MockUpstream::start(vec![sig_err(), sse_ok("one"), sig_err()]).await;
    let (store, state, _ids) = setup(pool.clone(), 1, &mock.base).await;
    let req = |sig: &str| {
        cc_request(serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "t", "signature": sig},
                {"type": "text", "text": "hello"}]},
            {"role": "user", "content": "again"}]}))
    };
    let (status, _, _) = send(&state, req("FROM_OLD_ACCOUNT")).await;
    assert_eq!(status, StatusCode::OK, "旧历史救活");
    assert_eq!(mock.seen().len(), 2);
    usage_logs(&store, 1).await;

    let (status, _, body) = send(&state, req("BRAND_NEW_INVALID")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
    assert_eq!(mock.seen().len(), 3, "无关的新签名不借救活之光，原样透传");
}

/// 「拒绝改坏的历史思考块」关掉：不判换没换号，回到兜底开关自己的老行为——本号在此会话应答过、
/// 签名认不出，也照样降级重试。
#[sqlx::test]
async fn reject_tampered_thinking_off_restores_unconditional_retry(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        sse_ok("first turn"),
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.2.content.0: Invalid `signature` in `thinking` block",
        ),
        sse_ok("rescued"),
    ])
    .await;
    let (store, state, _ids) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::REJECT_TAMPERED_THINKING, "0").await.unwrap();
    let (status, _, _) = send(&state, cc_request(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    usage_logs(&store, 1).await;
    let req = cc_request(serde_json::json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "let me think", "signature": "TAMPERED"},
            {"type": "text", "text": "hello"}]},
        {"role": "user", "content": "again"}]}));
    let (status, _, _) = send(&state, req).await;
    assert_eq!(status, StatusCode::OK, "开关关着：一律降级重试");
    assert_eq!(mock.seen().len(), 3);
}

/// 混合历史：首次救活的请求里既有外来块 A 也有已登记为本号签发的合法块 B。救活时只有 A 进救活
/// 缓存、只有 A 记成外来；之后客户端丢掉 A、保留 B，再带一个签名改坏的新块 U 来——B 不能给 U
/// 当重试凭据，本号已在此会话应答过，透传 400。
#[sqlx::test]
async fn legit_blocks_in_a_recovered_request_do_not_vouch_for_new_broken_ones(pool: sqlx::PgPool) {
    let sig_err = |path: &str| {
        Reply::json_error(
            400,
            "invalid_request_error",
            &format!("{path}: Invalid `signature` in `thinking` block"),
        )
    };
    let mock = MockUpstream::start(vec![
        sig_err("messages.2.content.0"),
        sse_ok("one"),
        sig_err("messages.4.content.0"),
    ])
    .await;
    let (store, state, ids) = setup(pool.clone(), 1, &mock.base).await;
    let fp = store::thinking_block_fingerprint;
    store.remember_thinking_blocks(Some(ids[0]), &[fp(HISTORY_SIGNATURE)]).await.unwrap();
    let turn = |first: &str, second: &str| {
        cc_request(serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "t", "signature": first},
                {"type": "text", "text": "a"}]},
            {"role": "user", "content": "more"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "t", "signature": second},
                {"type": "text", "text": "b"}]},
            {"role": "user", "content": "again"}]}))
    };
    let (status, _, _) = send(&state, turn("FROM_OLD_ACCOUNT", HISTORY_SIGNATURE)).await;
    assert_eq!(status, StatusCode::OK, "A 外来 + B 合法：救活");
    assert_eq!(mock.seen().len(), 2);
    assert!(store.history_recovered_recently(&[fp("FROM_OLD_ACCOUNT")]), "A 进了救活缓存");
    assert!(!store.history_recovered_recently(&[fp(HISTORY_SIGNATURE)]), "B 没进救活缓存");
    usage_logs(&store, 1).await;

    let (status, _, body) = send(&state, turn(HISTORY_SIGNATURE, "BRAND_NEW_INVALID")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
    assert_eq!(mock.seen().len(), 3, "B 不给 U 当重试凭据，原样透传");
    // 落库的外来登记同样只有 A。
    for _ in 0..300 {
        if store.thinking_block_issuers(&[fp("FROM_OLD_ACCOUNT")]).await.unwrap().known == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let got = store
        .thinking_block_issuers(&[fp("FROM_OLD_ACCOUNT"), fp(HISTORY_SIGNATURE)])
        .await
        .unwrap();
    assert_eq!(got.issuers, vec![None, Some(ids[0])], "A 外来、B 仍是本号");
}

/// 首次救活之后续轮还能救：未知的旧号历史第一次由当前号降级重试成功，流水里就有了当前号的 200
/// （`here`），而客户端下一轮仍原样带着那段历史——救活时把它记成了外来，下一轮据此照常重试，
/// 不会因为「当前号在此会话应答过」被判成改坏而拦下。
#[sqlx::test]
async fn recovered_history_stays_recoverable_on_the_next_turn(pool: sqlx::PgPool) {
    let sig_err = || {
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.1.content.0: Invalid `signature` in `thinking` block",
        )
    };
    let mock = MockUpstream::start(vec![sig_err(), sse_ok("one"), sig_err(), sse_ok("two")]).await;
    let (store, state, _ids) = setup(pool.clone(), 1, &mock.base).await;
    let req = || {
        cc_request(serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "old", "signature": "FROM_OLD_ACCOUNT"},
                {"type": "text", "text": "hello"}]},
            {"role": "user", "content": "again"}]}))
    };
    let (status, _, _) = send(&state, req()).await;
    assert_eq!(status, StatusCode::OK, "全新会话、历史未知：救活");
    assert_eq!(mock.seen().len(), 2);
    // 成功流水先落库、外来登记还没落（后台写）的窗口里紧接着续轮：进程内的救活标记要兜住。
    usage_logs(&store, 1).await;
    let (status, _, _) = send(&state, req()).await;
    assert_eq!(status, StatusCode::OK, "续轮照常救活，不等外来登记落库");
    assert_eq!(mock.seen().len(), 4, "又降级重试了一次");

    // 外来登记最终也落库（重启后靠它）。
    let fp = store::thinking_block_fingerprint("FROM_OLD_ACCOUNT");
    for _ in 0..300 {
        if store.thinking_block_issuers(std::slice::from_ref(&fp)).await.unwrap().known == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        store.thinking_block_issuers(std::slice::from_ref(&fp)).await.unwrap().issuers,
        vec![None],
        "救活过的历史记成外来"
    );
}

/// 截断的历史救活之后续轮还能救：最早那块（旧号的、上限之外）被上游点名，救活时按点名坐标把它
/// 登记成外来、会话也记下救活过——下一轮同一段历史紧接着来，不会因为「取到的 256 块全是本号的」
/// 加「本号在此会话应答过」被拦。上游点名的坐标是**出站体**的：luban 在首条用户消息后插了一条
/// 环境说明的 system 消息，来访的 `messages[1]` 出站时在 `messages[2]`。
#[sqlx::test]
async fn truncated_history_stays_recoverable_on_the_next_turn(pool: sqlx::PgPool) {
    let sig_err = || {
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.2.content.0: Invalid `signature` in `thinking` block",
        )
    };
    let mock = MockUpstream::start(vec![sig_err(), sse_ok("one"), sig_err(), sse_ok("two")]).await;
    let (store, state, ids) = setup(pool.clone(), 1, &mock.base).await;
    let cap = store::THINKING_BLOCK_CAP;
    let mut msgs = vec![serde_json::json!({"role": "user", "content": "hi"})];
    let mut known = Vec::new();
    for i in 0..=cap {
        let sig = format!("SIG{i}");
        msgs.push(serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "t", "signature": sig},
            {"type": "text", "text": "x"}]}));
        msgs.push(serde_json::json!({"role": "user", "content": "more"}));
        if i >= 1 {
            known.push(store::thinking_block_fingerprint(&sig));
        }
    }
    store.remember_thinking_blocks(Some(ids[0]), &known).await.unwrap();
    let req = || cc_request(serde_json::json!({"messages": msgs}));

    let (status, _, _) = send(&state, req()).await;
    assert_eq!(status, StatusCode::OK, "首轮救活");
    assert_eq!(mock.seen().len(), 2);
    usage_logs(&store, 1).await;
    let (status, _, _) = send(&state, req()).await;
    assert_eq!(status, StatusCode::OK, "续轮照常救活");
    assert_eq!(mock.seen().len(), 4);

    // 被点名的 SIG0 虽在指纹上限之外，也按坐标登记成了外来。
    let fp0 = store::thinking_block_fingerprint("SIG0");
    for _ in 0..300 {
        if store.thinking_block_issuers(std::slice::from_ref(&fp0)).await.unwrap().known == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        store.thinking_block_issuers(std::slice::from_ref(&fp0)).await.unwrap().issuers,
        vec![None]
    );
}

/// 请求里的历史块多过指纹上限时，取到的那部分全是当前号签发也不能断言整段历史：最早那块可能
/// 是旧号的——保留「历史未知」，全新会话上照常重试。
#[sqlx::test]
async fn truncated_history_does_not_count_as_fully_issued_here(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.1.content.0: Invalid `signature` in `thinking` block",
        ),
        sse_ok("hi"),
    ])
    .await;
    let (store, state, ids) = setup(pool.clone(), 1, &mock.base).await;
    let cap = store::THINKING_BLOCK_CAP;
    let mut msgs = vec![serde_json::json!({"role": "user", "content": "hi"})];
    let mut known = Vec::new();
    for i in 0..=cap {
        let sig = format!("SIG{i}");
        msgs.push(serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "t", "signature": sig},
            {"type": "text", "text": "x"}]}));
        msgs.push(serde_json::json!({"role": "user", "content": "more"}));
        if i >= 1 {
            known.push(store::thinking_block_fingerprint(&sig));
        }
    }
    // 最近的 cap 个都登记在当前号名下；最早的 SIG0 没登记（旧号的）。
    store.remember_thinking_blocks(Some(ids[0]), &known).await.unwrap();
    let (status, _, _) = send(&state, cc_request(serde_json::json!({"messages": msgs}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.seen().len(), 2, "截断了就不能说全是本号的，照常救活");
}

/// 降级重试成功的那一发开了保活：登记的是最终那条回复，客户端 touch 它的 request-id 照样转到原号；
/// 被拒的首发不留记录。历史思考块的签名登记成**外来**（不是现有任何一个号签发的），兜底才会出手——这条会话本身是
/// 全新的、没有任何换号痕迹（审查指出的场景：不带会话 id 的来访 system 一变，前缀键就变、落到
/// 新号，新键下查不到旧号；靠签发者才判得出换过号）。
#[sqlx::test]
async fn cache_touch_follows_the_reply_from_a_demote_retry(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![
        Reply::json_error(
            400,
            "invalid_request_error",
            "messages.1.content.0: Invalid `signature` in `thinking` block",
        )
        .with_headers(&[("request-id", "req_011CmockFirst400".into())]),
        sse_ok("hi").with_headers(&[
            ("request-id", "req_011CmockRetryOk".into()),
            ("anthropic-cache-keepalive", "active".into()),
        ]),
        Reply { status: 204, headers: vec![], body: vec![] },
    ])
    .await;
    let (store, state, _ids) = setup(pool.clone(), 1, &mock.base).await;
    // 登记成外来（不是现有任何一个号签发的）。
    store
        .remember_thinking_blocks(None, &[store::thinking_block_fingerprint(HISTORY_SIGNATURE)])
        .await
        .unwrap();
    let req = cc_request(serde_json::json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "let me think", "signature": HISTORY_SIGNATURE},
            {"type": "text", "text": "hello"}]},
        {"role": "user", "content": "again"}]}));
    let (status, headers, _) = send(&state, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.seen().len(), 2, "降级重试了一次");
    assert_eq!(headers.get("anthropic-cache-keepalive").unwrap(), "active");

    let touch = |id: &str| {
        let (h, _) = cc_request(serde_json::json!({}));
        (h, Bytes::from(format!(r#"{{"request_id":"{id}"}}"#)))
    };
    let (status, _, _) =
        send_to(&state, crate::config::CC_CACHE_TOUCH_PATH, touch("req_011CmockRetryOk")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let seen = mock.seen();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[2].path, crate::config::CC_CACHE_TOUCH_PATH);
    assert_eq!(seen[2].token, seen[1].token);
    let (status, _, _) =
        send_to(&state, crate::config::CC_CACHE_TOUCH_PATH, touch("req_011CmockFirst400")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "被拒的首发没记");
}

/// 非 CC 客户端给新一代模型写 `thinking.type:"enabled"`：开关开着时本地回 API Key 直连那条 400、
/// 不打上游，流水标 `rejected_locally:enabled-thinking`；同一请求体换成 haiku-4-5 照发；关掉照发。
#[sqlx::test]
async fn client_enabled_thinking_on_adaptive_only_model_is_rejected_locally(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![sse_ok("ok"), sse_ok("ok")]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    let req = |model: &str| {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 2048,
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "metadata": {"user_id": serde_json::json!({
                "device_id": "b".repeat(64), "account_uuid": "", "session_id": SESSION,
            }).to_string()},
            "messages": [{"role": "user", "content": "Reply with the single word OK."}],
        });
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(header::USER_AGENT, HeaderValue::from_static("Go-http-client/1.1"));
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        (headers, Bytes::from(serde_json::to_vec(&body).unwrap()))
    };

    assert_local_reject(
        &mock,
        &state,
        req("claude-opus-5-5"),
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "\"thinking.type.enabled\" is not supported for this model",
    )
    .await;
    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 400);
    assert_eq!(rows[0].forensics.rewrites.as_deref(), Some("rejected_locally:enabled-thinking"));

    let (status, _, _) = send(&state, req("claude-haiku-4-5")).await;
    assert_eq!(status, StatusCode::OK, "haiku-4-5 照收 enabled，照发");
    assert_eq!(mock.seen().len(), 1);

    store.set_setting(store::REJECT_ENABLED_THINKING, "0").await.unwrap();
    let (status, _, _) = send(&state, req("claude-opus-5-5")).await;
    assert_eq!(status, StatusCode::OK, "关掉就照发");
    assert_eq!(mock.seen().len(), 2);
}

/// 非 CC 客户端的一条请求：`patch` 并进基础体，`extra` 追加/覆盖请求头（值为 `None` 即删掉）。
fn go_request(
    patch: serde_json::Value,
    extra: &[(&'static str, Option<&'static str>)],
) -> (HeaderMap, Bytes) {
    let mut body = serde_json::json!({
        "model": "claude-haiku-4-5",
        "max_tokens": 2048,
        "metadata": {"user_id": serde_json::json!({
            "device_id": "b".repeat(64), "account_uuid": "", "session_id": SESSION,
        }).to_string()},
        "messages": [{"role": "user", "content": "Reply with the single word OK."}],
    });
    for (k, v) in patch.as_object().unwrap() {
        body[k] = v.clone();
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::USER_AGENT, HeaderValue::from_static("Go-http-client/1.1"));
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    for (k, v) in extra {
        match v {
            Some(v) => headers.insert(*k, HeaderValue::from_static(v)),
            None => headers.remove(*k),
        };
    }
    (headers, Bytes::from(serde_json::to_vec(&body).unwrap()))
}

/// `api_key_parity` 的三条：缺版本头、没带 beta 的 `display`、预算不小于 `max_tokens`，各自本地回
/// 直连原文、不打上游、流水标各自的类别；来访带了对应 beta 的照发；关掉照发。
#[sqlx::test]
async fn api_key_parity_rejects_what_the_api_key_channel_rejects(pool: sqlx::PgPool) {
    let mock = MockUpstream::start((0..5).map(|_| sse_ok("ok")).collect()).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    let no_version = || go_request(serde_json::json!({}), &[("anthropic-version", None)]);
    let updates = |beta: Option<&'static str>| {
        go_request(
            serde_json::json!({"model": "claude-opus-5-5", "thinking": {"type": "adaptive", "display": "updates"}}),
            &[("anthropic-beta", beta)],
        )
    };
    let budget = |beta: Option<&'static str>| {
        go_request(
            serde_json::json!({"max_tokens": 1024, "thinking": {"type": "enabled", "budget_tokens": 2000}}),
            &[("anthropic-beta", beta)],
        )
    };
    let cases: [(_, &str, &str); 3] = [
        (no_version(), "anthropic-version: header is required", "anthropic-version"),
        (
            updates(None),
            "thinking.adaptive.display: Input should be 'summarized', 'omitted'",
            "thinking-display",
        ),
        (
            budget(None),
            "`max_tokens` must be greater than `thinking.budget_tokens`",
            "thinking-budget",
        ),
    ];
    for (i, (req, needle, tag)) in cases.into_iter().enumerate() {
        assert_local_reject(
            &mock,
            &state,
            req,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            needle,
        )
        .await;
        let rows = usage_logs(&store, i + 1).await;
        assert_eq!(
            rows[0].forensics.rewrites.as_deref(),
            Some(&*format!("rejected_locally:{tag}"))
        );
    }

    let (status, _, _) = send(&state, updates(Some("thinking-display-updates-2026-08-18"))).await;
    assert_eq!(status, StatusCode::OK, "带了 beta 的交给上游");
    let (status, _, _) = send(&state, budget(Some("interleaved-thinking-2025-05-14"))).await;
    assert_eq!(status, StatusCode::OK, "带了 interleaved 直连也放行");
    assert_eq!(mock.seen().len(), 2);

    store.set_setting(store::API_KEY_PARITY, "0").await.unwrap();
    for req in [no_version(), updates(None), budget(None)] {
        let (status, _, _) = send(&state, req).await;
        assert_eq!(status, StatusCode::OK, "关掉就照发");
    }
    assert_eq!(mock.seen().len(), 5);
}

/// 改写成预热的探针也绕不过去：探针判定排在这几条之后会被预热吞掉、回成 200，直连却是 400。
#[sqlx::test]
async fn api_key_parity_applies_before_probe_prewarm(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::REJECT_PROBES, "0").await.unwrap();
    store.set_setting(store::PROBE_AS_PREWARM, "1").await.unwrap();
    let (headers, body) = go_probe(false);
    let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    v["model"] = "claude-opus-5-5".into();
    v["max_tokens"] = 2048.into();
    v["thinking"] = serde_json::json!({"type": "enabled", "budget_tokens": 1024});
    let req = (headers, Bytes::from(serde_json::to_vec(&v).unwrap()));
    assert_local_reject(
        &mock,
        &state,
        req,
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "\"thinking.type.enabled\" is not supported for this model",
    )
    .await;
}

/// billing-only 在 `system[0]` 前插了 billing header：上游报 `system.2`，客户端看到的是它自己那份
/// 的 `system.1`，与直连一致；体的其余部分原样。
#[sqlx::test]
async fn billing_only_unshifts_system_index_in_upstream_errors(pool: sqlx::PgPool) {
    const TTL_400: &str = "system.2.cache_control.ephemeral.ttl: Input should be '5m' or '1h'";
    let mock =
        MockUpstream::start(vec![Reply::json_error(400, "invalid_request_error", TTL_400)]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::SIM_BILLING_ONLY, "1").await.unwrap();
    let req = go_request(
        serde_json::json!({"system": [
            {"type": "text", "text": "a"},
            {"type": "text", "text": "b", "cache_control": {"type": "ephemeral", "ttl": "2h"}},
        ]}),
        &[],
    );
    let (status, _, body) = send(&state, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.starts_with("system.1.cache_control.ephemeral.ttl: Input should be '5m' or '1h'"),
        "{msg}"
    );
    let seen = mock.seen();
    let out: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(out["system"].as_array().unwrap().len(), 3, "出站确实多了一块 billing header");
}

/// `anthropic-beta` 分两条头发：官方两条都读，第二条里的 interleaved / display-updates 同样算数，
/// 照发不拦。
#[sqlx::test]
async fn api_key_parity_reads_every_anthropic_beta_header(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![sse_ok("ok"), sse_ok("ok")]).await;
    let (_store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    let twice = |patch: serde_json::Value, second: &'static str| {
        let (mut headers, body) =
            go_request(patch, &[("anthropic-beta", Some("effort-2025-11-24"))]);
        headers.append("anthropic-beta", HeaderValue::from_static(second));
        (headers, body)
    };
    let (status, _, body) = send(
        &state,
        twice(
            serde_json::json!({"max_tokens": 1024, "thinking": {"type": "enabled", "budget_tokens": 2000}}),
            "interleaved-thinking-2025-05-14",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let (status, _, body) = send(
        &state,
        twice(
            serde_json::json!({"model": "claude-opus-5-5", "thinking": {"type": "adaptive", "display": "updates"}}),
            "thinking-display-updates-2026-08-18",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(mock.seen().len(), 2);
}

/// 缺版本头又写了非法 display：直连先报版本头，这里也得先报它，不能让 thinking 那条抢先。
#[sqlx::test]
async fn missing_version_is_reported_before_thinking_checks(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![]).await;
    let (_store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    let req = go_request(
        serde_json::json!({"model": "claude-opus-5-5", "thinking": {"type": "adaptive", "display": "updates"}}),
        &[("anthropic-version", None)],
    );
    assert_local_reject(
        &mock,
        &state,
        req,
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "anthropic-version: header is required",
    )
    .await;
}

/// 客户端首块混着旧 billing header 与业务指令：billing-only 行级剥掉那行、块保留，再插新的
/// billing 首块；上游报 `system.1`，客户端看到的是它自己那份的 `system.0`。
#[sqlx::test]
async fn billing_only_maps_system_index_through_a_mixed_first_block(pool: sqlx::PgPool) {
    const TTL_400: &str = "system.1.cache_control.ephemeral.ttl: Input should be '5m' or '1h'";
    let mock =
        MockUpstream::start(vec![Reply::json_error(400, "invalid_request_error", TTL_400)]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    store.set_setting(store::SIM_BILLING_ONLY, "1").await.unwrap();
    let mixed = "x-anthropic-billing-header: cc_version=2.1.300.abc; cc_entrypoint=cli; cch=00000;\nbe brief";
    let req = go_request(
        serde_json::json!({"system": [
            {"type": "text", "text": mixed, "cache_control": {"type": "ephemeral", "ttl": "2h"}},
        ]}),
        &[],
    );
    let (status, _, body) = send(&state, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.starts_with("system.0.cache_control.ephemeral.ttl: Input should be '5m' or '1h'"),
        "{msg}"
    );
    let out: serde_json::Value = serde_json::from_slice(&mock.seen()[0].body).unwrap();
    assert_eq!(out["system"].as_array().unwrap().len(), 2, "出站：新 billing 首块 + 剥过的业务块");
    assert_eq!(out["system"][1]["text"], "be brief");
}

/// 非 CC 客户端带 OpenClaw 那两个原文片段（不到 1900 字节、不会被挪走）：开关开着时本地回上游那条
/// 第三方 400、不打上游，流水标 `rejected_locally:third-party`；关掉照发。
#[sqlx::test]
async fn known_third_party_signature_is_rejected_locally(pool: sqlx::PgPool) {
    let mock = MockUpstream::start(vec![sse_ok("ok")]).await;
    let (store, state, _) = setup(pool.clone(), 1, &mock.base).await;
    let req = || {
        let body = serde_json::json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "system": "[[reply_to_current]]\nstripped before rendering",
            "metadata": {"user_id": serde_json::json!({
                "device_id": "b".repeat(64), "account_uuid": "", "session_id": SESSION,
            }).to_string()},
            "messages": [{"role": "user", "content": "Reply with the single word OK."}],
            "tools": [{"name": "read", "description": "read", "input_schema": {"type": "object"}}],
        });
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(header::USER_AGENT, HeaderValue::from_static("Go-http-client/1.1"));
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        (headers, Bytes::from(serde_json::to_vec(&body).unwrap()))
    };

    assert_local_reject(
        &mock,
        &state,
        req(),
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "Third-party apps now draw from extra usage",
    )
    .await;
    let rows = usage_logs(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 400);

    store.set_setting(store::REJECT_KNOWN_THIRD_PARTY, "0").await.unwrap();
    let (status, _, _) = send(&state, req()).await;
    assert_eq!(status, StatusCode::OK, "关掉就照发");
    assert_eq!(mock.seen().len(), 1);
}

/// 第三方客户端的两个对话先后占到号上同一个会话槽位（A 的绑定释放后 B 进来）：B 出站是一个新的
/// 会话 id，会话链从头开始——首轮 `diagnostics.previous_message_id` 为 null、billing header 里没有
/// `cc_prev_req`、`cc_prompt_index` 从 1 起，与官方 `/clear` 开新对话一致
/// （`cap/auto-2.1.303-20261019-full` 00429 → 00434）。此前槽位复用同一个 id，B 首轮带着 A 的
/// `previous_message_id` 与 `cc_prev_req`（线上 `req_j1uuiSQrvphw6Orz`）。
#[sqlx::test]
async fn a_new_conversation_in_a_reused_slot_starts_a_fresh_session(pool: sqlx::PgPool) {
    let ok = || {
        sse_ok("done").with_headers(&[("request-id", "req_011CmockAAAAAAAAAAAAAAAA".to_string())])
    };
    let mock = MockUpstream::start(vec![ok(), ok()]).await;
    let (store, state, ids) = setup(pool.clone(), 1, &mock.base).await;
    // 线上配置：不要求来访带设备身份（第三方客户端本来就没有）。
    store.set_setting(store::REQUIRE_DEVICE_ID, "0").await.unwrap();
    let third_party = |text: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(header::USER_AGENT, HeaderValue::from_static("Go-http-client/1.1"));
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        let body = serde_json::json!({
            "model": "claude-haiku-5-5",
            "max_tokens": 1024,
            "stream": true,
            "system": "You translate text for a small agency.",
            "messages": [{"role": "user", "content": text}],
        });
        (headers, Bytes::from(serde_json::to_vec(&body).unwrap()))
    };
    let (status, _, body) = send(&state, third_party("Translate: good morning, everyone.")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let logs = usage_logs(&store, 1).await;
    let key_a = logs[0].forensics.session_key.clone().expect("模拟请求有会话键");
    // A 的绑定释放（相当于它闲过了 TTL），槽位 0 空出来给 B。
    assert!(store.unbind_session(ids[0], &key_a).await.unwrap());
    let (status, ..) = send(&state, third_party("Translate: the meeting is at noon.")).await;
    assert_eq!(status, StatusCode::OK);

    let seen = mock.seen();
    let main: Vec<serde_json::Value> = seen
        .iter()
        .filter(|s| s.path.starts_with("/v1/messages"))
        .map(|s| serde_json::from_slice(&s.body).unwrap())
        .collect();
    assert_eq!(main.len(), 2, "{:?}", seen.iter().map(|s| &s.path).collect::<Vec<_>>());
    let session_of = |v: &serde_json::Value| {
        let uid: serde_json::Value =
            serde_json::from_str(v["metadata"]["user_id"].as_str().unwrap()).unwrap();
        uid["session_id"].as_str().unwrap().to_string()
    };
    assert_ne!(session_of(&main[0]), session_of(&main[1]), "新对话换新的会话 id");
    let b = &main[1];
    assert_eq!(b["thread"]["type"], "create");
    assert_eq!(b["diagnostics"]["previous_message_id"], serde_json::Value::Null, "{b}");
    let billing = b["system"][0]["text"].as_str().unwrap();
    assert!(!billing.contains("cc_prev_req="), "{billing}");
    assert!(billing.contains("cc_prompt_index=1;"), "{billing}");
}
