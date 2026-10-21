use crate::proxy::test_support::gzip;
use crate::proxy::{ShapeBits, UsageSniffer, shape_summary};

/// 上游用了我们没开的编码时，只能跳过嗅探——但不得崩、不得把压缩字节当明文解析。
#[test]
fn unknown_encoding_is_skipped_not_misparsed() {
    let mut s = UsageSniffer::new(true, true);
    s.feed(&gzip(b"data: {\"usage\":{\"input_tokens\":999}}\n"));
    s.finish();
    assert!(!s.has_usage(), "解不开的响应体不应被当明文解析出用量");
    assert_eq!(s.model, None);
}

/// 落库的形态摘要：留住对照维度（顶层 key 顺序、system 哈希与块数、工具名、消息块计数、
/// 顶层参数），取出 session_id，且一个字的用户正文都不进去。
#[test]
fn shape_summary_keeps_shape_extracts_session_and_drops_user_text() {
    let body = serde_json::json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "我的银行卡号是 1234"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "tool_use", "name": "Bash", "input": {"command": "rm -rf secret"}},
                ]},
                {"role": "user", "content": "plain string turn"},
            ],
            "system": [
                {"type": "text", "text": "You are Claude Code.", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "private project notes"},
            ],
            "tools": [{"name": "Bash", "input_schema": {}}, {"name": "Read", "input_schema": {}}],
            "metadata": {"user_id": "user_ab12_account_cd34_session_9f8e7d6c-0000-1111-2222-333344445555"},
            "max_tokens": 32000,
            "stream": true,
            "thinking": {"type": "enabled", "budget_tokens": 1024}});
    let bytes = serde_json::to_vec(&body).unwrap();
    let ShapeBits { shape, session_id: session, device_id_out: device, .. } = shape_summary(&bytes);
    let shape = shape.expect("对象体必有摘要");
    assert_eq!(session.as_deref(), Some("9f8e7d6c-0000-1111-2222-333344445555"));
    assert_eq!(device.as_deref(), Some("ab12"), "扁平串的 device 段落进 device_id_out");
    let v: serde_json::Value = serde_json::from_str(&shape).unwrap();
    assert_eq!(
        v["keys"],
        serde_json::json!([
            "model",
            "messages",
            "system",
            "tools",
            "metadata",
            "max_tokens",
            "stream",
            "thinking"
        ]),
        "顶层 key 顺序是判据，要原样留住"
    );
    assert_eq!(v["system"]["blocks"].as_array().unwrap().len(), 2);
    assert_eq!(v["system"]["blocks"][0]["cache"], true);
    assert_eq!(v["system"]["sha"].as_str().unwrap().len(), 16);
    assert_eq!(v["tools"]["count"], 2);
    assert_eq!(v["tools"]["names"], serde_json::json!(["Bash", "Read"]));
    assert_eq!(v["messages"]["count"], 3);
    assert_eq!(v["messages"]["last_role"], "user");
    assert_eq!(v["messages"]["blocks"]["tool_use"], 1);
    assert_eq!(v["messages"]["blocks"]["thinking"], 1);
    assert_eq!(v["messages"]["blocks"]["text"], 2, "字符串 content 也算一块 text");
    assert_eq!(v["metadata"]["user_id"], true);
    assert_eq!(v["thinking"]["budget_tokens"], 1024);
    assert_eq!(v["max_tokens"], 32000);
    for leak in ["1234", "rm -rf", "private project", "You are Claude", "hmm", "user_ab12"] {
        assert!(!shape.contains(leak), "正文/身份不得进摘要: {leak} in {shape}");
    }
    // 非 JSON 体没有摘要，也不 panic。
    assert_eq!(shape_summary(b"not json"), ShapeBits::default());
}

/// CC 内嵌 JSON 格式的 `metadata.user_id`：device_id / session_id 都要认出来；
/// 没带 metadata 时两项为 `None`，摘要照出。
#[test]
fn shape_summary_reads_embedded_json_identity() {
    let body = serde_json::json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"user_id": "{\"device_id\":\"d230ce6e1111\",\"account_uuid\":\"acct\",\"session_id\":\"sess-1\"}"}});
    let bits = shape_summary(&serde_json::to_vec(&body).unwrap());
    assert!(bits.shape.is_some());
    assert_eq!(bits.session_id.as_deref(), Some("sess-1"));
    assert_eq!(bits.device_id_out.as_deref(), Some("d230ce6e1111"));

    let bare = serde_json::json!({"model": "claude-opus-5", "messages": []});
    let bits = shape_summary(&serde_json::to_vec(&bare).unwrap());
    assert!(bits.shape.is_some(), "没带 metadata 也要有摘要");
    assert_eq!((bits.session_id, bits.device_id_out), (None, None));
}

/// 嗅探器留下响应体开头：流式非流式都留、封顶不无界、截在多字节字符中间时丢掉半个字。
#[test]
fn the_sniffer_keeps_a_bounded_excerpt_of_the_raw_reply() {
    let mut s = crate::proxy::UsageSniffer::new(false, false);
    assert!(s.excerpt().is_none(), "一个字节都没收到时没有摘录");
    s.feed(br#"{"id":"msg_1","content":[],"stop_reason":"end_turn","#);
    s.feed(br#""usage":{"input_tokens":425,"output_tokens":0}}"#);
    s.finish();
    assert_eq!(s.output_tokens, Some(0));
    assert_eq!(
        s.excerpt().as_deref(),
        Some(
            r#"{"id":"msg_1","content":[],"stop_reason":"end_turn","usage":{"input_tokens":425,"output_tokens":0}}"#
        )
    );

    // 流式：逐行解析吃掉 `buf`，摘录仍是原样的字节。
    let mut st = crate::proxy::UsageSniffer::new(true, false);
    st.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"output_tokens\":0}}}\n\n");
    assert!(st.excerpt().unwrap().starts_with("event: message_start\ndata: "));

    // 封顶：超过上限的部分不留；截在「中」字中间时丢掉半个字符，不出替换符。
    let mut big = crate::proxy::UsageSniffer::new(false, false);
    let filler = "x".repeat(crate::proxy::RESPONSE_EXCERPT_BYTES - 1);
    big.feed(filler.as_bytes());
    big.feed("中文".as_bytes());
    let ex = big.excerpt().unwrap();
    assert_eq!(ex.len(), crate::proxy::RESPONSE_EXCERPT_BYTES - 1);
    assert!(ex.ends_with('x'));
    assert!(!ex.contains('\u{FFFD}'));

    // 解不开的编码：什么都不留。
    let mut opaque = crate::proxy::UsageSniffer::new(false, true);
    opaque.feed(b"gzip bytes");
    assert!(opaque.excerpt().is_none());
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

/// `message_stop` 是流正常收尾的唯一标志。缺了它、又没有 error 事件、连接层也没报错，
/// 是最安静的那种断流：这一层看什么都正常，客户端拿到的却是半截回复
/// （Claude Code 报 `Connection closed mid-response`）。
#[test]
fn message_stop_marks_a_complete_stream() {
    let mut truncated = crate::proxy::UsageSniffer::new(true, false);
    truncated.feed(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\"}}\n\n",
        );
    truncated.feed(
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":14}}\n\n",
        );
    assert!(!truncated.saw_message_stop, "流断在半路，收尾时要告警");
    assert_eq!(truncated.output_tokens, Some(14), "已生成的部分照旧计入用量");
    // 断点定位：光看 output_tokens 分不出「刚开口就断」和「只差收尾」，事件类型才行。
    assert_eq!(truncated.last_event.as_deref(), Some("message_delta"));
    assert_eq!(truncated.events, 2);

    let mut complete = crate::proxy::UsageSniffer::new(true, false);
    complete.feed(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    assert!(complete.saw_message_stop);
    assert_eq!(complete.last_event.as_deref(), Some("message_stop"));
}

/// `ping` 也算进 `events`（断流定位要它），但另记一份：2.1.305 的 `streamEventCount` 跳过 ping，
/// 抓包里每条都是事件总数减去那一个 ping。
#[test]
fn pings_are_counted_apart() {
    let mut s = crate::proxy::UsageSniffer::new(true, false);
    s.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n");
    s.feed(b"event: ping\ndata: {\"type\": \"ping\"}\n\n");
    s.feed(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    assert_eq!((s.events, s.pings), (3, 1));
}

/// 非流式响应体里的 `{"type":"error"}` 不走流内那套：那条路的 4xx 由
/// [`crate::proxy::classify_account_rejection`] 一侧处理，这里再记一份会让同一个错误告警两次。
/// 但**类型与文案照记**（`body_error`）——`tengu_api_error` 的 `errorType`/`error`
/// 要它，而 [`crate::proxy::capture_forensics`] 那份只在 400/401/403 与裸 429 上填。
#[test]
fn nonstream_error_body_is_not_taken_as_a_stream_error() {
    let mut s = crate::proxy::UsageSniffer::new(false, false);
    s.feed(br#"{"type":"error","error":{"type":"invalid_request_error","message":"nope"}}"#);
    s.finish();
    assert!(s.stream_error.is_none());
    assert_eq!(
        s.body_error,
        Some((Some("invalid_request_error".into()), "nope".into())),
        "错误体的类型与文案要留下来"
    );
}

/// `toolUseContentLengths`：流式下工具入参是 `input_json_delta` 一片片来的，按内容块
/// 序号拼回去；同名工具累加；键按首次出现排序；`mcp__*` 归成 `mcp_tool`。
/// 顺带钉住思考块的识别——`redacted_thinking` 一个字都没有，靠字数判定认不出来。
/// 同一条流无论被切成什么样的块，嗅探结果必须逐项相同。
///
/// 切行那段改过一次（逐行 `drain` + `collect` 换成扫完一次性 `drain`，见 [`UsageSniffer::feed`]），
/// 而它的正确性全压在「跨块的半行要留到下一块再拼」这一条上。这里拿三种切法对同一条流跑：
/// 整段一次喂、每行一块、以及**逐字节**喂（每个事件都被切得稀碎，最恶劣的一种）。
#[test]
fn the_sniffer_is_indifferent_to_how_the_stream_is_chunked() {
    let events = [
        r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":11,"cache_read_input_tokens":22}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Bash","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        // 多字节正文：按字节切一定会切在半个汉字中间。
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"一二三四五"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":33}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let wire: Vec<u8> =
        events.iter().flat_map(|e| format!("event: x\ndata: {e}\n\n").into_bytes()).collect();

    let run = |chunks: Vec<&[u8]>| {
        let mut s = crate::proxy::UsageSniffer::new(true, false);
        for c in chunks {
            s.feed(c);
        }
        s.finish();
        s
    };
    let whole = run(vec![&wire]);
    let per_byte = run(wire.chunks(1).collect());
    // 7 字节一块：与行长互质，故切点会落在行内各处。
    let ragged = run(wire.chunks(7).collect());

    for (label, got) in [("逐字节", &per_byte), ("7 字节一块", &ragged)] {
        assert_eq!(got.model, whole.model, "{label}");
        assert_eq!(got.message_id, whole.message_id, "{label}");
        assert_eq!(got.input_tokens, whole.input_tokens, "{label}");
        assert_eq!(got.output_tokens, whole.output_tokens, "{label}");
        assert_eq!(got.cache_read_tokens, whole.cache_read_tokens, "{label}");
        assert_eq!(got.stop_reason, whole.stop_reason, "{label}");
        assert_eq!(got.text_chars, whole.text_chars, "{label}");
        assert_eq!(got.reply_fp(), whole.reply_fp(), "{label}：回复指纹");
        assert_eq!(got.events, whole.events, "{label}：事件计数");
        assert_eq!(got.last_event, whole.last_event, "{label}");
        assert_eq!(got.saw_message_stop, whole.saw_message_stop, "{label}");
        assert_eq!(got.tool_use_lens(), whole.tool_use_lens(), "{label}：工具入参长度");
    }
    // 这条流本身得真的被认出来了，否则上面比的是一堆空值。
    assert_eq!(whole.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(whole.events, 8);
    assert_eq!(whole.text_chars, 5, "五个汉字，按 UTF-16 码元数");
    assert!(whole.saw_message_stop);
}

/// 回复指纹：流式的 `text_delta` / `thinking_delta` / `input_json_delta` 分几段来，与客户端
/// 下一轮带回的那条 assistant（入参重新序列化过、键序变了）按同一算法算出同一个数
/// （[`crate::proxy::session_link::ReplyFp`]）；非流式整段 `content[]` 也一样。改了正文、
/// 工具入参（id 不变）或 thinking 的都对不上。
#[test]
fn the_sniffer_fingerprints_the_reply_like_the_client_copy() {
    let wire = [
            r#"data: {"type":"message_start","message":{"id":"msg_x","usage":{"input_tokens":1}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"先看"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"目录"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"我看"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"一下"}}"#,
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\": \"ls\","}}"#,
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":" \"timeout\": 5}"}}"#,
            r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"。"}}"#,
            r#"data: {"type":"message_stop"}"#,
        ]
        .join("\n\n")
            + "\n\n";
    let mut s = crate::proxy::UsageSniffer::new(true, false);
    s.feed(wire.as_bytes());
    s.finish();
    let client = |content: serde_json::Value| {
        crate::proxy::body::thread_msg_of(
            &serde_json::json!({ "role": "assistant", "content": content }),
        )
        .reply
    };
    let blocks = |thinking: &str, text: &str, command: &str| {
        serde_json::json!([
            { "type": "thinking", "thinking": thinking, "signature": "sig" },
            { "type": "text", "text": text },
            { "type": "tool_use", "id": "toolu_1", "name": "Bash",
              "input": { "timeout": 5, "command": command } },
            { "type": "text", "text": "。" },
        ])
    };
    assert_eq!(s.reply_fp(), client(blocks("先看目录", "我看一下", "ls")));
    assert_eq!(s.client_tool_use_ids(), vec!["toolu_1".to_string()]);
    assert_ne!(s.reply_fp(), client(blocks("先看目录", "我改过了", "ls")), "改了正文");
    assert_ne!(s.reply_fp(), client(blocks("先看目录", "我看一下", "rm -rf x")), "改了入参");
    assert_ne!(s.reply_fp(), client(blocks("改过的推理", "我看一下", "ls")), "改了 thinking");

    let body = r#"{"id":"msg_y","type":"message","content":[{"type":"thinking","thinking":"先看目录","signature":"sig"},{"type":"text","text":"我看一下"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls","timeout":5}},{"type":"text","text":"。"}]}"#;
    let mut n = crate::proxy::UsageSniffer::new(false, false);
    n.feed(body.as_bytes());
    n.finish();
    assert_eq!(n.reply_fp(), s.reply_fp(), "非流式同一个数");
}

/// 服务端工具与引用也进回复指纹（形态照 `cap/auto-2.1.285-20260930/00056`：`server_tool_use`
/// 入参走 `input_json_delta`、`web_search_tool_result` 整块在 `content_block_start` 里、引用走
/// `citations_delta`）。正文不变、只改搜索结果 / 服务端工具入参 / 引用 URL 的都对不上；见过
/// 认不出的增量类型的，这条回复记成拼不出原貌，与谁都对不上。
#[test]
fn the_reply_fingerprint_covers_server_tools_and_citations() {
    let result = serde_json::json!({ "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
            "content": [{ "type": "web_search_result", "title": "CPython", "url": "https://a.example/",
                          "encrypted_content": "Et8Q", "page_age": null }] });
    let citation = serde_json::json!({ "type": "web_search_result_location", "cited_text": "Guido",
            "url": "https://a.example/", "title": "CPython", "encrypted_index": "Eo8B" });
    let events = |extra: &str| {
        let mut e = vec![
                r#"{"type":"message_start","message":{"id":"msg_s","usage":{"input_tokens":1}}}"#.to_string(),
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}"#.to_string(),
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\": "}}"#.to_string(),
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"cpython\"}"}}"#.to_string(),
                format!(r#"{{"type":"content_block_start","index":1,"content_block":{result}}}"#),
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#.to_string(),
                format!(r#"{{"type":"content_block_delta","index":2,"delta":{{"type":"citations_delta","citation":{citation}}}}}"#),
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"作者是 Guido"}}"#.to_string(),
            ];
        if !extra.is_empty() {
            e.push(extra.to_string());
        }
        e.push(r#"{"type":"message_stop"}"#.to_string());
        e.iter().map(|x| format!("data: {x}\n\n")).collect::<String>()
    };
    let sniff = |wire: String| {
        let mut s = crate::proxy::UsageSniffer::new(true, false);
        s.feed(wire.as_bytes());
        s.finish();
        s.reply_fp()
    };
    let client = |query: &str, url: &str, cite_url: &str| {
        let mut r = result.clone();
        r["content"][0]["url"] = url.into();
        let mut c = citation.clone();
        c["url"] = cite_url.into();
        crate::proxy::body::thread_msg_of(&serde_json::json!({ "role": "assistant", "content": [
                { "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": { "query": query } },
                r,
                { "type": "text", "text": "作者是 Guido", "citations": [c] },
            ] }))
            .reply
            .as_upstream()
    };
    let a = "https://a.example/";
    let got = sniff(events(""));
    assert_eq!(got, client("cpython", a, a), "原样带回对得上");
    assert_ne!(got, client("pypy", a, a), "改了服务端工具入参");
    assert_ne!(got, client("cpython", "https://evil.example/", a), "改了搜索结果");
    assert_ne!(got, client("cpython", a, "https://evil.example/"), "改了引用 URL");
    let odd = sniff(events(
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"future_delta","x":1}}"#,
    ));
    assert_ne!(odd, client("cpython", a, a), "认不出的增量：拼不出原貌，不给接");
}

/// 零参数工具（`cap/auto-2.1.285-20260930/00164` 的 ExitPlanMode）：`content_block_start` 的
/// `input: {}` 之后只来一条空的 `partial_json`。空增量不作废占位，入参仍是 `{}`，客户端原样
/// 带回照常对得上（官方下一轮 `00167` 就是接着它 `continue` 的）。
#[test]
fn an_empty_json_delta_keeps_the_placeholder_input() {
    let wire = [
            r#"data: {"type":"message_start","message":{"id":"msg_p","usage":{"input_tokens":1}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_p","name":"ExitPlanMode","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ]
        .join("\n\n")
            + "\n\n";
    let mut s = crate::proxy::UsageSniffer::new(true, false);
    s.feed(wire.as_bytes());
    s.finish();
    let client =
        crate::proxy::body::thread_msg_of(&serde_json::json!({ "role": "assistant", "content": [
            { "type": "tool_use", "id": "toolu_p", "name": "ExitPlanMode", "input": {} },
        ] }))
        .reply
        .as_upstream();
    assert_eq!(s.reply_fp(), client);
}

#[test]
fn the_sniffer_collects_tool_use_input_lengths() {
    let mut s = crate::proxy::UsageSniffer::new(true, false);
    let feed = |s: &mut crate::proxy::UsageSniffer, line: &str| {
        s.feed(format!("data: {line}\n\n").as_bytes());
    };
    feed(&mut s, r#"{"type":"message_start","message":{"model":"claude-opus-5"}}"#);
    feed(
        &mut s,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"xx"}}"#,
    );
    feed(
        &mut s,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"Bash","input":{}}}"#,
    );
    feed(
        &mut s,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\""}}"#,
    );
    feed(
        &mut s,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":":\"ls\"}"}}"#,
    );
    feed(
        &mut s,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t2","name":"Bash","input":{}}}"#,
    );
    feed(
        &mut s,
        r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"t3","name":"mcp__ide__getDiagnostics","input":{}}}"#,
    );
    feed(&mut s, r#"{"type":"message_stop"}"#);
    assert!(s.saw_thinking, "redacted_thinking 也算思考块");
    // `{"command":"ls"}` 是 16 个字符；没有增量的那个块是空 `{}` = 2；两个都叫 Bash，加起来 18。
    assert_eq!(s.tool_use_lens(), vec![("Bash".to_string(), 18), ("mcp_tool".to_string(), 2)]);
}

/// 对着抓包钉住那个长度：`cap/2.1.260-2/00065` 里那条 `stop=tool_use` 的
/// `tengu_api_success` 报的是 `toolUseContentLengths: '{"Bash":166}'`，而同一个
/// `tool_use` 块的 `input`（`00061` 的 `messages[5]`）紧凑序列化正好 166 字符。
/// 官方量的是 `JSON.stringify(input).length`——不带空格的那份。
#[test]
fn tool_use_length_matches_the_capture_when_it_is_present() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/cap/2.1.260-2/00061_174309.489.req.raw");
    let Ok(raw) = std::fs::read(path) else {
        eprintln!("skipped: {path} not present");
        return;
    };
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("http headers") + 4;
    let v: serde_json::Value = serde_json::from_slice(&raw[sep..]).unwrap();
    // 抓包里的 `tool_use` 块原样搬成一条非流式回复喂给嗅探器。
    let block = v["messages"][5]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("那条 assistant 消息里有一个 tool_use 块")
        .clone();
    let thinking = v["messages"][5]["content"][0].clone();
    let body = serde_json::json!({
        "id": "msg_x",
        "model": "claude-opus-5",
        "content": [thinking, block],
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    let mut s = crate::proxy::UsageSniffer::new(false, false);
    s.feed(&serde_json::to_vec(&body).unwrap());
    s.finish();
    assert_eq!(s.tool_use_lens(), vec![("Bash".to_string(), 166)]);
    // 那条回复只有一个空思考块加一个工具块：`textContentLength` 与
    // `thinkingContentLength` 抓包里都是 0，且后者**存在**——靠字数判不出来，靠块类型。
    assert_eq!(s.text_chars, 0);
    assert_eq!(s.thinking_chars, 0);
    assert!(s.saw_thinking, "空思考块也要算");
}

/// 非流式回复里的工具块同样要认（`content[]` 直接带完整 `input`）。
#[test]
fn the_sniffer_collects_tool_use_lengths_from_a_nonstream_body() {
    let mut s = crate::proxy::UsageSniffer::new(false, false);
    s.feed(
            br#"{"id":"msg_1","model":"claude-opus-5","content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/a"}}],"usage":{"input_tokens":1}}"#,
        );
    s.finish();
    assert!(!s.saw_thinking);
    // serde 重新序列化后的 `{"file_path":"/a"}` = 18 个字符。
    assert_eq!(s.tool_use_lens(), vec![("Read".to_string(), 18)]);
}

/// 回复里的 `tool_use` **原名**另记一份：遥测那张表把 `mcp__*` 归成 `mcp_tool`，答不了
/// 「模型到底调了哪个」。server tool 与 MCP server 的调用不算——上游自己跑完，客户端拿
/// 不到那个 tool_use。同名多次只记一次。
#[test]
fn the_sniffer_keeps_raw_tool_use_names_for_client_side_blocks() {
    let mut s = crate::proxy::UsageSniffer::new(true, false);
    let block = |i: u32, ty: &str, name: &str| -> Vec<u8> {
        const EV: &str = "event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":IDX,\"content_block\":{\"type\":\"TY\",\"id\":\"tIDX\",\"name\":\"NAME\",\"input\":{}}}

";
        EV.replace("IDX", &i.to_string()).replace("TY", ty).replace("NAME", name).into_bytes()
    };
    s.feed(&block(0, "tool_use", "Bash"));
    s.feed(&block(1, "tool_use", "mcp__luban__query_bas00"));
    s.feed(&block(2, "server_tool_use", "web_search"));
    s.feed(&block(3, "mcp_tool_use", "mcp__ide__getDiagnostics"));
    s.feed(&block(4, "tool_use", "Bash"));
    assert_eq!(s.tool_use_names(), ["Bash", "mcp__luban__query_bas00"]);
    // 遥测那张表照旧按归类名走，不受影响。
    let lens: Vec<String> = s.tool_use_lens().into_iter().map(|(n, _)| n).collect();
    assert_eq!(lens, ["Bash", "mcp_tool", "web_search"]);
}

/// 回复按 `inputTextCharLength` 口径的字数：原名、只算客户端执行的 `tool_use`。
#[test]
fn reply_input_chars_counts_raw_client_side_tool_uses() {
    let mut s = crate::proxy::UsageSniffer::new(true, false);
    let block = |i: u32, ty: &str, name: &str| -> Vec<u8> {
        const EV: &str = "event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":IDX,\"content_block\":{\"type\":\"TY\",\"id\":\"tIDX\",\"name\":\"NAME\",\"input\":{\"a\":1}}}

";
        EV.replace("IDX", &i.to_string()).replace("TY", ty).replace("NAME", name).into_bytes()
    };
    s.feed(&block(0, "tool_use", "mcp__ide__getDiagnostics"));
    s.feed(&block(1, "server_tool_use", "web_search"));
    s.feed(&block(2, "mcp_tool_use", "mcp__remote__x"));
    // 客户端那一块按原名 24 字 + `{"a":1}` 7 字；上游自己跑的两块不计。
    assert_eq!(s.reply_input_chars(), 24 + 7);
}

/// 走真实的嗅探路径：把 `cap/2.1.277/00031` 的响应（chunked + gzip 的 SSE）原样喂进去，
/// 得到的正是下一条续用请求少掉的那 280 字（正文 38 + `Bash` 与入参 242，事件 56026 与
/// 请求体可见部分之差）。
#[test]
fn reply_input_chars_matches_the_capture_when_it_is_present() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/cap/2.1.277");
    let Some(path) = std::fs::read_dir(dir).ok().and_then(|d| {
        d.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("00031_") && n.ends_with(".resp.raw"))
        })
    }) else {
        eprintln!("skipped: cap/2.1.277 not present");
        return;
    };
    let raw = std::fs::read(path).unwrap();
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    // 去掉 chunked 分块头，再解 gzip。
    let mut rest = &raw[sep..];
    let mut gz = Vec::new();
    loop {
        let eol = rest.windows(2).position(|w| w == b"\r\n").unwrap();
        let size_line = std::str::from_utf8(&rest[..eol]).unwrap();
        let n = usize::from_str_radix(size_line.split(';').next().unwrap().trim(), 16).unwrap();
        if n == 0 {
            break;
        }
        gz.extend_from_slice(&rest[eol + 2..eol + 2 + n]);
        rest = &rest[eol + 2 + n + 2..];
    }
    let mut sse = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&gz[..]), &mut sse).unwrap();
    let mut s = crate::proxy::UsageSniffer::new(true, false);
    // 按小块喂，顺带验证跨块拼接。
    for chunk in sse.chunks(97) {
        s.feed(chunk);
    }
    assert_eq!(s.reply_input_chars(), 280);
}

/// 回复里的 thinking 签名与 `redacted_thinking` 密文都被记成指纹：流式的签名按块把 `signature_delta`
/// 分片拼成整段、`content_block_stop` 时登记（没等到 stop 的在收尾时登记），`redacted_thinking` 从
/// `content_block_start` 取，`content_block_start` 里的空签名不算；非流式看顶层 `content[]`。
#[test]
fn sniffer_fingerprints_thinking_signatures_and_redacted_data() {
    let fp = crate::store::thinking_block_fingerprint;
    let mut s = UsageSniffer::new(true, false);
    for line in [
        r#"data: {"type":"message_start","message":{"id":"msg_1","model":"m","usage":{"input_tokens":1}}}"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"DATA_B"}}"#,
        r#"data: {"type":"content_block_stop","index":1}"#,
        r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
        r#"data: {"type":"content_block_stop","index":2}"#,
        r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
        r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"signature_delta","signature":"tail"}}"#,
        r#"data: {"type":"message_stop"}"#,
    ] {
        s.feed(format!("{line}\n\n").as_bytes());
    }
    s.finish();
    assert_eq!(
        s.thinking_blocks,
        vec![fp("sig-abc"), fp("DATA_B"), fp("tail")],
        "分片拼成整段；没等到 stop 的在 message_stop 时登记"
    );

    let mut j = UsageSniffer::new(false, false);
    j.feed(
        serde_json::json!({
            "id": "msg_2", "model": "m", "stop_reason": "end_turn",
            "content": [
                {"type": "thinking", "thinking": "hm", "signature": "SIG_C"},
                {"type": "redacted_thinking", "data": "DATA_D"},
                {"type": "text", "text": "ok"}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 2}
        })
        .to_string()
        .as_bytes(),
    );
    j.finish();
    assert_eq!(j.thinking_blocks, vec![fp("SIG_C"), fp("DATA_D")]);
}

/// 服务端压缩一次请求跑两轮采样：顶层 `usage` 只是最后一轮，计费按 `usage.iterations` 逐轮相加
/// （[`UsageSniffer::billed`]）；顶层字段照旧（遥测与上下文长度用它）。流式在最终的
/// `message_delta.usage` 里，非流式在顶层；数值取自 `cap/auto-2.1.303-20261019-compact/00172`。
#[test]
fn billed_usage_sums_the_compaction_iterations() {
    let iterations = r#"[{"input_tokens":48,"output_tokens":730,"cache_read_input_tokens":20084,"cache_creation_input_tokens":137152,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":137152},"type":"compaction"},{"input_tokens":6,"output_tokens":283,"cache_read_input_tokens":25369,"cache_creation_input_tokens":1221,"cache_creation":{"ephemeral_5m_input_tokens":1221,"ephemeral_1h_input_tokens":0},"type":"message"}]"#;
    let want = super::BilledUsage {
        input_tokens: Some(54),
        output_tokens: Some(1013),
        cache_creation_tokens: Some(138_373),
        cache_creation_5m: Some(1221),
        cache_creation_1h: Some(137_152),
        cache_read_tokens: Some(45_453),
    };

    let mut stream = UsageSniffer::new(true, false);
    stream.feed(
        br#"data: {"type":"message_start","message":{"model":"claude-opus-4-7","usage":{"input_tokens":48,"cache_creation_input_tokens":137152,"cache_read_input_tokens":20084,"output_tokens":1}}}"#,
    );
    stream.feed(b"\n\n");
    stream.feed(
        format!(
            r#"data: {{"type":"message_delta","delta":{{"stop_reason":"end_turn"}},"usage":{{"input_tokens":6,"cache_creation_input_tokens":1221,"cache_read_input_tokens":25369,"output_tokens":283,"iterations":{iterations}}}}}"#
        )
        .as_bytes(),
    );
    stream.feed(b"\n\n");
    stream.finish();
    assert_eq!(stream.billed(), want);
    assert_eq!((stream.input_tokens, stream.output_tokens), (Some(6), Some(283)), "顶层是最后一轮");

    let mut plain = UsageSniffer::new(false, false);
    plain.feed(
        format!(
            r#"{{"model":"claude-opus-4-7","usage":{{"input_tokens":6,"cache_creation_input_tokens":1221,"cache_read_input_tokens":25369,"output_tokens":283,"iterations":{iterations}}}}}"#
        )
        .as_bytes(),
    );
    plain.finish();
    assert_eq!(plain.billed(), want);

    // 没有 `iterations`（普通请求）：计费口径就是顶层。
    let mut single = UsageSniffer::new(false, false);
    single.feed(br#"{"model":"claude-opus-5","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":7}}"#);
    single.finish();
    let b = single.billed();
    assert_eq!(
        (b.input_tokens, b.output_tokens, b.cache_read_tokens),
        (Some(10), Some(5), Some(7))
    );
    assert_eq!(b.cache_creation_tokens, None);
}

/// 费用逐轮算（[`UsageSniffer::billed_cost`]），不能把各轮 token 加起来再按响应模型算一次。
#[test]
fn billed_cost_prices_each_iteration_on_its_own() {
    let sniff = |body: &str| {
        let mut s = UsageSniffer::new(false, false);
        s.feed(body.as_bytes());
        s.finish();
        s
    };
    let close = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;

    // Haiku 5.5 压缩：压缩前 70K、压缩后 40K，两轮各 1K 输出。各轮都在 100K 短提示档内，
    // 合计 110K 也不能触发长提示价：0.0075 + 0.0045 = 0.012（误按合计算是 0.060）。
    let haiku = sniff(
        r#"{"model":"claude-haiku-5-5","usage":{"input_tokens":40000,"output_tokens":1000,"iterations":[
            {"type":"compaction","input_tokens":70000,"output_tokens":1000},
            {"type":"message","input_tokens":40000,"output_tokens":1000}]}}"#,
    );
    assert!(
        close(haiku.billed_cost(Some("claude-haiku-5-5"), None, None, &[]), 0.012),
        "{:?}",
        haiku.billed_cost(Some("claude-haiku-5-5"), None, None, &[])
    );
    assert_eq!(haiku.billed().input_tokens, Some(110_000), "token 照样加总");

    // 服务端 fallback：主模型输出前就拒掉（535 输入、0 输出，回复里没给类别）不收钱，fallback 那轮按它
    // 自己的模型计价：412 × $5 + 264 × $25 = $0.008660（误把主模型那 535 一起按最终模型算是 $0.011335）。
    let fallback = sniff(
        r#"{"model":"claude-opus-5","usage":{"input_tokens":412,"output_tokens":264,"iterations":[
            {"type":"message","model":"claude-fable-5-1","input_tokens":535,"output_tokens":0},
            {"type":"fallback_message","model":"claude-opus-5","input_tokens":412,"output_tokens":264}]}}"#,
    );
    assert!(close(fallback.billed_cost(Some("claude-opus-5"), None, None, &[]), 0.008_660));
    assert_eq!(fallback.billed().input_tokens, Some(947), "被拒那轮的 token 仍计入用量");

    // 主模型拒答前已经流出了内容：那部分照常按主模型计价。
    let partial = sniff(
        r#"{"model":"claude-opus-5","usage":{"iterations":[
            {"type":"message","model":"claude-fable-5-1","input_tokens":1000,"output_tokens":100},
            {"type":"fallback_message","model":"claude-opus-5","input_tokens":412,"output_tokens":264}]}}"#,
    );
    let fable = 1000.0 * 10.0 / 1e6 + 100.0 * 50.0 / 1e6;
    assert!(close(partial.billed_cost(Some("claude-opus-5"), None, None, &[]), fable + 0.008_660));

    // 没有 iterations：与原来一样按顶层用量算。
    let plain =
        sniff(r#"{"model":"claude-opus-5","usage":{"input_tokens":412,"output_tokens":264}}"#);
    assert!(close(plain.billed_cost(Some("claude-opus-5"), None, None, &[]), 0.008_660));
}

/// 拒答按官方「How refusals are billed」逐次判：输出前就拒的只有 bio / frontier_llm /
/// reasoning_extraction 收钱，其余与 `null` 免费；拒答前已经出了内容的照常计；服务端 fallback 每次
/// 尝试的类别看回复里 `fallback` 块的 `trigger`，最后一次看顶层 `stop_details`；速度档逐次取。
#[test]
fn refusals_are_billed_by_category_and_attempt() {
    let sniff = |body: &str| {
        let mut s = UsageSniffer::new(false, false);
        s.feed(body.as_bytes());
        s.finish();
        s
    };
    let close = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;
    let est = |model: &str, speed: Option<&str>, input: i64, output: i64| {
        crate::pricing::estimate_usd(crate::pricing::Usage {
            model: Some(model),
            speed,
            input_tokens: Some(input),
            output_tokens: Some(output),
            ..Default::default()
        })
        .unwrap()
    };
    let fallback_block = |cat: &str| {
        format!(
            r#"{{"type":"fallback","from":{{"model":"claude-fable-5-1"}},"to":{{"model":"claude-opus-5"}},"trigger":{{"type":"refusal","category":{cat}}}}}"#
        )
    };

    // 1. 主模型输出前以 bio 拒掉、fallback 作答：bio 照收，535 × $10 + 0.008660 = $0.014010。
    let bio = sniff(&format!(
        r#"{{"model":"claude-opus-5","stop_reason":"end_turn","content":[{},{{"type":"text","text":"ok"}}],
            "usage":{{"input_tokens":412,"output_tokens":264,"iterations":[
            {{"type":"message","model":"claude-fable-5-1","input_tokens":535,"output_tokens":0}},
            {{"type":"fallback_message","model":"claude-opus-5","input_tokens":412,"output_tokens":264}}]}}}}"#,
        fallback_block(r#""bio""#)
    ));
    assert!(close(bio.billed_cost(Some("claude-opus-5"), None, None, &[]), 0.014_010));
    // 同样的形态换成 cyber：主模型那次免费，只剩 fallback 的 $0.008660。
    let cyber = sniff(&format!(
        r#"{{"model":"claude-opus-5","stop_reason":"end_turn","content":[{},{{"type":"text","text":"ok"}}],
            "usage":{{"input_tokens":412,"output_tokens":264,"iterations":[
            {{"type":"message","model":"claude-fable-5-1","input_tokens":535,"output_tokens":0}},
            {{"type":"fallback_message","model":"claude-opus-5","input_tokens":412,"output_tokens":264}}]}}}}"#,
        fallback_block(r#""cyber""#)
    ));
    assert!(close(cyber.billed_cost(Some("claude-opus-5"), None, None, &[]), 0.008_660));

    // 2. 主模型出了内容后被拒（照收：1000 × $10 + 100 × $50 = $0.015），fallback 随后以 cyber 输出前
    //    拒掉、成为最终回复：最后那次免费，合计 $0.015000（误把它也收了是 $0.017060）。
    let last_refused = sniff(&format!(
        r#"{{"model":"claude-opus-5","stop_reason":"refusal","stop_details":{{"type":"refusal","category":"cyber"}},
            "content":[{{"type":"text","text":"part"}},{}],
            "usage":{{"input_tokens":412,"output_tokens":0,"iterations":[
            {{"type":"message","model":"claude-fable-5-1","input_tokens":1000,"output_tokens":100}},
            {{"type":"fallback_message","model":"claude-opus-5","input_tokens":412,"output_tokens":0}}]}}}}"#,
        fallback_block("null")
    ));
    assert!(close(last_refused.billed_cost(Some("claude-opus-5"), None, None, &[]), 0.015));

    // 3. 主模型 standard、请求 `fallbacks[]` 给 opus-5 单独开 fast：主模型那次按 standard，
    //    fallback 那次（也是最终回复，`usage.speed` 报 fast）按 fast。
    let speeds = sniff(&format!(
        r#"{{"model":"claude-opus-5","stop_reason":"end_turn","content":[{},{{"type":"text","text":"ok"}}],
            "usage":{{"input_tokens":412,"output_tokens":264,"speed":"fast","iterations":[
            {{"type":"message","model":"claude-opus-5-5","input_tokens":1000,"output_tokens":100}},
            {{"type":"fallback_message","model":"claude-opus-5","input_tokens":412,"output_tokens":264}}]}}}}"#,
        fallback_block(r#""cyber""#)
    ));
    let fb = vec![("claude-opus-5".to_string(), Some("fast".to_string()))];
    let want =
        est("claude-opus-5-5", None, 1000, 100) + est("claude-opus-5", Some("fast"), 412, 264);
    assert!(close(
        speeds.billed_cost(Some("claude-opus-5"), Some("claude-opus-5-5"), Some("standard"), &fb),
        want
    ));
    assert!(
        est("claude-opus-5", Some("fast"), 412, 264) > est("claude-opus-5", None, 412, 264),
        "fast 档确有溢价"
    );

    // 4. 没有 iterations 的单次拒答也按类别：输出前 bio 照收、cyber / null 免费、出了内容的照收。
    let single = |cat: &str, output: i64, content: &str| {
        sniff(&format!(
            r#"{{"model":"claude-opus-5","stop_reason":"refusal","stop_details":{{"type":"refusal","category":{cat}}},
                "content":{content},"usage":{{"input_tokens":1000,"output_tokens":{output}}}}}"#
        ))
        .billed_cost(Some("claude-opus-5"), None, None, &[])
    };
    assert!(close(single(r#""bio""#, 0, "[]"), 0.005));
    assert!(close(single(r#""reasoning_extraction""#, 0, "[]"), 0.005));
    assert!(close(single(r#""cyber""#, 0, "[]"), 0.0));
    assert!(close(single("null", 0, "[]"), 0.0));
    assert!(close(single(r#""cyber""#, 40, r#"[{"type":"text","text":"part"}]"#), 0.006));
}

/// 服务端工具循环里同一个模型会采样好几轮、各记一条 `message`：一次「尝试」是同一模型的连续几轮，
/// 按模型切换的边界分组，不能一条算一次；速度档按这一轮实际跑的模型取，模型名只认精确相等与日期后缀。
#[test]
fn tool_loop_rounds_belong_to_one_attempt() {
    assert!(!super::same_model("claude-opus-5", "claude-opus-5-5"), "前缀不是同一个模型");
    assert!(super::same_model("claude-haiku-4-5", "claude-haiku-4-5-20251001"), "日期后缀是别名");
    assert!(super::same_model("claude-opus-5-5", "claude-opus-5-5"));

    let sniff = |body: &str| {
        let mut s = UsageSniffer::new(false, false);
        s.feed(body.as_bytes());
        s.finish();
        s
    };
    let close = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;
    let est = |model: &str, speed: Option<&str>, input: i64, output: i64| {
        crate::pricing::estimate_usd(crate::pricing::Usage {
            model: Some(model),
            speed,
            input_tokens: Some(input),
            output_tokens: Some(output),
            ..Default::default()
        })
        .unwrap()
    };
    let block = |from: &str, to: &str, cat: &str| {
        format!(
            r#"{{"type":"fallback","from":{{"model":"{from}"}},"to":{{"model":"{to}"}},"trigger":{{"type":"refusal","category":{cat}}}}}"#
        )
    };

    // 1. fable 先调了一次服务端工具（出了 50 输出）、下一轮以 bio 拒掉，换 opus-5 作答。fable 那两轮
    //    是同一次尝试、整次照收；`fallback` 块只有一个，类别 bio 属于这整次，不能错位到第二轮上。
    let tool_then_bio = sniff(&format!(
        r#"{{"model":"claude-opus-5","stop_reason":"end_turn","content":[{},{{"type":"text","text":"ok"}}],
            "usage":{{"iterations":[
            {{"type":"message","model":"claude-fable-5-1","input_tokens":500,"output_tokens":50}},
            {{"type":"message","model":"claude-fable-5-1","input_tokens":600,"output_tokens":0}},
            {{"type":"fallback_message","model":"claude-opus-5","input_tokens":412,"output_tokens":264}}]}}}}"#,
        block("claude-fable-5-1", "claude-opus-5", r#""bio""#)
    ));
    let want = est("claude-fable-5-1", None, 500, 50)
        + est("claude-fable-5-1", None, 600, 0)
        + est("claude-opus-5", None, 412, 264);
    assert!(close(
        tool_then_bio.billed_cost(Some("claude-opus-5"), Some("claude-fable-5-1"), None, &[]),
        want
    ));
    // fable 那次一点输出都没有、类别 cyber：整次免费（两轮都不收），只收 opus-5。
    let silent_cyber = sniff(&format!(
        r#"{{"model":"claude-opus-5","stop_reason":"end_turn","content":[{},{{"type":"text","text":"ok"}}],
            "usage":{{"iterations":[
            {{"type":"message","model":"claude-fable-5-1","input_tokens":500,"output_tokens":0}},
            {{"type":"message","model":"claude-fable-5-1","input_tokens":600,"output_tokens":0}},
            {{"type":"fallback_message","model":"claude-opus-5","input_tokens":412,"output_tokens":264}}]}}}}"#,
        block("claude-fable-5-1", "claude-opus-5", r#""cyber""#)
    ));
    assert!(close(
        silent_cyber.billed_cost(Some("claude-opus-5"), Some("claude-fable-5-1"), None, &[]),
        est("claude-opus-5", None, 412, 264)
    ));

    // 2. 主模型 opus-5-5（standard）跑了两轮工具循环后被拒，fallback opus-5 在请求里单独开 fast：
    //    opus-5-5 那两轮按 standard（不能因为 `opus-5` 是 `opus-5-5` 的前缀拿到 fast），opus-5 按 fast。
    let fb = vec![("claude-opus-5".to_string(), Some("fast".to_string()))];
    let prefix = sniff(&format!(
        r#"{{"model":"claude-opus-5","stop_reason":"end_turn","content":[{},{{"type":"text","text":"ok"}}],
            "usage":{{"speed":"fast","iterations":[
            {{"type":"message","model":"claude-opus-5-5","input_tokens":800,"output_tokens":60}},
            {{"type":"message","model":"claude-opus-5-5","input_tokens":900,"output_tokens":40}},
            {{"type":"fallback_message","model":"claude-opus-5","input_tokens":412,"output_tokens":264}}]}}}}"#,
        block("claude-opus-5-5", "claude-opus-5", r#""cyber""#)
    ));
    let want = est("claude-opus-5-5", Some("standard"), 800, 60)
        + est("claude-opus-5-5", Some("standard"), 900, 40)
        + est("claude-opus-5", Some("fast"), 412, 264);
    assert!(close(
        prefix.billed_cost(Some("claude-opus-5"), Some("claude-opus-5-5"), Some("standard"), &fb),
        want
    ));

    // 3. sticky routing：请求 opus-5-5（standard），直接由 fallback opus-5 跑，且跑了工具循环（先一条
    //    `message` 再 `fallback_message`）。两轮都是 opus-5、按它在 `fallbacks[]` 里的 fast；响应没报
    //    `usage.speed` 时也一样。
    let sticky = sniff(
        r#"{"model":"claude-opus-5","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}],
            "usage":{"iterations":[
            {"type":"message","model":"claude-opus-5","input_tokens":1000,"output_tokens":100},
            {"type":"fallback_message","model":"claude-opus-5","input_tokens":1200,"output_tokens":200}]}}"#,
    );
    let want = est("claude-opus-5", Some("fast"), 1000, 100)
        + est("claude-opus-5", Some("fast"), 1200, 200);
    assert!(close(
        sticky.billed_cost(Some("claude-opus-5"), Some("claude-opus-5-5"), Some("standard"), &fb),
        want
    ));
    assert!(want > est("claude-opus-5", None, 1000, 100) + est("claude-opus-5", None, 1200, 200));
}
