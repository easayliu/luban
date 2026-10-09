use crate::proxy::test_support::{gzip, rl_headers};
use crate::proxy::{Bytes, HeaderValue, UsageSniffer, config, header, store};

/// 拼回来的响应与原件同形：状态码、版本、头（含不放行给客户端的那些）、体都在。
#[tokio::test]
async fn rebuild_response_keeps_status_headers_and_body() {
    let mut h = crate::proxy::HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert("request-id", HeaderValue::from_static("req_011abc"));
    let body = Bytes::from_static(br#"{"type":"error"}"#);
    let up = super::rebuild_response(
        crate::proxy::StatusCode::FORBIDDEN,
        axum::http::Version::HTTP_2,
        h,
        body.clone(),
    );
    assert_eq!(up.status(), crate::proxy::StatusCode::FORBIDDEN);
    assert_eq!(up.version(), axum::http::Version::HTTP_2);
    assert_eq!(up.headers()["request-id"], "req_011abc");
    assert_eq!(super::resp_shape(&up), (false, None));
    assert_eq!(up.bytes().await.unwrap(), body);
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
    let cred = store.insert("t", None, "a", "r", 0, None, None, 1).unwrap();
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
            key_id: None,
            cred_label: cred.label.clone(),
            device_id: None,
            status: 200,
            sse_aggregated: false,
            sniffer,
            req_speed: None,
            req_model: Some("claude-opus-5".into()),
            ratelimit: rl_headers(&[]),
            stream_broke: None,
            upstream_done: false,
            request_id: "lb-test".into(),
            client_request_id: None,
            upstream_request_id: None,
            forensics: Default::default(),
            telemetry: None,
            cc_session: None,
            cc_thread: None,
            empty_reply_key: None,
            prompt_key: None,
            app_key: None,
            empty_replies: Default::default(),
            injected_tools: Vec::new(),
            tools_filled: false,
            store: store.clone(),
            _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
            _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(Default::default()),
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
    let cred = store.insert("t", None, "a", "r", 0, None, None, 1).unwrap();
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
            key_id: None,
            cred_label: cred.label.clone(),
            device_id: None,
            status: 200,
            sse_aggregated: false,
            sniffer,
            req_speed: None,
            req_model: Some("claude-fable-5".into()),
            ratelimit: rl_headers(&[]),
            stream_broke: None,
            upstream_done: false,
            request_id: "lb-test".into(),
            client_request_id: None,
            upstream_request_id: None,
            forensics: Default::default(),
            telemetry: None,
            cc_session: None,
            cc_thread: None,
            empty_reply_key: key.map(|(m, n)| (m.to_string(), n)),
            prompt_key: prompt.map(|(m, d)| (m.to_string(), d.to_string())),
            // 与提示词哈希同源的 system 哈希：按应用学的那条与按提示词学的那条一起验。
            app_key: prompt.map(|(m, d)| (m.to_string(), format!("app-{d}"))),
            empty_replies: mem.clone(),
            injected_tools: Vec::new(),
            tools_filled: false,
            store: store.clone(),
            _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
            _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(Default::default()),
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
    assert!(mem.read().prompts.contains_key(&("claude-opus-5".to_string(), "cafe".to_string())));
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
        logs[3].forensics.response_excerpt.as_deref().unwrap().starts_with("event: message_start")
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
        rows.iter().all(|r| r.kind != "refusal" || matches!(r.value.as_str(), "deadbeef" | "cafe")),
        "落库的拒答规则只有分类器判决那两条"
    );
}

/// 两份 UA 各存各的：入站记来访那份、出站记实际发出去那份，`-` 占位一律还原成 NULL
/// （存进去就成了一个真实存在的 UA，按 UA 分组时会凭空多出一类）。
#[test]
fn client_ua_lands_in_the_usage_log() {
    let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
    let cred = store.insert("t", None, "a", "r", 0, None, None, 1).unwrap();
    let log = |ua: &str, ua_out: &str| {
        drop(crate::proxy::ReqLog {
            started: std::time::Instant::now(),
            ttft_ms: None,
            method: "POST".into(),
            path: "/v1/messages?beta=true".into(),
            ua: ua.into(),
            ua_out: ua_out.into(),
            cred_id: cred.id,
            key_id: None,
            cred_label: cred.label.clone(),
            device_id: None,
            status: 200,
            sse_aggregated: false,
            sniffer: crate::proxy::UsageSniffer::new(false, false),
            req_speed: None,
            req_model: None,
            ratelimit: rl_headers(&[]),
            stream_broke: None,
            upstream_done: false,
            request_id: "lb-test".into(),
            client_request_id: None,
            upstream_request_id: None,
            forensics: Default::default(),
            telemetry: None,
            cc_session: None,
            cc_thread: None,
            empty_reply_key: None,
            prompt_key: None,
            app_key: None,
            empty_replies: Default::default(),
            injected_tools: Vec::new(),
            tools_filled: false,
            store: store.clone(),
            _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
            _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(Default::default()),
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
    let cred = store.insert("t", None, "a", "r", 0, None, None, 1).unwrap();
    let build = |injected: Vec<&'static str>| crate::proxy::ReqLog {
        started: std::time::Instant::now(),
        ttft_ms: None,
        method: "POST".into(),
        path: "/v1/messages?beta=true".into(),
        ua: "Go-http-client/1.1".into(),
        ua_out: config::CC_USER_AGENT.into(),
        cred_id: cred.id,
        key_id: None,
        cred_label: cred.label.clone(),
        device_id: None,
        status: 200,
        sse_aggregated: false,
        sniffer: crate::proxy::UsageSniffer::new(true, false),
        req_speed: None,
        req_model: Some("claude-opus-5".into()),
        ratelimit: rl_headers(&[]),
        stream_broke: None,
        upstream_done: false,
        request_id: "lb-test".into(),
        client_request_id: None,
        upstream_request_id: None,
        forensics: Default::default(),
        telemetry: None,
        cc_session: None,
        cc_thread: None,
        empty_reply_key: None,
        prompt_key: None,
        app_key: None,
        empty_replies: Default::default(),
        injected_tools: injected,
        tools_filled: false,
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
    // 来访没带工具、替它补了（`fill_absent_tools`）：打 `tools_filled`；模型真调了注入的
    // 工具再加一个 `injected_tool_called`。
    let mut rl = build(vec!["Bash", "Edit", "Read", "Write"]);
    rl.tools_filled = true;
    rl.sniffer.feed(&reply("mcp__luban__query_bas00"));
    drop(rl);
    let mut rl = build(vec!["Bash", "Edit", "Read", "Write"]);
    rl.tools_filled = true;
    rl.sniffer.feed(&reply("Bash"));
    drop(rl);

    let logs = store.list_usage_logs(10).unwrap();
    assert_eq!(logs.len(), 5);
    // list 按时间倒序：最后写入的在前。
    let tags: Vec<Option<&str>> =
        logs.iter().rev().map(|l| l.forensics.rewrites.as_deref()).collect();
    assert_eq!(
        tags,
        [
            Some("injected_tool_called"),
            None,
            None,
            Some("tools_filled"),
            Some("injected_tool_called,tools_filled"),
        ],
        "{tags:?}"
    );
    assert_eq!(logs[4].status, 200, "标签不改状态码与记账");
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
    let cred = store.insert("t", None, "a", "r", 0, None, None, 1).unwrap();
    let mut rl = crate::proxy::ReqLog {
        started: std::time::Instant::now(),
        ttft_ms: None,
        method: "POST".into(),
        path: "/v1/messages?beta=true".into(),
        ua: "-".into(),
        ua_out: "-".into(),
        cred_id: cred.id,
        key_id: None,
        cred_label: cred.label.clone(),
        device_id: None,
        status: 200,
        sse_aggregated: false,
        sniffer: crate::proxy::UsageSniffer::new(true, false),
        req_speed: None,
        req_model: None,
        ratelimit: rl_headers(&[]),
        stream_broke: None,
        upstream_done: false,
        request_id: "lb-test".into(),
        client_request_id: None,
        upstream_request_id: None,
        forensics: Default::default(),
        telemetry: None,
        cc_session: None,
        cc_thread: None,
        empty_reply_key: None,
        prompt_key: None,
        app_key: None,
        empty_replies: Default::default(),
        injected_tools: Vec::new(),
        tools_filled: false,
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
    let cred = store.insert("t", None, "a", "r", 0, None, None, 1).unwrap();
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
        key_id: None,
        cred_label: cred.label.clone(),
        device_id: None,
        status: 200,
        sse_aggregated: false,
        sniffer: crate::proxy::UsageSniffer::new(false, false),
        req_speed: None,
        req_model: None,
        ratelimit: rl_headers(&[]),
        stream_broke: None,
        upstream_done: false,
        request_id: "lb-test".into(),
        client_request_id: None,
        upstream_request_id: Some("req_first".into()),
        // 形态摘要建记录时就算好（生产路径同此：改写那一步顺手交出的 `Value`），
        // `note_retry` 负责换成重试实际发出去的那份。
        forensics: store::Forensics {
            shape: crate::proxy::shape_summary(&first).shape,
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
            agent: Default::default(),
            organization_id: None,
            started_at: std::time::SystemTime::now(),
        }),
        cc_session: None,
        cc_thread: None,
        empty_reply_key: None,
        prompt_key: None,
        app_key: None,
        empty_replies: Default::default(),
        injected_tools: Vec::new(),
        tools_filled: false,
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
    rl.note_retry("no_prefill", &h, &retried, None);
    assert_eq!(rl.upstream_request_id.as_deref(), Some("req_retry"));
    drop(rl);

    // 取走这一批（把「到期」时间推远，攒批规则就不拦着了）。
    let flushes = sink.take_due(std::time::Instant::now() + std::time::Duration::from_secs(3_600));
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
    assert_eq!(ctype.as_deref(), Some("application/json"), "上游那份 text/event-stream 必须被替掉");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("回给客户端的必须是整段 JSON");
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
    let cred = store.insert("t", None, "a", "r", 0, None, None, 1).unwrap();
    crate::proxy::ReqLog {
        started: std::time::Instant::now(),
        ttft_ms: None,
        method: "POST".into(),
        path: "/v1/messages".into(),
        ua: "-".into(),
        ua_out: "-".into(),
        cred_id: cred.id,
        key_id: None,
        cred_label: cred.label,
        device_id: None,
        status: 200,
        sse_aggregated: false,
        sniffer: crate::proxy::UsageSniffer::new(true, false),
        req_speed: None,
        req_model: None,
        ratelimit: rl_headers(&[]),
        stream_broke: None,
        upstream_done: false,
        request_id: "lb-test".into(),
        client_request_id: None,
        upstream_request_id: None,
        forensics: Default::default(),
        telemetry: None,
        cc_session: None,
        cc_thread: None,
        empty_reply_key: None,
        prompt_key: None,
        app_key: None,
        empty_replies: Default::default(),
        injected_tools: Vec::new(),
        tools_filled: false,
        store,
        _in_flight: crate::proxy::InFlightGuard::new(Default::default()),
        _session_concurrency: crate::proxy::SessionConcurrencyGuard::dummy(Default::default()),
        _route_load: crate::proxy::note_upstream_send(&Default::default(), 0, "-", 0),
    }
}
