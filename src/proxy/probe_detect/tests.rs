use crate::proxy::{StatusCode, config};

/// 探针命中时本地回的那条：200 + 一条最小的正常回复，按来访要的形态给（非流式整段 JSON、
/// 流式展成 SSE 且能原样聚合回来），model 原样回来访声明的那个、没写就留空，正文里不带
/// 任何「被拦了」的痕迹——探活看到的必须是「健康」。是 luban 就地答的这件事标在头上
/// （`x-luban-local` / `x-luban-probe-kind`）与 Message id 的 `msg_luban` 前缀里。
#[tokio::test]
async fn probe_reply_is_a_minimal_200_in_the_shape_the_request_asks_for() {
    async fn parts(resp: crate::proxy::Response) -> (StatusCode, axum::http::HeaderMap, String) {
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024).await.unwrap();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    // 非流式：整段 Message，字段齐全、状态 200。
    let (status, headers, body) = parts(crate::proxy::probe_reply(
        crate::proxy::ProbeKind::Ping,
        Some("claude-opus-5"),
        false,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type").unwrap(), "application/json");
    // 标记：这条是 luban 就地答的、命中的是哪条判据。
    assert_eq!(
        headers.get(crate::proxy::LOCAL_REPLY_HEADER).unwrap(),
        crate::proxy::REWRITE_PROBE_REPLY
    );
    assert_eq!(headers.get(crate::proxy::PROBE_KIND_HEADER).unwrap(), "ping");
    let msg: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(msg["type"], "message");
    assert_eq!(msg["role"], "assistant");
    assert_eq!(msg["model"], "claude-opus-5");
    assert_eq!(msg["stop_reason"], "end_turn");
    assert_eq!(msg["content"][0]["type"], "text");
    assert_eq!(msg["content"][0]["text"], crate::proxy::PROBE_REPLY_TEXT);
    assert_eq!(msg["usage"]["output_tokens"], 1);
    assert!(
        msg["id"]
            .as_str()
            .is_some_and(|id| id.starts_with(crate::proxy::PROBE_REPLY_ID_PREFIX) && id.len() > 16),
        "id 要一眼认得出是本地答的：{body}"
    );
    // 正文里除了 id 的前缀不留别的痕迹：探活读到的必须是一条正常回复。
    let without_id = body.replace(msg["id"].as_str().unwrap(), "");
    for word in ["probe", "health", "forward", "luban", "403"] {
        assert!(!without_id.contains(word), "本地回复不该露出被拦的痕迹：{word} in {body}");
    }
    // 两条的 id 不同：探活一条接一条，同一个 id 在下游那侧会被当成同一条回复。
    let (_, _, again) = parts(crate::proxy::probe_reply(
        crate::proxy::ProbeKind::Ping,
        Some("claude-opus-5"),
        false,
    ))
    .await;
    let msg2: serde_json::Value = serde_json::from_str(&again).unwrap();
    assert_ne!(msg["id"], msg2["id"]);

    // 流式：展成 SSE，再聚合回来是同一条 Message。
    let (status, headers, body) = parts(crate::proxy::probe_reply(
        crate::proxy::ProbeKind::ShortOpener,
        Some("claude-opus-5"),
        true,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type").unwrap(), crate::proxy::SSE_CONTENT_TYPE);
    assert_eq!(
        headers.get(crate::proxy::LOCAL_REPLY_HEADER).unwrap(),
        crate::proxy::REWRITE_PROBE_REPLY
    );
    assert_eq!(headers.get(crate::proxy::PROBE_KIND_HEADER).unwrap(), "short-opener");
    assert!(body.starts_with("event: message_start\n"), "{body}");
    assert!(body.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"), "{body}");
    let mut agg = crate::proxy::SseAggregator::default();
    agg.feed(body.as_bytes());
    let crate::proxy::Aggregated::Message(back) = agg.finish() else {
        panic!("本地回复的 SSE 应能聚合")
    };
    assert_eq!(back["content"][0]["text"], crate::proxy::PROBE_REPLY_TEXT);
    assert_eq!(back["stop_reason"], "end_turn");

    // 来访没写 model：留空，不替它编一个。
    let (_, _, body) =
        parts(crate::proxy::probe_reply(crate::proxy::ProbeKind::DuplicateIdentity, None, false))
            .await;
    let msg: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(msg["model"], "");

    // 1 token 探活：来访只要 1 个 token，真上游回的必是截断，stop_reason 给 max_tokens；
    // 流式那条 message_delta 里同样是它。
    let (_, headers, body) = parts(crate::proxy::probe_reply(
        crate::proxy::ProbeKind::OneTokenPing,
        Some("claude-opus-4-8"),
        false,
    ))
    .await;
    assert_eq!(headers.get(crate::proxy::PROBE_KIND_HEADER).unwrap(), "one-token-ping");
    let msg: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(msg["stop_reason"], "max_tokens");
    assert_eq!(msg["usage"]["output_tokens"], 1);
    let (_, _, body) = parts(crate::proxy::probe_reply(
        crate::proxy::ProbeKind::OneTokenPing,
        Some("claude-opus-4-8"),
        true,
    ))
    .await;
    let mut agg = crate::proxy::SseAggregator::default();
    agg.feed(body.as_bytes());
    let crate::proxy::Aggregated::Message(back) = agg.finish() else {
        panic!("本地回复的 SSE 应能聚合")
    };
    assert_eq!(back["stop_reason"], "max_tokens");
}

/// 探针判定（[`probe_signature`]）：封号复盘里的三类探活形态都命中，官方 CC 的三种
/// 无 tools 请求（预热 / haiku 工具调用 / 补全建议）与带 tools 的主请求都不命中；老设备的
/// 「一句话对话」不算；下游那个去掉基座、带空 tools 与 thinking 的客户端不算。
#[test]
fn probe_signature_matches_probes_and_spares_official_shapes() {
    use crate::proxy::ProbeKind::*;
    const DEV: &str = "4fef933b15e89f7060000573496ce0eab6e9f0d1cf43e31dd4c7dc1c6801cfb5";
    let identity = config::CC_SYSTEM_IDENTITY;
    // 官方两块：billing header + 身份句。
    let cc_sys = format!(
        r#"[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.abcdef"}},{{"type":"text","text":"{identity}"}}]"#
    );
    // ban.log 里探针那份 3 块：billing header + 身份句 + 身份句（第三块与身份句等长）。
    let probe_sys = format!(
        r#"[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.abcdef"}},{{"type":"text","text":"{identity}"}},{{"type":"text","text":"{identity}"}}]"#
    );
    let one_msg = r#"[{"role":"user","content":"hi"}]"#;
    let sig_beta = |body: &str, cc: bool, dev: Option<&str>, known: bool, beta: &[&str]| {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let beta: Vec<String> = beta.iter().map(|b| b.to_string()).collect();
        futures_util::FutureExt::now_or_never(crate::proxy::probe_signature(
            Some(&v),
            dev,
            &beta,
            cc,
            false,
            async { known },
        ))
        .expect("probe_signature does no IO here")
    };
    let strict = |body: &str| {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        futures_util::FutureExt::now_or_never(crate::proxy::probe_signature(
            Some(&v),
            None,
            &[],
            false,
            true,
            async { false },
        ))
        .expect("probe_signature does no IO here")
    };
    let sig =
        |body: &str, cc: bool, dev: Option<&str>, known: bool| sig_beta(body, cc, dev, known, &[]);

    // ---- ban.log 三类探针，按复盘里的形态复现 ----
    // A) 每 15 分钟一条的 haiku ping：3 块 system、max_tokens=2、老设备。
    //    身份句重复先于 ping 命中；把第三块换成别的文字后就是 ping。
    let ping3 = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","system":{probe_sys},"messages":{one_msg},"max_tokens":2,"metadata":{{}}}}"#
    );
    assert_eq!(sig(&ping3, true, Some(DEV), true), Some(DuplicateIdentity));
    let ping = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":2,"metadata":{{}}}}"#
    );
    assert_eq!(sig(&ping, true, Some(DEV), true), Some(Ping), "老设备也算：判据在 max_tokens");
    // B) 封前 21 条：3 块 system、无 tools/thinking、temperature=1、max_tokens=1024、新设备。
    let burst3 = format!(
        r#"{{"model":"claude-sonnet-5","system":{probe_sys},"messages":{one_msg},"max_tokens":1024,"temperature":1,"stream":true,"metadata":{{}}}}"#
    );
    assert_eq!(sig(&burst3, true, Some(DEV), false), Some(DuplicateIdentity));
    assert_eq!(
        sig(&burst3, true, Some(DEV), true),
        Some(DuplicateIdentity),
        "已知设备发同形态：身份句重复这条补上"
    );
    let burst = format!(
        r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":1024,"temperature":1,"stream":true,"metadata":{{}}}}"#
    );
    assert_eq!(sig(&burst, true, Some(DEV), false), Some(ThrowawayConversation));
    assert_eq!(sig(&burst, true, Some(DEV), true), None, "老设备发同样形态不算");
    // C) channel-test 身份**不在这里拒**：它的形态不命中三条（max_tokens=256、设备恒定），
    //    由 detect 送进模拟重建身份，见 [`cc_identity_well_formed`] 的测试。
    let channel_test = format!(
        r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":256,"stream":true,"thinking":{{"type":"adaptive"}},"metadata":{{}}}}"#
    );
    assert_eq!(sig(&channel_test, true, Some("channel-test"), true), None);

    // ---- 不限 UA（0.3.99 起）：非 CC UA 同样判——Go-http-client 的探活此前全走模拟 ----
    assert_eq!(sig(&ping3, false, Some(DEV), false), Some(DuplicateIdentity));
    assert_eq!(sig(&burst, false, Some(DEV), false), Some(ThrowawayConversation));
    // 不带 system 的 ping 也算：官方没有「无 system、max_tokens 2..16」的请求。
    let bare_ping =
        format!(r#"{{"model":"claude-sonnet-4-6","messages":{one_msg},"max_tokens":16}}"#);
    assert_eq!(sig(&bare_ping, false, None, false), Some(Ping));
    assert_eq!(sig(&bare_ping, true, None, false), Some(Ping));
    // 不带 system、max_tokens 正常的单句请求不算一次性会话：第三方小应用太常见。
    let bare_chat =
        format!(r#"{{"model":"claude-sonnet-4-6","messages":{one_msg},"max_tokens":1024}}"#);
    assert_eq!(sig(&bare_chat, false, Some(DEV), false), None);
    // 默认判据：带 tools 的不算 ping（复盘里 mt=16 带 4 个 tools 的那批照常放行）。
    let tooled_ping = format!(
        r#"{{"model":"claude-sonnet-4-6","messages":{one_msg},"max_tokens":16,"tools":[{{"name":"t","input_schema":{{"type":"object"}}}}]}}"#
    );
    assert_eq!(sig(&tooled_ping, false, None, false), None);

    // ---- 严格模式（默认关）：多收两刀 ----
    // 带 tools 的 ping 也算：16 个 token 装不下一次 tool_use。
    assert_eq!(strict(&tooled_ping), Some(Ping));
    // 短开场：无 system、无 tools、一句「hi」，max_tokens 随便多大。
    for mt in [50, 1024, 32000] {
        let opener =
            format!(r#"{{"model":"claude-opus-5","messages":{one_msg},"max_tokens":{mt}}}"#);
        assert_eq!(strict(&opener), Some(ShortOpener), "max_tokens {mt}");
        assert_eq!(sig(&opener, false, None, false), None, "默认判据不拦 max_tokens {mt}");
    }
    // 文本块形态、带首尾空白也算；不超过 32 字节。中文短语同样算，一整句话不算。
    let blocks = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":[{"type":"text","text":"  ping  "}]}],"max_tokens":1024}"#;
    assert_eq!(strict(blocks), Some(ShortOpener));
    let zh_opener = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"你好，测试一下"}],"max_tokens":1024}"#;
    assert_eq!(strict(zh_opener), Some(ShortOpener), "7 个汉字 21 字节");
    // 正文长了就是正常单轮对话，不算（30 个汉字 90 字节，按字符算会误伤）。
    let real = format!(
        r#"{{"model":"claude-opus-5","messages":[{{"role":"user","content":"{}"}}],"max_tokens":1024}}"#,
        "请把下面这段话翻译成英文，并保留原有的段落结构与专有名词。"
    );
    assert_eq!(strict(&real), None);
    // 带 system、带 tools、带附件、max_tokens=1（预热）、多条消息：都不是短开场。
    let with_sys = format!(
        r#"{{"model":"claude-opus-5","system":"be brief","messages":{one_msg},"max_tokens":1024}}"#
    );
    assert_eq!(strict(&with_sys), None);
    let with_tools = format!(
        r#"{{"model":"claude-opus-5","messages":{one_msg},"max_tokens":1024,"tools":[{{"name":"t","input_schema":{{"type":"object"}}}}]}}"#
    );
    assert_eq!(strict(&with_tools), None);
    let with_image = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image","source":{}}]}],"max_tokens":1024}"#;
    assert_eq!(strict(with_image), None);
    // 无 system、无身份、UA 不可信的 1 token 请求在严格模式下命中的是 1 token 探活（排在
    // 短开场之前）；短开场自己不认 max_tokens=1。
    let prewarm = format!(r#"{{"model":"claude-opus-5","messages":{one_msg},"max_tokens":1}}"#);
    assert_eq!(strict(&prewarm), Some(OneTokenPing));

    // ---- 1 token 探活：ban 导出里 Go-http-client 每 5 分钟一条的健康检查 ----
    // 非 CC UA：无 system、无 tools、一条消息、max_tokens=1 → 命中，带不带 device_id 都算
    // （Go 客户端抄了 metadata.user_id 的也有），流式与否无关，模型无关。
    let one_token = |model: &str, extra: &str| {
        format!(r#"{{"model":"{model}","messages":{one_msg},"max_tokens":1{extra}}}"#)
    };
    assert_eq!(sig(&one_token("claude-opus-4-8", ""), false, None, false), Some(OneTokenPing));
    assert_eq!(
        sig(&one_token("claude-opus-4-8", r#","stream":true"#), false, Some(DEV), true),
        Some(OneTokenPing),
        "带 device_id、老设备、流式：UA 不可信就算"
    );
    assert_eq!(
        sig(&one_token("claude-haiku-4-5-20251001", ""), false, None, false),
        Some(OneTokenPing),
        "haiku 也算：官方额度探测来自可信 UA"
    );
    // 非 CC UA 抄了官方额度探测的正文（haiku + `quota`）：仍算——不是可信 UA 发的就不是官方探测。
    let quota_probe = r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"quota"}]}"#;
    assert_eq!(sig(quota_probe, false, None, false), Some(OneTokenPing));
    // 可信 UA：带 device_id 的是桌面端预热，不碰（由模拟路径按形态放行）；额度探测形态
    // 不碰；什么身份都没有的 1 token 请求算——模拟路径本来也按第三方处理它。
    assert_eq!(sig(&one_token("claude-opus-5", ""), true, Some(DEV), false), None);
    assert_eq!(sig(quota_probe, true, None, false), None, "官方额度探测不带身份也放");
    assert_eq!(sig(&one_token("claude-opus-5", ""), true, None, false), Some(OneTokenPing));
    // 带 system 的 1 token 请求不算：桌面端另一种预热带 `[billing, 身份句]` 两块，第三方带
    // 长 system 的由模拟接管；`system: null` 按没有算，加个空 tools 也绕不过。
    let sys_prewarm = format!(
        r#"{{"model":"claude-opus-5","system":{cc_sys},"messages":{one_msg},"max_tokens":1}}"#
    );
    assert_eq!(sig(&sys_prewarm, false, None, false), None);
    assert_eq!(
        sig(&one_token("claude-opus-5", r#","system":"be brief""#), false, None, false),
        None
    );
    assert_eq!(
        sig(&one_token("claude-opus-5", r#","system":null,"tools":[]"#), false, None, false),
        Some(OneTokenPing)
    );
    // 带 tools 或多条消息的不算。
    assert_eq!(
        sig(
            &one_token(
                "claude-opus-5",
                r#","tools":[{"name":"t","input_schema":{"type":"object"}}]"#
            ),
            false,
            None,
            false
        ),
        None
    );
    let two_turn_one_token = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"},{"role":"user","content":"hi"}],"max_tokens":1}"#;
    assert_eq!(sig(two_turn_one_token, false, None, false), None);
    let two_msgs = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"},{"role":"user","content":"hi"}],"max_tokens":1024}"#;
    assert_eq!(strict(two_msgs), None);

    // ---- 加空字段绕不过 ----
    let padded = format!(
        r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":1024,"tools":[],"thinking":null,"stop_sequences":[],"metadata":{{}}}}"#
    );
    assert_eq!(sig(&padded, true, Some(DEV), false), Some(ThrowawayConversation));
    let padded_ping = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":8,"tools":null,"metadata":{{}}}}"#
    );
    assert_eq!(sig(&padded_ping, true, Some(DEV), true), Some(Ping));
    // thinking 给个非对象也不算「带了 thinking」。
    let fake_thinking = format!(
        r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":1024,"thinking":"adaptive","metadata":{{}}}}"#
    );
    assert_eq!(sig(&fake_thinking, true, Some(DEV), false), Some(ThrowawayConversation));
    // thinking 是对象但 max_tokens 不到官方工具调用那档 → 仍算。
    let small_thinking = format!(
        r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":1024,"thinking":{{"type":"adaptive"}},"metadata":{{}}}}"#
    );
    assert_eq!(sig(&small_thinking, true, Some(DEV), false), Some(ThrowawayConversation));

    // ---- 官方三种无 tools 请求都不命中（形态取自 cap/ 抓包） ----
    let prewarm = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":{one_msg},"metadata":{{}}}}"#
    );
    assert_eq!(sig(&prewarm, true, Some(DEV), false), None, "预热不算");
    // 2.1.187 Claude Desktop 的预热带 [billing, identity] 两块 system（ban.log）。
    let prewarm_with_sys = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","messages":{one_msg},"system":{cc_sys},"max_tokens":1,"metadata":{{}}}}"#
    );
    assert_eq!(sig(&prewarm_with_sys, true, Some(DEV), false), None, "带 system 的预热不算");
    // 只有 Helper / 安全分类的 body 取值、system 是普通 CC 两块、也没带各自的 beta：
    // 这不是官方那两种请求，是抄了几个字段的探针，**要**命中。完整形态的放行见下面。
    let haiku_body_only = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","messages":{one_msg},"system":{cc_sys},"tools":[],"metadata":{{}},"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"stream":true}}"#
    );
    assert_eq!(sig(&haiku_body_only, true, Some(DEV), false), Some(ThrowawayConversation));
    let classifier_body_only = format!(
        r#"{{"model":"claude-sonnet-5","max_tokens":64,"system":{cc_sys},"messages":{one_msg},"stop_sequences":["\\n"],"thinking":{{"type":"disabled"}},"metadata":{{}}}}"#
    );
    assert_eq!(sig(&classifier_body_only, true, Some(DEV), false), Some(ThrowawayConversation));
    let main = format!(
        r#"{{"model":"claude-opus-5","system":{cc_sys},"messages":{one_msg},"tools":[{{"name":"Bash"}}],"max_tokens":32000,"metadata":{{}}}}"#
    );
    assert_eq!(sig(&main, true, Some(DEV), false), None, "带 tools 的主请求不算");
    // 多轮对话不算，哪怕其余都像。
    let multi = format!(
        r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":[{{"role":"user","content":"a"}},{{"role":"assistant","content":"b"}},{{"role":"user","content":"c"}}],"max_tokens":8,"metadata":{{}}}}"#
    );
    assert_eq!(sig(&multi, true, Some(DEV), false), None);

    // ---- ban.log 里 claude-cli/2.1.165 那批：opus-5、去掉基座、tools: []、thinking 对象、
    //      max_tokens 10240，每台新设备同样四道题。上一版的宽豁免放过了它；按官方 Helper
    //      的取值逐项对之后（模型不是 haiku、max_tokens 不是 32000）它就是一次性对话。
    let rig = format!(
        r#"{{"model":"claude-opus-5","system":{cc_sys},"messages":{one_msg},"max_tokens":10240,"stream":true,"tools":[],"output_config":{{}},"thinking":{{"type":"adaptive"}},"metadata":{{}}}}"#
    );
    assert_eq!(sig(&rig, true, Some(DEV), false), Some(ThrowawayConversation));

    // ---- 豁免只认官方取值：下面每一条都是「抄了个字段名」的绕法，全部仍命中 ----
    for (label, body) in [
        (
            "thinking:{} + 4096",
            format!(
                r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":4096,"tools":[],"thinking":{{}},"stream":true,"metadata":{{}}}}"#
            ),
        ),
        (
            "thinking:{} + stop_sequences",
            format!(
                r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":64,"thinking":{{}},"stop_sequences":["x"],"metadata":{{}}}}"#
            ),
        ),
        (
            "helper 取值但模型是 opus",
            format!(
                r#"{{"model":"claude-opus-5","system":{cc_sys},"messages":{one_msg},"max_tokens":32000,"tools":[],"thinking":{{"type":"disabled"}},"stream":true,"metadata":{{}}}}"#
            ),
        ),
        (
            "helper 取值但 max_tokens 31999",
            format!(
                r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":31999,"tools":[],"thinking":{{"type":"disabled"}},"stream":true,"metadata":{{}}}}"#
            ),
        ),
        (
            "helper 取值但 tools 缺失",
            format!(
                r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":32000,"thinking":{{"type":"disabled"}},"stream":true,"metadata":{{}}}}"#
            ),
        ),
        (
            "helper 取值但非流式",
            format!(
                r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":32000,"tools":[],"thinking":{{"type":"disabled"}},"metadata":{{}}}}"#
            ),
        ),
        (
            "classifier 取值但 max_tokens 63",
            format!(
                r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":63,"thinking":{{"type":"disabled"}},"stop_sequences":["</x>"],"metadata":{{}}}}"#
            ),
        ),
        (
            "classifier 取值但 max_tokens 65",
            format!(
                r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":65,"thinking":{{"type":"disabled"}},"stop_sequences":["</x>"],"metadata":{{}}}}"#
            ),
        ),
        (
            "classifier 取值但 stop_sequences 里是空串",
            format!(
                r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":64,"thinking":{{"type":"disabled"}},"stop_sequences":[""],"metadata":{{}}}}"#
            ),
        ),
        (
            "classifier 取值但流式",
            format!(
                r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":64,"thinking":{{"type":"disabled"}},"stop_sequences":["</x>"],"stream":true,"metadata":{{}}}}"#
            ),
        ),
    ] {
        assert_eq!(sig(&body, true, Some(DEV), false), Some(ThrowawayConversation), "{label}");
    }
    // 只抄 Helper 的 body 五个字段、system 却是普通 CC 两块（billing + CC 身份句、没有 SDK
    // 身份句也没有长提示词）：不是 Helper 也不是标题生成，照样是一次性对话。
    let helper_body_only = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","messages":{one_msg},"system":{cc_sys},"tools":[],"metadata":{{}},"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"stream":true}}"#
    );
    assert_eq!(sig(&helper_body_only, true, Some(DEV), false), Some(ThrowawayConversation));

    // ---- 官方三种无 tools 请求抄全了才免：这是形态判据的边界，形态上它们就是官方请求 ----
    let sub_billing = "x-anthropic-billing-header: cc_version=2.1.260.d95; cc_entrypoint=cli; cch=ca354; cc_is_subagent=true; cc_prompt_id=bd224f00-5a91-4f15-b92f-0f47c663eae8;";
    let sdk = config::CC_SDK_AGENT_IDENTITY;
    let helper_exact = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","messages":{one_msg},"system":[{{"type":"text","text":"{sub_billing}"}},{{"type":"text","text":"{sdk}"}}],"tools":[],"metadata":{{}},"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"stream":true}}"#
    );
    let helper_betas: Vec<&str> =
        config::cc_profile(config::CcProfileKind::HelperSubagentHaiku).beta.split(',').collect();
    assert_eq!(
        sig_beta(&helper_exact, true, Some(DEV), false, &helper_betas),
        None,
        "官方 Helper 放行"
    );
    let long = "x".repeat(3059);
    let title_exact = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","messages":{one_msg},"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.ced; cch=00000"}},{{"type":"text","text":"{identity}"}},{{"type":"text","text":"{long}"}}],"tools":[],"metadata":{{}},"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"output_config":{{}},"stream":true}}"#
    );
    assert_eq!(
        sig_beta(&title_exact, true, Some(DEV), false, &[config::CC_BETA_STRUCTURED_OUTPUTS]),
        None,
        "官方标题生成放行（它与主请求并发，新设备上可能先到）"
    );
    let session_ctx = r"  ## Session Context\n\n- **User identity**: `e@x`";
    let classifier_exact = format!(
        r#"{{"model":"claude-sonnet-5","max_tokens":64,"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.3de; cch=00000"}},{{"type":"text","text":"{long}"}},{{"type":"text","text":"{session_ctx}"}}],"messages":{one_msg},"stop_sequences":["</severity>"],"thinking":{{"type":"disabled"}},"metadata":{{}}}}"#
    );
    assert_eq!(
        sig_beta(
            &classifier_exact,
            true,
            Some(DEV),
            false,
            &[config::CC_BETA_AUTO_MODE_CLASSIFIER]
        ),
        None,
        "官方安全分类放行"
    );

    // ---- 三种官方请求各少一样，都不放 ----
    for (label, body, beta) in [
        ("Helper 缺官方 beta", helper_exact.clone(), vec![]),
        ("Helper 只带了部分官方 beta", helper_exact.clone(), helper_betas[..3].to_vec()),
        (
            "Helper 第二块是 CC 身份句而不是 SDK 身份句",
            helper_exact.replace(sdk, identity),
            helper_betas.clone(),
        ),
        (
            "Helper thinking.type=enabled",
            helper_exact.replace(r#""type":"disabled""#, r#""type":"enabled""#),
            helper_betas.clone(),
        ),
        (
            "Helper 的 billing header 没有子代理标记",
            helper_exact.replace("cc_is_subagent=true; ", ""),
            helper_betas.clone(),
        ),
        ("标题生成缺 structured-outputs beta", title_exact.clone(), vec![]),
        (
            "标题生成的提示词不够长",
            title_exact.replace(&long, "short"),
            vec![config::CC_BETA_STRUCTURED_OUTPUTS],
        ),
        ("安全分类缺 auto-mode-classifier beta", classifier_exact.clone(), vec![]),
        (
            "安全分类 thinking.type=adaptive",
            classifier_exact.replace(r#""type":"disabled""#, r#""type":"adaptive""#),
            vec![config::CC_BETA_AUTO_MODE_CLASSIFIER],
        ),
        (
            "安全分类第一块不是 billing header",
            classifier_exact.replace("x-anthropic-billing-header: ", ""),
            vec![config::CC_BETA_AUTO_MODE_CLASSIFIER],
        ),
        (
            "安全分类只有两块 system",
            classifier_exact.replace(&format!(r#",{{"type":"text","text":"{session_ctx}"}}"#), ""),
            vec![config::CC_BETA_AUTO_MODE_CLASSIFIER],
        ),
        (
            "安全分类第三块不是 Session Context",
            classifier_exact.replace(session_ctx, "something else"),
            vec![config::CC_BETA_AUTO_MODE_CLASSIFIER],
        ),
        (
            "安全分类中间块不够长",
            classifier_exact.replace(&long, "short"),
            vec![config::CC_BETA_AUTO_MODE_CLASSIFIER],
        ),
    ] {
        assert_eq!(
            sig_beta(&body, true, Some(DEV), false, &beta),
            Some(ThrowawayConversation),
            "{label}"
        );
    }

    // 字符串形态的 system 里身份句只算一块。
    let as_string = format!(
        r#"{{"model":"claude-opus-5","system":"{identity} {identity}","messages":{one_msg},"tools":[{{"name":"Bash"}}],"max_tokens":32000}}"#
    );
    assert_eq!(sig(&as_string, true, Some(DEV), true), None);
}
