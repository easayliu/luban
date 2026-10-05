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
