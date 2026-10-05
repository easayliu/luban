use super::{HeaderValue, StatusCode, header, request_speed, store};

/// 本地拒绝的响应体必须是**上游那副 JSON 形态**，且 `content-type` 说的就是 JSON。
///
/// 曾经这几条是 `(StatusCode, "一句话")`，发出去是 `text/plain`：客户端按 JSON 读错误体，
/// 读不出来就退回一句按状态码编的通用话，我们写的原因（等多久、缺哪个字段、该升到哪版）
/// 全丢了。限流那条还要盯住 `retry-after`——它是「该等多久」的唯一来源，丢了客户端就只能
/// 立刻再撞一次。
#[tokio::test]
async fn local_rejections_speak_the_upstream_error_shape() {
    async fn parts(
        resp: super::Response,
    ) -> (StatusCode, Option<String>, Option<String>, serde_json::Value) {
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let retry = resp
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        (status, ctype, retry, serde_json::from_slice(&bytes).expect("错误体必须是 JSON"))
    }

    let (status, ctype, retry, body) =
        parts(super::error_response(StatusCode::FORBIDDEN, "permission_error", "nope")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    assert_eq!(retry, None, "非限流的错误不该凭空带上 retry-after");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "permission_error");
    assert_eq!(body["error"]["message"], "nope");

    let (status, ctype, retry, body) = parts(super::rate_limit_response(41, "slow down")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    assert_eq!(retry.as_deref(), Some("41"), "限流必须带 retry-after");
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(body["error"]["message"], "slow down");
}

/// 本地拒绝也进流水：没选到账号（`cred_id` 空），但路径、UA、模型、设备、会话、状态、
/// 错误类型/文案与请求 id 都在，控制台按 id 能查到。模型/设备/体里的会话来自 handle_inner
/// 解析体时放下的那份，这里自己不解析体。
#[test]
fn a_local_rejection_becomes_a_usage_row_without_a_credential() {
    let ctx = super::LocalRejectCtx {
        method: "POST".into(),
        path: "/v1/messages?beta=true".into(),
        ua: "python-httpx/0.27.0".into(),
        session_header: None,
    };
    let parsed = super::ParsedRequestBits {
        model: Some("claude-sonnet-5".into()),
        device_id: Some("abc123".into()),
        session_id: Some("11111111-2222-4333-8444-555555555555".into()),
    };
    let err = super::LocalErrorFields {
        etype: Some("invalid_request_error".into()),
        message: Some("messages.0.role: system is not accepted".into()),
    };
    let rec = super::local_reject_record(
        &ctx,
        Some(parsed),
        StatusCode::BAD_REQUEST,
        Some(err),
        "req_test0001",
        std::time::Instant::now(),
    );
    assert_eq!(rec.cred_id, None);
    assert_eq!(rec.cred_label, "");
    assert_eq!(rec.status, 400);
    assert_eq!(rec.path, "/v1/messages?beta=true");
    assert_eq!(rec.ua.as_deref(), Some("python-httpx/0.27.0"));
    assert_eq!(rec.ua_out, None, "没出站");
    assert_eq!(rec.model.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(rec.device_id.as_deref(), Some("abc123"));
    assert_eq!(
        rec.forensics.session_id_in.as_deref(),
        Some("11111111-2222-4333-8444-555555555555"),
        "头上没有就取体里的会话段"
    );
    assert_eq!(rec.forensics.session_id, None, "没到上游，没有出站会话 id");
    assert_eq!(rec.forensics.error_type.as_deref(), Some("invalid_request_error"));
    assert_eq!(
        rec.forensics.error_message.as_deref(),
        Some("messages.0.role: system is not accepted")
    );
    assert_eq!(rec.forensics.rewrites.as_deref(), Some("rejected_locally"));
    assert_eq!(rec.request_id.as_deref(), Some("req_test0001"));
    assert!(!rec.has_usage);
    assert!(rec.total_ms.is_some());

    // 体解析之前就拒掉的（API key / 版本闸 / 头上的会话限流）：没有 parsed，模型、设备都空，
    // 会话只认头上的；UA 占位 `-` 还原成 NULL。
    let ctx = super::LocalRejectCtx {
        ua: "-".into(),
        session_header: Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into()),
        ..ctx
    };
    let rec = super::local_reject_record(
        &ctx,
        None,
        StatusCode::TOO_MANY_REQUESTS,
        None,
        "req_test0002",
        std::time::Instant::now(),
    );
    assert_eq!(rec.status, 429);
    assert_eq!(rec.model, None);
    assert_eq!(rec.device_id, None);
    assert_eq!(rec.ua, None);
    assert_eq!(
        rec.forensics.session_id_in.as_deref(),
        Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"),
        "头上那个优先"
    );
    assert_eq!(rec.forensics.error_type, None);
}

/// 错误体改写时顺手读出的 type/message 是**改写前**的原文，不带追加的请求 id。
#[tokio::test]
async fn annotating_an_error_reports_the_original_error_fields() {
    let resp = super::error_response(StatusCode::BAD_REQUEST, "invalid_request_error", "nope");
    let (resp, fields) = super::annotate_error_response(resp, "req_x").await;
    assert_eq!(
        fields,
        Some(super::LocalErrorFields {
            etype: Some("invalid_request_error".into()),
            message: Some("nope".into())
        })
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["message"], "nope (luban request id: req_x)", "体照常改写");

    // 非 JSON 的响应不碰也不读。
    let plain =
        axum::response::IntoResponse::into_response((StatusCode::BAD_GATEWAY, "upstream down"));
    let (_, fields) = super::annotate_error_response(plain, "req_y").await;
    assert_eq!(fields, None);
}

/// 请求 id 取法：来访合法的 X-Request-Id 沿用（UUID / ULID / req_… / 32 位 hex 都算），
/// 带空格、引号、超长或没带的一律生成 `req_` + 16 位 base62。
#[test]
fn request_id_reuses_a_sane_inbound_x_request_id_else_generates_one() {
    let with = |v: &str| {
        let mut h = super::HeaderMap::new();
        h.insert("x-request-id", HeaderValue::from_str(v).unwrap());
        super::request_id_for(&h)
    };
    for ok in [
        "550e8400-e29b-41d4-a716-446655440000",
        "01J9Z3QK5V8N2X6M4P7R9T1W3Y",
        "req_01ABCxyz",
        "9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d",
    ] {
        assert_eq!(with(ok), ok, "合法的应沿用");
    }
    let is_generated = |s: &str| {
        s.len() == 4 + super::REQUEST_ID_RANDOM_LEN
            && s.starts_with("req_")
            && s[4..].bytes().all(|b| b.is_ascii_alphanumeric())
    };
    for bad in ["has space", "quote\"d", "", &"x".repeat(129)] {
        let got = with(bad);
        assert!(is_generated(&got), "{bad:?} 不合法应重新生成，得到 {got}");
    }
    assert!(is_generated(&super::request_id_for(&super::HeaderMap::new())), "没带就生成");
    assert_ne!(super::new_request_id(), super::new_request_id(), "随机，不该重复");
    // 前后空白容忍。
    assert_eq!(with("  abc-123  "), "abc-123");
}

/// 错误体里带 luban 请求 id：message 末尾追加、顶层加 luban_request_id、不动上游的 request_id。
#[test]
fn error_bodies_carry_the_luban_request_id() {
    let rid = "lb-0000-test";
    // 上游形态（带 Anthropic 自己的 request_id）。
    let up = br#"{"type":"error","error":{"type":"invalid_request_error","message":"`temperature` is deprecated for this model."},"request_id":"req_01ABC"}"#;
    let out = super::annotate_error_json(up, rid).expect("是 JSON 对象");
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v["error"]["message"],
        "`temperature` is deprecated for this model. (luban request id: lb-0000-test)"
    );
    assert_eq!(v["request_id"], "req_01ABC", "上游的 request_id 不能被盖掉");
    assert_eq!(v["luban_request_id"], rid);
    // 再来一遍不重复追加。
    let again: serde_json::Value =
        serde_json::from_slice(&super::annotate_error_json(&out, rid).unwrap()).unwrap();
    assert_eq!(again["error"]["message"], v["error"]["message"]);
    // luban 自己的错误体同样处理。
    let own = super::error_body("permission_error", "nope");
    let v: serde_json::Value =
        serde_json::from_slice(&super::annotate_error_json(&own, rid).unwrap()).unwrap();
    assert_eq!(v["error"]["message"], "nope (luban request id: lb-0000-test)");
    // 不是 JSON 对象：放行。
    assert!(super::annotate_error_json(b"Error", rid).is_none());
    assert!(super::annotate_error_json(b"[1,2]", rid).is_none());
}

/// 请求体顶层 `speed` 字段能被读出；缺字段/非法 JSON 返回 None（不阻断转发）。
///
/// 入参是**已解析**的 body（handler 全程只解析一次，见 [`super::handle`]），故「非法
/// JSON」在这里表现为 `None`——解析失败那步已经在上游发生了。
#[test]
fn reads_speed_from_request_body() {
    let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).ok();
    let with = parse(r#"{"model":"claude-opus-5","speed":"fast","messages":[]}"#);
    assert_eq!(request_speed(with.as_ref()).as_deref(), Some("fast"));
    let without = parse(r#"{"model":"claude-opus-5","messages":[]}"#);
    assert_eq!(request_speed(without.as_ref()), None);
    assert_eq!(parse("not json"), None, "非法 JSON 在解析那步就是 None");
    assert_eq!(request_speed(None), None);
}

/// 不带设备身份的探活走**整条 [`handle`]**：拿到的是本地那条 200（而不是设备身份闸的
/// 403），头上带标记、体是一条正常回复，并且流水里落了一条标 `probe_reply`、花费 0、
/// 没有凭证的本地记录。
///
/// 守的是两件真出过问题的事：一、`require_device_id` 默认开着，不带 `metadata.user_id` 的
/// 计费请求在 2.2 就是一条 403，而封号复盘里 Go-http-client 那批探活恰恰不带身份、探针判据
/// 也明确允许 `device_id` 为 `None`——两段一旦调了个个儿，这类探活又会拿到 403，下游照旧把
/// 整个 key 摘下去；二、本地以 200 收尾的请求不标 `local_replay` 就会从流水、请求查询与
/// 统计里整个消失（0.3.98 把本地 403 改成回放 200 时正是这样漏掉的）。
#[tokio::test]
async fn a_device_less_probe_is_answered_locally_and_logged() {
    let store = std::sync::Arc::new(store::CredentialStore::open_in_memory().unwrap());
    assert!(store.require_device_id(), "这条用例的前提是设备身份闸开着");
    assert!(store.forward_flags().reject_probes, "这条用例的前提是探针开关开着");
    let state = crate::web::AppState::for_test(store.clone());

    // 复盘里那批 Go-http-client 探活的形态：没有 metadata（即没有 device_id）、没有 system、
    // 没有 tools、一条消息、max_tokens 8。
    let body = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 8,
            "messages": [{ "role": "user", "content": "ping" }]});
    let mut headers = super::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::USER_AGENT, HeaderValue::from_static("Go-http-client/1.1"));
    let resp = super::handle(
        axum::extract::State(state),
        axum::http::Method::POST,
        "/v1/messages".parse::<axum::http::Uri>().unwrap(),
        headers,
        axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK, "设备身份闸不该抢在探针前面回一条 403");
    assert_eq!(resp.headers().get(super::LOCAL_REPLY_HEADER).unwrap(), "probe_reply");
    assert_eq!(resp.headers().get(super::PROBE_KIND_HEADER).unwrap(), "ping");
    assert!(resp.headers().get("x-request-id").is_some(), "本地作答也要带请求 id");
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let msg: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(msg["content"][0]["text"], super::PROBE_REPLY_TEXT);
    assert_eq!(msg["model"], "claude-opus-5");
    assert!(msg["id"].as_str().unwrap().starts_with(super::PROBE_REPLY_ID_PREFIX));

    // 流水是 `spawn_blocking` 写的，等它落下来（最多 2 秒，正常几毫秒）。
    let mut rows = Vec::new();
    for _ in 0..200 {
        rows = store.list_usage_logs(10).unwrap();
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(rows.len(), 1, "本地以 200 收尾的探针也要进流水");
    let row = &rows[0];
    assert_eq!(row.status, 200);
    assert_eq!(row.path, "/v1/messages");
    assert_eq!(row.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(row.forensics.rewrites.as_deref(), Some(super::REWRITE_PROBE_REPLY));
    assert_eq!(row.cost_usd, Some(0.0));
    assert_eq!(row.cred_id, None, "没到上游，不该挂在任何账号上");
    assert!(!row.has_usage, "没到上游，没有用量");
}

/// 开关关掉之后同一条探活照常往下走（走到没有可用账号那步），确认上面那条 200 是
/// `reject_probes` 给的，而不是别的什么早退路径顺手回的。
#[tokio::test]
async fn the_same_probe_goes_on_when_the_switch_is_off() {
    let store = std::sync::Arc::new(store::CredentialStore::open_in_memory().unwrap());
    // `"0"` / `"false"` 才算关，别的取值一律算开，见 `store::setting_is_on`。
    store.set_setting(store::REJECT_PROBES, "0").unwrap();
    store.set_setting(store::REQUIRE_DEVICE_ID, "0").unwrap();
    let state = crate::web::AppState::for_test(store.clone());
    let body = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 8,
            "messages": [{ "role": "user", "content": "ping" }]});
    let mut headers = super::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::USER_AGENT, HeaderValue::from_static("Go-http-client/1.1"));
    let resp = super::handle(
        axum::extract::State(state),
        axum::http::Method::POST,
        "/v1/messages".parse::<axum::http::Uri>().unwrap(),
        headers,
        axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
    )
    .await;
    // 关掉之后这条探活照常往下走，最终停在「没有可用账号」那一步（本地库里一个号都没有），
    // 而不是由 2.1a 就地作答。
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(resp.headers().get(super::LOCAL_REPLY_HEADER).is_none());
}

/// `max_tokens` 的读法：只认顶层的整数，读不出来一律 `None`（日志里落成 0）。
#[test]
fn request_max_tokens_reads_the_declared_output_cap() {
    let mt = |s: &str| {
        let v: serde_json::Value = serde_json::from_str(s).unwrap();
        super::request_max_tokens(Some(&v))
    };
    assert_eq!(mt(r#"{"max_tokens":64000}"#), Some(64000));
    assert_eq!(mt(r#"{"model":"claude-sonnet-5"}"#), None, "没写就是没写，别猜一个默认值");
    assert_eq!(mt(r#"{"max_tokens":"64000"}"#), None, "字符串不算——上游认的是整数");
    assert_eq!(super::request_max_tokens(None), None, "body 不是 JSON 时同样读不出");
}
