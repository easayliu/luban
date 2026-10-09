use crate::proxy::Bytes;
use crate::proxy::test_support::{all_on, rewrite_body, test_cred};

/// 改写之后，thinking 块的原始字节仍要落回**它自己**那一块。
///
/// [`preserve_thinking_encoding`] 原先按 `(消息下标, 块下标)` 配对；下标一旦移动，A 块的原始
/// 字节会被盖到 B 块上——历史与签名一起错乱，上游按签名校验必拒。现在带 `signature` / `data`
/// 的按那个值配，与下标无关。空壳 system 原样出站（luban 不替客户端删）。
#[test]
fn thinking_bytes_follow_their_own_block_when_messages_are_dropped() {
    // 两条空壳 system 夹在两轮之间；两轮 thinking 的正文都带 \u003c 转义（serde 重新
    // 序列化会解码成字面量 `<`，正是这套字节还原存在的理由）。
    const A: &str = r#"{"type":"thinking","thinking":"A\u003cx\u003e","signature":"sigA=="}"#;
    const B: &str = r#"{"type":"thinking","thinking":"B\u003cy\u003e","signature":"sigB=="}"#;
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","system":[{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}}],"messages":[{{"role":"system","content":[]}},{{"role":"user","content":"hi"}},{{"role":"assistant","content":[{A},{{"type":"text","text":"a"}}]}},{{"role":"system","content":[]}},{{"role":"user","content":"more"}},{{"role":"assistant","content":[{B},{{"type":"text","text":"b"}}]}}]}}"#
    ));
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
    let s = String::from_utf8(out.to_vec()).unwrap();
    assert_eq!(s.matches(r#""role":"system""#).count(), 2, "空壳原样出站: {s}");
    assert!(s.contains(A), "A 轮的原始字节该原样落回 A 块: {s}");
    assert!(s.contains(B), "B 轮的原始字节该原样落回 B 块: {s}");
    assert_eq!(s.matches("sigA==").count(), 1, "A 的签名不该被复制到第二轮: {s}");
    assert_eq!(s.matches("sigB==").count(), 1, "B 的签名该还在: {s}");
    assert_eq!(s.matches(r"A\u003cx").count(), 1, "A 的正文只该出现一次: {s}");
}

// ---------- thinking 签名兜底 ----------

/// 只认「signature + thinking 同现」这一种 400，别的 `invalid_request_error` 一律不碰——
/// 误判的代价是给每个普通请求错误都白搭一次上游往返。
#[test]
fn detects_only_the_thinking_signature_400() {
    let hit = br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: Invalid `signature` in `thinking` block"}}"#;
    assert!(crate::proxy::is_thinking_signature_error(hit));

    for miss in [
            // 普通请求形态错误。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be greater than 0"}}"#[..],
            // 提到了 thinking 但不是签名问题（工具续跑那条）——降级救不了它，不该触发。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"a final `assistant` message must start with a thinking block"}}"#[..],
            // 非 JSON 的拦截页：整段当 message 扫，同样不该命中。
            &b"<html>403 Forbidden</html>"[..],
        ] {
            assert!(!crate::proxy::is_thinking_signature_error(miss), "不该命中: {}", String::from_utf8_lossy(miss));
        }
}

/// `redacted_thinking` 的密文那条 400 自成一档：与签名、被改、空块三条判据互不误触。
#[test]
fn detects_only_the_redacted_thinking_data_400() {
    let hit = br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.5.content.48: Invalid `data` in `redacted_thinking` block"}}"#;
    assert!(crate::proxy::is_redacted_thinking_data_error(hit));
    // 三条老判据都不该认领它，否则日志与重试原因会张冠李戴。
    assert!(!crate::proxy::is_thinking_signature_error(hit));
    assert!(!crate::proxy::is_thinking_modified_error(hit));

    for miss in [
            // 签名那条：有 thinking 没 redacted_thinking。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: Invalid `signature` in `thinking` block"}}"#[..],
            // 被改那条：提到了 redacted_thinking，但没提 data，且另有专属判据。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: `thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified."}}"#[..],
            // 普通请求形态错误。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be greater than 0"}}"#[..],
            &b"<html>403 Forbidden</html>"[..],
        ] {
            assert!(
                !crate::proxy::is_redacted_thinking_data_error(miss),
                "不该命中: {}",
                String::from_utf8_lossy(miss)
            );
        }
}

/// 三条 400 共用同一段取证，但各记各的 `kind`；不属这一类的 400 一概不认领——认领了就是
/// 给每个普通请求错误白打一行对照日志。
#[test]
fn classifies_all_three_thinking_400s() {
    for (msg, kind) in [
        ("messages.1.content.0: Invalid `signature` in `thinking` block", "signature"),
        (
            "messages.43.content.110: `thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified.",
            "modified",
        ),
        ("messages.5.content.48: Invalid `data` in `redacted_thinking` block", "redacted_data"),
    ] {
        let body = format!(
            r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"{msg}"}}}}"#
        );
        assert_eq!(
            crate::proxy::thinking_block_error_kind(body.as_bytes()),
            Some(kind),
            "该归到 {kind}: {msg}"
        );
    }

    for miss in [
            // 空 thinking 块那条：另有专属的「整体 dump 入站体」路径，不该在这里再打一行。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: each thinking block must contain thinking"}}"#[..],
            // 提到了 thinking，但说的是末轮形态，不是某个块验不过。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"a final `assistant` message must start with a thinking block"}}"#[..],
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be greater than 0"}}"#[..],
            &b"<html>403 Forbidden</html>"[..],
        ] {
            assert_eq!(
                crate::proxy::thinking_block_error_kind(miss),
                None,
                "不该认领: {}",
                String::from_utf8_lossy(miss)
            );
        }
}

/// 坐标解析：官方那句以 `messages.<i>.content.<j>:` 开头，别的形态一律给 `None`。
#[test]
fn parses_the_error_block_path() {
    assert_eq!(
        crate::proxy::error_block_path(
            "messages.5.content.48: Invalid `data` in `redacted_thinking` block"
        ),
        Some((5, 48))
    );
    assert_eq!(crate::proxy::error_block_path("messages.0.content.0: whatever"), Some((0, 0)));
    for miss in [
        "max_tokens: must be greater than 0",
        "messages.5: system content must contain at least one block",
        "messages.x.content.1: nope",
        "messages.5.content.x: nope",
        "",
    ] {
        assert_eq!(crate::proxy::error_block_path(miss), None, "不该解析出坐标: {miss}");
    }
}

// ---------- 被拒的 redacted_thinking 块：入站 / 出站对照 ----------

/// 构造一份带两轮 assistant 的体：第 2 轮（下标 `2`）末块是 `redacted_thinking`。
fn traceable_body(data: &str, tool: &str, lead_system: bool) -> Vec<u8> {
    let lead = if lead_system {
        r#"{"role":"system","content":[{"type":"text","text":"x"}]},"#
    } else {
        ""
    };
    format!(
        concat!(
            r#"{{"model":"claude-sonnet-5","messages":["#,
            r#"{{"role":"user","content":[{{"type":"text","text":"hi"}}]}},"#,
            "{lead}",
            r#"{{"role":"assistant","content":["#,
            r#"{{"type":"thinking","thinking":"t","signature":"SIG"}},"#,
            r#"{{"type":"tool_use","id":"tu1","name":"{tool}","input":{{}}}},"#,
            r#"{{"type":"redacted_thinking","data":"{data}"}}]}}]}}"#
        ),
        lead = lead,
        tool = tool,
        data = data
    )
    .into_bytes()
}

/// luban 一个字节没动：坐标两侧相同，那一轮也逐字节相同——这就是「坏在客户端发来的那份」。
#[test]
fn traces_an_untouched_redacted_block() {
    let body = traceable_body("ENCRYPTED", "Bash", false);
    let t = crate::proxy::trace_thinking_block(&body, &body, 1, 2).expect("该坐标上有思考块");
    assert_eq!(t.outbound_at, "messages.1.content.2");
    assert_eq!(t.inbound_at, "messages.1.content.2");
    assert_eq!(t.payload_len, "ENCRYPTED".len());
    assert_eq!(t.turn_identical, Some(true));
    assert_eq!(t.inbound_turn, t.outbound_turn);
    assert!(t.outbound_turn.contains("redacted_thinking(data_len=9)"), "{}", t.outbound_turn);
}

/// 出站少了一条消息（丢空壳 / 提升 role:"system"）：下标前移，但按密文仍找得到同一个块。
#[test]
fn traces_a_shifted_redacted_block() {
    let inbound = traceable_body("ENCRYPTED", "Bash", true);
    let outbound = traceable_body("ENCRYPTED", "Bash", false);
    let t =
        crate::proxy::trace_thinking_block(&inbound, &outbound, 1, 2).expect("该坐标上有思考块");
    assert_eq!(t.outbound_at, "messages.1.content.2");
    assert_eq!(t.inbound_at, "messages.2.content.2", "按密文配对，不受下标前移影响");
    assert_eq!(t.turn_identical, Some(true), "那一轮本身没被改");
}

/// 出站那段密文入站体里根本没有：只有这一种情形是 luban 把它改坏了。
#[test]
fn traces_a_corrupted_redacted_block() {
    let inbound = traceable_body("ENCRYPTED", "Bash", false);
    let outbound = traceable_body("CORRUPTED", "Bash", false);
    let t =
        crate::proxy::trace_thinking_block(&inbound, &outbound, 1, 2).expect("该坐标上有思考块");
    assert_eq!(t.inbound_at, "none");
    assert_eq!(t.turn_identical, None, "入站那一轮都定位不到，无从比对");
}

/// 密文原样、但那一轮里的 `tool_use.name` 被改过（工具名混淆）：两份 turn 摘要一比即见。
#[test]
fn traces_a_rewritten_turn_around_the_redacted_block() {
    let inbound = traceable_body("ENCRYPTED", "Bash", false);
    let outbound = traceable_body("ENCRYPTED", "mcp__luban__abcBas00", false);
    let t =
        crate::proxy::trace_thinking_block(&inbound, &outbound, 1, 2).expect("该坐标上有思考块");
    assert_eq!(t.inbound_at, "messages.1.content.2", "块本身没动");
    assert_eq!(t.turn_identical, Some(false));
    assert!(t.inbound_turn.contains("tool_use(Bash)"), "{}", t.inbound_turn);
    assert!(t.outbound_turn.contains("tool_use(mcp__luban__abcBas00)"), "{}", t.outbound_turn);
}

/// 坐标上没有思考块（判据与形态对不上）：给 `None`，调用方打原文而不是编一份对照。
#[test]
fn traces_nothing_when_the_named_block_is_not_a_thinking_block() {
    let body = traceable_body("ENCRYPTED", "Bash", false);
    assert!(crate::proxy::trace_thinking_block(&body, &body, 1, 1).is_none(), "那是 tool_use");
    assert!(crate::proxy::trace_thinking_block(&body, &body, 9, 0).is_none(), "越界");
}

/// 两轮 assistant 带着**同一段**密文（客户端把同一块贴了两遍，经中转站转发的历史里常见）。
/// 两轮的 `tool_use.name` 不同，配错了轮次 `turn_identical` 立刻变 false。
///
/// `leads` 是前面垫几条会被 luban 丢掉的消息（空壳 `role:"system"`）：垫 n 条再与垫 0 条
/// 的那份对照，就是「出站整串下标前移 n 位」。
fn duplicate_payload_body(leads: usize) -> Vec<u8> {
    let lead = r#"{"role":"system","content":[{"type":"text","text":"x"}]},"#.repeat(leads);
    format!(
        concat!(
            r#"{{"model":"claude-sonnet-5","messages":["#,
            r#"{{"role":"user","content":[{{"type":"text","text":"hi"}}]}},"#,
            "{lead}",
            r#"{{"role":"assistant","content":["#,
            r#"{{"type":"tool_use","id":"tu1","name":"Bash","input":{{}}}},"#,
            r#"{{"type":"redacted_thinking","data":"DUP"}}]}},"#,
            r#"{{"role":"user","content":[{{"type":"text","text":"more"}}]}},"#,
            r#"{{"role":"assistant","content":["#,
            r#"{{"type":"tool_use","id":"tu2","name":"Read","input":{{}}}},"#,
            r#"{{"type":"redacted_thinking","data":"DUP"}}]}}]}}"#
        ),
        lead = lead
    )
    .into_bytes()
}

/// 同一段密文出现两处、但坐标对得上：按坐标 + 载荷双证认下第二轮那个。
///
/// 这条守的是按载荷配对那步少了唯一性检查的回归——`find` 一律给第一处，于是出站问的是
/// 第 3 条消息、配回来的是第 1 条，`turn_identical` 变 false，日志报「luban 改了那一轮」，
/// 而 luban 一个字节都没动。
#[test]
fn traces_the_right_one_of_two_identical_payloads() {
    let body = duplicate_payload_body(0);
    let t = crate::proxy::trace_thinking_block(&body, &body, 3, 1).expect("该坐标上有思考块");
    assert_eq!(t.outbound_at, "messages.3.content.1");
    assert_eq!(t.inbound_at, "messages.3.content.1", "不该配到第一处那个同款密文");
    assert_eq!(t.turn_identical, Some(true), "luban 什么都没改");
    assert!(t.outbound_turn.contains("tool_use(Read)"), "{}", t.outbound_turn);
}

/// 同一段密文出现两处，坐标又因为前移对不上：认不出是哪一个，落 `ambiguous`、不下结论。
/// 宁可少一条判断也不能给一个错的——`turn_identical=false` 会被读成「luban 改了那一轮」。
#[test]
fn refuses_to_guess_between_two_identical_payloads() {
    let inbound = duplicate_payload_body(1);
    let outbound = duplicate_payload_body(0);
    let t =
        crate::proxy::trace_thinking_block(&inbound, &outbound, 3, 1).expect("该坐标上有思考块");
    assert_eq!(t.inbound_at, "ambiguous(x2)");
    assert_eq!(t.turn_identical, None, "认不出对应块，就没有可比的那一轮");
    assert_eq!(t.inbound_turn, "-");
    assert!(t.outbound_turn.contains("tool_use(Read)"), "{}", t.outbound_turn);
}

/// 载荷重复、坐标也对得上，但坐标上装的是**另一个**同款块：仍要认 `ambiguous`。
///
/// 丢掉两条空壳 `role:"system"` 后整串前移两位，出站 `messages.3.content.1` 是 Read 那轮的
/// 密文，入站同一坐标上恰好是 Bash 那轮的同款密文——载荷对得上、坐标也对得上，配出来却是
/// 两条不同的轮次。只认「坐标 + 载荷」双证的话这里会报 `turn_identical=false`，等于凭空
/// 指认 luban 改了那一轮。
#[test]
fn refuses_a_coordinate_that_lands_on_the_other_duplicate() {
    let inbound = duplicate_payload_body(2);
    let outbound = duplicate_payload_body(0);
    // 前提先钉住：入站那个坐标上确实有一个同款载荷的块，否则这条用例是空转的。
    let decoy = crate::proxy::trace_thinking_block(&inbound, &inbound, 3, 1)
        .expect("入站同一坐标上也有一个同款密文块");
    assert!(decoy.outbound_turn.contains("tool_use(Bash)"), "{}", decoy.outbound_turn);

    let t =
        crate::proxy::trace_thinking_block(&inbound, &outbound, 3, 1).expect("该坐标上有思考块");
    assert!(
        t.outbound_turn.contains("tool_use(Read)"),
        "出站问的是 Read 那轮: {}",
        t.outbound_turn
    );
    assert_eq!(t.inbound_at, "ambiguous(x2)", "坐标撞上了另一个同款块，不能认");
    assert_eq!(t.turn_identical, None);
    assert_eq!(t.inbound_turn, "-");
}

/// 坐标落点：定位不到思考块时唯一能打的东西，三项各答一个问题。
#[test]
fn block_site_reports_where_the_coordinate_lands() {
    let body = traceable_body("ENCRYPTED", "Bash", false);
    assert_eq!(
        crate::proxy::block_site(&body, 1, 2),
        "msgs=2 role=assistant blocks=3 at=redacted_thinking(data_len=9)"
    );
    // 上游点名的位置上不是思考块——「cannot be modified」那条 400 的现网形态。
    assert_eq!(
        crate::proxy::block_site(&body, 1, 1),
        "msgs=2 role=assistant blocks=3 at=tool_use(Bash)"
    );
    // 出站体里那条消息被剥短了：`blocks=` 两侧一比就看得出来。
    assert_eq!(
        crate::proxy::block_site(&body, 1, 9),
        "msgs=2 role=assistant blocks=3 at=<out of range>"
    );
    assert_eq!(crate::proxy::block_site(&body, 0, 0), "msgs=2 role=user blocks=1 at=text(len=2)");
    assert_eq!(crate::proxy::block_site(&body, 7, 0), "msgs=2 <no messages.7>");
    assert_eq!(crate::proxy::block_site(b"not json", 0, 0), "<unparsable>");
    assert_eq!(crate::proxy::block_site(br#"{"model":"x"}"#, 0, 0), "<no messages>");
}

// ---------- 最后一条 assistant 消息的对照 ----------

/// luban 一个字节没动：两侧同一条消息、逐字节相同。这是「坐标靠不住」时唯一还能作数的判断。
#[test]
fn latest_assistant_diff_sees_an_untouched_turn() {
    let body = traceable_body("ENCRYPTED", "Bash", false);
    let d = crate::proxy::latest_assistant_diff(&body, &body);
    assert_eq!(d.inbound_at, "messages.1");
    assert_eq!(d.outbound_at, "messages.1");
    assert_eq!(d.turn_same, Some(true));
    assert_eq!(d.thinking_bytes_same, Some(true));
    assert!(d.outbound_turn.starts_with("assistant:thinking("), "{}", d.outbound_turn);
}

/// 出站少了一条消息（丢空壳 / 提升 role:"system"）：下标不同，但那一轮本身没变。
/// 下标一动就判为改过的话，每条被丢过空壳的请求都会被诬告一次。
#[test]
fn latest_assistant_diff_is_not_fooled_by_an_index_shift() {
    let inbound = traceable_body("ENCRYPTED", "Bash", true);
    let outbound = traceable_body("ENCRYPTED", "Bash", false);
    let d = crate::proxy::latest_assistant_diff(&inbound, &outbound);
    assert_eq!(d.inbound_at, "messages.2");
    assert_eq!(d.outbound_at, "messages.1");
    assert_eq!(d.turn_same, Some(true), "挪了位置不等于改了内容");
    assert_eq!(d.thinking_bytes_same, Some(true), "块的字节也没动");
}

/// 那一轮真被改过（工具名混淆）：`turn_same=false`，两份摘要一比就看出改的是哪一块。
/// 而思考块的字节没动——两项分开给才说得清「改的是结构，不是上游校验的那部分」。
#[test]
fn latest_assistant_diff_catches_a_rewritten_turn() {
    let inbound = traceable_body("ENCRYPTED", "Bash", false);
    let outbound = traceable_body("ENCRYPTED", "mcp__luban__abcBas00", false);
    let d = crate::proxy::latest_assistant_diff(&inbound, &outbound);
    assert_eq!(d.turn_same, Some(false), "结构变了");
    assert_eq!(d.thinking_bytes_same, Some(true), "改的是工具名，思考块的字节没动");
    assert!(d.inbound_turn.contains("tool_use(Bash)"), "{}", d.inbound_turn);
    assert!(d.outbound_turn.contains("tool_use(mcp__luban__abcBas00)"), "{}", d.outbound_turn);
}

/// 一条 assistant 消息都没有：给 `none`，不下结论。
#[test]
fn latest_assistant_diff_reports_none_without_an_assistant_turn() {
    let body = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
    let d = crate::proxy::latest_assistant_diff(body, body);
    assert_eq!(d.inbound_at, "none");
    assert_eq!(d.outbound_at, "none");
    assert_eq!(d.turn_same, None);
    assert_eq!(d.thinking_bytes_same, None);
    assert_eq!(d.inbound_turn, "-");
}

/// 末轮是连续两条 assistant（上游并成一轮）：区间写成 `messages.A-B`，块按合并后的顺序
/// 连起来，而 luban 改的是**靠前**那一条。只比数组里最后那一条会给出 `turn_same=true`
/// 的伪无罪——改的那条压根没进比较。
#[test]
fn latest_assistant_diff_covers_the_whole_merged_run() {
    let run = |name: &str| {
        format!(
            concat!(
                r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}}]}},"#,
                r#"{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"{name}","input":{{}}}}]}},"#,
                r#"{{"role":"assistant","content":[{{"type":"text","text":"done"}}]}}]}}"#
            ),
            name = name
        )
    };
    let inbound = run("Bash");
    let outbound = run("mcp__luban__abcBas00");
    let d = crate::proxy::latest_assistant_diff(inbound.as_bytes(), outbound.as_bytes());
    assert_eq!(d.inbound_at, "messages.1-2", "整串都算这一轮");
    assert_eq!(d.turn_same, Some(false), "改的是串里靠前那条，不能算没改");
    assert!(d.inbound_turn.contains("tool_use(Bash)"), "{}", d.inbound_turn);
    assert!(d.inbound_turn.contains("text(len=4)"), "块要按合并后的顺序连起来: {}", d.inbound_turn);
}

/// 思考块在串里靠前那条上、被 luban 改了字节：同样要抓到。
#[test]
fn latest_assistant_diff_checks_thinking_bytes_across_the_run() {
    let run = |sig: &str| {
        format!(
            concat!(
                r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"t","signature":"{sig}"}}]}},"#,
                r#"{{"role":"assistant","content":[{{"type":"text","text":"done"}}]}}]}}"#
            ),
            sig = sig
        )
    };
    let d =
        crate::proxy::latest_assistant_diff(run(r"ab\/cd==").as_bytes(), run("ab/cd==").as_bytes());
    assert_eq!(d.turn_same, Some(true), "逻辑值相同");
    assert_eq!(d.thinking_bytes_same, Some(false), "字节变了，且那个块不在串的最后一条上");
}

/// 逻辑值相同、字节不同：客户端把签名里的 `/` 写成 `\/`（PHP 那类编码器的默认），
/// luban 出站写回 `/`。`turn_same` 看不出来（Value 往返抹平转义），而上游对签名是按字节
/// 校验的——只认结构那一项就会把这条 400 的真凶判成无罪。
#[test]
fn latest_assistant_diff_catches_a_reencoded_signature() {
    let turn = |sig: &str| {
        format!(
            concat!(
                r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}}]}},"#,
                r#"{{"role":"assistant","content":[{{"type":"thinking","thinking":"t","signature":"{sig}"}}]}}]}}"#
            ),
            sig = sig
        )
    };
    let inbound = turn(r"ab\/cd==");
    let outbound = turn("ab/cd==");
    let d = crate::proxy::latest_assistant_diff(inbound.as_bytes(), outbound.as_bytes());
    assert_eq!(d.turn_same, Some(true), "逻辑值确实相同，这一项看不出问题");
    assert_eq!(d.thinking_bytes_same, Some(false), "字节变了，上游校验的正是它");
}

/// 反过来：整轮排版变了（客户端发缩进 JSON，出站恒紧凑），但思考块的字节被
/// `preserve_thinking_encoding` 原样还原。字节这一项只取思考块，正是为了不把这种
/// 「什么实质都没改」的请求指认成改过。
#[test]
fn latest_assistant_diff_ignores_reformatting_around_the_blocks() {
    let inbound = concat!(
        "{\n  \"messages\": [\n    {\"role\": \"user\", \"content\": [{\"type\": \"text\", \"text\": \"hi\"}]},\n",
        "    {\"role\": \"assistant\", \"content\": [{\"type\":\"thinking\",\"thinking\":\"t\",\"signature\":\"SIG\"}]}\n  ]\n}"
    );
    let outbound = concat!(
        r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]},"#,
        r#"{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"SIG"}]}]}"#
    );
    assert_ne!(inbound, outbound, "两份原始字节本来就不同");
    let d = crate::proxy::latest_assistant_diff(inbound.as_bytes(), outbound.as_bytes());
    assert_eq!(d.turn_same, Some(true));
    assert_eq!(d.thinking_bytes_same, Some(true), "块本身逐字相同，排版不算改");
}

/// 轮摘要封顶：块标签不含正文，但一轮几十块拼起来照样刷屏。
#[test]
fn latest_assistant_diff_caps_a_long_turn_label() {
    let blocks = (0..80)
        .map(|i| format!(r#"{{"type":"tool_use","id":"t{i}","name":"Bash","input":{{}}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(r#"{{"messages":[{{"role":"assistant","content":[{blocks}]}}]}}"#);
    let d = crate::proxy::latest_assistant_diff(body.as_bytes(), body.as_bytes());
    assert!(d.outbound_turn.contains("…(+"), "该截断: {}", d.outbound_turn);
    assert!(d.outbound_turn.chars().count() < 450, "截断后仍太长: {}", d.outbound_turn);
    assert_eq!(d.turn_same, Some(true), "截断只影响日志，不影响比对");
}

/// 块既没有 `signature` 也没有 `data`：没有可当身份的载荷，落 `unkeyed`。
/// 不能落 `none`——那一档的意思是「luban 把它改坏了」。
#[test]
fn marks_a_payloadless_block_unkeyed_not_missing() {
    let body = concat!(
        r#"{"model":"claude-sonnet-5","messages":["#,
        r#"{"role":"user","content":[{"type":"text","text":"hi"}]},"#,
        r#"{"role":"assistant","content":[{"type":"thinking","thinking":"t"}]}]}"#
    )
    .as_bytes();
    let t = crate::proxy::trace_thinking_block(body, body, 1, 0).expect("该坐标上有思考块");
    assert_eq!(t.inbound_at, "unkeyed");
    assert_eq!(t.payload_len, 0);
    assert_eq!(t.turn_identical, None);
}

/// 签名那条 400 走的是同一段对照：`thinking` 块按 `signature` 配对，与密文侧对称。
/// 配错块型的话 `payload_len` 会是密文那 9 个字节。
#[test]
fn traces_a_thinking_block_by_its_signature() {
    let body = traceable_body("ENCRYPTED", "Bash", false);
    let t = crate::proxy::trace_thinking_block(&body, &body, 1, 0).expect("该坐标上有思考块");
    assert_eq!(t.outbound_at, "messages.1.content.0");
    assert_eq!(t.inbound_at, "messages.1.content.0");
    assert_eq!(t.payload_len, "SIG".len(), "载荷记的是签名，不是同一轮里那段密文");
    assert_eq!(t.turn_identical, Some(true));
}

/// 签名侧的「luban 改坏了」：出站那个签名入站体里根本没有，`inbound_at=none`。
#[test]
fn traces_a_corrupted_thinking_signature() {
    let inbound = traceable_body("ENCRYPTED", "Bash", false);
    let outbound = String::from_utf8(inbound.clone())
        .expect("固定字面量")
        .replace("\"SIG\"", "\"XIG\"")
        .into_bytes();
    let t =
        crate::proxy::trace_thinking_block(&inbound, &outbound, 1, 0).expect("该坐标上有思考块");
    assert_eq!(t.inbound_at, "none");
    assert_eq!(t.turn_identical, None, "入站那一轮都定位不到，无从比对");
}

/// thinking 原文搬进 text、redacted_thinking 直接删，其余块与 key 序原样不动。
#[test]
fn demotes_thinking_to_text() {
    let raw = concat!(
        r#"{"model":"claude-opus-5","messages":["#,
        r#"{"role":"user","content":[{"type":"text","text":"hi"}]},"#,
        r#"{"role":"assistant","content":["#,
        r#"{"type":"thinking","thinking":"想了想","signature":"AAAA"},"#,
        r#"{"type":"redacted_thinking","data":"ZZZZ"},"#,
        r#"{"type":"text","text":"答案"}]}]}"#
    );
    let out = crate::proxy::demote_thinking_blocks(&Bytes::from(raw)).expect("应有可降级的块");
    let s = String::from_utf8(out.to_vec()).unwrap();

    assert!(!s.contains("\"thinking\""), "thinking 块应已消失: {s}");
    assert!(!s.contains("AAAA"), "签名应已丢弃: {s}");
    assert!(!s.contains("ZZZZ"), "redacted_thinking 应整块删掉: {s}");
    assert!(
        s.contains("<previous_thinking>\\n想了想\\n</previous_thinking>"),
        "推理原文应搬进 text: {s}"
    );
    assert!(s.contains(r#"{"type":"text","text":"答案"}"#), "原有 text 块应原样保留: {s}");
    // 降级块自己也照官方内容块的 type→text 键序写。
    assert!(s.contains(r#"{"type":"text","text":"<previous_thinking>"#), "降级块 key 被重排: {s}");
}

/// user 轮不碰（它本来就没有 thinking 块，扫到也不该动），没得降级时返回 None——
/// 避免为一条另有原因的 400 白发一次重试。
#[test]
fn skips_when_nothing_to_demote() {
    let raw = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
    assert!(crate::proxy::demote_thinking_blocks(&Bytes::from(raw)).is_none());
    // 非 JSON、以及没有 messages 的请求体都不该 panic。
    assert!(crate::proxy::demote_thinking_blocks(&Bytes::from_static(b"not json")).is_none());
    assert!(
        crate::proxy::demote_thinking_blocks(&Bytes::from_static(br#"{"model":"x"}"#)).is_none()
    );
}

/// 整轮只有 thinking 的 assistant 消息原样留着：降级完 `content` 会是空数组，
/// 那是上游必拒的形态，发出去反而把「多一次往返」变成「多一次注定失败的往返」。
#[test]
fn keeps_assistant_turn_that_would_become_empty() {
    let raw = concat!(
        r#"{"messages":[{"role":"assistant","content":["#,
        r#"{"type":"thinking","thinking":"  ","signature":"AAAA"}]},"#,
        r#"{"role":"assistant","content":[{"type":"thinking","thinking":"实打实","signature":"BBBB"},"#,
        r#"{"type":"text","text":"答案"}]}]}"#
    );
    let out = crate::proxy::demote_thinking_blocks(&Bytes::from(raw)).expect("第二轮可降级");
    let s = String::from_utf8(out.to_vec()).unwrap();
    assert!(s.contains("AAAA"), "空 thinking 那轮应原样留着: {s}");
    assert!(!s.contains("BBBB"), "第二轮仍应降级: {s}");
}

// ---------- thinking 块编码保持 ----------

#[test]
fn preserves_thinking_encoding_unicode_escape() {
    // 原始 body：thinking 内容含 < / >（如 Python json.dumps 产出）。
    let original = b"{\"model\":\"x\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"},{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"hello \\u003cworld\\u003e\",\"signature\":\"sig==\"},{\"type\":\"text\",\"text\":\"ok\"}]},{\"role\":\"user\",\"content\":\"bye\"}],\"stream\":true}";

    let orig_str = std::str::from_utf8(original.as_ref()).unwrap();
    assert!(orig_str.contains(r"\u003c"), "original should contain \\u003c escape: {orig_str}");

    // serde 反序列化把 < 解码成 <，重新序列化变成 literal <world>。
    let mut v: serde_json::Value = serde_json::from_slice(original.as_ref()).unwrap();
    v["stream"] = serde_json::Value::Bool(false);
    let rewritten = serde_json::to_vec(&v).unwrap();
    let rw_str = std::str::from_utf8(&rewritten).unwrap();
    assert!(
        rw_str.contains("hello <world>") && !rw_str.contains(r"\u003c"),
        "serde should decode \\u003c to literal <: {rw_str}"
    );

    let fixed = crate::proxy::preserve_thinking_encoding(original, rewritten);
    let fixed_str = std::str::from_utf8(&fixed).unwrap();

    // thinking 块应保留原始的 < 编码——不是解码后的 <。
    assert!(
        fixed_str.contains(r"\u003c") && fixed_str.contains(r"\u003e"),
        "thinking content should preserve original \\u003c encoding: {fixed_str}"
    );
    // 非 thinking 内容的改动（stream: false）应保留。
    assert!(
        fixed_str.contains(r#""stream":false"#),
        "non-thinking modifications should be preserved: {fixed_str}"
    );
}

#[test]
fn preserve_thinking_noop_when_no_thinking() {
    let body = br#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#;
    let rewritten = body.to_vec();
    let result = crate::proxy::preserve_thinking_encoding(body, rewritten.clone());
    assert_eq!(result, rewritten);
}

/// 只有 `redacted_thinking` 块、顶层没有 `thinking` 字段的体：入口判据以前只找
/// `"thinking"`，这类体整段跳过不还原。而 base64 里有 `/`、`\/` 又是合法 JSON 转义
/// （PHP 那类编码器默认就这么写），serde 往返会把它还原成 `/`——逻辑值没变、字节变了，
/// 上游按字节校验必拒，且降级重试也救不回下一轮。
#[test]
fn preserve_thinking_restores_escaped_slashes_in_a_redacted_only_body() {
    let original = br#"{"model":"claude-sonnet-5","messages":[{"role":"assistant","content":[{"type":"redacted_thinking","data":"ab\/cd+ef\/gh"}]}]}"#;
    let mut v: serde_json::Value = serde_json::from_slice(original.as_ref()).unwrap();
    v["stream"] = true.into(); // 任意一处真实改写，逼出一次重新序列化
    let rewritten = serde_json::to_vec(&v).unwrap();
    assert!(
        !String::from_utf8(rewritten.clone()).unwrap().contains(r"ab\/cd"),
        "前提：serde 会把 \\/ 写回成 /"
    );

    let fixed = crate::proxy::preserve_thinking_encoding(original, rewritten);
    let s = String::from_utf8(fixed).unwrap();
    assert!(
        s.contains(r#"{"type":"redacted_thinking","data":"ab\/cd+ef\/gh"}"#),
        "整块原始字节应还原（含 \\/ 转义）: {s}"
    );
    assert!(s.contains(r#""stream":true"#), "还原只针对思考块，改写本身不该被回滚: {s}");
}

#[test]
fn preserve_thinking_handles_redacted() {
    let original = br#"{"messages":[{"role":"assistant","content":[{"type":"redacted_thinking","data":"abc+123"},{"type":"text","text":"ok"}]}]}"#;
    let mut v: serde_json::Value = serde_json::from_slice(original.as_ref()).unwrap();
    v["stream"] = true.into();
    let rewritten = serde_json::to_vec(&v).unwrap();

    let fixed = crate::proxy::preserve_thinking_encoding(original, rewritten);
    let s = std::str::from_utf8(&fixed).unwrap();
    assert!(
        s.contains(r#""data":"abc+123""#),
        "redacted_thinking data should preserve original encoding: {s}"
    );
}

// ---------- 空 thinking 块剥除 ----------
