use super::FAKE_TOOL_NS;
use crate::proxy::test_support::{
    ACCOUNT_UUID, API_SHAPE_BODY, PLAIN_BODY, all_on, base_block, detect_for, detect_with,
    err_json, parsed, platform_headers, rewrite_body, sim_for, test_cred,
};
use crate::proxy::{
    Bytes, HeaderValue, apply_tool_names, build_forward_headers, build_tool_name_map, config,
    ensure_billing_cch, header, is_billable_messages, merge_beta, normalize_tool_choice,
    replace_json_str_field, store, strip_extra_fields,
};

/// 设备身份校验与出站体改写的作用域：只认 `/v1/messages`，且 `count_tokens` 除外
/// ——那条路径的请求体没有 `metadata` 可带，卡它等于把客户端的 token 预估打死。
///
/// 入参是 `uri.path()`（不含查询串），故 `?beta=true` 不影响判定。
#[test]
fn count_tokens_is_not_billable() {
    assert!(is_billable_messages("/v1/messages"));
    assert!(!is_billable_messages("/v1/messages/count_tokens"));
    assert!(!is_billable_messages("/v1/models"));
}

/// 豁免精确匹配：任何「顶着 count_tokens 前缀但归一化后不是它」的路径都必须落回计费侧。
/// 出站 URL 交给 wreq 时点段会按 RFC 3986 消解，`…/count_tokens/../` 到上游就成了
/// `/v1/messages/`——前缀匹配会在这里漏掉设备校验，等于放开 `device_limit`。
#[test]
fn count_tokens_exemption_does_not_leak_via_prefix() {
    assert!(is_billable_messages("/v1/messages/count_tokens/.."));
    assert!(is_billable_messages("/v1/messages/count_tokens/../"));
    assert!(is_billable_messages("/v1/messages/count_tokens/"));
    assert!(is_billable_messages("/v1/messages/count_tokensX"));
}

/// 会话 id 的 body 兜底提取：两种 `metadata.user_id` 格式都要认得，且与设备 id 取的是
/// **同一串里的不同段**——两者串了的话，会话闸会按设备分桶（同机多会话又挤在一起），
/// 而这恰好是它要解决的问题。
#[test]
fn session_id_comes_from_either_user_id_format() {
    // 1) CC 内嵌 JSON。
    let inner = Bytes::from(
            r#"{"messages":[],"metadata":{"user_id":"{\"device_id\":\"d0\",\"account_uuid\":\"a0\",\"session_id\":\"5e3f\"}"}}"#
                .to_string(),
        );
    assert_eq!(crate::proxy::extract_session_id(parsed(&inner).as_ref()).as_deref(), Some("5e3f"));
    assert_eq!(crate::proxy::extract_device_id(parsed(&inner).as_ref()).as_deref(), Some("d0"));

    // 2) 扁平串（Windows 客户端那种形态），account 段允许为空。
    let flat = Bytes::from(
        r#"{"messages":[],"metadata":{"user_id":"user_dev9_account__session_sess9"}}"#.to_string(),
    );
    assert_eq!(crate::proxy::extract_session_id(parsed(&flat).as_ref()).as_deref(), Some("sess9"));
    assert_eq!(crate::proxy::extract_device_id(parsed(&flat).as_ref()).as_deref(), Some("dev9"));

    // 3) 认不出的格式 / 没有 metadata → None，此时这条请求不受会话闸管（由设备闸兜）。
    let odd =
        Bytes::from(r#"{"messages":[],"metadata":{"user_id":"whatever-new-format"}}"#.to_string());
    assert!(crate::proxy::extract_session_id(parsed(&odd).as_ref()).is_none());
    assert!(crate::proxy::extract_session_id(parsed(&Bytes::from("{}")).as_ref()).is_none());
}

/// 真实 CC（API-key 模式）的请求，body 侧要配套补 `thinking.display:"updates"`
/// （fable：`cap/2.1.258/00013`；2.1.260 起 opus 主线程也发，`cap/2.1.260-2/00025`）。
/// 客户端自己写了 `display` 的不动；`merge_beta` 关着就不补。
///
/// **判据只有一个**：出站头里有没有那项 beta（这里的 `display_beta` 参数恒为 true）。
/// 哪一族在哪一版发它由 [`crate::proxy::merge_beta_for`] 决定，见
/// [`skips_thinking_display_without_the_beta`]。模拟路径不走这条（它由
/// `ensure_thinking` 直接产出完整形态）。
#[test]
fn fills_thinking_display_for_cc_fable_requests() {
    let body = |model: &str, thinking: &str| {
        Bytes::from(format!(
            r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}}],"thinking":{thinking}}}"#
        ))
    };
    let run = |b: &Bytes, flags: store::ForwardFlags| -> serde_json::Value {
        serde_json::from_slice(&rewrite_body(b, &test_cred(), "fp", flags, None, None)).unwrap()
    };
    let fable = run(&body("claude-fable-5-1", r#"{"type":"adaptive"}"#), all_on());
    assert_eq!(
        fable["thinking"],
        serde_json::json!({"type": "adaptive", "display": "updates"}),
        "{fable}"
    );
    let fable_1m = run(&body("claude-fable-5-1[1m]", r#"{"type":"adaptive"}"#), all_on());
    assert_eq!(fable_1m["thinking"]["display"], "updates", "{fable_1m}");
    // 2.1.260 起 opus 主线程也发这项 beta，头上有了体里就该配套写。
    let opus = run(&body("claude-opus-5", r#"{"type":"adaptive"}"#), all_on());
    assert_eq!(
        opus["thinking"],
        serde_json::json!({"type": "adaptive", "display": "updates"}),
        "头上有 beta 就补，与模型族无关: {opus}"
    );
    let own =
        run(&body("claude-fable-5-1", r#"{"type":"adaptive","display":"summarized"}"#), all_on());
    assert_eq!(own["thinking"]["display"], "summarized", "客户端自己写的不动: {own}");
    let off = store::ForwardFlags { merge_beta: false, ..all_on() };
    let v = run(&body("claude-fable-5-1", r#"{"type":"adaptive"}"#), off);
    assert!(v["thinking"].get("display").is_none(), "merge_beta 关着就不补: {v}");
}

/// 回归 2026-09-02 的 400：`claude-vscode, agent-sdk/0.3.258` 发来的 fable-5-1 请求，
/// beta 串没有 `advisor-tool`，`merge_beta` 不给它补 `thinking-display-updates`，
/// body 却被写了 `display:"updates"`，上游回 `Input should be 'summarized', 'omitted'`。
/// 体侧的补写必须跟着「出站头里到底有没有那项 beta」走。
#[test]
fn skips_thinking_display_without_the_beta() {
    let body = Bytes::from(
        r#"{"model":"claude-fable-5-1","messages":[{"role":"user","content":"hi"}],"system":[{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}],"thinking":{"type":"adaptive"}}"#,
    );
    let out = crate::proxy::rewrite_body_out(
        &body,
        &test_cred(),
        "fp",
        all_on(),
        None,
        None,
        None,
        false,
        None,
        false,
        false,
        None,
        None,
        crate::proxy::CcRequestKind::Main,
        None,
    )
    .0;
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["thinking"], serde_json::json!({"type": "adaptive"}), "头上没 beta 就不补: {v}");

    // agent-sdk 那串（无 advisor-tool）经 merge_beta 也确实不会带上那项 beta——两边口径一致。
    let sdk_beta = "claude-code-20250219,interleaved-thinking-2025-05-14,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,effort-2025-11-24";
    let merged = merge_beta(Some(sdk_beta), Some("claude-fable-5-1"), Some((2, 1, 258)));
    assert!(!merged.contains("thinking-display-updates"), "老世代的串不补: {merged}");
}

/// 真 CC（API-key 三块形态）请求，工具五种形态各一个：内建、客户端显式写了 `false` 的内建、
/// `mcp__*`、`defer_loading` 占位、服务端工具。
fn eager_body(model: &str, with_identity: bool) -> Bytes {
    // 非 CC 形态：既没有身份句也没有 billing header（[`is_cc_shaped`] 两样任一都算 CC）。
    let system = if with_identity {
        serde_json::json!([
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli;"},
            {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude.", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "\nBASE\n\nWrite code that reads like the surrounding code.", "cache_control": {"type": "ephemeral"}}
        ])
    } else {
        serde_json::json!([{"type": "text", "text": "You are a helpful assistant."}])
    };
    serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "system": system,
            "tools": [
                {"name": "Bash", "description": "run", "input_schema": {"type": "object"}},
                {"name": "Read", "description": "read", "input_schema": {"type": "object"}, "eager_input_streaming": false},
                {"name": "mcp__ide__getDiagnostics", "description": "d", "input_schema": {"type": "object"}},
                {"name": "DeferredToolPlaceholder", "description": "p", "input_schema": {"type": "object"}, "defer_loading": true},
                {"type": "web_search_20250305", "name": "web_search", "max_uses": 3}
            ],
            "max_tokens": 64000,
            "stream": true
        })
        .to_string()
        .into()
}

/// 真 CC 路径跑一遍 [`rewrite_body`]，只拨 eager 相关的几个入参。
fn run_eager(
    model: &str,
    version: Option<&str>,
    kind: crate::proxy::CcRequestKind,
    adv_beta: bool,
    flags: store::ForwardFlags,
    with_identity: bool,
) -> serde_json::Value {
    let out = crate::proxy::rewrite_body_out(
        &eager_body(model, with_identity),
        &test_cred(),
        "fp",
        flags,
        None,
        None,
        None,
        false,
        None,
        true,
        adv_beta,
        version.map(|version| crate::proxy::CcClient { version, entrypoint: "cli" }),
        None,
        kind,
        None,
    )
    .0;
    serde_json::from_slice(&out).unwrap()
}

fn eager_of<'a>(v: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .and_then(|t| t.get("eager_input_streaming"))
}

/// 真 CC 路径：已证「全带」的 profile（2.1.258 四族、2.1.260 opus、2.1.270 sonnet）给内建工具
/// 补 `eager_input_streaming: true`，键落在对象末尾（官方声明序）；客户端显式写的 `false`、
/// `mcp__*`、占位、服务端工具一个不动。猜下一句那条用途同样补（`cap/2.1.258/00025`）。
#[test]
fn fills_eager_input_streaming_on_verified_real_cc_profiles() {
    use crate::proxy::CcRequestKind::{Main, Suggestion};
    for (model, version, kind) in [
        ("claude-opus-5", "2.1.258", Main),
        ("claude-haiku-4-5-20251001", "2.1.258", Main),
        ("claude-fable-5-1", "2.1.258", Main),
        ("claude-opus-5", "2.1.260", Main),
        ("claude-sonnet-5", "2.1.270", Main),
        ("claude-opus-5", "2.1.258", Suggestion),
    ] {
        let v = run_eager(model, Some(version), kind, true, all_on(), true);
        let tag = format!("{model} {version} {kind:?}");
        assert_eq!(eager_of(&v, "Bash"), Some(&serde_json::json!(true)), "{tag}: 内建该补");
        assert_eq!(
            eager_of(&v, "Read"),
            Some(&serde_json::json!(false)),
            "{tag}: 显式 false 不覆盖"
        );
        assert!(eager_of(&v, "mcp__ide__getDiagnostics").is_none(), "{tag}: mcp 不猜");
        assert!(eager_of(&v, "DeferredToolPlaceholder").is_none(), "{tag}: 占位不动");
        assert!(eager_of(&v, "web_search").is_none(), "{tag}: 服务端工具不动");
        let bash = v["tools"].as_array().unwrap().iter().find(|t| t["name"] == "Bash").unwrap();
        assert_eq!(
            bash.as_object().unwrap().keys().next_back().map(String::as_str),
            Some("eager_input_streaming"),
            "{tag}: 键在末尾"
        );
    }
}

/// 真 CC 路径不补的每一种：profile 证实不带（2.1.260 fable）、没有样本（2.1.260 sonnet /
/// haiku、2.1.270 opus、样本之间的 2.1.261）、出站头没有 `advanced-tool-use`、读不出版本、
/// 用途不是主线程、开关关着、来访不是 CC 形态。
///
/// 2.1.270 / 2.1.261 的 opus 钉的是「证据查表不走 beta 那套兜底」：beta 参照给 2.1.270 的
/// opus 落回 2.1.260 那行是兼容需要，eager 没抓过就是没证据。
#[test]
fn skips_eager_input_streaming_without_evidence() {
    use crate::proxy::CcRequestKind::{Helper, Main, Subagent};
    let none = |v: &serde_json::Value, why: &str| {
        assert!(eager_of(v, "Bash").is_none(), "{why}: {}", v["tools"]);
    };
    none(
        &run_eager("claude-fable-5-1", Some("2.1.260"), Main, true, all_on(), true),
        "fable 2.1.260 证实不带",
    );
    none(
        &run_eager("claude-sonnet-5", Some("2.1.260"), Main, true, all_on(), true),
        "sonnet 2.1.260 没样本",
    );
    none(
        &run_eager("claude-haiku-4-5-20251001", Some("2.1.260"), Main, true, all_on(), true),
        "haiku 2.1.260 没样本",
    );
    none(
        &run_eager("claude-opus-5", Some("2.1.270"), Main, true, all_on(), true),
        "opus 2.1.270 没样本，不继承 2.1.260",
    );
    none(
        &run_eager("claude-opus-5", Some("2.1.261"), Main, true, all_on(), true),
        "样本之间的版本没样本",
    );
    none(
        &run_eager("claude-opus-5", Some("2.1.258"), Main, false, all_on(), true),
        "头上没 advanced-tool-use",
    );
    none(&run_eager("claude-opus-5", None, Main, true, all_on(), true), "读不出版本");
    none(
        &run_eager("claude-opus-5", Some("2.1.258"), Subagent, true, all_on(), true),
        "子代理没证据",
    );
    none(
        &run_eager("claude-opus-5", Some("2.1.258"), Helper, true, all_on(), true),
        "helper 没证据",
    );
    let off = store::ForwardFlags { eager_tool_streaming: false, ..all_on() };
    none(&run_eager("claude-opus-5", Some("2.1.258"), Main, true, off, true), "开关关着");
    none(&run_eager("claude-opus-5", Some("2.1.258"), Main, true, all_on(), false), "非 CC 形态");
}

/// 模拟路径：按**出站 profile** 判，与来访自报的版本无关。opus（On）给客户端保留的工具补；
/// fable（Off）不补；sonnet（Unknown）跟随注入的 opus 资产，也补。注入的 14 个官方工具不受
/// 影响——它们自带取值（opus 全带、fable 全不带）。
#[test]
fn simulated_eager_input_streaming_follows_the_outbound_profile() {
    let body = |model: &str| -> Bytes {
        serde_json::json!({
                "model": model,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [
                    {"name": "my_tool", "description": "t", "input_schema": {"type": "object"}},
                    {"name": "explicit_off", "description": "t", "input_schema": {"type": "object"}, "eager_input_streaming": false},
                    {"name": "mcp__x__y", "description": "t", "input_schema": {"type": "object"}}
                ],
                "stream": true
            })
            .to_string()
            .into()
    };
    // 2.1.277 四族主线程全带（fable 在 2.1.260 时不带，`cap/2.1.277/00023` 起也带了）。
    for (model, expect) in
        [("claude-opus-5", true), ("claude-fable-5-1", true), ("claude-sonnet-5", true)]
    {
        let b = body(model);
        let sim = sim_for(std::str::from_utf8(&b).unwrap());
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        // 客户端工具在混淆之后带 `mcp__luban__` 前缀，按后缀找。
        let find = |suffix: &str| {
            v["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
                .cloned()
                .unwrap_or_else(|| panic!("{model}: 找不到 {suffix}: {}", v["tools"]))
        };
        assert_eq!(
            find("my_tool").get("eager_input_streaming"),
            expect.then(|| serde_json::json!(true)).as_ref(),
            "{model}: 客户端工具"
        );
        assert_eq!(
            find("explicit_off")["eager_input_streaming"],
            false,
            "{model}: 显式 false 不覆盖"
        );
        assert!(
            find("mcp__x__y").get("eager_input_streaming").is_none(),
            "{model}: 来访自带的 mcp 不猜"
        );
        // 注入的官方工具与资产一致：2.1.277 的资产四族都带。
        let bash = find("Bash");
        assert_eq!(bash["eager_input_streaming"], true, "{model}: 注入的 Bash");
    }
}

/// 三块改写成官方的四块，且逐字段与 `cap/raw/00006` 的形态一致：
/// 身份句不再带断点、基座 `{type,ttl:1h,scope:global}`、其余 `{type,ttl:1h}`，
/// 消息里的断点也补上 `ttl`。切开处那个 `\n\n` 两边都不保留。
#[test]
fn aligns_system_to_official_four_blocks() {
    let out = rewrite_body(&Bytes::from(API_SHAPE_BODY), &test_cred(), "fp", all_on(), None, None);
    let s = String::from_utf8(out.to_vec()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    let sys = v["system"].as_array().unwrap();

    assert_eq!(sys.len(), 4, "应拆成四块: {s}");
    assert!(sys[0].get("cache_control").is_none(), "billing header 不该有断点: {s}");
    assert!(sys[1].get("cache_control").is_none(), "身份句上的断点应去掉: {s}");
    assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "基座切错: {s}");
    assert!(
        sys[3]["text"].as_str().unwrap().starts_with("Write code that reads like"),
        "其余部分应从锚点开始: {s}"
    );
    assert!(sys[3]["text"].as_str().unwrap().ends_with("\n\nREST"), "其余部分被截断: {s}");

    // 键序也要对：type → text → cache_control，cache_control 内 type → ttl → scope
    // （逐字节取自 `cap/raw/00006`）。
    assert!(
        s.contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#),
        "基座的 cache_control 形态不对: {s}"
    );
    // 官方三个断点**都**带 ttl，只有基座带 scope——包括来访自己标在消息上的那个：
    // 只补 system 那两个会得到「两个有、一个没有」这种官方不产生的组合，见
    // [`crate::proxy::fill_cache_ttl`]。
    assert_eq!(
        s.matches(r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#).count(),
        2,
        "system 末块与消息断点都该带 ttl、不带 scope: {s}"
    );
    assert!(
        !s.contains(r#""cache_control":{"type":"ephemeral"}"#),
        "不该再有裸 ephemeral（半对齐）: {s}"
    );

    // 关掉 `cache_ttl_1h` 即回到「沿用客户端时长」：一个 ttl 都不写。
    let no_ttl = store::ForwardFlags { cache_ttl_1h: false, ..all_on() };
    let out = rewrite_body(&Bytes::from(API_SHAPE_BODY), &test_cred(), "fp", no_ttl, None, None);
    let s = String::from_utf8(out.to_vec()).unwrap();
    assert!(!s.contains(r#""ttl""#), "关掉后不该替客户端写 ttl: {s}");
    assert!(s.contains(r#""cache_control":{"type":"ephemeral","scope":"global"}"#), "{s}");
}

/// 客户端 `tools` 上带 `ttl:"5m"` 时，`fill_cache_ttl` 应升级为 `"1h"`——
/// 否则处理序 tools(5m) → system(1h) 违反上游单调不增约束，产生 400。
#[test]
fn upgrades_short_ttl_to_1h() {
    let body = API_SHAPE_BODY.replace(
        r#""metadata":{""#,
        r#""tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"5m"}}],"metadata":{""#,
    );
    let out = rewrite_body(&Bytes::from(body), &test_cred(), "fp", all_on(), None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["tools"][0]["cache_control"]["ttl"], "1h", "tools 上的 5m 应升级为 1h: {v}");
    assert!(
        !out.as_ref().windows(3).any(|w| w == b"5m\""),
        "body 里不该残留 5m: {}",
        String::from_utf8_lossy(&out)
    );
}

/// 一份 body 里可能**同时**含多条锚点，此时必须切在最早的那个上。
///
/// 实例是 fable-5（`cap/raw/00035` 直连 ↔ `00037` 经 luban）：它自己的锚点
/// `# Communicating with the user` 在合并块偏移 1212，而 opus 那句
/// `Write code that reads like…` 也在正文里、偏移 3284。按表序先到先得会切在 3282，
/// 基座凭空多出 2072 字节；取最早命中才得到官方那 1210B 的基座。
#[test]
fn splits_at_earliest_anchor_when_several_match() {
    let raw = Bytes::from(API_SHAPE_BODY.replace(
            r#"\nBASE — 基座\n\nWrite code that reads like the surrounding code: match its comment density, naming, and idiom.\n\nREST"#,
            r#"\nBASE — 基座\n\n# Communicating with the user\n\nWrite code that reads like the surrounding code: match its comment density, naming, and idiom.\n\nREST"#,
        ));
    let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();

    assert_eq!(sys.len(), 4, "应拆成四块: {v}");
    assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "该切在最早的锚点上: {v}");
    assert!(
        sys[3]["text"].as_str().unwrap().starts_with("# Communicating with the user"),
        "其余部分应从最早那个锚点开始: {v}"
    );
}

/// 锚点是**按模型族**的：sonnet-5 的基座后面跟的不是 opus 那句，而是 `# Text output …`
/// （`cap/raw/00009` 直连 10676B 基座 ↔ `00012` 经 luban 合并块偏移 10678）。
/// haiku-4.5 与 sonnet-5 共用基座，命中的也是这一条。
#[test]
fn aligns_sonnet_shape_by_its_own_anchor() {
    let raw = Bytes::from(API_SHAPE_BODY.replace(
            "Write code that reads like the surrounding code: match its comment density, naming, and idiom.",
            "# Text output (does not apply to tool calls)",
        ));
    let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();

    assert_eq!(sys.len(), 4, "sonnet 锚点应能切块: {v}");
    assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "基座切错: {v}");
    assert!(
        sys[3]["text"].as_str().unwrap().starts_with("# Text output"),
        "其余部分应从 sonnet 锚点开始: {v}"
    );
}

/// 锚点还**按 CC 版本**漂：claude-cli/2.1.258 下 fable-5-1 的其余部分以
/// `Before you start, say in a line what you're about to do; …` 开头（`cap/2.1.258/00013`
/// 直连，基座 1214B，合并块偏移 1216），2.1.251 的三句一句都不在 body 里。没有这条锚点，
/// fable-5-1 的请求整形退回三块，`ttl:"1h"` 与 `scope:"global"` 一个都不写。
#[test]
fn aligns_fable_5_1_shape_by_its_2_1_258_anchor() {
    let raw = Bytes::from(
            API_SHAPE_BODY
                .replace("claude-opus-5", "claude-fable-5-1")
                .replace(
                    "Write code that reads like the surrounding code: match its comment density, naming, and idiom.",
                    "Before you start, say in a line what you're about to do; brief updates while you work help the user follow along.",
                ),
        );
    let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();

    assert_eq!(sys.len(), 4, "fable-5-1 @2.1.258 锚点应能切块: {v}");
    assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "基座切错: {v}");
    assert!(
        sys[3]["text"].as_str().unwrap().starts_with("Before you start, say in a line"),
        "其余部分应从 2.1.258 锚点开始: {v}"
    );
    assert_eq!(
        sys[2]["cache_control"],
        serde_json::json!({"type": "ephemeral", "ttl": "1h", "scope": "global"}),
        "整形成了才有 ttl:1h + scope:global: {v}"
    );
    assert_eq!(
        sys[3]["cache_control"],
        serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
        "其余那块只带 ttl: {v}"
    );
}

/// fable 族 API-key 模式是四块 `[billing, 身份(断点), reporting, 合并块(断点)]`
/// （`cap/2.1.258-api/00013`），要拆成订阅端官方的五块 `[billing, 身份, reporting, 基座, 其余]`
/// （`cap/2.1.258/00013`）。reporting 块逐字节匹配才认，其它四块形态不动。
#[test]
fn splits_fable_api_shape_with_reporting_block() {
    let merged =
        "\nBASE — 基座\n\nBefore you start, say in a line what you're about to do; brief updates.";
    let body = |system: serde_json::Value| {
        let mut v: serde_json::Value = serde_json::from_str(API_SHAPE_BODY).unwrap();
        v["model"] = "claude-fable-5-1".into();
        v["system"] = system;
        Bytes::from(serde_json::to_vec(&v).unwrap())
    };
    let billing =
        serde_json::json!({"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"});
    let identity = serde_json::json!({"type":"text","text":config::CC_SYSTEM_IDENTITY,"cache_control":{"type":"ephemeral"}});
    let reporting = serde_json::json!({"type":"text","text":config::CC_SYSTEM_REPORTING});

    let four = body(serde_json::json!([
        billing, identity, reporting,
        {"type":"text","text":merged,"cache_control":{"type":"ephemeral"}},
    ]));
    {
        let (name, raw) = ("四块", four);
        let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        assert_eq!(sys.len(), 5, "{name}: 应拆成官方五块: {v}");
        assert!(sys[1].get("cache_control").is_none(), "{name}: 身份句不带断点");
        assert_eq!(sys[2]["text"], config::CC_SYSTEM_REPORTING, "{name}: 第 2 块是 reporting");
        assert!(sys[2].get("cache_control").is_none(), "{name}: reporting 块不带断点");
        assert_eq!(sys[3]["text"], "\nBASE — 基座", "{name}: 基座切错: {v}");
        assert_eq!(
            sys[3]["cache_control"],
            serde_json::json!({"type":"ephemeral","ttl":"1h","scope":"global"}),
            "{name}: 基座断点"
        );
        assert!(
            sys[4]["text"].as_str().unwrap().starts_with("Before you start"),
            "{name}: 其余部分应从锚点开始"
        );
        assert_eq!(sys[4]["cache_control"], serde_json::json!({"type":"ephemeral","ttl":"1h"}));
    }

    // 四块但第三块不是 reporting（比如官方订阅四块形态 `[billing, 身份, 基座, 其余]`，或
    // 别的中间层塞的东西）：不动。
    let other = body(serde_json::json!([
        billing, identity,
        {"type":"text","text":"not the reporting block"},
        {"type":"text","text":merged,"cache_control":{"type":"ephemeral"}},
    ]));
    let out = rewrite_body(&other, &test_cred(), "fp", all_on(), None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["system"].as_array().unwrap().len(), 4, "认不出的四块不该动: {v}");
    assert!(!String::from_utf8_lossy(&out).contains("\"ttl\""), "没整形就不补 ttl");
}

/// 锚点匹配不到（未知模型族/新版本改了措辞）时**不动结构**，退回三块原样转发——
/// 宁可不拆，也不切在错误的位置上。其余两项改写照常。
#[test]
fn leaves_system_alone_when_anchor_missing() {
    let raw = Bytes::from(API_SHAPE_BODY.replace("Write code that reads like", "改了措辞的"));
    let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
    let s = String::from_utf8(out.to_vec()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();

    assert_eq!(v["system"].as_array().unwrap().len(), 3, "不该拆块: {s}");
    assert!(!s.contains("\"ttl\""), "不拆块时不应注入 ttl: {s}");
    assert!(!s.contains("\"scope\""), "不拆块时不应标 scope: {s}");
    assert!(s.contains("; cch="), "其余改写仍应生效: {s}");
}

/// 客户端本来就是订阅形态（四块）时不动 `system`——它已经是目标形态了。
/// cch 取这份 body 的真值：随手写的值对不上 body，`cch_real_recompute` 会把它重算掉。
#[test]
fn leaves_official_four_block_shape_alone() {
    let mut raw = (
            r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli; cch=00000;"},
                          {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
                          {"type":"text","text":"base","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}},
                          {"type":"text","text":"Write code that reads like the surrounding code: match its comment density, naming, and idiom.","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#
        )
            .as_bytes()
            .to_vec();
    super::apply_cch(&mut raw).unwrap();
    let raw = Bytes::from(raw);
    let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
    assert_eq!(out, raw, "四块形态应原样返回");
}

/// 一份 body JSON 文本里 `system` 的块数——拆没拆块看这个，不要去数 `cache_control`：
/// 拆块会同时**去掉**身份句上那个多余断点，总数不变（3 → 3），数不出差别。
fn sys_len(body: &str) -> usize {
    serde_json::from_str::<serde_json::Value>(body).unwrap()["system"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0)
}

/// 三项 body 改写全关 = **逐字节原样透传**：不重新序列化，故连缩进、换行、转义写法
/// 这些 serde 会归一化掉的细节都保持不变（重新序列化本身就是个形态 tell）。
#[test]
fn body_flags_off_passes_through_byte_for_byte() {
    // 刻意带上多余空白与换行：一旦走了 serde 往返，这些都会被抹平。
    let raw = Bytes::from(format!(" {}\n", API_SHAPE_BODY));
    let flags = store::ForwardFlags {
        spoof_identity: false,
        spoof_device_id: false,
        normalize_device_fp: false,
        billing_cch: false,
        cch_real_recompute: false,
        cch_sim_compute: false,
        fill_client_headers: false,
        merge_beta: false,
        system_shape: false,
        orig_header_case: false,
        thinking_signature_retry: false,
        thinking_modified_retry: false,
        redacted_thinking_retry: false,
        simulate_cc: false,
        simulate_full_system: false,
        fill_absent_tools: false,
        sim_trim_tools: false,
        sim_billing_only: false,
        sim_message_threads: false,
        fill_metadata: false,
        rate_limit_retry: false,
        cache_scope_global: false,
        cache_ttl_1h: false,
        eager_tool_streaming: false,
        nonstream_as_sse: false,
        strip_extra_fields: false,
        tool_name_mimic: false,
        inject_thinking: false,
        flatten_tool_schemas: true,
        strip_empty_text: true,
        hoist_system_role: false,
        reject_openai_shape: false,
        reject_session_conflict: false,
        reject_probes: false,
        reject_probes_strict: false,
        reject_refusals: false,
        reject_empty_replies: false,
        reject_learned_shapes: false,
        api_telemetry: false,
        keepalive_telemetry: false,
        fable_refusal_fallback: false,
        opus_refusal_fallback: false,
    };
    let out = rewrite_body(&raw, &test_cred(), "fp", flags, None, None);
    assert_eq!(out, raw, "全关时必须原样返回");

    // 逐项开一个，就只有那一项生效，其余仍不动。
    let only_cch = store::ForwardFlags { billing_cch: true, ..flags };
    let s =
        String::from_utf8(rewrite_body(&raw, &test_cred(), "fp", only_cch, None, None).to_vec())
            .unwrap();
    assert!(s.contains("; cch="), "只开 cch 时应补 cch: {s}");
    assert_eq!(sys_len(&s), 3, "system_shape 关着不应拆块: {s}");
    assert!(s.contains(r#"\"account_uuid\":\"\""#), "spoof 关着应保留空 uuid: {s}");

    // 拆块只需要 system_shape：它标的是裸 `{"type":"ephemeral"}`，GA 能力，不吃任何 beta。
    // cache_scope_global 开着但 merge_beta 关着：scope 仍不该出现。
    let shape_only = store::ForwardFlags { system_shape: true, cache_scope_global: true, ..flags };
    let s =
        String::from_utf8(rewrite_body(&raw, &test_cred(), "fp", shape_only, None, None).to_vec())
            .unwrap();
    assert_eq!(sys_len(&s), 4, "只开 system_shape 也该拆成四块: {s}");
    assert!(!s.contains(r#""scope""#), "scope 要 merge_beta 补的 beta 认，此时不该出现: {s}");

    // `scope:"global"` 才连着 merge_beta（prompt-caching-scope beta 由它补）。
    let with_beta = store::ForwardFlags { merge_beta: true, ..shape_only };
    let s =
        String::from_utf8(rewrite_body(&raw, &test_cred(), "fp", with_beta, None, None).to_vec())
            .unwrap();
    assert!(s.contains(r#""scope":"global""#), "两个开关都开时才标 global: {s}");
    assert!(!s.contains("cch="), "billing_cch 关着不应补 cch: {s}");

    // 单独关掉 cache_scope_global：照样拆块，只是不标 global。
    let no_scope = store::ForwardFlags { cache_scope_global: false, ..with_beta };
    let s =
        String::from_utf8(rewrite_body(&raw, &test_cred(), "fp", no_scope, None, None).to_vec())
            .unwrap();
    assert_eq!(sys_len(&s), 4, "关 scope 不影响拆块: {s}");
    assert!(!s.contains(r#""scope""#), "关掉后不该标 global: {s}");
}

/// [`rewrite_body_out`] 交回来的那份 `Value` 必须与它同时交回的出站字节**同构**。
///
/// 取证的形态摘要现在是拿这份 `Value` 算的，不再从出站字节重新解析一遍（一条请求少一次
/// 几 MB 的 JSON 解析，见 [`crate::proxy::shape_summary_of`]）。这条等式一旦不成立，
/// 流水里的 `shape` 列就开始描述一份**没发出去过**的体——那正是这一列唯一要回答的问题，
/// 而且错了没有任何症状。故在这里钉死：两条路算出来的摘要必须逐字相同。
///
/// 三种 `None` 也一并验：那几条路出站字节与入参逐字节相同，调用方拿来访那份算摘要，
/// 结果同样必须一致。
#[test]
fn the_returned_value_summarizes_the_same_as_the_bytes_it_sent() {
    let cases: Vec<(&str, Bytes, store::ForwardFlags)> = vec![
        (
            "改写过的模拟主线程",
            Bytes::from(
                r#"{"model":"claude-opus-5","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"tools":[{"name":"my_tool","description":"d","input_schema":{"type":"object"}}],"metadata":{"user_id":"{\"device_id\":\"dd\",\"account_uuid\":\"aa\",\"session_id\":\"ss\"}"},"max_tokens":64000}"#,
            ),
            all_on(),
        ),
        (
            "开关全关、无可改之处（走 None 那条）",
            Bytes::from(
                r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"}],"max_tokens":8}"#,
            ),
            // 走得进快速路径（连解析都不做）的那一套：这几项正是它检查的开关。
            store::ForwardFlags {
                system_shape: false,
                spoof_identity: false,
                billing_cch: false,
                cch_real_recompute: false,
                cch_sim_compute: false,
                strip_extra_fields: false,
                ..all_on()
            },
        ),
        ("不是 JSON（同样走 None）", Bytes::from_static(b"not json at all"), all_on()),
    ];
    for (label, body, flags) in cases {
        let (sent, value) = crate::proxy::rewrite_body_out(
            &body,
            &test_cred(),
            "fp",
            flags,
            None,
            None,
            None,
            false,
            None,
            true,
            true,
            None,
            None,
            super::CcRequestKind::Main,
            None,
        );
        // `None` 的约定是「出站字节与入参逐字节相同」，先把它本身钉住。
        if value.is_none() {
            assert_eq!(sent, body, "{label}：没交 Value 就必须是原样透传");
        }
        // 调用方的取值顺序：改写那份优先，没有就用来访那份（见 `Upstream::shape_outbound`）。
        let inbound = serde_json::from_slice::<serde_json::Value>(&body).ok();
        let from_value = match value.as_ref().or(inbound.as_ref()) {
            Some(v) => crate::proxy::shape_summary_of(v),
            None => Default::default(),
        };
        assert_eq!(
            from_value,
            crate::proxy::shape_summary(&sent),
            "{label}：借 Value 算的摘要与从出站字节解析出来的必须一致"
        );
    }
}

/// 改写后 body 的 key 顺序必须与入站逐字节一致，只允许新增字段追加在末尾。
///
/// serde_json 默认 `Map = BTreeMap`，会把整个 body（含嵌套对象）的 key 按字母序重排，
/// 得到官方客户端不会产生的排列。靠 `preserve_order` feature 兜住，本测试是它的看门狗：
/// 一旦该 feature 被摘掉，这里立刻失败。
#[test]
fn preserves_key_order() {
    // 客户端的真实字段次序，取自 cap/raw/00002 的原始报文体：顶层是
    // model→messages→system→tools→metadata→max_tokens→…→stream，system 块是 type→text，
    // cache_control 是 type→ttl→scope（luban 自己写的那份没有 ttl），metadata.user_id 内层是
    // device_id→account_uuid→session_id。字母序全都不是这样。
    //
    // （cap/*.json 里看到的字母序是抓包工具重新序列化的产物，不是线上的样子。）
    let raw = concat!(
        r#"{"model":"claude-opus-5","messages":[],"#,
        r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"},"#,
        r#"{"type":"text","text":"ident","cache_control":{"type":"ephemeral"}},"#,
        r#"{"type":"text","text":"base\n\nWrite code that reads like the surrounding code: "#,
        r#"match its comment density, naming, and idiom.","cache_control":{"type":"ephemeral"}}],"#,
        r#""tools":[],"#,
        r#""metadata":{"user_id":"{\"device_id\":\"dddd\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}"},"#,
        r#""max_tokens":64000,"stream":true}"#
    );
    let out = rewrite_body(&Bytes::from(raw), &test_cred(), "fp", all_on(), None, None);
    let s = String::from_utf8(out.to_vec()).unwrap();

    // 三项改写都生效了（否则会走 body.clone() 早退，测试空过）。
    assert!(s.contains("; cch="), "应补 cch: {s}");
    assert!(s.contains(r#""scope":"global""#), "应对齐 system 形态: {s}");
    assert!(s.contains(&format!(r#"\"account_uuid\":\"{}\""#, ACCOUNT_UUID)), "应填 uuid: {s}");

    // 顶层顺序不变，未被字母序重排（重排后 max_tokens/messages 会跑到 model 前）。
    let mut at = 0;
    for k in ["model", "messages", "system", "tools", "metadata", "max_tokens", "stream"] {
        let needle = format!("\"{k}\":");
        let pos = s[at..].find(&needle).unwrap_or_else(|| panic!("顶层 key {k} 顺序错乱: {s}"));
        at += pos + needle.len();
    }

    // 嵌套对象同样不重排：system 块是 type→text（字母序会变成 text→type），
    // 拆块后新建的两块也按这个键序写回，cache_control 内是 type→ttl→scope
    // （字母序会变成 scope→ttl→type）。
    assert!(s.contains(r#"{"type":"text","text":"base""#), "system 块 key 被重排: {s}");
    assert!(
        s.contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#),
        "cache_control key 被重排: {s}"
    );

    // 内层 user_id 仍走定点替换，device_id→account_uuid→session_id 原序。
    assert!(
        s.contains(r#"\"device_id\":\""#)
            && s.find(r#"\"device_id\":\""#) < s.find(r#"\"account_uuid\":\""#),
        "内层 user_id key 被重排: {s}"
    );
}

fn body_with_system0(text: &str) -> serde_json::Value {
    serde_json::json!({"system": [{"type": "text", "text": text}]})
}

/// `ensure_billing_cch` 先把 `cch=00000;` 占位符追加到 billing header 末尾（真值由出站
/// 字节定型后的 [`apply_cch`] 回填，见 [`cch_matches_official_egress`]）：形态是
/// `; cch=00000;`，紧跟在原前缀之后、不动前缀。
#[test]
fn adds_cch_placeholder_in_official_shape() {
    const HEAD: &str = "x-anthropic-billing-header: cc_version=2.1.218.0b9; cc_entrypoint=cli;";
    let mut v = body_with_system0(HEAD);
    assert!(ensure_billing_cch(&mut v));
    let text = v["system"][0]["text"].as_str().unwrap();
    assert_eq!(text, format!("{HEAD} cch=00000;"), "占位符追加在前缀之后: {text}");
}

/// 已带 cch（订阅模式客户端）不重复追加；非 billing 块不动。
#[test]
fn cch_is_idempotent_and_scoped() {
    let mut has = body_with_system0(
        "x-anthropic-billing-header: cc_version=2.1.218.2d7; cc_entrypoint=cli; cch=0848d;",
    );
    assert!(!ensure_billing_cch(&mut has));

    let mut other = body_with_system0("You are Claude Code, Anthropic's official CLI for Claude.");
    assert!(!ensure_billing_cch(&mut other));

    let mut empty = serde_json::json!({"messages": []});
    assert!(!ensure_billing_cch(&mut empty));
}

/// 白名单策略：官方名/MCP前缀/server tool 保留原名，其余 custom tool 一律混淆。
#[test]
fn tool_map_skips_official_mcp_and_server_tools() {
    let body = serde_json::json!({"tools": [
        {"name": "Bash"},                                      // 官方白名单 → 保留
        {"name": "Read"},                                      // 官方白名单 → 保留
        {"name": "mcp__hermes__skill_manage"},                 // MCP前缀 → 保留
        {"type": "web_search_20250305", "name": "web_search"}, // server tool → 保留
        {"name": "delegate_task"},                             // 非官方 custom → 混淆
        {"name": "skill_manage"},                              // 非官方 custom → 混淆
        {"name": "sessions_spawn"},                            // 非官方 custom → 混淆
        {"name": "memory_search"},                             // 非官方 custom → 混淆
    ]});
    let map = build_tool_name_map(Some(&body)).expect("有非官方 custom tool 就该有映射");
    assert_eq!(map.forward.len(), 4, "应混淆 4 个非官方名: {:?}", map.forward);
    for should_mimic in ["delegate_task", "skill_manage", "sessions_spawn", "memory_search"] {
        assert!(map.forward.contains_key(should_mimic), "{should_mimic} 该被混淆");
    }
    for kept in ["Bash", "Read", "mcp__hermes__skill_manage", "web_search"] {
        assert!(!map.forward.contains_key(kept), "{kept} 该保留原名");
    }
    // 假名必须走已验证豁免的 MCP 命名空间。
    for fake in map.forward.values() {
        assert!(fake.starts_with("mcp__luban__"), "假名必须是 mcp__luban__ 前缀: {fake}");
    }

    // 全是官方名或 MCP 前缀 → 无映射。
    let clean = serde_json::json!({"tools": [
        {"name": "Bash"}, {"name": "mcp__x__y"}, {"name": "Edit"},
    ]});
    assert!(build_tool_name_map(Some(&clean)).is_none());
    assert!(build_tool_name_map(Some(&serde_json::json!({"tools": []}))).is_none());
    assert!(build_tool_name_map(Some(&serde_json::json!({}))).is_none());
}

/// 老版本 CC（2.1.258 之前）主线程直接声明 `Glob` / `Grep`，更老的还叫 `Task` / `TodoWrite` /
/// `KillShell` / `BashOutput`——都是官方名，混淆成 `mcp__luban__*` 反而是官方从不发的形态。
/// 同一条请求里 OpenClaw 那种小写业务名（`read` / `exec` / `sessions_spawn`）照旧混淆：
/// 白名单按大小写精确匹配，`read` 不因为有个 `Read` 就放行。
#[test]
fn tool_map_keeps_legacy_official_names_but_still_mimics_lookalikes() {
    let body = serde_json::json!({"tools": [
        {"name": "Glob"}, {"name": "Grep"}, {"name": "Task"}, {"name": "TodoWrite"},
        {"name": "KillShell"}, {"name": "BashOutput"}, {"name": "EndConversation"},
        {"name": "ArtifactComments"}, {"name": "MultiEdit"},
        {"name": "read"}, {"name": "exec"}, {"name": "sessions_spawn"},
        {"name": "qieman__GetFundDiagnosis"},
    ]});
    let map = build_tool_name_map(Some(&body)).expect("小写业务名该有映射");
    assert_eq!(map.forward.len(), 4, "只混淆 4 个非官方名: {:?}", map.forward);
    for kept in [
        "Glob",
        "Grep",
        "Task",
        "TodoWrite",
        "KillShell",
        "BashOutput",
        "EndConversation",
        "ArtifactComments",
        "MultiEdit",
    ] {
        assert!(!map.forward.contains_key(kept), "{kept} 是官方旧名，该保留");
    }
    for mimic in ["read", "exec", "sessions_spawn", "qieman__GetFundDiagnosis"] {
        assert!(map.forward.contains_key(mimic), "{mimic} 该被混淆");
    }

    // 只有老版本官方名 → 无映射，请求与回程两侧零开销。
    let legacy_only = serde_json::json!({"tools": [
        {"name": "Glob"}, {"name": "Grep"}, {"name": "Task"}, {"name": "Bash"},
    ]});
    assert!(build_tool_name_map(Some(&legacy_only)).is_none());
}

/// 同一组工具名两次构造得到同一套假名——否则每轮请求的假名都变，上游 prompt cache 全丢。
#[test]
fn tool_map_is_stable_for_the_same_tool_set() {
    let body = serde_json::json!({"tools": [
        {"name": "skill_manage"}, {"name": "skill_view"}, {"name": "skills_list"},
    ]});
    let a = build_tool_name_map(Some(&body)).unwrap();
    let b = build_tool_name_map(Some(&body)).unwrap();
    assert_eq!(a.forward, b.forward);

    // 工具集变了假名就该变（否则新旧两套名字会撞在一起）。
    let other = serde_json::json!({"tools": [{"name": "skill_manage"}]});
    let c = build_tool_name_map(Some(&other)).unwrap();
    assert_ne!(a.forward.get("skill_manage"), c.forward.get("skill_manage"));
}

/// 来访若恰好已有一个和生成假名同名的 MCP 工具，第三方工具仍必须得到映射，
/// 不能为了避免撞名就把真名漏给上游。
#[test]
fn tool_map_resolves_declared_mcp_alias_collision() {
    let one = serde_json::json!({"tools": [{"name": "skill_manage"}]});
    let first = build_tool_name_map(Some(&one)).unwrap();
    let occupied = first.forward["skill_manage"].clone();
    let collided = serde_json::json!({"tools": [
        {"name": "skill_manage"},
        {"name": occupied},
    ]});
    let map = build_tool_name_map(Some(&collided)).expect("撞名不得让映射消失");
    let alias = &map.forward["skill_manage"];
    assert_ne!(alias, &occupied);
    assert!(alias.starts_with(&format!("{occupied}_")), "应以稳定后缀解决撞名: {alias}");
}

/// 请求侧三处必须同时改：`tools[]`、`tool_choice`、历史里的 `tool_use`。
/// 漏掉第三处的话上游会因为 `tool_use` 引用未声明的工具名而拒掉整条请求。
#[test]
fn applies_tool_names_to_all_three_places() {
    let mut v = serde_json::json!({
    "tools": [{"name": "skill_manage"}, {"name": "Bash"}],
    "tool_choice": {"type": "tool", "name": "skill_manage"},
    "messages": [
        {"role": "assistant", "content": [
            {"type": "tool_use", "name": "skill_manage", "input": {}},
            {"type": "text", "text": "skill_manage 只是正文，不该动"},
        ]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "x"}]},
    ]});
    let snapshot = v.clone();
    let map = build_tool_name_map(Some(&snapshot)).unwrap();
    assert!(apply_tool_names(&mut v, &map));
    let fake = map.forward["skill_manage"].clone();

    assert_eq!(v["tools"][0]["name"], serde_json::json!(fake));
    assert_eq!(v["tools"][1]["name"], serde_json::json!("Bash"), "白名单不该动");
    assert_eq!(v["tool_choice"]["name"], serde_json::json!(fake));
    assert_eq!(v["messages"][0]["content"][0]["name"], serde_json::json!(fake));
    assert!(
        v["messages"][0]["content"][1]["text"].as_str().unwrap().contains("skill_manage"),
        "正文里的同名字符串不该被请求侧改写"
    );
}

/// 回程还原：假名换回真名，且**必须扛得住分块从假名中间切开**。
/// 切断那次还原不了的话，客户端会拿到假名、下一轮带着假名回来，上游再回一个 400。
#[test]
fn restores_tool_names_across_chunk_boundaries() {
    let body = serde_json::json!({"tools": [
        {"name": "skill_manage"}, {"name": "skill_view"}, {"name": "skills_list"},
    ]});
    let map = build_tool_name_map(Some(&body)).unwrap();
    let fake = map.forward["skill_manage"].clone();
    let wire = format!(
        r#"data: {{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"t","name":"{fake}","input":{{}}}}}}"#
    ) + "\n\n";

    // 一次性还原。
    assert_eq!(sse_restore(&map, wire.as_bytes(), wire.len()), wire.replace(&fake, "skill_manage"));

    // 逐字节喂（最坏的分块），必须拼回同样的结果，且尾巴要 flush 出来。
    let mut pending = Vec::new();
    let mut out = Vec::new();
    for b in wire.as_bytes() {
        out.extend_from_slice(&map.feed(&mut pending, &[*b], true));
    }
    out.extend_from_slice(&map.flush(&mut pending, true));
    assert_eq!(
        String::from_utf8(out).unwrap(),
        wire.replace(&fake, "skill_manage"),
        "分块还原结果必须与整段一致"
    );
}

/// 短假名是长假名的子串时，必须先替长的——否则长假名会被先吃掉一截。
#[test]
fn restore_replaces_longer_aliases_first() {
    let long = format!("{FAKE_TOOL_NS}fetch_abc00_long");
    let short = format!("{FAKE_TOOL_NS}fetch_abc00");
    let map = crate::proxy::ToolNameMap {
        forward: Default::default(),
        reverse: vec![
            (long.clone(), "REAL_LONG".to_string()),
            (short.clone(), "REAL_SHORT".to_string()),
        ],
    };
    let wire = format!("x {long} y {short} z");
    assert_eq!(
        String::from_utf8(map.restore(wire.as_bytes())).unwrap(),
        "x REAL_LONG y REAL_SHORT z"
    );
    // 命名空间前缀出现、后面却不是任何一个假名：原样留着，不许吃掉也不许错配。
    let stray = format!("see {FAKE_TOOL_NS}something_else and {short}");
    assert_eq!(
        String::from_utf8(map.restore(stray.as_bytes())).unwrap(),
        format!("see {FAKE_TOOL_NS}something_else and REAL_SHORT")
    );
}

/// [`ToolNameMap::restore`] 一趟扫的前提：**每个假名都以 [`FAKE_TOOL_NS`] 开头**。
///
/// 这个前提由 [`build_tool_name_map`] 独家保证。哪天那边换了拼法（比如为了缩短假名把
/// 命名空间去掉），还原侧会一个都扫不到——而请求照发、响应照回，症状要到客户端拿着假名
/// 发下一轮、上游回 400 才暴露。故在这里对着真正的生成结果验一遍，不靠手搓的映射表。
#[test]
fn every_generated_alias_lives_under_the_shared_namespace() {
    let body = serde_json::json!({"tools": [
            {"name": "my_tool", "input_schema": {"type": "object"}},
            {"name": "另一个工具", "input_schema": {"type": "object"}},
            {"name": "x", "type": "custom", "input_schema": {"type": "object"}},
            // 这几类保留原名，不该进映射表。
            {"name": "Bash", "input_schema": {"type": "object"}},
            {"name": "mcp__ide__getDiagnostics", "input_schema": {"type": "object"}},
            {"name": "web_search", "type": "web_search_20250305"}]});
    let map = build_tool_name_map(Some(&body)).expect("有可混淆的工具就该有映射表");
    assert_eq!(map.reverse.len(), 3, "只混淆那三个 custom tool");
    for (fake, _) in &map.reverse {
        assert!(fake.starts_with(FAKE_TOOL_NS), "假名 {fake} 不在共用命名空间下");
    }
    // 倒序也是还原的前提（同一位置先命中的就得是最长的那个）。
    assert!(
        map.reverse.windows(2).all(|w| w[0].0.len() >= w[1].0.len()),
        "reverse 必须按假名长度倒序"
    );
}

/// OpenAI 方言的 `tool_choice` 翻译成 Anthropic 对象形态；上游对非对象直接 400
/// `tool_choice: Input should be an object`。已是 Anthropic 形态或认不出的，一律不动。
#[test]
fn normalizes_openai_style_tool_choice() {
    let run = |tc: serde_json::Value| {
        let mut v =
            serde_json::json!({ "model": "claude-sonnet-5", "tool_choice": tc, "messages": [] });
        let changed = normalize_tool_choice(&mut v);
        (changed, v.get("tool_choice").cloned())
    };
    assert_eq!(run(serde_json::json!("auto")), (true, Some(serde_json::json!({"type": "auto"}))));
    assert_eq!(run(serde_json::json!("none")), (true, Some(serde_json::json!({"type": "none"}))));
    assert_eq!(
        run(serde_json::json!("required")),
        (true, Some(serde_json::json!({"type": "any"})))
    );
    assert_eq!(run(serde_json::json!("ANY")), (true, Some(serde_json::json!({"type": "any"}))));
    assert_eq!(run(serde_json::Value::Null), (true, None), "null 等于没写，删掉");
    assert_eq!(
        run(serde_json::json!({"type": "function", "function": {"name": "get_weather"}})),
        (true, Some(serde_json::json!({"type": "tool", "name": "get_weather"})))
    );
    assert_eq!(
        run(serde_json::json!({"type": "function"})),
        (true, Some(serde_json::json!({"type": "any"})))
    );
    // Anthropic 形态原样不动，附加键也不动。
    for keep in [
        serde_json::json!({"type": "auto"}),
        serde_json::json!({"type": "tool", "name": "x"}),
        serde_json::json!({"type": "any", "disable_parallel_tool_use": true}),
        serde_json::json!({"type": "none"}),
    ] {
        assert_eq!(run(keep.clone()), (false, Some(keep.clone())), "不该动: {keep}");
    }
    // 认不出的方言放行，让上游报它自己的错。
    assert_eq!(run(serde_json::json!("whatever")), (false, Some(serde_json::json!("whatever"))));
    assert_eq!(run(serde_json::json!(42)), (false, Some(serde_json::json!(42))));
    // 没有这个字段：零操作。
    let mut none = serde_json::json!({ "model": "claude-sonnet-5", "messages": [] });
    assert!(!normalize_tool_choice(&mut none));
    // 归一后与剥字段接力：`"auto"` 最终整个消失，与官方形态一致。
    let mut chain =
        serde_json::json!({ "model": "claude-sonnet-5", "tool_choice": "auto", "messages": [] });
    assert!(normalize_tool_choice(&mut chain));
    assert!(strip_extra_fields(&mut chain, false));
    assert!(chain.get("tool_choice").is_none(), "{chain}");
}

/// 官方从不发的顶层字段要剥掉，客户端真正要的语义不能动。
///
/// 判据取自 `cap/raw/00006`/`00009`：两份直连抓包都没有 `tool_choice`，
/// `thinking` 也都是裸的 `{"type":"adaptive"}`。
#[test]
fn strips_only_the_fields_official_never_sends() {
    // 等价于缺省的 tool_choice + thinking.display：都该剥。
    let mut v = serde_json::json!({
            "model": "claude-opus-5",
            "tool_choice": {"type": "auto"},
            "thinking": {"type": "adaptive", "display": "summarized"}});
    assert!(strip_extra_fields(&mut v, false));
    assert!(v.get("tool_choice").is_none(), "官方不发 tool_choice: {v}");
    assert_eq!(v["thinking"], serde_json::json!({"type": "adaptive"}), "display 应剥掉: {v}");

    // 强制选工具 / 强制用工具 / 关并行：都是客户端要的语义，一个都不能动。
    for keep in [
        serde_json::json!({"type": "tool", "name": "Bash"}),
        serde_json::json!({"type": "any"}),
        serde_json::json!({"type": "auto", "disable_parallel_tool_use": true}),
    ] {
        let mut v = serde_json::json!({ "tool_choice": keep.clone() });
        assert!(!strip_extra_fields(&mut v, false), "不该动: {keep}");
        assert_eq!(v["tool_choice"], keep);
    }

    // thinking.type == "disabled"：**只有 fable 族**要删（它不支持，上游直接 400），
    // 删掉整个字段让上游走 adaptive 默认值。
    let mut v = serde_json::json!({
            "model": "claude-fable-5",
            "thinking": {"type": "disabled"}});
    assert!(strip_extra_fields(&mut v, false));
    assert!(v.get("thinking").is_none(), "fable 上 disabled 应整个删掉: {v}");

    // 别的族不动：`{"type":"disabled"}` 是 2.1.260 三个官方辅助 profile 的正常形态
    // （无工具 helper / 标题生成是 haiku，安全分类是 sonnet）。删了既造出一个官方不
    // 产生的形态，又把客户端「不要思考」翻成了「随你」——那是要花钱的。
    for model in ["claude-haiku-4-5-20251001", "claude-sonnet-5", "claude-opus-5"] {
        let mut v = serde_json::json!({
                "model": model,
                "thinking": {"type": "disabled"}});
        assert!(!strip_extra_fields(&mut v, false), "{model}: 不该动");
        assert_eq!(v["thinking"], serde_json::json!({"type": "disabled"}), "{model}");
    }

    // thinking.type == "enabled" 不动。
    let mut v = serde_json::json!({
            "thinking": {"type": "enabled", "budget_tokens": 10000}});
    assert!(!strip_extra_fields(&mut v, false));
    assert_eq!(v["thinking"]["type"], "enabled");

    // 官方形态本身：走一遍什么也不改（对真实 CC 是空操作）。
    let mut official = serde_json::json!({
            "model": "claude-opus-5",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"}});
    let before = official.clone();
    assert!(!strip_extra_fields(&mut official, false));
    assert_eq!(official, before);
}

/// thinking 开着时上游要求 `top_p` 「不传或 >= 0.95」（线上撞到的原话：`top_p must be
/// greater than or equal to 0.95 or unset when thinking is enabled or in adaptive mode`）。
/// 与 temperature 那条同源：客户端设了不合规的值就剥掉，合规的与 thinking 关着的都不动。
#[test]
fn strips_low_top_p_when_thinking_is_on() {
    let req = |thinking: serde_json::Value, top_p: serde_json::Value| {
        serde_json::json!({
                "model": "claude-opus-4-6",
                "messages": [{"role": "user", "content": "hi"}],
                "thinking": thinking,
                "top_p": top_p})
    };
    // enabled / adaptive 两种开法，低于 0.95 都剥；非数字也剥（上游一样 400）。
    for thinking in [
        serde_json::json!({"type": "enabled", "budget_tokens": 2048}),
        serde_json::json!({"type": "adaptive"}),
    ] {
        for bad in [serde_json::json!(0.9), serde_json::json!(0.949), serde_json::json!("x")] {
            let mut v = req(thinking.clone(), bad.clone());
            assert!(strip_extra_fields(&mut v, false), "{thinking} + top_p={bad}: 应有改动");
            assert!(v.get("top_p").is_none(), "{thinking} + top_p={bad}: 应剥掉: {v}");
            assert!(v.get("thinking").is_some(), "thinking 自己不能动: {v}");
        }
        // 合规的取值照发。
        for ok in [serde_json::json!(0.95), serde_json::json!(1.0)] {
            let mut v = req(thinking.clone(), ok.clone());
            assert!(!strip_extra_fields(&mut v, false), "{thinking} + top_p={ok}: 不该动");
            assert_eq!(v["top_p"], ok);
        }
    }
    // thinking 关着（disabled 且非 fable 族）或压根没传：top_p 随便填，不归这里管。
    let mut v = req(serde_json::json!({"type": "disabled"}), serde_json::json!(0.5));
    assert!(!strip_extra_fields(&mut v, false), "disabled: 不该动: {v}");
    assert_eq!(v["top_p"], 0.5);
    let mut v = serde_json::json!({ "model": "claude-opus-4-6", "top_p": 0.5 });
    assert!(!strip_extra_fields(&mut v, false), "无 thinking: 不该动: {v}");
    assert_eq!(v["top_p"], 0.5);
}

/// 2.1.258 起官方 CC 自己发 `thinking: {type: adaptive, display: "updates"}`
/// （`cap/2.1.258/00013`）。CC 形态的来访不剥 `display`；非 CC 形态照剥。
#[test]
fn keeps_thinking_display_for_cc_shaped_requests() {
    let mut cc = serde_json::json!({
            "model": "claude-fable-5-1",
            "system": [{"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}],
            "thinking": {"type": "adaptive", "display": "updates"}});
    assert!(!strip_extra_fields(&mut cc, true), "官方形态无可剥: {cc}");
    assert_eq!(
        cc["thinking"],
        serde_json::json!({"type": "adaptive", "display": "updates"}),
        "CC 自己发的 display 不能动: {cc}"
    );

    let mut third_party = serde_json::json!({
            "model": "claude-fable-5-1",
            "system": "You are a helpful assistant.",
            "thinking": {"type": "adaptive", "display": "updates"}});
    assert!(strip_extra_fields(&mut third_party, false));
    assert_eq!(
        third_party["thinking"],
        serde_json::json!({"type": "adaptive"}),
        "非 CC 形态照剥: {third_party}"
    );
}

/// 空壳 system 的清理要能**自己撑起整条改写**：所有改写开关都关着时，入口的快速返回与
/// 末尾的「什么都没改就回原体」都不能把它漏掉——漏掉就是空壳照样出站、上游照样 400。
#[test]
fn dropping_an_empty_system_message_survives_both_early_returns() {
    let flags = store::ForwardFlags {
        system_shape: false,
        spoof_identity: false,
        billing_cch: false,
        cch_real_recompute: false,
        cch_sim_compute: false,
        strip_extra_fields: false,
        flatten_tool_schemas: false,
        strip_empty_text: false,
        // 提升那步关掉：空壳的清理不该挂在它身上。
        hoist_system_role: false,
        ..store::ForwardFlags::default()
    };
    let body = Bytes::from(
        r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"},{"role":"system","content":[]}]}"#,
    );
    let out = rewrite_body(&body, &test_cred(), "fp", flags, None, None);
    let s = String::from_utf8(out.to_vec()).unwrap();
    assert!(!s.contains(r#""role":"system""#), "空壳该被丢掉: {s}");
    assert!(s.contains(r#""content":"hi""#), "用户消息要留着: {s}");
    // 反向：同一套开关下，没有空壳的体一个字节都不该动。
    let clean =
        Bytes::from(r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"}]}"#);
    assert_eq!(rewrite_body(&clean, &test_cred(), "fp", flags, None, None), clean);
}

/// 入口快速路径的粗筛容得下缩进：`"role": "system"`（键值之间有空白、还带换行）与紧凑写法
/// 一样要进解析路径，否则 pretty-print 过的体会带着空壳原样出站。
#[test]
fn the_fast_path_probe_tolerates_pretty_printed_json() {
    let pair = |b: &str| crate::proxy::body_has_pair(b.as_bytes(), b"\"role\"", b"\"system\"");
    assert!(pair(r#"{"role":"system"}"#));
    assert!(pair("{\"role\": \"system\"}"));
    assert!(pair("{\n  \"role\"\t:\r\n    \"system\"\n}"));
    assert!(!pair(r#"{"role":"user","system":"x"}"#), "别的键值对不算");
    assert!(!pair(r#"{"role":"systematic"}"#), "值要整段对上：闭引号把它钉死，systematic 不算");
    assert!(!pair(r#"{"role" "system"}"#), "缺冒号不算");
    assert!(!pair(r#"{"rolex":"system"}"#), "键不是 role 不算");
    assert!(!pair(r#"{"role":"#), "截断的体不算，也不能越界");

    // 空 text 块那一项同样容空白。
    let text = |b: &str| crate::proxy::body_has_pair(b.as_bytes(), b"\"text\"", b"\"\"");
    assert!(text(r#"{"text":""}"#));
    assert!(text("{\"text\" : \"\"}"));
    assert!(!text(r#"{"text":"x"}"#));

    // 端到端：所有改写开关都关着 + 缩进过的体，空壳照样被丢掉。
    let flags = store::ForwardFlags {
        system_shape: false,
        spoof_identity: false,
        billing_cch: false,
        cch_real_recompute: false,
        cch_sim_compute: false,
        strip_extra_fields: false,
        flatten_tool_schemas: false,
        strip_empty_text: false,
        hoist_system_role: false,
        ..store::ForwardFlags::default()
    };
    let pretty = Bytes::from(
        "{\n  \"model\": \"claude-opus-5\",\n  \"messages\": [\n    \
             {\"role\": \"user\", \"content\": \"hi\"},\n    \
             {\"role\": \"system\", \"content\": []}\n  ]\n}",
    );
    let out = rewrite_body(&pretty, &test_cred(), "fp", flags, None, None);
    let s = String::from_utf8(out.to_vec()).unwrap();
    assert!(!s.contains(r#""role":"system""#), "缩进过的体里的空壳也该被丢掉: {s}");
    assert!(s.contains(r#""content":"hi""#), "用户消息要留着: {s}");
}

/// 提升跑不跑只看这一处：严格检查开着时不跑（放行的中途 system 是原生消息，原样出站），
/// CC 形态不跑，`hoist_system_role` 关掉不跑。
#[test]
fn hoisting_is_off_while_the_strict_check_is_on() {
    let on = all_on();
    assert!(on.hoist_system_role && on.reject_openai_shape, "默认两个都开");
    assert!(!super::hoists_system_role(&on, false), "严格检查开着：不提升");
    let repair = store::ForwardFlags { reject_openai_shape: false, ..all_on() };
    assert!(super::hoists_system_role(&repair, false));
    assert!(!super::hoists_system_role(&repair, true), "CC 形态不提升");
    let off = store::ForwardFlags { hoist_system_role: false, ..repair };
    assert!(!super::hoists_system_role(&off, false));

    // 走完整条改写：默认开关下中途那条 system 连同它自带的字段原样出站；关掉严格检查才提升。
    let body = Bytes::from(
        r#"{"model":"claude-sonnet-5","max_tokens":8,"messages":[{"role":"user","content":"hi"},{"role":"system","content":"be brief","clear_at":"x"}]}"#,
    );
    let kept: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&body, &test_cred(), "fp", on, None, None)).unwrap();
    assert_eq!(kept["messages"][1]["role"], "system");
    assert_eq!(kept["messages"][1]["clear_at"], "x", "消息级字段不能丢");
    let hoisted: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&body, &test_cred(), "fp", repair, None, None))
            .unwrap();
    assert_eq!(hoisted["messages"].as_array().unwrap().len(), 1);
    assert!(hoisted["system"].to_string().contains("be brief"));
}

/// 开头与对话中途的 `role:"system"` 一并提升：中途那条老模型不认，留着就是一条修得好
/// 却没修的 400。顺序按原序，客户端原有的顶层 system 排在后面。
#[test]
fn all_system_role_messages_are_hoisted() {
    let mut v = serde_json::json!({
        "system": "orig",
        "messages": [
            {"role": "system", "content": "a"},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "go on"},
            {"role": "system", "content": [{"type": "text", "text": "mid"}]}
        ]
    });
    assert!(crate::proxy::body::hoist_system_role_messages(&mut v));
    let texts: Vec<_> =
        v["system"].as_array().unwrap().iter().map(|b| b["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["a", "mid", "orig"]);
    let roles: Vec<_> =
        v["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
}

/// 空壳 `role:"system"` 消息在出站前被丢掉：五种空形态都算（空数组、空串、`null`、
/// 字段缺失、整条只有空 text 块），带内容的一字不动，别的角色一概不碰。
///
/// 最后一段走完整条 `rewrite_body`：**CC 形态的请求同样会丢**——`hoist_system_role` 的
/// 「CC 形态跳过」保的是官方带内容的那条 `role:"system"`（deferred tools），不是空壳；
/// 实跑里正是一条 agent-sdk 的 CC 请求带着空壳换回一次 400（`req_grlwDAtQQpqvf54d`）。
/// 指令式 system（`content: []` + 消息级 `output_config`）不是空壳：2.1.285 官方就这么发，
/// 上游放哪儿都收；当空壳丢掉等于把客户端中途调的 effort 删了。
#[test]
fn system_directive_is_not_dropped_as_an_empty_shell() {
    let directive = serde_json::json!({ "role": "system", "output_config": { "effort": "high" }, "content": [] });
    let user = serde_json::json!({ "role": "user", "content": "hi" });
    let mut v = serde_json::json!({ "messages": [user.clone(), directive.clone()] });
    assert!(!crate::proxy::drop_empty_system_messages(&mut v));
    assert_eq!(v["messages"], serde_json::json!([user.clone(), directive]));

    // 口径逐字照上游：只有空数组算指令式，空串 / 缺 content 带 output_config 照旧当空壳。
    for shell in [
        serde_json::json!({ "role": "system", "output_config": { "effort": "high" }, "content": "" }),
        serde_json::json!({ "role": "system", "output_config": { "effort": "high" } }),
    ] {
        let mut v = serde_json::json!({ "messages": [user.clone(), shell.clone()] });
        assert!(crate::proxy::drop_empty_system_messages(&mut v), "这条该算空壳: {shell}");
    }
}

/// 末尾是指令式 system 时，消息断点补在它前一条上，指令留在原位、不挂断点。
/// 两条补断点的路（模拟路径 [`align_message_shape`]、真 CC [`ensure_cc_message_breakpoint`]）都验。
#[test]
fn trailing_system_directive_does_not_block_the_message_breakpoint() {
    let directive = serde_json::json!({ "role": "system", "output_config": { "effort": "high" }, "content": [] });
    let body = || {
        serde_json::json!({
            "system": [{ "type": "text", "text": "sys", "cache_control": { "type": "ephemeral", "ttl": "1h" } }],
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "ok" }] },
                { "role": "user", "content": [{ "type": "text", "text": "go on" }] },
                directive.clone(),
            ]
        })
    };

    let mut v = body();
    assert!(crate::proxy::body::ensure_cc_message_breakpoint(&mut v));
    assert_eq!(v["messages"][2]["content"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(v["messages"][3], directive, "指令原样留在末尾");

    let mut v = body();
    assert!(crate::proxy::body::align_message_shape(
        &mut v,
        crate::proxy::CacheShape { global: false, ttl_1h: false }
    ));
    assert!(v["messages"][2]["content"][0].get("cache_control").is_some());
    assert_eq!(v["messages"][3], directive, "指令原样留在末尾");
}

/// 对话中途 system 的位置：照上游原话判，连续几条算一段（官方抓包里的几种形态全放行）。
#[test]
fn misplaced_mid_conversation_system_is_rejected_with_the_upstream_wording() {
    let check = |msgs: serde_json::Value| {
        crate::proxy::misplaced_system_role(Some(&serde_json::json!({ "messages": msgs })))
    };
    let u = serde_json::json!({ "role": "user", "content": "hi" });
    let a = serde_json::json!({ "role": "assistant", "content": "ok" });
    let s = serde_json::json!({ "role": "system", "content": "be brief" });
    let clear = serde_json::json!({ "role": "system", "content": "x", "clear_at": "t" });
    let directive = serde_json::json!({ "role": "system", "output_config": { "effort": "high" }, "content": [] });
    let shell = serde_json::json!({ "role": "system", "content": "" });

    // 官方实际发的几种（2.1.260–2.1.285 抓包）：后面跟 assistant、在末尾、连续两条再跟
    // assistant / 在末尾、指令式跟 assistant。
    for ok in [
        serde_json::json!([u, s, a]),
        serde_json::json!([u, s]),
        serde_json::json!([u, s, clear, a]),
        serde_json::json!([u, a, u, s, clear]),
        serde_json::json!([u, directive, a]),
    ] {
        assert_eq!(check(ok.clone()), None, "官方形态不该拦: {ok}");
    }

    // 跟着 user：拒，下标与原话照上游。
    let msg = check(serde_json::json!([u, s, u])).expect("system 后跟 user 该拒");
    assert_eq!(
        msg,
        "messages.1: role 'system' must precede an 'assistant' message or end the array; \
             the directive-only form (content: [] with output_config) is accepted at any position"
    );
    // 连续一段后跟 user：点名段里第一条。
    assert!(check(serde_json::json!([u, a, u, s, clear, u])).unwrap().starts_with("messages.3:"));
    // 前面的空壳出站会被丢掉，不算数；点名的是第一条会送到上游的。
    assert!(check(serde_json::json!([u, shell, s, u])).unwrap().starts_with("messages.2:"));

    // 拿不准的一律放过，交给上游：指令式本身、段里夹着指令式、整段都是空壳、
    // 后面跟的不是 user（别的 role 交给上游去说）、开头那段（另一条路管）。
    for unsure in [
        serde_json::json!([u, directive, u]),
        serde_json::json!([u, s, directive, u]),
        serde_json::json!([u, shell, u]),
        serde_json::json!([u, s, { "role": "tool", "content": "x" }]),
        serde_json::json!([s, u]),
    ] {
        assert_eq!(check(unsure.clone()), None, "拿不准的不该本地拒: {unsure}");
    }
    assert_eq!(crate::proxy::misplaced_system_role(None), None);
}

#[test]
fn empty_system_messages_are_dropped_before_going_out() {
    let sys = |content: Option<serde_json::Value>| match content {
        Some(c) => serde_json::json!({ "role": "system", "content": c }),
        None => serde_json::json!({ "role": "system" }),
    };
    let user = serde_json::json!({ "role": "user", "content": "hi" });

    // 五种空形态，逐个单独验：丢掉之后只剩那条用户消息。
    for content in [
        Some(serde_json::json!([])),
        Some(serde_json::json!("")),
        Some(serde_json::Value::Null),
        None,
        Some(serde_json::json!([{ "type": "text", "text": "" }, { "type": "text", "text": "" }])),
    ] {
        let mut v = serde_json::json!({ "messages": [user.clone(), sys(content.clone())] });
        assert!(crate::proxy::drop_empty_system_messages(&mut v), "这条该算空壳: {content:?}");
        assert_eq!(v["messages"], serde_json::json!([user.clone()]));
    }

    // 带内容的一律不动：官方 deferred tools 那条、空格、数组里混着一个非空块。
    for content in [
        serde_json::json!("deferred"),
        serde_json::json!(" "),
        serde_json::json!([{ "type": "text", "text": "x" }]),
        serde_json::json!([{ "type": "text", "text": "" }, { "type": "text", "text": "x" }]),
        serde_json::json!({ "type": "text", "text": "" }),
    ] {
        let mut v = serde_json::json!({ "messages": [sys(Some(content.clone()))] });
        assert!(!crate::proxy::drop_empty_system_messages(&mut v), "这条不该算空壳: {content}");
        assert_eq!(v["messages"], serde_json::json!([sys(Some(content))]));
    }

    // 只碰 role:"system"：空 content 的 user / assistant 留着（删了会改轮次交替）。
    let mut v = serde_json::json!({
        "messages": [
            { "role": "user", "content": [] },
            { "role": "assistant", "content": [] },
        ]
    });
    assert!(!crate::proxy::drop_empty_system_messages(&mut v));
    assert_eq!(v["messages"].as_array().unwrap().len(), 2);

    // 多条空壳一起丢，其余消息的相对顺序不变；没有 messages 的体不动。
    let mut v = serde_json::json!({
        "messages": [sys(Some(serde_json::json!([]))), user.clone(), sys(None), user.clone()]
    });
    assert!(crate::proxy::drop_empty_system_messages(&mut v));
    assert_eq!(v["messages"], serde_json::json!([user.clone(), user.clone()]));
    let mut v = serde_json::json!({ "model": "claude-opus-5" });
    assert!(!crate::proxy::drop_empty_system_messages(&mut v));

    // 整条 rewrite_body：CC 形态（system 里有身份句）的请求，空壳照丢。
    let body = Bytes::from(
        serde_json::json!({
                "model": "claude-opus-5",
                "messages": [user.clone(), sys(Some(serde_json::json!([]))), user.clone()],
                "system": [{
                    "type": "text",
                    "text": "You are Claude Code, Anthropic's official CLI for Claude."}]})
        .to_string(),
    );
    let out: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&body, &test_cred(), "fp", all_on(), None, None))
            .unwrap();
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "空壳该被丢掉: {out}");
    assert!(msgs.iter().all(|m| m["role"] == "user"), "留下的必须是那两条用户消息: {out}");
}

#[test]
fn strip_extra_fields_is_wired_and_switchable() {
    let body = br#"{"model":"claude-opus-5","tool_choice":{"type":"auto"},"thinking":{"type":"adaptive","display":"summarized"},"messages":[]}"#;
    let only_strip = store::ForwardFlags {
        strip_extra_fields: true,
        ..store::ForwardFlags {
            spoof_identity: false,
            spoof_device_id: false,
            normalize_device_fp: false,
            billing_cch: false,
            cch_real_recompute: false,
            cch_sim_compute: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
            thinking_signature_retry: false,
            thinking_modified_retry: false,
            redacted_thinking_retry: false,
            simulate_cc: false,
            simulate_full_system: false,
            fill_absent_tools: false,
            sim_trim_tools: false,
            sim_billing_only: false,
            sim_message_threads: false,
            fill_metadata: false,
            rate_limit_retry: false,
            cache_scope_global: false,
            cache_ttl_1h: false,
            eager_tool_streaming: false,
            nonstream_as_sse: false,
            strip_extra_fields: false,
            tool_name_mimic: false,
            inject_thinking: false,
            flatten_tool_schemas: true,
            strip_empty_text: true,
            hoist_system_role: false,
            reject_openai_shape: false,
            reject_session_conflict: false,
            reject_probes: false,
            reject_probes_strict: false,
            reject_refusals: false,
            reject_empty_replies: false,
            reject_learned_shapes: false,
            api_telemetry: false,
            keepalive_telemetry: false,
            fable_refusal_fallback: false,
            opus_refusal_fallback: false,
        }
    };
    let out = rewrite_body(&Bytes::from(&body[..]), &test_cred(), "fp", only_strip, None, None);
    let s = String::from_utf8(out.to_vec()).unwrap();
    assert!(!s.contains("tool_choice"), "{s}");
    assert!(!s.contains("display"), "{s}");

    let off = store::ForwardFlags { strip_extra_fields: false, ..only_strip };
    let out = rewrite_body(&Bytes::from(&body[..]), &test_cred(), "fp", off, None, None);
    assert_eq!(out.as_ref(), &body[..], "关掉后必须逐字节透传");
}

// 真实 CC 抓包形态：字段顺序 device_id → account_uuid → session_id。
const CC: &str = r#"{"device_id":"dddd","account_uuid":"aaaa","session_id":"ssss"}"#;

#[test]
fn replaces_value_and_preserves_order() {
    let s = replace_json_str_field(CC, "account_uuid", "NEW").unwrap();
    let s = replace_json_str_field(&s, "device_id", "DEV").unwrap();
    assert_eq!(s, r#"{"device_id":"DEV","account_uuid":"NEW","session_id":"ssss"}"#);
}

#[test]
fn fills_empty_account_uuid() {
    let empty = r#"{"device_id":"dddd","account_uuid":"","session_id":"ssss"}"#;
    let s = replace_json_str_field(empty, "account_uuid", "FILLED").unwrap();
    assert_eq!(s, r#"{"device_id":"dddd","account_uuid":"FILLED","session_id":"ssss"}"#);
}

#[test]
fn missing_field_returns_none_no_insert() {
    assert!(replace_json_str_field(CC, "not_here", "X").is_none());
}

// ---------- 空 text 块剥除 ----------

#[test]
fn strips_empty_text_blocks_mixed() {
    let mut v = serde_json::json!({
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": ""},
                {"type": "text", "text": "hello"},
                {"type": "text", "text": ""}
            ]}
        ]
    });
    assert!(crate::proxy::strip_empty_text_blocks(&mut v));
    let content = v["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["text"], "hello");
}

#[test]
fn keeps_all_empty_text_blocks_when_nothing_else() {
    let mut v = serde_json::json!({
        "messages": [
            {"role": "assistant", "content": [
                {"type": "text", "text": ""}
            ]}
        ]
    });
    assert!(!crate::proxy::strip_empty_text_blocks(&mut v));
    assert_eq!(v["messages"][0]["content"].as_array().unwrap().len(), 1);
}

#[test]
fn noop_when_no_empty_text() {
    let mut v = serde_json::json!({
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        ]
    });
    assert!(!crate::proxy::strip_empty_text_blocks(&mut v));
}

// ---------- input_schema allOf/oneOf/anyOf 展平 ----------

#[test]
fn flattens_allof_in_tool_schema() {
    let mut v = serde_json::json!({
        "tools": [{
            "name": "my_tool",
            "input_schema": {
                "allOf": [
                    {"type": "object", "properties": {"a": {"type": "string"}}},
                    {"properties": {"b": {"type": "number"}}, "required": ["a", "b"]}
                ]
            }
        }]
    });
    assert!(crate::proxy::flatten_tool_schemas(&mut v));
    let schema = &v["tools"][0]["input_schema"];
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["a"].is_object());
    assert!(schema["properties"]["b"].is_object());
    let req = schema["required"].as_array().unwrap();
    assert!(req.contains(&serde_json::json!("a")));
    assert!(req.contains(&serde_json::json!("b")));
    assert!(schema.get("allOf").is_none());
}

#[test]
fn flattens_oneof_single_element() {
    let mut v = serde_json::json!({
        "tools": [{
            "name": "t",
            "input_schema": {
                "oneOf": [{"type": "object", "properties": {"x": {"type": "string"}}}]
            }
        }]
    });
    assert!(crate::proxy::flatten_tool_schemas(&mut v));
    let schema = &v["tools"][0]["input_schema"];
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["x"].is_object());
    assert!(schema.get("oneOf").is_none());
}

#[test]
fn flattens_allof_with_existing_top_level_props() {
    let mut v = serde_json::json!({
        "tools": [{
            "name": "t",
            "input_schema": {
                "type": "object",
                "description": "desc",
                "allOf": [
                    {"properties": {"a": {"type": "string"}}, "required": ["a"]}
                ]
            }
        }]
    });
    assert!(crate::proxy::flatten_tool_schemas(&mut v));
    let schema = &v["tools"][0]["input_schema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["description"], "desc");
    assert!(schema["properties"]["a"].is_object());
    assert!(schema.get("allOf").is_none());
}

#[test]
fn noop_when_no_compound_schema() {
    let mut v = serde_json::json!({
        "tools": [{
            "name": "t",
            "input_schema": {"type": "object", "properties": {"a": {"type": "string"}}}
        }]
    });
    assert!(!crate::proxy::flatten_tool_schemas(&mut v));
}

/// `refusal_fallbacks_for`：只给主线程、计费、且该族开关开着的 fable / opus-5 补；fable 用
/// 官方那份（默认开），opus-5 用 luban 自定的 4.8 → 4.6 链（默认关、要显式开）；两档互不
/// 影响；sonnet/haiku 不补；上游拒过的模型不补。
#[test]
fn refusal_fallbacks_are_chosen_per_family_and_gated() {
    use crate::proxy::CcRequestKind::*;
    let mem = crate::proxy::DeprecatedFieldMemory::default();
    let defaults = all_on();
    assert!(!defaults.fable_refusal_fallback && !defaults.opus_refusal_fallback, "两档都默认关");
    let on = store::ForwardFlags {
        fable_refusal_fallback: true,
        opus_refusal_fallback: true,
        ..defaults
    };
    let off = store::ForwardFlags {
        fable_refusal_fallback: false,
        opus_refusal_fallback: false,
        ..defaults
    };
    let pick = |m: &str, flags: store::ForwardFlags, billable: bool, kind| {
        crate::proxy::refusal_fallbacks_for(Some(m), flags, billable, kind, &mem)
    };
    assert_eq!(
        pick("claude-fable-5-1", on, true, Main),
        Some(r#"[{"model":"claude-opus-5"}]"#),
        "fable 补官方 2.1.260 那份（cap/2.1.260/00018）"
    );
    assert_eq!(pick("claude-fable-5", on, true, Main), Some(r#"[{"model":"claude-opus-5"}]"#));
    assert_eq!(pick("claude-opus-5", on, true, Main), Some(config::OPUS_REFUSAL_FALLBACKS));
    assert_eq!(pick("claude-opus-5[1m]", on, true, Main), Some(config::OPUS_REFUSAL_FALLBACKS));
    assert_eq!(pick("claude-sonnet-5", on, true, Main), None, "sonnet 官方客户端不发该字段，不补");
    assert_eq!(pick("claude-opus-4-8", on, true, Main), None, "4.x 不补");
    assert_eq!(pick("claude-haiku-4-5-20251001", on, true, Main), None);
    assert_eq!(pick("claude-fable-5-1", off, true, Main), None, "开关关着不补");
    assert_eq!(pick("claude-opus-5", off, true, Main), None, "开关关着不补");
    // 默认值：两档都不补——fable 那份替用户决定换模型作答，opus 那份是官方不产生的形态，
    // 都得显式打开。
    assert_eq!(pick("claude-fable-5-1", defaults, true, Main), None, "fable 默认关");
    assert_eq!(pick("claude-opus-5", defaults, true, Main), None, "opus 默认关");
    assert_eq!(pick("claude-opus-5[1m]", defaults, true, Main), None, "opus 默认关");
    // 两档各管各的：只开 opus 时 fable 不补，反之亦然。
    let opus_only = store::ForwardFlags { fable_refusal_fallback: false, ..on };
    assert_eq!(pick("claude-fable-5-1", opus_only, true, Main), None, "fable 关着不受 opus 影响");
    assert_eq!(pick("claude-opus-5", opus_only, true, Main), Some(config::OPUS_REFUSAL_FALLBACKS));
    assert_eq!(pick("claude-fable-5-1", on, false, Main), None, "count_tokens 不补");
    for kind in [Subagent, Suggestion, Helper, Title, Classifier, QuotaProbe] {
        assert_eq!(pick("claude-fable-5-1", on, true, kind), None, "辅助请求不补: {kind:?}");
        assert_eq!(pick("claude-opus-5", on, true, kind), None, "辅助请求不补: {kind:?}");
    }
    assert_eq!(crate::proxy::refusal_fallbacks_for(None, on, true, Main, &mem), None);

    // 上游以 400 拒了 opus-5 的 fallback 目标：学下来，之后不补；fable 不受影响。
    let err = err_json("fallbacks.1.model: 'claude-opus-4-6' is not an allowed fallback model");
    assert!(crate::proxy::is_fallback_rejection(&err));
    assert!(!crate::proxy::is_fallback_rejection(&err_json("max_tokens: must be positive")));
    let row =
        crate::proxy::remember_fallback_rejection(&mem, "claude-opus-5", &err).expect("首次学到");
    assert_eq!(
        (row.kind.as_str(), row.model.as_str(), row.field.as_str(), row.value.as_str()),
        ("deprecated", "claude-opus-5", "fallbacks", "")
    );
    assert!(crate::proxy::remember_fallback_rejection(&mem, "claude-opus-5", &err).is_none());
    assert_eq!(pick("claude-opus-5", on, true, Main), None, "学过就不补");
    assert_eq!(pick("claude-fable-5-1", on, true, Main), Some(r#"[{"model":"claude-opus-5"}]"#));
    // 这条 deprecated 规则能经 seed 回填（`fallbacks` 不在 DEPRECATABLE_FIELDS 里，seed 单独放行）。
    let shape2 = crate::proxy::ShapeMemory::default();
    let dep2 = crate::proxy::DeprecatedFieldMemory::default();
    let empty2 = crate::proxy::EmptyReplyMemory::default();
    let seeded = crate::proxy::seed_learned_memories(&shape2, &dep2, &empty2, vec![row.clone()]);
    assert_eq!(seeded.deprecated, 1);
    assert_eq!(
        crate::proxy::refusal_fallbacks_for(Some("claude-opus-5"), on, true, Main, &dep2),
        None
    );
    // 单条删除也认。
    assert!(crate::proxy::forget_learned_memory(&shape2, &dep2, &empty2, &row));
    assert_eq!(
        crate::proxy::refusal_fallbacks_for(Some("claude-opus-5"), on, true, Main, &dep2),
        Some(config::OPUS_REFUSAL_FALLBACKS)
    );
    // `fallbacks` 不在采样参数名单里：客户端自带的不会被 sampling_policy 当采样参数剥掉。
    assert!(!crate::proxy::DEPRECATABLE_FIELDS.contains(&"fallbacks"));
}

/// [`client_supplied_fallbacks`]：客户端带了数组（哪怕是空数组）算它自己的；字符串
/// `"default"`、缺失都不算——那两种出站的是 luban 的字面量。
#[test]
fn client_supplied_fallbacks_means_any_non_string_field() {
    let body = |f: serde_json::Value| {
        let mut v = serde_json::json!({"model": "claude-fable-5-1", "messages": []});
        v["fallbacks"] = f;
        v
    };
    assert!(crate::proxy::client_supplied_fallbacks(Some(&body(
        serde_json::json!([{"model": "claude-opus-4-8"}])
    ))));
    assert!(crate::proxy::client_supplied_fallbacks(Some(&body(serde_json::json!([])))));
    assert!(!crate::proxy::client_supplied_fallbacks(Some(&body(serde_json::json!("default")))));
    assert!(!crate::proxy::client_supplied_fallbacks(Some(
        &serde_json::json!({"model": "claude-fable-5-1"})
    )));
    assert!(!crate::proxy::client_supplied_fallbacks(None));
}

/// [`outbound_carries_fallbacks`]：客户端自带 `fallbacks`（数组非空或字符串 "default"）的、
/// 或 luban 按族开关要补的请求算「带」；空数组不算；开关关着且客户端没带的不算；helper
/// 之类非主线程请求 luban 不补，也不算。带的请求 2.3a4 不本地 403。
#[test]
fn outbound_carries_fallbacks_sees_client_arrays_and_luban_padding() {
    let mem = crate::proxy::DeprecatedFieldMemory::default();
    let defaults = store::ForwardFlags::default();
    let off = store::ForwardFlags {
        fable_refusal_fallback: false,
        opus_refusal_fallback: false,
        ..defaults
    };
    let main = serde_json::json!({
        "model": "claude-fable-5-1", "max_tokens": 32000, "stream": true,
        "system": [{"type": "text", "text": "You are Claude Code"}],
        "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "hi"}]
    });
    let carries = |body: &serde_json::Value, model: &str, flags| {
        crate::proxy::outbound_carries_fallbacks(Some(body), Some(model), flags, &[], &mem, false)
    };
    // fable 默认关 → 不带；显式开 → luban 会补 → 带。
    assert!(!carries(&main, "claude-fable-5-1", defaults));
    let fable_on = store::ForwardFlags { fable_refusal_fallback: true, ..defaults };
    assert!(carries(&main, "claude-fable-5-1", fable_on));
    // 全关、客户端也没带 → 不带。
    assert!(!carries(&main, "claude-fable-5-1", off));
    // opus-5 默认关 → 不带；显式开 → 带。
    assert!(!carries(&main, "claude-opus-5", defaults));
    assert!(carries(
        &main,
        "claude-opus-5",
        store::ForwardFlags { opus_refusal_fallback: true, ..defaults }
    ));
    // sonnet：luban 不补 → 不带。
    assert!(!carries(&main, "claude-sonnet-5", defaults));
    // 客户端自带合法数组：开关关着也算带；空数组不算。
    let mut with_arr = main.clone();
    with_arr["fallbacks"] = serde_json::json!([{"model": "claude-opus-4-8"}]);
    assert!(carries(&with_arr, "claude-sonnet-5", off));
    // 字符串 "default"：有计划时会被换成计划 → 带；没计划时不算带（见函数文档），命中已学到的
    // 拒答就本地回放。
    let mut with_default = main.clone();
    with_default["fallbacks"] = serde_json::json!("default");
    assert!(carries(&with_default, "claude-fable-5-1", fable_on));
    assert!(!carries(&with_default, "claude-fable-5-1", off));
    assert!(!carries(&with_default, "claude-sonnet-5", off));
    assert!(!carries(&with_default, "claude-sonnet-5", defaults));
    // 客户端带的非字符串形态 luban 不动，出站就是它那份：空数组、null、对象、元素不是
    // 带 model 的对象——上游一定 400，开关开着也不算带了 fallback。
    for bogus in [
        serde_json::json!([]),
        serde_json::json!(null),
        serde_json::json!({}),
        serde_json::json!([null]),
        serde_json::json!([{}]),
        serde_json::json!(["bogus"]),
        serde_json::json!([{"model": ""}]),
        serde_json::json!([{"model": "claude-opus-4-8"}, {}]),
    ] {
        let mut with_bogus = main.clone();
        with_bogus["fallbacks"] = bogus.clone();
        assert!(!carries(&with_bogus, "claude-fable-5-1", fable_on), "{bogus} 不算");
        assert!(!carries(&with_bogus, "claude-fable-5-1", off), "{bogus} 不算");
    }
    // 客户端字符串写错、但开关开着：luban 会把字符串换成自己的计划 → 带。
    let mut bad_string = main.clone();
    bad_string["fallbacks"] = serde_json::json!("auto");
    assert!(carries(&bad_string, "claude-fable-5-1", fable_on));
    // 别的字符串同样不算：官方从没发过、上游一定 400。
    for bogus in ["", "auto", "Default"] {
        let mut with_bogus = main.clone();
        with_bogus["fallbacks"] = serde_json::json!(bogus);
        assert!(!carries(&with_bogus, "claude-sonnet-5", off), "{bogus:?} 不算");
    }
    // 非主线程（无 tools 的 helper）luban 不补 → 不带。
    let mut helper = main.clone();
    helper.as_object_mut().unwrap().remove("tools");
    assert!(!carries(&helper, "claude-fable-5-1", defaults));
    // 无体 → 不带。
    assert!(!crate::proxy::outbound_carries_fallbacks(
        None,
        Some("claude-fable-5-1"),
        defaults,
        &[],
        &mem,
        false
    ));
    // 上游 400 拒过这个模型的 fallback 目标：luban 不再补 → 不带。
    mem.write().insert(
        ("claude-fable-5-1".to_string(), crate::proxy::FALLBACKS_FIELD.to_string()),
        "rejected".into(),
    );
    assert!(!carries(&main, "claude-fable-5-1", defaults));
}

/// billing-only（最后一个入参为真）：luban 不注入族 `fallbacks`，故「客户端没带」就是真没带、
/// 不能当作「会带」而跳过本地拒答回放；客户端自带的数组照样透传算带；客户端字符串 "default"
/// 只有能被 [`normalize_fallbacks`] 归一成数组的 fable 才算带。
#[test]
fn outbound_carries_fallbacks_billing_only_counts_only_what_actually_ships() {
    let mem = crate::proxy::DeprecatedFieldMemory::default();
    // 族开关全开：非 billing-only 时下面这几条都会被判成「会带」。
    let fable_on = store::ForwardFlags {
        fable_refusal_fallback: true,
        opus_refusal_fallback: true,
        ..store::ForwardFlags::default()
    };
    let carries = |body: &serde_json::Value, model: &str, billing_only| {
        crate::proxy::outbound_carries_fallbacks(
            Some(body),
            Some(model),
            fable_on,
            &[],
            &mem,
            billing_only,
        )
    };
    // 主线程形态（带 tools + system + stream），只是不带 fallbacks。
    let no_fb = serde_json::json!({
        "model": "claude-fable-5-1", "max_tokens": 32000, "stream": true,
        "system": [{"type": "text", "text": "You are Claude Code"}],
        "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "hi"}]
    });
    // 客户端没带：非 billing-only 时 luban 会补 → 带；billing-only 不补 → 不带（修复点）。
    assert!(carries(&no_fb, "claude-fable-5-1", false), "非 billing-only：luban 补 fallback");
    assert!(
        !carries(&no_fb, "claude-fable-5-1", true),
        "billing-only：没带就是没带，不能跳过本地拒答回放"
    );
    // 客户端自带数组：两种模式都透传 → 都算带。
    let arr = serde_json::json!({
        "model": "claude-sonnet-5", "max_tokens": 100,
        "fallbacks": [{"model": "claude-opus-5"}],
        "messages": [{"role": "user", "content": "hi"}]
    });
    assert!(carries(&arr, "claude-sonnet-5", true), "billing-only 也透传客户端数组");
    // 客户端字符串 "default"：fable 会被归一成数组 → 算带；非 fable 原样出站 → 不算。
    let s = |model: &str| {
        serde_json::json!({
            "model": model, "max_tokens": 100, "fallbacks": "default",
            "messages": [{"role": "user", "content": "hi"}]
        })
    };
    assert!(carries(&s("claude-fable-5-1"), "claude-fable-5-1", true), "fable 字符串归一成数组");
    assert!(
        !carries(&s("claude-sonnet-5"), "claude-sonnet-5", true),
        "非 fable 字符串原样、不算带"
    );
}

/// 真 CC 来访 `messages` 里一个断点都没有时补第三个断点（[`ensure_cc_message_breakpoint`]），
/// 且只在缓存前缀与上一轮相同时补（[`cache_prefix_stable`]）。
/// 形态照现网 `req_zu6ELzACscXlpGSg`：claude-vscode 2.1.273 的 agent-sdk 构建，5 块
/// `system`（billing、身份句、基座、无断点块、尾块），身份句 / 基座 / 尾块各带 `5m` 断点，
/// 末条是 `tool_result`、`messages` 里零断点；它的尾块每轮长 51 字节，那种轮次不能标。
#[test]
fn cc_request_without_message_breakpoint_gets_one_on_the_last_block() {
    let system = |ttl: &str| {
        format!(
            concat!(
                r#"[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.273.abc; cc_entrypoint=claude-vscode;"}},"#,
                r#"{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.","cache_control":{{"type":"ephemeral","ttl":"{ttl}"}}}},"#,
                r#"{{"type":"text","text":"{base}","cache_control":{{"type":"ephemeral","ttl":"{ttl}"}}}},"#,
                r#"{{"type":"text","text":"env"}},"#,
                r#"{{"type":"text","text":"tail","cache_control":{{"type":"ephemeral","ttl":"{ttl}"}}}}]"#
            ),
            ttl = ttl,
            base = "x".repeat(1200),
        )
    };
    let tool_loop = concat!(
        r#"[{"role":"user","content":"ls"},"#,
        r#"{"role":"system","content":"<total_tokens>15000000 tokens left</total_tokens>"},"#,
        r#"{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"a"}}]},"#,
        r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"a.txt"}]}]"#
    );
    let body = |system: &str, messages: &str| {
        Bytes::from(format!(
            r#"{{"model":"claude-opus-5","system":{system},"messages":{messages},"max_tokens":64000,"stream":true,"metadata":{{"user_id":"{{\"device_id\":\"dddd\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}}"}}}}"#
        ))
    };
    // 每个用例一个新会话 id；同一份体发两轮，第二轮的前缀与第一轮相同，闸才放行。
    let once = |b: &Bytes, flags: store::ForwardFlags, sid: Option<&str>| -> serde_json::Value {
        serde_json::from_slice(&crate::proxy::test_support::rewrite_body_with_session(
            b,
            &test_cred(),
            "fp",
            flags,
            None,
            None,
            sid,
        ))
        .unwrap()
    };
    let run = |b: &Bytes, flags: store::ForwardFlags| -> serde_json::Value {
        let sid = crate::proxy::uuid_v4();
        once(b, flags, Some(&sid));
        once(b, flags, Some(&sid))
    };

    // 会话第一轮：没有上一轮可比，不标。
    let sid = crate::proxy::uuid_v4();
    let v = once(&body(&system("5m"), tool_loop), all_on(), Some(&sid));
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 0, "第一轮不标: {v}");
    // 第二轮同一份前缀 → 标。
    let v = once(&body(&system("5m"), tool_loop), all_on(), Some(&sid));
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 1, "第二轮该标: {v}");
    // 第三轮 system 尾块变了（现网那种每轮长 51 字节）→ 不标：前缀变了，标了也是未命中。
    let grown = system("5m").replace(
        r#""text":"tail""#,
        r#""text":"tail\n\n<total_tokens>1 tokens left</total_tokens>""#,
    );
    let v = once(&body(&grown, tool_loop), all_on(), Some(&sid));
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 0, "尾块变了不标: {v}");
    // 第四轮尾块又稳住 → 再标。
    let v = once(&body(&grown, tool_loop), all_on(), Some(&sid));
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 1, "稳住后再标: {v}");
    // tools 变了是另一条谱系，对它是第一轮 → 不标。
    let with_tools = body(&grown, tool_loop);
    let with_tools = Bytes::from(String::from_utf8(with_tools.to_vec()).unwrap().replace(
        r#""max_tokens":64000"#,
        r#""tools":[{"name":"Read","input_schema":{"type":"object"}}],"max_tokens":64000"#,
    ));
    let v = once(&with_tools, all_on(), Some(&sid));
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 0, "tools 变了不标: {v}");
    // 只有 billing header 变（cch / cc_prev_req 逐轮不同）不算前缀变。
    let cch = body(&grown, tool_loop);
    let cch = Bytes::from(
        String::from_utf8(cch.to_vec())
            .unwrap()
            .replace("cc_entrypoint=claude-vscode;", "cc_entrypoint=claude-vscode; cch=abcde;"),
    );
    // tools 换回去：那条谱系的记录还在、system 没变 → 稳定照标（谱系按 tools 分，
    // 中间夹的另一套 tools 不算这条谱系的「上一轮」）。
    let v = once(&body(&grown, tool_loop), all_on(), Some(&sid));
    assert_eq!(
        crate::proxy::count_cache_control_in(&v["messages"]),
        1,
        "tools 换回去，旧谱系仍稳定: {v}"
    );
    let v = once(&cch, all_on(), Some(&sid));
    assert_eq!(
        crate::proxy::count_cache_control_in(&v["messages"]),
        1,
        "只有 billing header 变仍算稳定: {v}"
    );
    // 没有会话 id 可作键 → 不标。
    let v = once(&body(&system("5m"), tool_loop), all_on(), None);
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 0, "没有会话键不标: {v}");

    // 正例：末块 tool_result 拿到断点，ttl 抄 system 的 5m 而不是开关的 1h，不带 scope；
    // 字符串形态的旧 reminder 不被转成块数组；总数正好 4。
    let v = run(&body(&system("5m"), tool_loop), all_on());
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4, "messages 不该增删: {v}");
    assert!(msgs[1]["content"].is_string(), "旧 reminder 的字符串形态不该被转: {v}");
    let last = msgs[3]["content"].as_array().unwrap().last().unwrap();
    assert_eq!(last["type"], "tool_result");
    assert_eq!(
        last["cache_control"],
        serde_json::json!({"type": "ephemeral", "ttl": "5m"}),
        "断点该抄 system 尾块的 ttl: {v}"
    );
    assert_eq!(last["content"], "a.txt", "正文不动");
    assert_eq!(crate::proxy::count_cache_control(&v), 4, "总数正好封顶: {v}");
    assert_eq!(v["system"].as_array().unwrap().len(), 5, "system 块数不变: {v}");

    // 客户端 system 断点不带 ttl → 消息断点也不带。
    let bare = system("5m").replace(r#","ttl":"5m""#, "");
    let v = run(&body(&bare, tool_loop), all_on());
    let last = v["messages"][3]["content"].as_array().unwrap().last().unwrap();
    assert_eq!(last["cache_control"], serde_json::json!({"type": "ephemeral"}), "{v}");

    // 反例一：客户端 messages 里自己标过（哪怕标在中间那条）→ 一个字节不动。
    let marked = tool_loop.replace(
            r#"{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"a"}}"#,
            r#"{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"a"},"cache_control":{"type":"ephemeral"}}"#,
        );
    let v = run(&body(&system("5m"), &marked), all_on());
    assert!(
        v["messages"][3]["content"][0].get("cache_control").is_none(),
        "客户端自己标过就不再标: {v}"
    );
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 1);

    // 反例二：末条是字符串 content → 不转、不标（官方 CLI 自己就混着发）。
    let str_tail = concat!(
        r#"[{"role":"user","content":"ls"},"#,
        r#"{"role":"assistant","content":[{"type":"text","text":"ok"}]},"#,
        r#"{"role":"user","content":"and then?"}]"#
    );
    let v = run(&body(&system("5m"), str_tail), all_on());
    assert!(v["messages"][2]["content"].is_string(), "末条字符串不该被转: {v}");
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 0);

    // 反例三：预算满（system 里 4 个断点）→ 不标。
    let full = system("5m").replace(
        r#"{"type":"text","text":"env"}"#,
        r#"{"type":"text","text":"env","cache_control":{"type":"ephemeral","ttl":"5m"}}"#,
    );
    let v = run(&body(&full, tool_loop), all_on());
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 0, "预算满不标: {v}");
    assert_eq!(crate::proxy::count_cache_control(&v), 4);

    // 反例四：末块是 thinking → 不标。
    let thinking_tail = concat!(
        r#"[{"role":"user","content":"ls"},"#,
        r#"{"role":"assistant","content":[{"type":"thinking","thinking":"想","signature":"AAAA"}]}]"#
    );
    let v = run(&body(&system("5m"), thinking_tail), all_on());
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 0, "{v}");

    // 反例五：system_shape 开关关着 → 不标。
    let mut off = all_on();
    off.system_shape = false;
    let v = run(&body(&system("5m"), tool_loop), off);
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 0, "开关关着不标: {v}");

    // 反例六：非 CC 形态（没有身份句、没有 billing header）走非模拟路径 → 不标，
    // 这一步只给真 CC 补。
    let plain_sys =
        r#"[{"type":"text","text":"You are a helpful bot.","cache_control":{"type":"ephemeral"}}]"#;
    let v = run(&body(plain_sys, tool_loop), all_on());
    assert_eq!(crate::proxy::count_cache_control_in(&v["messages"]), 0, "非 CC 形态不标: {v}");
}

/// 体侧 `ensure_fallbacks`：没写的补在 `context_management` 之后、`output_config` 之前
/// （官方键序），字符串 `"default"` 换成数组，客户端自己的数组不动。
/// 整形不能把缓存断点顶过 4 个。合并块那一个拆成基座 + 其余是净 +1，身份句没标断点时
/// 抵不回来；客户端又在 `messages` 里标满三个，出去就是上游那条
/// `A maximum of 4 blocks with cache_control may be provided. Found 5.`——整条被拒，
/// 而少拆一次只是少一次基座级缓存命中。
#[test]
fn align_system_shape_respects_the_breakpoint_budget() {
    let merged = format!("base text\n\n{}\nrest of it", crate::config::CC_SYSTEM_BASE_ANCHORS[0]);
    // `msg_breakpoints` 条客户端自己标在 messages 里的断点，加 system 合并块那一个。
    let mk = |msg_breakpoints: usize| {
        let blocks: Vec<serde_json::Value> = (0..msg_breakpoints)
            .map(|i| {
                serde_json::json!({
                    "type": "text",
                    "text": format!("m{i}"),
                    "cache_control": {"type": "ephemeral"}
                })
            })
            .collect();
        serde_json::json!({
            "model": "claude-opus-5",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli;"},
                // 身份句**不带**断点：这一档的净变化才是 +1，官方 API-key 形态带着它、净 0。
                {"type": "text", "text": crate::config::CC_SYSTEM_IDENTITY},
                {"type": "text", "text": merged, "cache_control": {"type": "ephemeral"}},
            ],
            "messages": [{"role": "user", "content": blocks}],
        })
    };
    let shape = crate::proxy::CacheShape { global: true, ttl_1h: true };

    // 3 + 1 = 4，已经满了：不整形，且一个字节都不动。
    let mut full = mk(3);
    let before = full.clone();
    assert!(!crate::proxy::align_system_shape(&mut full, shape), "满了不该再拆");
    assert_eq!(full, before, "不拆就该原样留着，别留下半拆的形态");
    assert_eq!(crate::proxy::count_cache_control(&full), 4);

    // 2 + 1 = 3，还差一个：照常拆，拆完正好顶到 4。
    let mut room = mk(2);
    assert!(crate::proxy::align_system_shape(&mut room, shape));
    assert_eq!(
        crate::proxy::count_cache_control(&room),
        crate::proxy::MAX_CACHE_BREAKPOINTS,
        "还有位置就该拆"
    );
    assert_eq!(room["system"].as_array().unwrap().len(), 4, "拆成 [billing, 身份, 基座, 其余]");

    // 身份句自带断点的那一档（官方 API-key 三块形态）：2 + 2 = 4 已经满着，但拆开是净 0
    // ——身份句那个被去掉、合并块那个变两个——这道闸不该拦它。
    let mut official = mk(2);
    official["system"][1]["cache_control"] = serde_json::json!({"type": "ephemeral"});
    assert_eq!(crate::proxy::count_cache_control(&official), 4, "拆之前就已经满了");
    assert!(crate::proxy::align_system_shape(&mut official, shape), "净 0，这道闸不该拦它");
    assert_eq!(crate::proxy::count_cache_control(&official), 4, "拆完还是 4");
    assert!(official["system"][1].get("cache_control").is_none(), "身份句那个该被去掉");

    // 工具 schema 的参数名、工具入参里的同名字段不是断点：真断点 1 + 1 = 2，递归去数会是 4，
    // 拆完算成 5、整形被拦。
    let mut business = mk(1);
    business["tools"] = serde_json::json!([{ "name": "t", "input_schema": {
            "type": "object", "properties": { "cache_control": { "type": "string" } },
        } }]);
    business["messages"].as_array_mut().unwrap().push(serde_json::json!({
        "role": "assistant",
        "content": [{ "type": "tool_use", "id": "x", "name": "t",
            "input": { "cache_control": { "type": "ephemeral" } } }],
    }));
    assert_eq!(crate::proxy::count_cache_control(&business), 2);
    assert!(crate::proxy::align_system_shape(&mut business, shape), "业务字段不占预算");
    assert_eq!(crate::proxy::count_cache_control(&business), 3);
}

/// 真 CC 路径：历史里 `tool_use.input.cache_control` 不是「消息已有断点」，schema 参数名也
/// 不占预算——末条照常补上断点。
#[test]
fn cc_message_breakpoint_ignores_business_cache_control_fields() {
    let cc = serde_json::json!({ "type": "ephemeral", "ttl": "1h" });
    let mut v = serde_json::json!({
        "tools": [{ "name": "t", "input_schema": {
            "type": "object", "properties": { "cache_control": { "type": "string" } },
        } }],
        "system": [
            { "type": "text", "text": "a", "cache_control": cc },
            { "type": "text", "text": "b", "cache_control": cc },
        ],
        "messages": [
            { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
            { "role": "assistant", "content": [{ "type": "tool_use", "id": "x", "name": "t",
                "input": { "cache_control": { "type": "ephemeral" } } }] },
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "x", "content": "ok" },
            ] },
        ],
    });
    assert!(super::ensure_cc_message_breakpoint(&mut v), "{v}");
    assert_eq!(v["messages"][2]["content"][0]["cache_control"], cc, "{v}");
    assert_eq!(crate::proxy::count_cache_control(&v), 3);
}

/// `rewrite_body` 的「全关且不模拟」快路径不能吞掉 `fallbacks`：头上按同一个判断补了
/// beta，体里必须写字段，否则 fable 的拒答换模型重跑名存实亡。反例：不补时快路径照走、
/// 体原样。
#[test]
fn rewrite_body_fast_path_still_writes_fallbacks() {
    let off = store::ForwardFlags {
        spoof_identity: false,
        spoof_device_id: false,
        normalize_device_fp: false,
        billing_cch: false,
        cch_real_recompute: false,
        cch_sim_compute: false,
        fill_client_headers: false,
        merge_beta: false,
        system_shape: false,
        orig_header_case: false,
        thinking_signature_retry: false,
        thinking_modified_retry: false,
        redacted_thinking_retry: false,
        simulate_cc: false,
        simulate_full_system: false,
        fill_absent_tools: false,
        sim_trim_tools: false,
        sim_billing_only: false,
        sim_message_threads: false,
        fill_metadata: false,
        rate_limit_retry: false,
        cache_scope_global: false,
        cache_ttl_1h: false,
        eager_tool_streaming: false,
        nonstream_as_sse: false,
        strip_extra_fields: false,
        tool_name_mimic: false,
        inject_thinking: false,
        flatten_tool_schemas: false,
        strip_empty_text: false,
        hoist_system_role: false,
        reject_openai_shape: false,
        reject_session_conflict: false,
        reject_probes: false,
        reject_probes_strict: false,
        reject_refusals: false,
        reject_empty_replies: false,
        reject_learned_shapes: false,
        api_telemetry: false,
        keepalive_telemetry: false,
        fable_refusal_fallback: true,
        opus_refusal_fallback: false,
    };
    let body = Bytes::from_static(
            br#"{"model":"claude-fable-5-1","max_tokens":32000,"messages":[{"role":"user","content":"hi"}]}"#,
        );
    let plan = crate::proxy::cc_profile_for("claude-fable-5-1").fallbacks.unwrap();
    let shape = |fallbacks: Option<&str>| {
        crate::proxy::rewrite_body_out(
            &body,
            &test_cred(),
            "fp",
            off,
            None,
            None,
            None,
            false,
            None,
            false,
            false,
            None,
            None,
            crate::proxy::CcRequestKind::Main,
            fallbacks,
        )
        .0
    };
    // 不补：快路径，体原样。
    assert_eq!(shape(None), body);
    // 补：体里必须有官方那份数组，且按官方键序落在 max_tokens 之后。
    let out: serde_json::Value = serde_json::from_slice(&shape(Some(plan))).unwrap();
    assert_eq!(out["fallbacks"], serde_json::json!([{"model": "claude-opus-5"}]));
    let keys: Vec<&str> = out.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["model", "max_tokens", "fallbacks", "messages"]);
}

#[test]
fn ensure_fallbacks_inserts_at_the_official_position_and_keeps_client_arrays() {
    let plan = config::OPUS_REFUSAL_FALLBACKS;
    let mut v = serde_json::json!({
        "model": "claude-opus-5", "messages": [], "max_tokens": 8, "thinking": {"type": "adaptive"},
        "context_management": {"edits": []}, "output_config": {"effort": "high"}, "stream": true
    });
    assert!(crate::proxy::ensure_fallbacks(&mut v, plan));
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "model",
            "messages",
            "max_tokens",
            "thinking",
            "context_management",
            "fallbacks",
            "output_config",
            "stream"
        ],
        "{v}"
    );
    assert_eq!(
        v["fallbacks"],
        serde_json::json!([{"model": "claude-opus-4-8"}, {"model": "claude-opus-4-6"}])
    );
    // 2.1.258 的字符串形态：换成数组、位置不动。
    let mut v =
        serde_json::json!({"model": "claude-fable-5-1", "fallbacks": "default", "messages": []});
    assert!(crate::proxy::ensure_fallbacks(&mut v, r#"[{"model":"claude-opus-5"}]"#));
    assert_eq!(v["fallbacks"], serde_json::json!([{"model": "claude-opus-5"}]));
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["model", "fallbacks", "messages"]);
    // 客户端自己带的数组：原样不动。
    let mut v = serde_json::json!({"model": "claude-opus-5", "fallbacks": [{"model": "claude-sonnet-5"}], "messages": []});
    assert!(!crate::proxy::ensure_fallbacks(&mut v, plan));
    assert_eq!(v["fallbacks"], serde_json::json!([{"model": "claude-sonnet-5"}]));
    // 经 rewrite_body 走一遍模拟路径：fable 有字面量就补，位置按 profile 键序归位。
    let body = Bytes::from(r#"{"model":"claude-fable-5-1","messages":[{"role":"user","content":"hi"}],"max_tokens":16}"#.to_string());
    let sim = sim_for(std::str::from_utf8(&body).unwrap());
    let out = crate::proxy::rewrite_body_out(
        &body,
        &test_cred(),
        "fp",
        all_on(),
        Some(&sim),
        None,
        None,
        false,
        None,
        true,
        true,
        None,
        None,
        crate::proxy::CcRequestKind::Main,
        Some(r#"[{"model":"claude-opus-5"}]"#),
    )
    .0;
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["fallbacks"], serde_json::json!([{"model": "claude-opus-5"}]), "{v}");
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    let idx =
        |k: &str| keys.iter().position(|x| *x == k).unwrap_or_else(|| panic!("{k} 缺失: {keys:?}"));
    assert!(idx("fallbacks") > idx("max_tokens"), "{keys:?}");
    assert!(
        idx("fallbacks") < idx("diagnostics"),
        "官方序里 fallbacks 在 diagnostics 之前: {keys:?}"
    );
}

/// 注入的工具声明是 2.1.291 官方主线程恒带的 14 个真工具：opus / sonnet / fable 一份，haiku 一份
/// （名字与先后相同、六条描述是长版），不是四个。
///
/// 依据：`cap/auto-2.1.291-20261006-full` 默认权限模式的主线程（`00340` opus / `00253` sonnet /
/// `00303` haiku），去掉 `ToolSearch` + `DeferredToolPlaceholder` 那一对延迟加载机制、用户自己的
/// `mcp__*` 与服务端工具 `advisor` 就是这 14 个。
#[test]
fn core_tool_stubs_are_profile_specific() {
    use config::CcProfileKind::*;
    let opus = crate::proxy::cc_tools_core(config::cc_profile(MainOpus), false);
    let haiku = crate::proxy::cc_tools_core(config::cc_profile(MainHaiku), false);
    let names = |t: &[serde_json::Value]| -> Vec<String> {
        t.iter().map(|x| x["name"].as_str().unwrap_or("?").to_string()).collect()
    };
    // **顺序也是抓包的一部分**：这 14 个的相对次序是官方声明序，不是字母序（`Write` 排在
    // `Workflow` 之后）。资产按字母序或按手写顺序排都会得到一个官方不产生的排列，而这种
    // 错不会有任何运行期症状。
    let expected = [
        "Agent",
        "Artifact",
        "AskUserQuestion",
        "Bash",
        "Edit",
        "ListAgents",
        "Read",
        "ReportFindings",
        "ScheduleWakeup",
        "SendFeedback",
        "ShareOnboardingGuide",
        "Skill",
        "Workflow",
        "Write",
    ];
    assert_eq!(names(opus), expected, "官方主线程恒带的 14 个真工具与其次序");
    assert_eq!(names(haiku), expected, "haiku 名字与先后同一份");
    for asset in [opus, haiku] {
        // 延迟加载那一对、服务端工具与 MCP 工具故意不注。
        for absent in ["ToolSearch", "DeferredToolPlaceholder", "advisor"] {
            assert!(!names(asset).iter().any(|n| n == absent), "{absent} 不该注入");
        }
        assert!(!names(asset).iter().any(|n| n.starts_with("mcp__")), "用户的 MCP 工具不注");
        // `eager_input_streaming` 原样保留：四族每个内建工具都带。
        assert!(asset.iter().all(|t| t["eager_input_streaming"] == true), "全带");
        assert!(asset.iter().all(|t| t.get("defer_loading").is_none()), "正文声明的都不是延迟池的");
        assert!(asset.iter().all(|t| t.get("type").is_none()), "没有服务端工具");
    }
    // opus / sonnet / fable 同一份。
    for kind in [MainFable, MainSonnet] {
        assert_eq!(crate::proxy::cc_tools_core(config::cc_profile(kind), false), opus, "{kind:?}");
    }
    // haiku 那份六条是长描述，其余八条与 opus 逐字节相同。
    let d = |a: &[serde_json::Value], n: &str| {
        serde_json::to_string(a.iter().find(|t| t["name"] == n).unwrap()).unwrap()
    };
    for n in ["Agent", "AskUserQuestion", "Bash", "Edit", "Read", "Write"] {
        assert_ne!(d(opus, n), d(haiku, n), "{n}");
    }
    for n in ["Artifact", "ListAgents", "ReportFindings", "ScheduleWakeup", "SendFeedback"] {
        assert_eq!(d(opus, n), d(haiku, n), "{n}");
    }
    // Bash 取的是默认权限模式那版（多一句别用 Bash 跑 cat 那类命令），是资产真的换到了 2.1.291
    // 默认模式的最短证据；auto 模式那版（2707 字节）没有这一句。2.1.293 的 Bash 逐字未变。
    let bash = opus.iter().find(|t| t["name"] == "Bash").unwrap();
    assert!(
        bash["description"].as_str().unwrap().contains("Avoid using this tool to run `cat`"),
        "Bash 描述取自 cap/auto-2.1.293-20261008-full/00419: {}",
        bash["description"]
    );
    assert_eq!(d(opus, "Bash").chars().count(), 3018);
    // 2.1.293 的 Artifact 改了一段措辞（2.1.291 是 34399）。
    assert_eq!(d(opus, "Artifact").chars().count(), 34605);
    assert_eq!(d(haiku, "Bash").chars().count(), 11913);
}

/// 开关 `sim_trim_tools`：注入的官方工具去掉 Artifact / ListAgents / SendFeedback，剩 11 条、先后
/// 不变、其余逐字节同完整那份；四族都是（`cap/auto-2.1.291-20261006` 关前 / 关后各一对）。整条模拟
/// 路径开着这项时出站也正好是这 11 条。
#[test]
fn trim_switch_drops_exactly_the_three_user_switchable_tools() {
    use config::CcProfileKind::*;
    for kind in [MainOpus, MainFable, MainSonnet, MainHaiku] {
        let profile = config::cc_profile(kind);
        let full = crate::proxy::cc_tools_core(profile, false);
        let trim = crate::proxy::cc_tools_core(profile, true);
        let kept: Vec<&serde_json::Value> = full
            .iter()
            .filter(|t| {
                !["Artifact", "ListAgents", "SendFeedback"].contains(&t["name"].as_str().unwrap())
            })
            .collect();
        assert_eq!(trim.len(), 11, "{kind:?}");
        assert_eq!(trim.iter().collect::<Vec<_>>(), kept, "{kind:?}: 其余 11 条逐条相同、先后不变");
    }
    use crate::proxy::test_support::{all_on, detect_for, rewrite_body, test_cred};
    let raw = Bytes::from_static(
        br#"{"model":"claude-opus-5-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    let flags = store::ForwardFlags { sim_trim_tools: true, ..all_on() };
    let sim = detect_for(&raw, flags).unwrap();
    assert!(sim.trim_tools);
    let v: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&raw, &test_cred(), "fp", flags, Some(&sim), None))
            .unwrap();
    let names: Vec<&str> =
        v["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 11, "{names:?}");
    assert!(!names.iter().any(|n| ["Artifact", "ListAgents", "SendFeedback"].contains(n)));
    // 默认开着；关掉是完整的 14 条。
    assert!(all_on().sim_trim_tools, "默认启用");
    let off = store::ForwardFlags { sim_trim_tools: false, ..all_on() };
    let sim = detect_for(&raw, off).unwrap();
    assert!(!sim.trim_tools);
    let v: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&raw, &test_cred(), "fp", off, Some(&sim), None))
            .unwrap();
    assert_eq!(v["tools"].as_array().unwrap().len(), 14);
}

/// 开关 `sim_billing_only`：开着时模拟路径只在 `system[0]` 注一条最小 billing header，客户端自己的
/// system 块 / 工具 / metadata 原样透传，不补身份句 / 基座 / 第四块 / 官方工具 / diagnostics /
/// output_config / thread；防 400 的无损归一照做，cch 仍按出站字节重算。默认停用。
#[cfg(test)]
mod billing_only {
    use super::*;
    use crate::proxy::test_support::{all_on, detect_for, rewrite_body, test_cred};

    /// 一条非 CC 来访：两块客户端 system、两个自定义工具、自带 `metadata.user_id`。
    const RAW: &str = concat!(
        r#"{"model":"claude-opus-5-5","max_tokens":64,"#,
        r#""system":[{"type":"text","text":"CLIENT-A 指令"},{"type":"text","text":"CLIENT-B 追加"}],"#,
        r#""tools":[{"name":"my_lookup","input_schema":{"type":"object"}},"#,
        r#"{"name":"my_exec","input_schema":{"type":"object"}}],"#,
        r#""metadata":{"user_id":"client-uid-123"},"#,
        r#""messages":[{"role":"user","content":"hi"}]}"#
    );

    fn on_flags() -> store::ForwardFlags {
        store::ForwardFlags { sim_billing_only: true, ..all_on() }
    }

    fn out(flags: store::ForwardFlags) -> serde_json::Value {
        let raw = Bytes::from_static(RAW.as_bytes());
        let sim = detect_for(&raw, flags).unwrap();
        serde_json::from_slice(&rewrite_body(&raw, &test_cred(), "fp", flags, Some(&sim), None))
            .unwrap()
    }

    /// 开着时 `system` = 一条最小 billing header + 客户端原块；官方整形一律不做，工具原样。
    #[test]
    fn emits_only_billing_header_and_client_blocks() {
        let raw = Bytes::from_static(RAW.as_bytes());
        let sim = detect_for(&raw, on_flags()).unwrap();
        assert!(sim.billing_only, "开关开着时 detect 应置 billing_only");
        let v = out(on_flags());
        let sys = v["system"].as_array().unwrap();
        assert_eq!(sys.len(), 3, "billing header + 两块客户端原块: {v}");
        assert!(
            sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"),
            "system[0] 是 billing header: {}",
            sys[0]
        );
        assert_eq!(sys[1]["text"], "CLIENT-A 指令", "客户端首块原样");
        assert_eq!(sys[2]["text"], "CLIENT-B 追加", "客户端次块原样");
        assert!(
            !sys[0]["text"].as_str().unwrap().contains("You are Claude Code"),
            "不补身份句/基座"
        );
        assert!(v.get("thinking").is_none(), "不补 thinking");
        assert!(v.get("context_management").is_none(), "不补 context_management");
        assert!(v.get("output_config").is_none(), "不补 output_config");
        assert!(v.get("diagnostics").is_none(), "不补 diagnostics");
        assert!(v.get("thread").is_none(), "不写 thread");
        let names: Vec<&str> =
            v["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["my_lookup", "my_exec"], "工具原样，不注官方工具");
    }

    /// `metadata.user_id` 完全原样透传，不剥、不重建。
    #[test]
    fn preserves_client_metadata() {
        let v = out(on_flags());
        assert_eq!(v["metadata"]["user_id"], "client-uid-123");
    }

    /// cch 仍按出站字节重算：出站 `system[0]` 里的 cch 等于对（cch 归零后的）出站字节算的真值，
    /// 且不是跨账号恒定的 `00000`。
    #[test]
    fn cch_matches_outbound_bytes() {
        let raw = Bytes::from_static(RAW.as_bytes());
        let sim = detect_for(&raw, on_flags()).unwrap();
        let bytes = rewrite_body(&raw, &test_cred(), "fp", on_flags(), Some(&sim), None);
        let mut copy = bytes.to_vec();
        let recomputed =
            crate::proxy::body::apply_cch(&mut copy).expect("出站体里应有 billing header 的 cch");
        assert_ne!(recomputed, "00000", "cch 不应留占位");
        // apply_cch 对已定型的出站字节再算一次应得同值 → 证明出站里那条就是真值。
        assert_eq!(&copy, bytes.as_ref(), "出站 cch 已是对出站字节算的真值");
    }

    /// 防 400 的无损归一照做：空壳 `role:"system"` 消息丢弃、重复工具去重——即便开着 billing-only。
    #[test]
    fn still_drops_empty_system_and_dedups_tools() {
        let raw = Bytes::from_static(
            concat!(
                r#"{"model":"claude-opus-5-5","max_tokens":64,"#,
                r#""system":[{"type":"text","text":"C"}],"#,
                r#""tools":[{"name":"dup","input_schema":{"type":"object"}},"#,
                r#"{"name":"dup","input_schema":{"type":"object"}}],"#,
                r#""messages":[{"role":"system","content":[]},{"role":"user","content":"hi"}]}"#
            )
            .as_bytes(),
        );
        let sim = detect_for(&raw, on_flags()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewrite_body(
            &raw,
            &test_cred(),
            "fp",
            on_flags(),
            Some(&sim),
            None,
        ))
        .unwrap();
        let names: Vec<&str> =
            v["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["dup"], "重复工具去重");
        let roles: Vec<&str> =
            v["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user"], "空壳 system 消息被丢弃");
    }

    /// 不 cap、不 strip 客户端块：客户端给了 6 块 system（超过官方 5 块上限），billing-only 下
    /// 原样保留（只在前面加一条 billing header = 7 块），不合并封顶。
    #[test]
    fn does_not_cap_or_strip_client_blocks() {
        let raw = Bytes::from_static(
            concat!(
                r#"{"model":"claude-opus-5-5","max_tokens":64,"#,
                r#""system":[{"type":"text","text":"B1"},{"type":"text","text":"B2"},"#,
                r#"{"type":"text","text":"B3"},{"type":"text","text":"B4"},"#,
                r#"{"type":"text","text":"B5"},{"type":"text","text":"B6"}],"#,
                r#""messages":[{"role":"user","content":"hi"}]}"#
            )
            .as_bytes(),
        );
        let sim = detect_for(&raw, on_flags()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewrite_body(
            &raw,
            &test_cred(),
            "fp",
            on_flags(),
            Some(&sim),
            None,
        ))
        .unwrap();
        let texts: Vec<&str> = v["system"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .map(|b| b["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["B1", "B2", "B3", "B4", "B5", "B6"], "六块原样保留、不合并封顶");
    }

    /// 修复 1：客户端把旧 billing header 与业务指令写在同一块（单换行分隔）时，billing-only **行级**
    /// 剥——只去掉 header 那一行，保留业务指令；且全局只剩 system[0] 一条 billing header。
    #[test]
    fn strips_old_billing_line_but_keeps_business_instructions_in_the_same_block() {
        let raw = Bytes::from_static(
            concat!(
                r#"{"model":"claude-opus-5-5","max_tokens":64,"#,
                r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=9.9.9.aaa; cc_entrypoint=cli; cch=12345;\n保留我的业务指令"}],"#,
                r#""messages":[{"role":"user","content":"hi"}]}"#
            )
            .as_bytes(),
        );
        let sim = detect_for(&raw, on_flags()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewrite_body(
            &raw,
            &test_cred(),
            "fp",
            on_flags(),
            Some(&sim),
            None,
        ))
        .unwrap();
        let sys = v["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2, "只剩注入的 billing header + 客户端那块（指令保留）: {v}");
        assert!(sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"));
        assert_eq!(sys[1]["text"], "保留我的业务指令", "业务指令未被整块删掉");
        let billing_headers = sys
            .iter()
            .filter(|b| {
                b["text"].as_str().is_some_and(|t| t.contains("x-anthropic-billing-header:"))
            })
            .count();
        assert_eq!(billing_headers, 1, "全局只剩一条 billing header，客户端抄的那行已剥");
    }

    /// 修复 2：billing-only 保留客户端 metadata 时，出站会话 id 沿用客户端自带的合法值——
    /// 头与 body 两处同值。这里断言 `sim.session_id` 即客户端那个（头由它派生）。
    #[test]
    fn prefers_client_session_id_so_header_matches_body() {
        const SID: &str = "11111111-1111-4111-8111-111111111111";
        let raw = Bytes::from(format!(
            concat!(
                r#"{{"model":"claude-opus-5-5","max_tokens":64,"#,
                r#""metadata":{{"user_id":"{{\"device_id\":\"d\",\"account_uuid\":\"\",\"session_id\":\"{sid}\"}}"}},"#,
                r#""messages":[{{"role":"user","content":"hi"}}]}}"#
            ),
            sid = SID
        ));
        let on = store::ForwardFlags { sim_billing_only: true, ..all_on() };
        let sim =
            crate::proxy::test_support::detect_with(&raw, &crate::proxy::HeaderMap::new(), on)
                .unwrap();
        assert!(sim.billing_only);
        assert_eq!(sim.session_id, SID, "billing-only 沿用客户端自带会话 id");
        // 关着时走派生：不等于客户端那个原值。
        let sim_off = crate::proxy::test_support::detect_with(
            &raw,
            &crate::proxy::HeaderMap::new(),
            all_on(),
        )
        .unwrap();
        assert_ne!(sim_off.session_id, SID, "完整模拟按账号派生、不照抄客户端 uuid");
    }

    /// 修复 3：客户端自带字符串 `fallbacks:"default"` 仍按防 400 归一成官方数组——即便 billing-only。
    /// （出站头仍会按 body 有 fallbacks 声明 `server-side-fallback` beta，新日期下上游只收数组。）
    #[test]
    fn normalizes_client_string_fallback_for_validity() {
        let raw = Bytes::from_static(
            concat!(
                r#"{"model":"claude-fable-5-1","max_tokens":64,"#,
                r#""fallbacks":"default","#,
                r#""messages":[{"role":"user","content":"hi"}]}"#
            )
            .as_bytes(),
        );
        let sim = detect_for(&raw, on_flags()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewrite_body(
            &raw,
            &test_cred(),
            "fp",
            on_flags(),
            Some(&sim),
            None,
        ))
        .unwrap();
        assert!(v["fallbacks"].is_array(), "字符串 default 应归一成数组: {}", v["fallbacks"]);
        assert_eq!(v["fallbacks"][0]["model"], "claude-opus-5");
    }

    /// 关着时（默认）走完整官方形态：注入官方工具、补 output_config——证明门控是条件性的、
    /// 默认行为不变。
    #[test]
    fn disabled_falls_back_to_full_sim() {
        assert!(!all_on().sim_billing_only, "默认停用");
        let raw = Bytes::from_static(RAW.as_bytes());
        let sim = detect_for(&raw, all_on()).unwrap();
        assert!(!sim.billing_only);
        let v: serde_json::Value = serde_json::from_slice(&rewrite_body(
            &raw,
            &test_cred(),
            "fp",
            all_on(),
            Some(&sim),
            None,
        ))
        .unwrap();
        let names: Vec<&str> =
            v["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.iter().any(|n| config::CC_TOOL_NAMES.contains(n)), "完整模拟注官方工具");
        assert!(v.get("output_config").is_some(), "完整模拟补 output_config");
    }
}

/// [`inject_cc_tools`] 的身份参数：只进日志，取什么值都不影响这几个用例验的东西。
fn who() -> super::ToolAlignWho<'static> {
    super::ToolAlignWho { cred_id: 1, cred: "t", session: "s" }
}

/// [`cc_tools_to_inject`] 是注入与流水共用的那一份判据：注进去的名单与流水拿去对
/// 回复 tool_use 的名单必须是同一份，否则「模型调了注入工具」会被记错对象。
#[test]
fn cc_tools_to_inject_names_exactly_what_gets_injected() {
    let profile = config::cc_profile(config::CcProfileKind::MainOpus);
    let all: Vec<&str> = crate::proxy::cc_tools_core(profile, false)
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let body = |tools: &str| -> serde_json::Value {
        serde_json::from_str(&format!(r#"{{"model":"claude-opus-5","messages":[]{tools}}}"#))
            .unwrap()
    };
    // 不带工具的三种写法（没有键、null、空数组）一视同仁：开关关着一个都不注，开着全缺。
    for tools in ["", r#","tools":null"#, r#","tools":[]"#] {
        assert!(
            super::cc_tools_to_inject(&body(tools), profile, false, false).is_empty(),
            "{tools}"
        );
        assert_eq!(super::cc_tools_to_inject(&body(tools), profile, true, false), all, "{tools}");
    }
    // 不带工具却要求必须调工具（any / 指定工具）：不补，别逼模型去调注入的工具。
    for choice in [r#"{"type":"any"}"#, r#"{"type":"tool","name":"x"}"#] {
        let v = body(&format!(r#","tool_choice":{choice}"#));
        assert!(super::cc_tools_to_inject(&v, profile, true, false).is_empty(), "{choice}");
    }
    // auto / none 照补：none 下模型本来就不会调。
    for choice in [r#"{"type":"auto"}"#, r#"{"type":"none"}"#] {
        let v = body(&format!(r#","tool_choice":{choice}"#));
        assert_eq!(super::cc_tools_to_inject(&v, profile, true, false), all, "{choice}");
    }
    // 带了工具的请求与开关无关：照旧补缺。
    let own = body(r#","tools":[{"name":"exec"}],"tool_choice":{"type":"any"}"#);
    assert_eq!(super::cc_tools_to_inject(&own, profile, false, false), all);
    // 怪值（不是数组也不是 null）不动。
    assert!(super::cc_tools_to_inject(&body(r#","tools":{}"#), profile, true, false).is_empty());
    // 已带部分官方名：只补缺的，顺序仍是官方声明序。
    let partial = body(r#","tools":[{"name":"Skill"},{"name":"Bash"},{"name":"TaskCreate"}]"#);
    let expect: Vec<&str> =
        all.iter().copied().filter(|n| !["Skill", "Bash"].contains(n)).collect();
    assert_eq!(super::cc_tools_to_inject(&partial, profile, false, false), expect);
    // 14 个全声明了：不注。
    let full = body(&format!(
        r#","tools":[{}]"#,
        all.iter().map(|n| format!(r#"{{"name":"{n}"}}"#)).collect::<Vec<_>>().join(",")
    ));
    assert!(super::cc_tools_to_inject(&full, profile, false, false).is_empty());
    // 只有第三方名：全部 14 个，与真正注进去的一致。
    let mut v = body(r#","tools":[{"name":"exec"},{"name":"read_file"}]"#);
    let planned = super::cc_tools_to_inject(&v, profile, false, false);
    assert_eq!(planned, all);
    assert!(super::inject_cc_tools(&mut v, profile, false, false, who()));
    let injected: Vec<&str> = v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .filter(|n| config::CC_TOOL_NAMES.contains(n))
        .collect();
    assert_eq!(injected, planned, "注进去的名单必须就是判据给出的那份");
    // 客户端自己的工具仍在后面，一个没丢。
    assert_eq!(v["tools"].as_array().unwrap().len(), all.len() + 2);
}

/// 流水的注入统计按**出站**算：`tool_choice` 用 OpenAI 方言（`"required"` / `"any"` /
/// `{"type":"function"}`）写的无工具请求，归一成 `any` / `tool` 之后注入那一步不补，统计也
/// 必须是「没补」；拿来访原文预判会把它们误记成 `tools_filled`。`"auto"` 归一后照补。
#[test]
fn injection_stats_follow_the_rewritten_body_not_the_inbound_one() {
    use crate::proxy::test_support::{all_on, detect_for, rewrite_body, test_cred};
    let flags = all_on();
    let stats = |choice: &str| {
        let raw = Bytes::from(format!(
            r#"{{"model":"claude-sonnet-5","max_tokens":64,"tool_choice":{choice},"messages":[{{"role":"user","content":"hi"}}]}}"#
        ));
        let inbound: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let sim = detect_for(&raw, flags).unwrap();
        let out: serde_json::Value = serde_json::from_slice(&rewrite_body(
            &raw,
            &test_cred(),
            "fp",
            flags,
            Some(&sim),
            None,
        ))
        .unwrap();
        let injected = super::injected_tools_of(&inbound, &out, sim.profile);
        (injected.len(), !injected.is_empty() && super::declares_no_tools(&inbound), out)
    };
    for choice in [
        r#""required""#,
        r#""any""#,
        r#"{"type":"function"}"#,
        r#"{"type":"function","function":{"name":"x"}}"#,
    ] {
        let (n, filled, out) = stats(choice);
        assert!(out.get("tools").is_none(), "{choice}: 强制调工具不补: {out}");
        assert_eq!((n, filled), (0, false), "{choice}: 统计按出站，不记 tools_filled");
    }
    let (n, filled, _) = stats(r#""auto""#);
    // 默认开着 `sim_trim_tools`：注的是 11 条。
    assert_eq!((n, filled), (11, true), "auto 归一后照补，统计也记上");
    // 自带工具只被补缺的：有注入名单，但不是 `tools_filled`。
    let raw = Bytes::from_static(
            br#"{"model":"claude-sonnet-5","max_tokens":64,"tools":[{"name":"exec","input_schema":{"type":"object"}}],"messages":[{"role":"user","content":"hi"}]}"#,
        );
    let inbound: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let sim = detect_for(&raw, flags).unwrap();
    let out: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&raw, &test_cred(), "fp", flags, Some(&sim), None))
            .unwrap();
    assert_eq!(super::injected_tools_of(&inbound, &out, sim.profile).len(), 11);
    assert!(!super::declares_no_tools(&inbound));
}

/// 开关 `fill_absent_tools`：来访整个没带 `tools` 时，开着（默认）就在官方键序里
/// （`system` 之后）补一个 14 条官方工具的数组，流水那侧的注入名单与之一致；关着则
/// 出站没有 `tools` 键。
#[test]
fn tool_less_requests_get_the_official_tools_only_when_the_switch_is_on() {
    use crate::proxy::test_support::{all_on, detect_for, rewrite_body, test_cred};
    let raw = Bytes::from_static(
            br#"{"model":"claude-sonnet-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#,
        );
    let on = all_on();
    assert!(on.fill_absent_tools, "测试夹具里这项默认开");
    let sim = detect_for(&raw, on).unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&raw, &test_cred(), "fp", on, Some(&sim), None))
            .unwrap();
    let names: Vec<&str> =
        v["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    let all: Vec<&str> = crate::proxy::cc_tools_core(sim.profile, sim.trim_tools)
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(sim.trim_tools, "默认开着精简");
    assert_eq!(names, all, "按官方声明序补齐（默认精简后 11 个）");
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    let at = |k: &str| keys.iter().position(|x| *x == k).unwrap();
    assert!(at("system") < at("tools") && at("tools") < at("metadata"), "官方键序: {keys:?}");
    assert!(v.get("tool_choice").is_none(), "不补 tool_choice（接受模型会调用的风险）");
    let body_in: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        super::cc_tools_to_inject(&body_in, sim.profile, sim.fill_absent_tools, sim.trim_tools),
        all,
        "流水认注入工具的名单与真正注进去的一致"
    );

    let off = store::ForwardFlags { fill_absent_tools: false, ..all_on() };
    let sim = detect_for(&raw, off).unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&raw, &test_cred(), "fp", off, Some(&sim), None))
            .unwrap();
    assert!(v.get("tools").is_none(), "关着不凭空造 tools: {v}");

    // `tools: null` 原位换成官方数组；`tools: []` 关着开关时保持空数组。
    let null = Bytes::from_static(
            br#"{"model":"claude-sonnet-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"tools":null}"#,
        );
    let sim = detect_for(&null, on).unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&null, &test_cred(), "fp", on, Some(&sim), None))
            .unwrap();
    assert_eq!(v["tools"].as_array().map(Vec::len), Some(all.len()), "{v}");
    let empty = Bytes::from_static(
            br#"{"model":"claude-sonnet-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"tools":[]}"#,
        );
    let sim = detect_for(&empty, off).unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&empty, &test_cred(), "fp", off, Some(&sim), None))
            .unwrap();
    assert_eq!(v["tools"], serde_json::json!([]), "关着时空数组也不补: {v}");
}

/// 同名替换保留客户端显式写的 `eager_input_streaming`：opus 资产带 true，客户端 Read 写了
/// false → 出站 Read 是官方对象但 eager 为 false；fable 资产没有这个键，客户端 Read 写了
/// true → 出站 Read 带 true；没写的一律等于资产。整条模拟路径走完（注入 → 补 eager → 混淆）
/// 结论不变——补 eager 那步对已有键不动。
#[test]
fn same_named_client_tools_keep_their_explicit_eager_setting() {
    for (kind, model, client_value) in [
        (config::CcProfileKind::MainOpus, "claude-opus-5", false),
        (config::CcProfileKind::MainFable, "claude-fable-5-1", true),
    ] {
        let profile = config::cc_profile(kind);
        let asset = crate::proxy::cc_tools_core(profile, false);
        let asset_read = asset.iter().find(|t| t["name"] == "Read").unwrap();
        let asset_bash = asset.iter().find(|t| t["name"] == "Bash").unwrap();
        let mut v = serde_json::json!({
            "model": model, "messages": [],
            "tools": [
                {"name": "Read", "description": "mine", "input_schema": {"type": "object"}, "eager_input_streaming": client_value},
                {"name": "Bash", "description": "mine", "input_schema": {"type": "object"}}
            ]
        });
        assert!(super::inject_cc_tools(&mut v, profile, false, false, who()));
        let tools = v["tools"].as_array().unwrap();
        let read = tools.iter().find(|t| t["name"] == "Read").unwrap();
        let bash = tools.iter().find(|t| t["name"] == "Bash").unwrap();
        let mut expect_read = asset_read.clone();
        expect_read["eager_input_streaming"] = serde_json::json!(client_value);
        assert_eq!(
            serde_json::to_string(read).unwrap(),
            serde_json::to_string(&expect_read).unwrap(),
            "{model}: 官方对象 + 客户端显式 eager"
        );
        assert_eq!(
            serde_json::to_string(bash).unwrap(),
            serde_json::to_string(asset_bash).unwrap(),
            "{model}: 没写的等于资产"
        );

        // 整条模拟路径：补 eager 那步不覆盖已有键，混淆不动白名单里的官方名。
        let body: Bytes = serde_json::json!({
                "model": model, "max_tokens": 1024,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "Read", "description": "mine", "input_schema": {"type": "object"}, "eager_input_streaming": client_value}],
                "stream": true
            })
            .to_string()
            .into();
        let sim = sim_for(std::str::from_utf8(&body).unwrap());
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let out: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let read = out["tools"].as_array().unwrap().iter().find(|t| t["name"] == "Read").unwrap();
        assert_eq!(read["eager_input_streaming"], client_value, "{model}: 出站保留显式值");
    }
}

/// 客户端已带部分官方名的「半抄」克隆：缺的补到头部，同名的一律整条换成官方声明（参数
/// 表面一致与否都换，不一致的只多一行日志），老版本多出来的不删。依据是现网一条
/// Go-http-client：15 个官方名（含 2.1.258 才有的 TaskCreate 等）、缺 2.1.260 恒带的四个，
/// 原先一个都不补。
#[test]
fn partial_cc_clones_get_the_missing_tools_and_official_replacements() {
    let profile = config::cc_profile(config::CcProfileKind::MainOpus);
    let official = crate::proxy::cc_tools_core(profile, false);
    let official_read = official.iter().find(|t| t["name"] == "Read").unwrap();
    let official_bash = official.iter().find(|t| t["name"] == "Bash").unwrap();
    // Read：抄了参数表面（同一组 properties / required），自己写的描述 → 换。
    let mut client_read = official_read.clone();
    client_read["description"] = serde_json::json!("reads a file, my own wording");
    client_read.as_object_mut().unwrap().remove("eager_input_streaming");
    // Bash：多要一个必填 `cwd`，参数表面与官方不一致 → 照样换，只是多一行日志。
    let client_bash = serde_json::json!({
        "name": "Bash", "description": "run",
        "input_schema": {"type": "object", "properties": {"command": {"type": "string"}, "cwd": {"type": "string"}}, "required": ["command", "cwd"]}
    });
    let mut v = serde_json::json!({
        "model": "claude-opus-5", "messages": [],
        "tools": [{"name": "my_tool", "input_schema": {"type": "object"}}, client_read, client_bash, {"name": "TaskCreate", "input_schema": {"type": "object"}}]
    });
    let planned = super::cc_tools_to_inject(&v, profile, false, false);
    assert!(!planned.contains(&"Read") && !planned.contains(&"Bash"), "{planned:?}");
    assert_eq!(planned.len(), 12, "14 个里客户端已有 Read / Bash 两个");
    assert!(super::inject_cc_tools(&mut v, profile, false, false, who()));
    let tools = v["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    // 头部是完整的 14 条、按官方声明序（客户端的 Read / Bash 被挪进这一段）；客户端其余
    // 工具紧随其后、相对次序不变。
    let all: Vec<&str> = official.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(&names[..14], all.as_slice());
    assert_eq!(&names[14..], ["my_tool", "TaskCreate"]);
    let read = tools.iter().find(|t| t["name"] == "Read").unwrap();
    assert_eq!(read, official_read, "参数表面一致的 Read 整条换成官方声明");
    let bash = tools.iter().find(|t| t["name"] == "Bash").unwrap();
    assert_eq!(bash, official_bash, "参数表面不一致的 Bash 同样换成官方声明");
    assert!(!super::same_schema_surface(&client_bash, official_bash), "日志判据认得出它不一致");
    assert!(names.contains(&"TaskCreate"), "老版本多出来的不删");
}

/// 客户端把 14 个全声明了，但次序不是官方的、其中一条键序不同（内容全同）：出站仍要是
/// 按官方序排列的 14 条逐字节官方声明。`Value` 相等忽略键序，按它跳过替换会把客户端键序
/// 原样发出去；只缺几条时把缺的插头部则会把次序排乱。
#[test]
fn official_tools_are_emitted_in_asset_order_and_byte_exact() {
    let profile = config::cc_profile(config::CcProfileKind::MainOpus);
    let official = crate::proxy::cc_tools_core(profile, false);
    let expected = serde_json::to_string(official).unwrap();
    // 倒序声明，并把第一条（Write）的键序打乱：input_schema 提到 name 之前。
    let mut declared: Vec<serde_json::Value> = official.iter().rev().cloned().collect();
    let scrambled = {
        let src = declared[0].as_object().unwrap();
        let mut m = serde_json::Map::new();
        m.insert("input_schema".into(), src["input_schema"].clone());
        for (k, val) in src.iter().filter(|(k, _)| *k != "input_schema") {
            m.insert(k.clone(), val.clone());
        }
        serde_json::Value::Object(m)
    };
    assert_eq!(scrambled, declared[0], "Value 相等看不出键序不同——这正是要防的");
    assert_ne!(
        serde_json::to_string(&scrambled).unwrap(),
        serde_json::to_string(&declared[0]).unwrap()
    );
    declared[0] = scrambled;
    declared.push(serde_json::json!({"name": "my_tool", "input_schema": {"type": "object"}}));
    let mut v = serde_json::json!({"model": "claude-opus-5", "messages": [], "tools": declared});
    assert!(super::cc_tools_to_inject(&v, profile, false, false).is_empty(), "一个都不缺");
    assert!(super::inject_cc_tools(&mut v, profile, false, false, who()), "次序与键序都要改");
    let tools = v["tools"].as_array().unwrap();
    assert_eq!(
        serde_json::to_string(&tools[..14]).unwrap(),
        expected,
        "14 条逐字节等于资产、按资产序"
    );
    assert_eq!(tools[14]["name"], "my_tool");
    // 已经是官方形态的再过一遍什么都不动。
    assert!(!super::inject_cc_tools(&mut v, profile, false, false, who()));
}

/// Windows 那种**扁平** `metadata.user_id` 同样要认，额度探测复用它的**原文**。
///
/// 只解内嵌 JSON 的话，扁平串解析失败 → 退回「device 为空 + 凭证账号」：主请求有设备、
/// 握手/eval/启动遥测/额度探测却没有。而把它重拼成 JSON 又会造出「同一会话一条扁平、
/// 一条 JSON」——`spoof_identity` 那边特意保住了扁平形态，这里不能给拆了。
#[test]
fn outbound_identity_handles_the_windows_flat_form() {
    const SID: &str = "9f8e7d6c-0000-1111-2222-333344445555";
    let cred = test_cred();
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","max_tokens":64000,"messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"{}"}}],"metadata":{{"user_id":"user_winDev1_account_oldacct_session_{SID}"}}}}"#,
        config::CC_SYSTEM_IDENTITY
    ));

    // 默认配置：`spoof_identity` 换掉 device 与 account，**仍以扁平串回写**。
    let sent = rewrite_body(&body, &cred, "fp", all_on(), None, None);
    let ident = crate::proxy::outbound_identity(&sent, &cred);
    let raw = ident.raw_user_id.clone().expect("出站体里有 user_id");
    assert!(raw.starts_with("user_"), "出站仍是扁平串: {raw}");
    assert!(raw.ends_with(&format!("_session_{SID}")), "session 段保留: {raw}");
    assert_eq!(ident.device_id, cred.spoof_device_id("fp").unwrap(), "device 段解出来了");
    assert_eq!(ident.account_uuid, ACCOUNT_UUID, "account 段也解出来了");
    assert!(!ident.device_id.is_empty(), "不能像只解 JSON 那样退回空 device");

    // `spoof_device_id=false`：device 段保留客户端的，握手跟着。
    let sent = rewrite_body(
        &body,
        &cred,
        "fp",
        store::ForwardFlags { spoof_device_id: false, ..all_on() },
        None,
        None,
    );
    let keep = crate::proxy::outbound_identity(&sent, &cred);
    assert_eq!(keep.device_id, "winDev1");

    // 额度探测复用原文，连编码形态一起——不会变成 JSON。
    let probe = crate::proxy::with_outbound_identity(
        crate::proxy::probe_body(crate::proxy::QUOTA_PROBE_MODEL),
        &keep,
    );
    let v: serde_json::Value = serde_json::from_slice(&probe).unwrap();
    let probe_uid = v["metadata"]["user_id"].as_str().unwrap();
    assert_eq!(Some(probe_uid), keep.raw_user_id.as_deref(), "逐字节同一串");
    assert!(probe_uid.starts_with("user_winDev1_account_"), "还是扁平串: {probe_uid}");
}

/// 关掉 `spoof_identity` 之后，客户端自己带的 `metadata.user_id` **必须原样留着**。
///
/// 剥这一步原先只看 `sim.is_some()`，而重建那步要 `flags.spoof_identity`：开关一关，
/// 身份就被删掉且没人补回来——头上还有会话 id、体里什么都没有。那既违背这个开关的
/// 语义（「别改身份」被执行成了「把身份删了」），也违背客户端数据透传契约。
#[test]
fn identity_spoofing_off_keeps_the_client_metadata() {
    const USER_ID: &str = r#"{\"device_id\":\"dev-1\",\"account_uuid\":\"acct-1\",\"session_id\":\"d0c1fb05-9b19-4576-9465-e2b8a206dabf\"}"#;
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","max_tokens":64000,"messages":[{{"role":"user","content":"hi"}}],"metadata":{{"user_id":"{USER_ID}"}}}}"#
    ));
    let sim = detect_for(&body, all_on()).expect("非 CC 形态该走模拟");

    let off = store::ForwardFlags { spoof_identity: false, ..all_on() };
    let out = rewrite_body(&body, &test_cred(), "fp", off, Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let kept = v["metadata"]["user_id"].as_str().expect("身份不该被删掉");
    assert!(kept.contains("dev-1"), "device_id 原样留着: {kept}");
    assert!(kept.contains("acct-1"), "account_uuid 原样留着: {kept}");

    // 开关开着时照旧重建成该凭证自洽的那份（原有行为不变）。
    let on = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&on).unwrap();
    let rebuilt = v["metadata"]["user_id"].as_str().unwrap();
    assert!(rebuilt.contains(ACCOUNT_UUID), "开着就换成凭证自己的: {rebuilt}");
    assert!(!rebuilt.contains("acct-1"));
}

/// [`crate::proxy::sync_metadata_session`] 把体里的会话段对齐到出站那个，**保持原格式**：
/// 内嵌 JSON 定点替换（字段序与其余内容逐字节不变）、扁平串重拼；本来没有会话段的
/// 就按各自格式补一段——头上有合法会话 id、体里没有，是官方绝不产生的组合。
#[test]
fn syncing_the_metadata_session_keeps_the_original_shape() {
    const SID: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
    let user_id = |v: &serde_json::Value| v["metadata"]["user_id"].as_str().unwrap().to_string();

    // 内嵌 JSON：只有 session_id 那段变了，字段顺序与其余内容原样。
    let mut v = serde_json::json!({
        "metadata": { "user_id": r#"{"device_id":"dd","account_uuid":"aa","session_id":"sess-9"}"# }
    });
    assert!(crate::proxy::sync_metadata_session(&mut v, SID));
    assert_eq!(
        user_id(&v),
        format!(r#"{{"device_id":"dd","account_uuid":"aa","session_id":"{SID}"}}"#)
    );
    // 已经同值 → 不动。
    assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
    // 值只差首尾空白也**要**改：逐字节比，不 trim——否则头上写的是干净的 uuid，体里留着
    // 带空格的那份，两处不再逐字相同。
    let mut v = serde_json::json!({
        "metadata": { "user_id": format!(r#"{{"device_id":"dd","account_uuid":"aa","session_id":" {SID} "}}"#) }
    });
    assert!(crate::proxy::sync_metadata_session(&mut v, SID), "带空白的同值也得改写");
    assert_eq!(
        user_id(&v),
        format!(r#"{{"device_id":"dd","account_uuid":"aa","session_id":"{SID}"}}"#)
    );

    // 扁平串：device 与 account 段原样，只换 session 段，仍以扁平串回写。
    let mut v = serde_json::json!({
        "metadata": { "user_id": "user_deadbeef_account_acct-1_session_sess-9" }
    });
    assert!(crate::proxy::sync_metadata_session(&mut v, SID));
    assert_eq!(user_id(&v), format!("user_deadbeef_account_acct-1_session_{SID}"));
    assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
    // 扁平串的 session 段带尾部空白同样要改写。
    let mut v = serde_json::json!({
        "metadata": { "user_id": format!("user_deadbeef_account_acct-1_session_{SID} ") }
    });
    assert!(crate::proxy::sync_metadata_session(&mut v, SID));
    assert_eq!(user_id(&v), format!("user_deadbeef_account_acct-1_session_{SID}"));

    // 内嵌 JSON 没有会话段 → 追加到末尾（官方键序 device → account → session），
    // 其余内容逐字节不变。补过之后再同步一次是幂等的。
    let mut v = serde_json::json!({
        "metadata": { "user_id": r#"{"device_id":"dd","account_uuid":"aa"}"# }
    });
    assert!(crate::proxy::sync_metadata_session(&mut v, SID));
    assert_eq!(
        user_id(&v),
        format!(r#"{{"device_id":"dd","account_uuid":"aa","session_id":"{SID}"}}"#)
    );
    assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
    // 空对象也能补，不多出前导逗号。
    let mut v = serde_json::json!({ "metadata": { "user_id": "{}" } });
    assert!(crate::proxy::sync_metadata_session(&mut v, SID));
    assert_eq!(user_id(&v), format!(r#"{{"session_id":"{SID}"}}"#));
    // 扁平串缺 session 段 → 追加整段，仍是扁平串。
    let mut v = serde_json::json!({
        "metadata": { "user_id": "user_deadbeef_account_acct-1" }
    });
    assert!(crate::proxy::sync_metadata_session(&mut v, SID));
    assert_eq!(user_id(&v), format!("user_deadbeef_account_acct-1_session_{SID}"));
    assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
    // 两种格式都认不出 → 不动。
    let mut v = serde_json::json!({ "metadata": { "user_id": "opaque-user-42" } });
    assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
    assert_eq!(user_id(&v), "opaque-user-42");
    // 压根没有 metadata.user_id → 不动（那条交给 `ensure_cc_metadata`）。
    let mut v = serde_json::json!({ "model": "claude-opus-5" });
    assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
    assert!(v.get("metadata").is_none());
}

/// 回归 2026-08-07 的拒绝日志：客户端的顶层顺序是
/// `model, system, messages, max_tokens, stream, tools, metadata, output_config`，即使
/// system 和工具名都已整形，这个顺序仍把第三方客户端指纹原样带了出去。
#[test]
fn simulated_request_reorders_existing_top_level_keys() {
    let body = Bytes::from(
            r#"{"model":"claude-sonnet-5","system":"third party","messages":[],"max_tokens":65536,"stream":true,"tools":[{"name":"skill_manage"}],"metadata":{},"output_config":{"effort":"high"}}"#
                .to_string(),
        );
    let parsed_body = parsed(&body);
    let sim = detect_for(&body, all_on()).expect("该请求应走模拟路径");
    let map = build_tool_name_map(parsed_body.as_ref()).unwrap();
    let out = crate::proxy::rewrite_body_out(
        &body,
        &test_cred(),
        "fp",
        all_on(),
        Some(&sim),
        None,
        None,
        false,
        Some(&map),
        true,
        true,
        None,
        None,
        crate::proxy::CcRequestKind::Main,
        None,
    )
    .0;
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "model",
            "messages",
            "system",
            "tools",
            "metadata",
            "max_tokens",
            "thinking",
            "context_management",
            "output_config",
            // 官方主线程每条都带（首轮值为 null），见 [`crate::proxy::ensure_diagnostics`]。
            "diagnostics",
            "stream",
        ],
        "模拟后顶层键序必须与官方抓包一致: {}",
        String::from_utf8_lossy(&out)
    );
    // 模拟路径注入了官方主线程的 14 个工具，它们排在前面。
    let tool_names: Vec<&str> = v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(tool_names.contains(&"Bash"), "模拟路径应注入 CC 核心工具: {tool_names:?}");
    assert!(
        tool_names.iter().any(|n| n.starts_with("mcp__luban__")),
        "客户端自有工具应改成 MCP 形态: {tool_names:?}"
    );
}

/// 补 `thinking` 的形态按 profile：opus/sonnet 是 `{adaptive, display:"updates"}`
/// （2.1.260 起 opus 也带 display，`cap/2.1.260-2/00025`），haiku 是
/// `{budget_tokens, type:enabled, display:"updates"}`（key 序 budget 在前，
/// `cap/2.1.260/00020`）。给 opus-5 / sonnet-5 发 `budget_tokens` 会直接 400。
#[test]
fn injects_thinking_shape_per_model_family() {
    let run = |body: &str| -> serde_json::Value {
        let b = Bytes::from(body.to_string());
        let sim = sim_for(body);
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        serde_json::from_slice(&out).unwrap()
    };
    let opus = run(
        r#"{"model":"claude-opus-5","max_tokens":64000,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(
        opus["thinking"],
        serde_json::json!({"type": "adaptive", "display": "updates"}),
        "{opus}"
    );
    let sonnet = run(
        r#"{"model":"claude-sonnet-5","max_tokens":64000,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(
        sonnet["thinking"],
        serde_json::json!({"type": "adaptive", "display": "updates"}),
        "{sonnet}"
    );
    let haiku = run(
        r#"{"model":"claude-haiku-4-5-20251001","max_tokens":32000,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    let s = serde_json::to_string(&haiku).unwrap();
    assert!(
        s.contains(r#""thinking":{"budget_tokens":31999,"type":"enabled","display":"updates"}"#),
        "haiku 的 thinking 逐字节对齐官方: {s}"
    );
}

/// luban 注入的 thinking 自己收拾它造成的冲突，不靠 `strip_extra_fields` 开着：客户端的
/// `temperature≠1` / `top_p<0.95` 剥掉，`max_tokens == 1024` 时预算直接是 1024，不是 1023。
#[test]
fn injected_thinking_cleans_its_own_conflicts_without_strip_extra_fields() {
    let flags = store::ForwardFlags { strip_extra_fields: false, ..all_on() };
    let run = |body: &str| -> serde_json::Value {
        let b = Bytes::from(body.to_string());
        let sim = sim_for(body);
        let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
        serde_json::from_slice(&out).unwrap()
    };
    let opus = run(
        r#"{"model":"claude-opus-5","max_tokens":64000,"temperature":0.5,"top_p":0.5,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(opus["thinking"]["type"], "adaptive", "{opus}");
    assert!(opus.get("temperature").is_none() && opus.get("top_p").is_none(), "{opus}");
    // temperature 恰为 1、top_p >= 0.95 不冲突，照发。
    let keep = run(
        r#"{"model":"claude-opus-5","max_tokens":64000,"temperature":1,"top_p":0.99,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(keep["temperature"], 1, "{keep}");
    assert_eq!(keep["top_p"], 0.99, "{keep}");
    let haiku = run(
        r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1024,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(haiku["thinking"]["budget_tokens"], 1024, "{haiku}");
}

/// OpenAI 写法的强制工具（`"required"`、`{"type":"function",…}`）要先归一再判注入 thinking：
/// 否则注入时剥掉的 `temperature` / `top_p`，在 thinking 因强制工具被删之后就找不回来了；
/// 「移除多余字段」关着时更会把「手动预算 thinking + 强制工具」这对上游必拒的组合发出去。
#[test]
fn openai_forced_tool_choice_blocks_thinking_injection() {
    for strip in [true, false] {
        let flags = store::ForwardFlags {
            strip_extra_fields: strip,
            reject_openai_shape: false,
            ..all_on()
        };
        for tc in [
            serde_json::json!("required"),
            serde_json::json!({"type": "function", "function": {"name": "Bash"}}),
        ] {
            let body = serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 32000,
                "temperature": 0.5, "top_p": 0.5, "tool_choice": tc,
                "tools": [{"name": "Bash", "description": "d", "input_schema": {"type": "object"}}],
                "messages": [{"role": "user", "content": "hi"}],
            })
            .to_string();
            let b = Bytes::from(body.clone());
            let sim = sim_for(&body);
            let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert!(v.get("thinking").is_none(), "strip={strip}: 强制工具不该注入 thinking: {v}");
            assert_eq!(v["temperature"], 0.5, "strip={strip}: {v}");
            assert_eq!(v["top_p"], 0.5, "strip={strip}: {v}");
        }
    }
}

/// 族开关关着（没计划）时，客户端的字符串 `fallbacks:"default"` 在模拟路径上归一成官方数组：
/// 新日期的 `server-side-fallback` 下上游只收数组（2026-10-08 实测字符串回 400）。
#[test]
fn string_fallbacks_normalized_without_a_plan() {
    let body = r#"{"model":"claude-fable-5-1","fallbacks":"default","max_tokens":1024,"messages":[{"role":"user","content":"hi"}]}"#;
    let b = Bytes::from(body.to_string());
    let sim = sim_for(body);
    let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["fallbacks"], serde_json::json!([{"model": "claude-opus-5"}]), "{v}");
}

/// 模拟路径要补 `context_management`：`cap/raw` 八份抓包逐字节相同，而声明它的
/// `context-management-2025-06-27` 已在两份 seed 里，不补就是「头上声明了、体里没有」。
///
/// **但只在客户端自己开了 thinking 时补**——`clear_thinking` 依赖它，没开硬补上游回
/// `` `clear_thinking_20251015` strategy requires `thinking` to be enabled or adaptive ``。
/// 抓包八份全开着 thinking，这层依赖看不出来，v0.2.51 即因此让普通请求 400。
#[test]
fn simulated_body_carries_official_context_management() {
    const OFFICIAL: &str =
        r#""context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#;
    // 开着 thinking 的来访（官方 opus/sonnet/fable 那族的形态）。
    let thinking_body = concat!(
        r#"{"model":"claude-opus-5","max_tokens":1024,"#,
        r#""messages":[{"role":"user","content":"hi"}],"#,
        r#""thinking":{"type":"adaptive"},"stream":true}"#
    );
    let body = Bytes::from(thinking_body.to_string());
    let sim = sim_for(thinking_body);
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let text = String::from_utf8(out.to_vec()).unwrap();
    assert!(text.contains(OFFICIAL), "取值要与官方逐字节相同: {text}");

    // 官方位置：`thinking` 之后、`stream` 之前。
    let at = text.find(OFFICIAL).unwrap();
    assert!(at > text.find(r#""thinking""#).unwrap(), "该排在 thinking 之后: {text}");
    assert!(at < text.find(r#""stream""#).unwrap(), "该排在 stream 之前: {text}");
    assert!(at > text.find(r#""metadata""#).unwrap(), "该排在 metadata 之后: {text}");

    // 头上那份声明确实在，否则补了体就是反向的自相矛盾。
    assert!(sim.profile.beta.contains("context-management-2025-06-27"), "seed 里该有对应的 beta");

    // haiku 那族的 `{"type":"enabled","budget_tokens":N}` 同样算开着。
    let haiku = concat!(
        r#"{"model":"claude-haiku-4-5-20251001","max_tokens":32000,"#,
        r#""messages":[{"role":"user","content":"hi"}],"#,
        r#""thinking":{"budget_tokens":31999,"type":"enabled"}}"#
    );
    let b = Bytes::from(haiku.to_string());
    let sim = sim_for(haiku);
    let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
    assert!(String::from_utf8(out.to_vec()).unwrap().contains(OFFICIAL), "enabled 也该补");

    // 没开 thinking 的几种写法都不补——补了上游直接 400。
    // max_tokens 低于阈值时也不补（探测级请求不值得加 thinking）。
    for body in [
        // max_tokens 太小，不注入 thinking
        r#"{"model":"claude-opus-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#
            .to_string(),
        // thinking 显式 disabled
        concat!(
            r#"{"model":"claude-opus-5","max_tokens":16,"#,
            r#""messages":[{"role":"user","content":"hi"}],"thinking":{"type":"disabled"}}"#
        )
        .to_string(),
        // thinking 显式 null
        concat!(
            r#"{"model":"claude-opus-5","max_tokens":16,"#,
            r#""messages":[{"role":"user","content":"hi"}],"thinking":null}"#
        )
        .to_string(),
    ] {
        let b = Bytes::from(body.clone());
        let sim = sim_for(&body);
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("context_management").is_none(), "没开 thinking 却补了: {body}");
    }
}

/// 客户端自己带了 `context_management` 就一个字节都不动——那是它自己的编辑策略。
/// CC 形态的来访（非模拟路径）则根本不补。
#[test]
fn context_management_respects_client_and_skips_non_simulated() {
    let mine = concat!(
        r#"{"model":"claude-opus-5","max_tokens":1024,"#,
        r#""messages":[{"role":"user","content":"hi"}],"#,
        r#""context_management":{"edits":[]},"stream":true}"#
    );
    let body = Bytes::from(mine.to_string());
    let sim = sim_for(mine);
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["context_management"]["edits"].as_array().unwrap().len(), 0, "客户端的被改写了");

    // 非模拟路径不补：那条路是尽量原样透传，来访本来就是 CC 形态、自己会带。
    let cc = Bytes::from(API_SHAPE_BODY);
    let out = rewrite_body(&cc, &test_cred(), "fp", all_on(), None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v.get("context_management").is_none(), "非模拟路径不该补: {v}");
}

/// 模拟路径补 `metadata.user_id`：键序与 CC 一致，session_id 与请求头同值且逐设备稳定；
/// 客户端自己带了 user_id 就不新造（交给 spoof_identity 原格式改写）。
#[test]
fn injects_cc_metadata_only_when_absent() {
    let body = Bytes::from(PLAIN_BODY.to_string());
    let sim = sim_for(PLAIN_BODY);
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let user_id = v["metadata"]["user_id"].as_str().unwrap();
    let inner: serde_json::Value = serde_json::from_str(user_id).unwrap();

    assert_eq!(
        inner.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["device_id", "account_uuid", "session_id"],
        "键序应与 CC 一致: {user_id}"
    );
    assert_eq!(inner["account_uuid"], ACCOUNT_UUID);
    assert_eq!(inner["device_id"], test_cred().spoof_device_id("fp").unwrap());
    assert_eq!(inner["session_id"], sim.session_id, "两处 session_id 必须同值");
    assert_eq!(sim.session_id, sim_for(PLAIN_BODY).session_id, "同设备同账号应恒定");

    // 客户端自己带了 user_id 但 UA 不是 claude-cli → 走模拟，原有 user_id 被剥掉，
    // ensure_cc_metadata 用 sim.session_id 重建，确保头体自洽。
    let with_meta = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"metadata":{"user_id":"user_aa_account_bb_session_cc"}}"#
                .to_string(),
        );
    assert!(detect_for(&with_meta, all_on()).is_some(), "非 CC UA 带 user_id 也走模拟");
    let sim2 = detect_for(&with_meta, all_on()).unwrap();
    let out2 = rewrite_body(&with_meta, &test_cred(), "fp", all_on(), Some(&sim2), None);
    let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
    let uid2 = v2["metadata"]["user_id"].as_str().unwrap();
    let inner2: serde_json::Value = serde_json::from_str(uid2).unwrap();
    assert_eq!(
        inner2["session_id"].as_str().unwrap(),
        sim2.session_id,
        "重建后 session_id 应与 sim 一致"
    );

    // 反面：真 CC 客户端（UA 带 claude-cli/ **且** system 是 CC 形态）带了 user_id →
    // 不走模拟，spoof_identity 原格式改写。只有 UA 没有形态的那种见
    // [`cc_client_needs_both_ua_and_cc_shape`]。
    // 扁平串里 device 段与 session 段都得是官方格式（64 位 hex / uuid），否则 detect 会把它
    // 当成身份写错的非官方客户端送去模拟，测不到透传那条路。
    const FLAT_DEV: &str = "832cb7e697190bc475b926c7994ef183a0f8a58e29818f182e11f924e1ea2870";
    const FLAT_SESS: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
    let with_meta = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","system":[{{"type":"text","text":"{}"}},{}],"messages":[],"metadata":{{"user_id":"user_{FLAT_DEV}_account_bb_session_{FLAT_SESS}"}}}}"#,
        config::CC_SYSTEM_IDENTITY,
        base_block()
    ));
    let mut cc_ua = crate::proxy::HeaderMap::new();
    cc_ua
        .insert(header::USER_AGENT, HeaderValue::from_static("claude-cli/2.1.226 (external, cli)"));
    assert!(detect_with(&with_meta, &cc_ua, all_on()).is_none(), "真 CC 客户端不走模拟");
    let out3 = rewrite_body(&with_meta, &test_cred(), "fp", all_on(), None, None);
    let v3: serde_json::Value = serde_json::from_slice(&out3).unwrap();
    assert_eq!(
        v3["metadata"]["user_id"],
        format!(
            "user_{}_account_{ACCOUNT_UUID}_session_{FLAT_SESS}",
            test_cred().spoof_device_id("fp").unwrap()
        ),
        "扁平串形态应原格式改写，而不是被换成 CC 的 JSON 形态"
    );
}

/// 出站 UA 进设备指纹：**同一台设备只会有一个客户端版本**，换版本就是换设备。
///
/// 复盘依据见 [`crate::proxy::device_fingerprint`]：归一化把同平台的客户端收敛成一个 device_id，
/// 各自的 UA 却原样透传，上游看到的是「同一台设备同一秒里既是 2.1.141 的 sdk-cli
/// 又是 2.1.263 的 VSCode 扩展」——官方客户端不可能产生的形态。
#[test]
fn device_fingerprint_separates_client_versions() {
    const UA_OLD: &str = "claude-cli/2.1.141 (external, sdk-cli)";
    const UA_NEW: &str = "claude-cli/2.1.263 (external, claude-vscode, agent-sdk/0.3.263)";
    let h = platform_headers(None);

    let old = crate::proxy::device_fingerprint(None, &h, UA_OLD);
    let new = crate::proxy::device_fingerprint(None, &h, UA_NEW);
    assert_ne!(old, new, "同平台、不同客户端版本必须落在两台设备上");
    assert_ne!(
        test_cred().spoof_device_id(&old),
        test_cred().spoof_device_id(&new),
        "指纹不同，派生出的伪装 device_id 也必须不同"
    );
    // 同一版本恒定：升级才换设备，逐请求不会漂。
    assert_eq!(old, crate::proxy::device_fingerprint(None, &h, UA_OLD));

    // 归一化开着（`client_device_id = None`）时，同平台 + 同版本的多个客户端仍收敛成
    // 一台设备——这是本次改动**没有**动的那一半。
    assert_eq!(
        crate::proxy::device_fingerprint(None, &h, UA_OLD),
        crate::proxy::device_fingerprint(None, &platform_headers(None), UA_OLD),
    );
    // 归一化关着时照旧按客户端设备分开。
    assert_ne!(
        crate::proxy::device_fingerprint(Some("dev-a"), &h, UA_OLD),
        crate::proxy::device_fingerprint(Some("dev-b"), &h, UA_OLD),
    );
    // 平台仍然参与：同版本、不同系统不是一台设备。
    let mut win = crate::proxy::HeaderMap::new();
    win.insert("x-stainless-arch", HeaderValue::from_static("x64"));
    win.insert("x-stainless-os", HeaderValue::from_static("Windows"));
    assert_ne!(old, crate::proxy::device_fingerprint(None, &win, UA_OLD));
}

/// 模拟路径的指纹只看**实际发出去的那套头**：来访自报什么 UA、什么平台、有没有平台头都
/// 落在同一台设备上，而那台设备的平台段与 [`config::CC_SIM_HEADERS`] 一致。
/// arm64 mac 的来访指纹与非模拟路径下用官方 UA 算出来的逐字相同（存量设备 id 不变）。
#[test]
fn simulated_requests_share_one_device_fingerprint() {
    let sim = crate::proxy::sim_device_fingerprint(None);
    assert_eq!(
        sim,
        crate::proxy::device_fingerprint(None, &platform_headers(None), config::CC_USER_AGENT),
        "arm64 mac 来访的指纹与原来一样"
    );
    assert!(sim.ends_with(config::CC_USER_AGENT), "UA 段是模拟路径发出去的那串: {sim}");
    assert!(sim.contains("|arm64|MacOS|"), "平台段取 CC_SIM_HEADERS 的定值: {sim}");
    // 与非模拟路径的对照：Windows 来访在非模拟路径是另一台设备，在模拟路径不是。
    let mut win = crate::proxy::HeaderMap::new();
    win.insert("x-stainless-arch", HeaderValue::from_static("x64"));
    win.insert("x-stainless-os", HeaderValue::from_static("Windows"));
    assert_ne!(sim, crate::proxy::device_fingerprint(None, &win, config::CC_USER_AGENT));
    // 归一化关着时来访自带的设备 id 仍分开。
    assert_ne!(
        crate::proxy::sim_device_fingerprint(Some("dev-a")),
        crate::proxy::sim_device_fingerprint(Some("dev-b"))
    );
}

/// 会话绑定键的两段来源与版本位，见 [`session_binding_key`]。抓包（`cap/2.1.277`）里
/// 主线程、子代理、辅助三类请求共用同一个 `X-Claude-Code-Session-Id`，这条测试把那个
/// 口径钉住：带会话 id 的来访，子代理不另起会话。
#[test]
fn the_binding_key_carries_its_version_and_source() {
    use super::{SESSION_KEY_VERSION, session_binding_key};
    let key = |body: &str| super::sim_session_key(&serde_json::from_str(body).unwrap());
    // 官方一条会话里主线程与子代理的 tools / system 并不相同（`00048` 与 `00049`），
    // 单看缓存前缀必然是两个键。
    let main = key(
        r#"{"system":[{"type":"text","text":"S"}],"tools":[{"name":"Bash"}],"messages":[{"role":"user","content":"hi"}]}"#,
    );
    let sub = key(
        r#"{"system":[{"type":"text","text":"SUB"}],"tools":[{"name":"Read"}],"messages":[{"role":"user","content":"find it"}]}"#,
    );
    assert_ne!(main, sub, "两条支线的缓存前缀本来就不同");
    // 来访带了会话 id 就一律按它——两条支线并回一条会话、占一份名额。
    let sid = "7fe47444-c834-44e0-b568-d61e07daa35e";
    assert_eq!(
        session_binding_key(Some(sid), &main),
        session_binding_key(Some(sid), &sub),
        "同一条来访会话里的主线程与子代理是一条会话"
    );
    assert_eq!(session_binding_key(Some(sid), &main), format!("lb:v2:sid:{sid}"));
    // 没带会话 id 时只能按前缀分，子代理各算各的：`pfx` 分支已知的近似。
    assert_eq!(session_binding_key(None, &main), format!("lb:v2:pfx:{main}"));
    assert_ne!(session_binding_key(None, &main), session_binding_key(None, &sub));
    // 两种来源不会撞：uuid 去掉横线也是 32 个 hex，没有来源段就分不开——后台原先正是
    // 靠「是不是 32 个 hex」猜的。
    let flat = sid.replace('-', "");
    assert_ne!(session_binding_key(Some(&flat), &main), session_binding_key(None, &flat));
    // 两种来源都带版本前缀：库里的旧行正是靠它被认出来清掉的。
    for k in [session_binding_key(Some(sid), &main), session_binding_key(None, &main)] {
        assert!(k.starts_with(SESSION_KEY_VERSION), "{k}");
    }
}

/// 模拟路径的会话键 = 缓存前缀 + 对话起点：tools、system、首条用户消息三者相同的请求同一个
/// 键，任一处变了就是另一个；同一对话追加后续消息不变；billing header 那块与 `cache_control`
/// 不算；同一应用（同 tools/system）里首条消息不同的两个对话必须是两个键。
#[test]
fn sim_session_key_follows_the_cache_prefix_and_the_first_user_message() {
    let key = |body: &str| crate::proxy::sim_session_key(&serde_json::from_str(body).unwrap());
    let a = key(
        r#"{"system":[{"type":"text","text":"S"}],"tools":[{"name":"Bash"}],"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(a.len(), 32, "16 字节 hex: {a}");
    assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
    // 同一对话第二轮：首条不动、末尾追加、断点挪到末条——键不变。
    assert_eq!(
        a,
        key(
            r#"{"system":[{"type":"text","text":"S","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"Bash"}],"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]},{"role":"assistant","content":"ok"},{"role":"user","content":[{"type":"text","text":"more","cache_control":{"type":"ephemeral"}}]}]}"#
        ),
        "同一对话追加消息、首条改成块数组、断点挪动，仍是同一条会话"
    );
    assert_eq!(
        a,
        key(
            r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1"},{"type":"text","text":"S"}],"tools":[{"name":"Bash"}],"messages":[{"role":"user","content":"hi"}]}"#
        ),
        "billing header 那块不进缓存键"
    );
    assert_eq!(
        a,
        key(
            r#"{"system":[{"type":"text","text":"S"}],"tools":[{"name":"Bash"}],"messages":[{"role":"system","content":"hoisted"},{"role":"user","content":"hi"}]}"#
        ),
        "夹在前面的 role:system 消息不算对话起点"
    );
    // 同一应用的另一个对话：tools/system 一样、首条用户消息不同，必须是另一个键。
    assert_ne!(
        a,
        key(
            r#"{"system":[{"type":"text","text":"S"}],"tools":[{"name":"Bash"}],"messages":[{"role":"user","content":"bye"}]}"#
        ),
        "首条用户消息变了"
    );
    assert_ne!(
        a,
        key(
            r#"{"system":[{"type":"text","text":"S2"}],"tools":[{"name":"Bash"}],"messages":[{"role":"user","content":"hi"}]}"#
        ),
        "system 变了"
    );
    assert_ne!(
        a,
        key(
            r#"{"system":[{"type":"text","text":"S"}],"tools":[{"name":"Read"}],"messages":[{"role":"user","content":"hi"}]}"#
        ),
        "tools 变了"
    );
    assert_ne!(
        a,
        key(
            r#"{"system":[{"type":"text","text":"S"}],"messages":[{"role":"user","content":"hi"}]}"#
        ),
        "tools 没了"
    );
    assert_eq!(
        a,
        key(
            r#"{"system":"S","tools":[{"name":"Bash"}],"messages":[{"role":"user","content":"hi"}]}"#
        ),
        "字符串 system 与单块等价"
    );
    // 纯聊天：没有 tools 也没有 system，只剩首条消息在分。
    assert_ne!(
        key(r#"{"messages":[{"role":"user","content":"a"}]}"#),
        key(r#"{"messages":[{"role":"user","content":"b"}]}"#)
    );
    assert_eq!(
        key(r#"{"messages":[{"role":"user","content":"a"}]}"#),
        key(r#"{"messages":[{"role":"user","content":"a"},{"role":"assistant","content":"x"}]}"#)
    );
    // 非文本块也算：文本相同、图片不同的两个对话是两个键；同一图片对话追加轮次不变；
    // 块内键序与断点不影响；纯图片（没有 text）的两个对话也分得开。
    let img = |data: &str, extra: &str| {
        format!(
            r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"描述这张图片"}},{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{data}"}}}}]}}{extra}]}}"#
        )
    };
    let cat = key(&img("Y2F0", ""));
    assert_ne!(cat, key(&img("ZG9n", "")), "文本相同、图片不同是两个对话");
    assert_eq!(
        cat,
        key(&img(
            "Y2F0",
            r#",{"role":"assistant","content":"a cat"},{"role":"user","content":"and this?"}"#
        )),
        "同一图片对话追加轮次不变"
    );
    assert_eq!(
        cat,
        key(
            r#"{"messages":[{"role":"user","content":[{"text":"描述这张图片","type":"text","cache_control":{"type":"ephemeral"}},{"source":{"data":"Y2F0","media_type":"image/png","type":"base64"},"type":"image"}]}]}"#
        ),
        "键序与断点不影响"
    );
    let only_img = |data: &str| {
        format!(
            r#"{{"messages":[{{"role":"user","content":[{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{data}"}}}}]}}]}}"#
        )
    };
    assert_ne!(key(&only_img("Y2F0")), key(&only_img("ZG9n")), "纯图片对话也分得开");
}

/// `spoof_device_id` 关掉时只换 account 段，来访自带的 `device_id` 原样保留。
///
/// **判据取自真实抓包对**：`cap/raw/00002`（API-key 模式经 luban）与 `00006`（订阅模式
/// 直连）是同机、同客户端、同模型、相隔 28 秒的两条请求，两者的 `device_id` **完全相同**
/// （`832cb7e6…`），只有 `account_uuid` 不同（空串 ↔ 真 uuid）。故「补 account、留 device」
/// 正是官方两种模式之间真实存在的那一处差别，见 [`store::ForwardFlags::spoof_device_id`]。
#[test]
fn keeps_client_device_id_when_spoof_device_off() {
    const CLIENT_DEVICE: &str = "832cb7e697190bc475b926c7994ef183a0f8a58e29818f182e11f924e1ea2870";
    let off = store::ForwardFlags { spoof_device_id: false, ..all_on() };

    // 格式一：CC 内嵌 JSON（键序与官方一致）。
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"metadata":{{"user_id":"{{\"device_id\":\"{CLIENT_DEVICE}\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}}"}}}}"#
    ));
    let out = rewrite_body(&body, &test_cred(), "fp", off, None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let inner: serde_json::Value =
        serde_json::from_str(v["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(inner["device_id"], CLIENT_DEVICE, "关掉后 device_id 该原样保留");
    assert_eq!(inner["account_uuid"], ACCOUNT_UUID, "account 段照样要补——那才是两模式的差别");
    assert_eq!(inner["session_id"], "ssss", "session 段一如既往不动");

    // 开着时（默认）仍换成派生值：本开关不改变既有行为。
    let on = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
    let v_on: serde_json::Value = serde_json::from_slice(&on).unwrap();
    let inner_on: serde_json::Value =
        serde_json::from_str(v_on["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(inner_on["device_id"], test_cred().spoof_device_id("fp").unwrap());

    // 格式二：扁平串——device 段同样保留，仍以扁平串回写。
    let flat = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"metadata":{"user_id":"user_aa_account_bb_session_cc"}}"#
                .to_string(),
        );
    let out = rewrite_body(&flat, &test_cred(), "fp", off, None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["metadata"]["user_id"], format!("user_aa_account_{ACCOUNT_UUID}_session_cc"));

    // 模拟路径不受本开关影响：那条路来访压根没有 device_id，只能派生——否则产出的是
    // 一份没有 device_id 的 metadata，官方从不发那种形态。
    let bare = Bytes::from(PLAIN_BODY.to_string());
    let sim = detect_for(&bare, off).expect("裸请求仍应走模拟");
    let out = rewrite_body(&bare, &test_cred(), "fp", off, Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let inner: serde_json::Value =
        serde_json::from_str(v["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(
        inner["device_id"],
        test_cred().spoof_device_id("fp").unwrap(),
        "模拟路径必须派生，不受开关影响"
    );
}

/// CC 形态但不带 `metadata.user_id` 的来访（第三方 CC 兼容客户端）：照样补一份官方身份，
/// **且头体两处的 session_id 逐字节相同**。
///
/// 判据逐条取自 `cap/raw/00006`（claude-cli/2.1.220 直连，opus-5）的原始报文：
/// ```text
/// "metadata":{"user_id":"{\"device_id\":\"832cb7…2870\",\"account_uuid\":\"edded6bb-…\",\"session_id\":\"bc201916-d0bc-4b4e-adba-caf41fb58746\"}"}
/// X-Claude-Code-Session-Id: bc201916-d0bc-4b4e-adba-caf41fb58746
/// ```
/// 即：内层是紧凑 JSON 字符串、键序 device_id→account_uuid→session_id、device_id 是
/// 64 位小写 hex、session_id 是 uuid 且与那个头**同值**。`00009`（sonnet-5）同形。
/// 老版本 API-key 模式的 CC：system 是 `[身份句(带断点), 基座, 其余…]`，有身份句、没 billing
/// header（现网 `claude-cli/2.1.238`，req_ujomarOOPtXL38jx 的形态）。补前缀只该插一块
/// billing header，客户端的身份句连同它的 `cache_control` 留在第二块——原先会再插一句
/// 身份句，出站 `[billing, 身份, 身份(带断点), …]`。
#[test]
fn prefix_injection_keeps_the_client_identity_block() {
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}","cache_control":{{"type":"ephemeral"}}}},{},{{"type":"text","text":"tail"}}]}}"#,
        config::CC_SYSTEM_IDENTITY,
        base_block()
    ));
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    assert!(sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"), "{sys:?}");
    assert_eq!(sys[1]["text"], config::CC_SYSTEM_IDENTITY);
    assert_eq!(sys[1]["cache_control"]["type"], "ephemeral", "客户端身份句的断点原样保留");
    let identities = sys
        .iter()
        .filter(|b| {
            b["text"].as_str().is_some_and(|t| t.contains(config::CC_SYSTEM_IDENTITY_PREFIX))
        })
        .count();
    assert_eq!(identities, 1, "身份句只能有一句: {sys:?}");
    assert_eq!(sys.len(), 4, "原 3 块 + billing header");

    // 字符串形态的 system 同理：已含身份句就只在前面加 billing header。
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"system":"{}\n\nrest"}}"#,
        config::CC_SYSTEM_IDENTITY
    ));
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    assert_eq!(sys.len(), 2, "{sys:?}");
    assert!(sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"));
    assert!(sys[1]["text"].as_str().unwrap().starts_with(config::CC_SYSTEM_IDENTITY));
}

/// 封顶必须排在补前缀之后：客户端 5 块、没 billing header 的来访，补前缀后是 6 块，超过
/// [`MAX_SYSTEM_BLOCKS`]。原先封顶在前、补前缀在后，这 6 块就原样出站，上游
/// 按第三方应用计费——现网 2.1.238 那条出站是 7 块。
#[test]
fn system_block_cap_runs_after_prefix_injection() {
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}","cache_control":{{"type":"ephemeral"}}}},{},{{"type":"text","text":"env"}},{{"type":"text","text":"memory"}},{{"type":"text","text":"tail"}}]}}"#,
        config::CC_SYSTEM_IDENTITY,
        base_block()
    ));
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    assert!(
        sys.len() <= crate::proxy::simulation::MAX_SYSTEM_BLOCKS,
        "封顶没管住补前缀后的块数: {}",
        sys.len()
    );
    assert!(sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"));
    assert_eq!(sys[1]["text"], config::CC_SYSTEM_IDENTITY);
    let all: String = sys.iter().filter_map(|b| b["text"].as_str()).collect();
    assert!(all.contains("memory") && all.contains("tail"), "并块不能丢内容: {sys:?}");
}

#[test]
fn cc_shaped_without_metadata_gets_aligned_identity() {
    // CC 形态 + 真 CC 客户端：system 里有那句身份声明且 UA 是 claude-cli，
    // detect 返回 None（不模拟），走 bare_session 路径补 metadata。
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}},{}]}}"#,
        config::CC_SYSTEM_IDENTITY,
        base_block()
    ));
    let mut cc_ua = crate::proxy::HeaderMap::new();
    cc_ua
        .insert(header::USER_AGENT, HeaderValue::from_static("claude-cli/2.1.226 (external, cli)"));
    assert!(detect_with(&body, &cc_ua, all_on()).is_none(), "真 CC 客户端不该走模拟");
    assert!(
        !crate::proxy::body_has_user_id(parsed(&body).as_ref()),
        "这条来访本来就没有 metadata.user_id"
    );

    // 来访没带会话 id 头 → 派生一个，头体同步补。
    let client = crate::proxy::HeaderMap::new();
    let sid = crate::proxy::bare_session_id(
        &client,
        all_on(),
        None,
        true,
        crate::proxy::body_has_user_id(parsed(&body).as_ref()),
        &test_cred(),
        "fp",
    )
    .expect("CC 形态 + 无 metadata 应补身份");
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, Some(sid.as_str()));
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let user_id = v["metadata"]["user_id"].as_str().expect("应补出 metadata.user_id");
    let inner: serde_json::Value = serde_json::from_str(user_id).unwrap();

    assert_eq!(
        inner.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["device_id", "account_uuid", "session_id"],
        "键序应与 00006 一致: {user_id}"
    );
    assert!(!user_id.contains(": "), "内层须是紧凑 JSON（无空白），同 00006");
    let device_id = inner["device_id"].as_str().unwrap();
    assert_eq!(device_id.len(), 64, "device_id 同 00006 是 64 位 hex");
    assert!(
        device_id.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "device_id 须是小写 hex: {device_id}"
    );
    let session_id = inner["session_id"].as_str().unwrap();
    let seg: Vec<usize> = session_id.split('-').map(str::len).collect();
    assert_eq!(seg, vec![8, 4, 4, 4, 12], "session_id 同 00006 是 uuid 形态: {session_id}");

    // 头体同值——00006 里这两处逐字节相同，这正是本条路径最容易做错的地方。
    let headers = build_forward_headers(&client, "sk-ant-oat01-REAL", all_on(), None, Some(&sid));
    assert_eq!(
        headers.get("x-claude-code-session-id").unwrap().to_str().unwrap(),
        session_id,
        "头与 metadata 里的 session_id 必须逐字节相同"
    );

    // 来访自己带了那个头 → 沿用它（按账号钉住，[`crate::proxy::account_session_id`]），不按设备
    // 另派生；头体落的是同一个值。
    const CLIENT_SID: &str = "bc201916-d0bc-4b4e-adba-caf41fb58746";
    let mut with_sid = crate::proxy::HeaderMap::new();
    with_sid.insert(
        crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
        HeaderValue::from_static(CLIENT_SID),
    );
    let sid2 =
        crate::proxy::bare_session_id(&with_sid, all_on(), None, true, false, &test_cred(), "fp")
            .unwrap();
    assert_eq!(
        sid2,
        crate::proxy::account_session_id(&test_cred(), CLIENT_SID).unwrap(),
        "应沿用来访自己的会话 id（按账号钉住）"
    );
    assert_ne!(sid2, sid, "来访带了会话 id 就不按设备派生");
    let out2 = rewrite_body(&body, &test_cred(), "fp", all_on(), None, Some(sid2.as_str()));
    let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
    let inner2: serde_json::Value =
        serde_json::from_str(v2["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(inner2["session_id"], sid2, "体里要用同一个值");
    let headers2 = build_forward_headers(&with_sid, "tok", all_on(), None, Some(&sid2));
    assert_eq!(
        headers2.get("x-claude-code-session-id").unwrap().to_str().unwrap(),
        sid2,
        "头上那个来访原值要被钉住后的值顶掉，与体同值"
    );
}

/// 补出来的 metadata 必须与官方报文**逐字节同形**（只有取值不同）。
///
/// 金标准逐字取自 `cap/raw/00006_101505.964.req.raw`（claude-cli/2.1.220 直连 opus-5）
/// 的请求体原文，位置在 `tools` 之后、`max_tokens` 之前：
/// ```text
/// …,"metadata":{"user_id":"{\"device_id\":\"832cb7e6…2870\",\"account_uuid\":\"edded6bb-2521-4a68-94cb-241bb4d96bb9\",\"session_id\":\"bc201916-d0bc-4b4e-adba-caf41fb58746\"}"},"max_tokens":64000,…
/// ```
/// 抓包不入库（`cap/` 未跟踪），故把这串固化在这里——与基座字节数、beta 串同一做法。
/// 逐字节比对是为了钉住**转义写法**：内层是「字符串里的 JSON」，序列化器只要把
/// `\"` 写成别的形式（或插进任何空白），出去的就不是官方那串了。
#[test]
fn injected_metadata_matches_raw_capture_bytes() {
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}}],"max_tokens":64000}}"#,
        config::CC_SYSTEM_IDENTITY
    ));
    let sid = "bc201916-d0bc-4b4e-adba-caf41fb58746";
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, Some(sid));
    let text = String::from_utf8(out.to_vec()).unwrap();

    let expected = format!(
        r#""metadata":{{"user_id":"{{\"device_id\":\"{}\",\"account_uuid\":\"{ACCOUNT_UUID}\",\"session_id\":\"{sid}\"}}"}}"#,
        test_cred().spoof_device_id("fp").unwrap()
    );
    assert!(text.contains(&expected), "与 00006 的 metadata 形态不符\n实际: {text}");
    // 位置也照抓包：metadata 在 max_tokens 之前。
    assert!(
        text.find(r#""metadata""#) < text.find(r#""max_tokens""#),
        "metadata 应落在 max_tokens 之前（同 00006 的 key 序）: {text}"
    );
}

/// 裸客户端的日志设备标识：只在真伪装过时才有值，且带 `sim:` 前缀以免被当成真实设备。
#[test]
fn logs_simulated_device_only_when_spoofed() {
    let sim = sim_for(PLAIN_BODY);
    let expect = format!("sim:{}", test_cred().spoof_device_id("fp").unwrap());
    let id = crate::proxy::sim_device_id(Some(&sim), None, all_on(), &test_cred(), "fp").unwrap();
    assert_eq!(id, expect);

    // CC 形态补身份那条路（sim 为 None、bare_session 有值）同样把这个 id 发了出去，
    // 日志要记它——否则这段流量在库里只留下 `-`，无从聚合。
    let bare =
        crate::proxy::sim_device_id(None, Some("sess"), all_on(), &test_cred(), "fp").unwrap();
    assert_eq!(bare, expect, "两条补身份的路径记的是同一个 id");

    // 两条路都没走（来访是 CC 形态且自带 metadata）→ 出站体里根本没有这个 id，不该记。
    assert!(crate::proxy::sim_device_id(None, None, all_on(), &test_cred(), "fp").is_none());
    // spoof_identity 关着时同理：ensure_cc_metadata 不会写 metadata。
    let no_spoof = store::ForwardFlags { spoof_identity: false, ..all_on() };
    assert!(crate::proxy::sim_device_id(Some(&sim), None, no_spoof, &test_cred(), "fp").is_none());
    // 凭证没有 account_uuid 就派生不出来，退回 `-`。
    let no_uuid = crate::credentials::Credential { account_uuid: None, ..test_cred() };
    assert!(crate::proxy::sim_device_id(Some(&sim), None, all_on(), &no_uuid, "fp").is_none());
}

/// 日志用的 UA 取值：缺失/空串取 `-`，过长按 char 截断（不能按字节切，会劈开多字节 UTF-8）。
#[test]
fn client_ua_falls_back_and_truncates() {
    let ua = |v: Option<&str>| {
        let mut h = crate::proxy::HeaderMap::new();
        if let Some(v) = v {
            h.insert(crate::proxy::header::USER_AGENT, HeaderValue::from_str(v).unwrap());
        }
        crate::proxy::ua_of(&h)
    };
    assert_eq!(ua(None), "-", "没有该头");
    assert_eq!(ua(Some("   ")), "-", "空白等于没带");
    assert_eq!(ua(Some(config::CC_USER_AGENT)), config::CC_USER_AGENT, "正常那串原样保留");
    let long = "a".repeat(300);
    assert_eq!(ua(Some(&long)).len(), 120, "超长的截到 120");
    // 非 ASCII 的头值 `to_str()` 直接失败，落回 `-`——所以库里存的 UA 恒为可见 ASCII。
    let cjk = crate::proxy::HeaderValue::from_bytes("中文客户端".as_bytes()).unwrap();
    let mut h = crate::proxy::HeaderMap::new();
    h.insert(crate::proxy::header::USER_AGENT, cjk);
    assert_eq!(crate::proxy::ua_of(&h), "-");
}

/// 版本串解析：段数不齐按 0 补齐，预发布后缀按主版本算，非数字段作废。
#[test]
fn parses_version_strings() {
    let v = crate::proxy::parse_version;
    assert_eq!(v("2.1.220"), Some((2, 1, 220)));
    assert_eq!(v("2.1"), Some((2, 1, 0)), "缺的段补 0");
    assert_eq!(v("2"), Some((2, 0, 0)));
    assert_eq!(v(" 2.1.220 "), Some((2, 1, 220)), "首尾空白不算数");
    assert_eq!(v("2.1.220-beta.1"), Some((2, 1, 220)), "预发布按主版本算，不判成更旧");
    assert_eq!(v("1.2.3.4"), Some((1, 2, 3)), "第四段忽略");
    assert_eq!(v(""), None);
    assert_eq!(v("v2.1.220"), None, "带前缀的不猜，交给调用方按「读不出」放行");
    assert_eq!(v("2.x.1"), None, "写了但不是数字的段整串作废");
    // 数值比较，不是字典序：字符串比的话 "2.1.9" 会大于 "2.1.220"。
    assert!(v("2.1.9") < v("2.1.220"));
}

/// UA 里的 CC 版本：认 `claude-cli/<版本>`，后面跟什么都不影响；别的客户端读不出版本。
#[test]
fn reads_the_cc_version_from_the_user_agent() {
    let v = crate::proxy::cc_cli_version;
    assert_eq!(v(config::CC_USER_AGENT), Some((2, 1, 293)), "官方那串");
    assert_eq!(v("claude-cli/2.1.251"), Some((2, 1, 251)), "光秃秃一串也认");
    assert_eq!(v("claude-cli/1.0 (external, cli)"), Some((1, 0, 0)));
    assert_eq!(v("python-httpx/0.27.0"), None, "非 CC 客户端没有版本可比");
    assert_eq!(v("claude-cli/"), None, "有前缀没版本");
    assert_eq!(v("claude-cli/next (external, cli)"), None, "版本位不是数字");
}

/// UA 里的 entrypoint：括号里第二段，与官方 billing header 的 `cc_entrypoint` 同源。
#[test]
fn reads_the_cc_entrypoint_from_the_user_agent() {
    let e = crate::proxy::cc_ua_entrypoint;
    assert_eq!(e(config::CC_USER_AGENT), Some("cli"));
    assert_eq!(e("claude-cli/2.1.291 (external, sdk-cli)"), Some("sdk-cli"));
    assert_eq!(
        e("claude-cli/2.1.273 (external, claude-vscode, agent-sdk/0.3.273)"),
        Some("claude-vscode"),
        "第三段起不算"
    );
    assert_eq!(e("claude-cli/2.1.251"), None, "没有括号");
    assert_eq!(e("claude-cli/2.1.251 (external)"), None, "只有一段");
    assert_eq!(e("claude-cli/2.1.251 (external, Bad;Value)"), None, "不规矩的段不收");
    assert_eq!(e("python-httpx/0.27.0 (external, cli)"), None, "非 CC 客户端");
}

/// 自报版本高于官方最新发布版的 UA 不算官方客户端；等于或更低的照认。
#[test]
fn a_cc_version_newer_than_the_latest_release_is_not_trusted() {
    let t = |ua: &str| crate::proxy::trusted_cc_version_against(ua, (2, 1, 260));
    assert_eq!(t("claude-cli/2.1.260 (external, cli)"), Some((2, 1, 260)), "正好最新版");
    assert_eq!(t("claude-cli/2.1.226 (external, cli)"), Some((2, 1, 226)), "旧版照认");
    assert_eq!(t("claude-cli/2.5.0 (external, cli)"), None, "官方没发过 2.5.0");
    assert_eq!(t("claude-cli/2.1.261 (external, cli)"), None, "哪怕只高一个补丁号");
    assert_eq!(t("claude-cli/3.0.0 (external, cli)"), None);
    assert_eq!(t("python-httpx/0.27.0"), None, "非 CC 客户端本来就读不出");
}

/// 没学到 `latest` 时上限退回 [`config::CC_LATEST_KNOWN_RELEASE`]，且学到的值不会把上限
/// 拉到它之下。模拟版本 [`config::CC_VERSION_BASE`] 更旧，自然也在上限之内。
#[test]
fn known_latest_release_is_at_least_the_baked_in_version() {
    let v = |s: &str| crate::proxy::parse_version(s).unwrap();
    let latest = crate::proxy::known_latest_release();
    assert!(latest >= v(config::CC_LATEST_KNOWN_RELEASE));
    assert!(latest >= v(config::CC_VERSION_BASE));
    // 抓包证实的 2.1.270 来访：没学到 `latest` 也得认，不能落成「读不出版本」。
    assert_eq!(
        crate::proxy::trusted_cc_version("claude-cli/2.1.270 (external, cli)"),
        Some((2, 1, 270))
    );
}

/// 最低版本闸的三态：低于门槛才拒，等于/高于放行；闸没配、UA 不是 CC、版本读不出来
/// 全都放行——这道闸只用来逼旧版 CC 升级，不该把别的客户端一起挡在门外。
#[test]
fn rejects_only_cc_clients_below_the_minimum() {
    let gate = |ua: &str, min: Option<&str>| crate::proxy::below_min_client_version(ua, min);
    let old = "claude-cli/2.0.30 (external, cli)";

    let (got, want) = gate(old, Some("2.1.220")).expect("旧版该被拦下");
    assert_eq!(got, "2.0.30", "拦下时要报出自报版本，日志与提示都靠它");
    assert_eq!(want, "2.1.220", "提示里给的是配置原样，不是解析后的三元组");

    assert!(gate(config::CC_USER_AGENT, Some("2.1.220")).is_none(), "正好等于门槛要放行");
    assert!(gate("claude-cli/3.0.0 (external, cli)", Some("2.1.220")).is_none(), "更新的放行");
    assert!(gate(old, Some("2.1")).is_some(), "门槛写两段即 2.1.0，2.0.30 更旧——照拦");
    assert!(gate(old, None).is_none(), "闸没配");
    assert!(gate(old, Some("   ")).is_none(), "空串等于没配");
    assert!(gate(old, Some("最新版")).is_none(), "门槛不是版本号 → 当没配，不能全拒");
    assert!(gate("python-httpx/0.27.0", Some("2.1.220")).is_none(), "非 CC 客户端不受这道闸管");
    assert!(gate("-", Some("2.1.220")).is_none(), "没带 UA 的（ua_of 落 `-`）照旧放行");
}

/// 上一条里「2.1 门槛拦下 2.0.30」的反面：同一个门槛不能把 2.1.0 之后的版本也拦了。
#[test]
fn a_two_segment_minimum_means_dot_zero() {
    assert!(crate::proxy::below_min_client_version("claude-cli/2.1.0", Some("2.1")).is_none());
    assert!(crate::proxy::below_min_client_version("claude-cli/2.1.220", Some("2.1")).is_none());
    assert!(crate::proxy::below_min_client_version("claude-cli/2.0.999", Some("2.1")).is_some());
}

/// 出站 URL 上那个 `?beta=true`：官方 `cap/raw` 八份抓包的请求行全带，Anthropic 公开 API
/// 里却没有这个参数——它是 CC 客户端自己的标记，故只在模拟路径上补。
#[test]
fn appends_official_beta_query() {
    let base = "https://api.anthropic.com/v1/messages";
    assert_eq!(
        crate::proxy::ensure_beta_query(base),
        format!("{base}?beta=true"),
        "没有查询串就加 ?"
    );
    assert_eq!(
        crate::proxy::ensure_beta_query(&format!("{base}?foo=1")),
        format!("{base}?foo=1&beta=true"),
        "已有查询串就接 &"
    );

    // 客户端自己写了 beta= 的一律不动——包括它显式关掉的情形。
    for already in ["?beta=true", "?beta=false", "?foo=1&beta=true", "?beta=true&foo=1"] {
        let url = format!("{base}{already}");
        assert_eq!(crate::proxy::ensure_beta_query(&url), url, "客户端自己的 beta= 被改写了");
    }
    // `betas=`/`xbeta=` 不是 `beta=`，不该被当成已有。
    assert!(crate::proxy::ensure_beta_query(&format!("{base}?betas=1")).ends_with("&beta=true"));
    assert!(crate::proxy::ensure_beta_query(&format!("{base}?xbeta=1")).ends_with("&beta=true"));
}

// ---------- 非流式改流式 + SSE 聚合 ----------

/// `stream` 的判定口径：只有布尔 `true` 算流式。字符串 `"true"`、数字、缺失都不是——
/// 上游那边它们同样回整段 JSON，判据跟着响应形态走才不会错配。
#[test]
fn stream_requested_only_counts_boolean_true() {
    let case = |body: &str| crate::proxy::stream_requested(&serde_json::from_str(body).unwrap());
    assert!(case(r#"{"stream":true}"#));
    assert!(!case(r#"{"stream":false}"#));
    assert!(!case(r#"{"model":"claude-opus-5"}"#), "字段缺失 = 非流式");
    assert!(!case(r#"{"stream":"true"}"#), "字符串不算");
    assert!(!case(r#"{"stream":1}"#), "数字不算");
}

/// 流式化把 `stream` 置成 `true`，且落在官方 key 序该在的位置（队尾）：
/// 来访带了就原位改值，没带就追加——两条路都与官方线序一致。
#[test]
fn forces_stream_true_and_keeps_key_order() {
    let keys = |bytes: &Bytes| {
        let v: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        v.as_object().unwrap().keys().cloned().collect::<Vec<_>>()
    };
    // 只开流式化这一项，确保观察到的差异只来自它。
    let only_stream = store::ForwardFlags {
        simulate_cc: false,
        simulate_full_system: false,
        fill_absent_tools: false,
        spoof_identity: false,
        system_shape: false,
        billing_cch: false,
        cch_real_recompute: false,
        cch_sim_compute: false,
        ..all_on()
    };
    let call = |body: &str| {
        crate::proxy::rewrite_body_out(
            &Bytes::from(body.to_string()),
            &test_cred(),
            "fp",
            only_stream,
            None,
            None,
            None,
            true,
            None,
            true,
            true,
            None,
            None,
            crate::proxy::CcRequestKind::Main,
            None,
        )
        .0
    };

    // 1) 来访压根没带 `stream`：追加到末尾（官方线序里它就是最后一个）。
    let out = call(r#"{"model":"claude-opus-5","messages":[],"max_tokens":64}"#);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["stream"], serde_json::json!(true));
    assert_eq!(keys(&out), vec!["model", "messages", "max_tokens", "stream"]);

    // 2) 来访带了 `stream:false`：原位改值，位置不动。
    let out = call(r#"{"model":"claude-opus-5","stream":false,"max_tokens":64}"#);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["stream"], serde_json::json!(true));
    assert_eq!(keys(&out), vec!["model", "stream", "max_tokens"], "已有字段不该挪位置");

    // 3) 开关关着：一个字节都不动（哪怕 body 是非流式的）。
    let untouched = crate::proxy::rewrite_body_out(
        &Bytes::from(r#"{"model":"claude-opus-5","stream":false}"#.to_string()),
        &test_cred(),
        "fp",
        only_stream,
        None,
        None,
        None,
        false,
        None,
        true,
        true,
        None,
        None,
        crate::proxy::CcRequestKind::Main,
        None,
    )
    .0;
    assert_eq!(untouched, Bytes::from(r#"{"model":"claude-opus-5","stream":false}"#.to_string()));
}

/// message thread 测试的一轮：按模拟路径改写，返回出站体与等回程提交的那份。
fn thread_turn(
    body: &serde_json::Value,
    flags: store::ForwardFlags,
) -> (serde_json::Value, Option<crate::proxy::session_link::ThreadPending>) {
    let raw = body.to_string();
    let sim = sim_for(&raw);
    let out = rewrite_body(&Bytes::from(raw), &test_cred(), "fp", flags, Some(&sim), None);
    (serde_json::from_slice(&out).unwrap(), sim.take_thread())
}

/// 上游回了 `content` 这样一条回复时的回复指纹：来访原样带回它就对得上（两边同一算法，
/// 回程那侧见 `logging` 里嗅探器的测试）。
fn reply_of(content: serde_json::Value) -> crate::proxy::session_link::ReplyFp {
    super::thread_msg_of(&serde_json::json!({ "role": "assistant", "content": content }))
        .reply
        .as_upstream()
}

fn bash_use(id: &str, command: &str) -> serde_json::Value {
    serde_json::json!({ "type": "tool_use", "id": id, "name": "Bash", "input": { "command": command } })
}

/// 同一会话槽里两段开场相同的对话（同样的首句）：纯文本回复两边 tool_use id 都是空的，只比
/// id 会让第一段接上第二段的回复。正文指纹对不上就只能 `create`。
#[test]
fn sim_threads_do_not_continue_onto_another_conversations_text_reply() {
    use crate::proxy::session_link::CcSessionLink;
    let user1 = serde_json::json!({ "role": "user", "content": "线程测试·串线 hi" });
    let first = thread_body("claude-opus-5-5", serde_json::json!([user1]));
    let (_, pa) = thread_turn(&first, all_on());
    CcSessionLink::record_thread(
        &pa.unwrap(),
        "msg_r1",
        Vec::new(),
        reply_of("你好，A".into()),
        100,
    );
    // 第二段对话：同一个开场，create 提交后替掉了第一段那条状态。
    let (_, pb) = thread_turn(&first, all_on());
    CcSessionLink::record_thread(
        &pb.unwrap(),
        "msg_r2",
        Vec::new(),
        reply_of("你好，B".into()),
        100,
    );

    let follow = |reply: &str| {
        thread_body(
            "claude-opus-5-5",
            serde_json::json!([
                user1,
                { "role": "assistant", "content": reply },
                { "role": "user", "content": "继续" },
            ]),
        )
    };
    let (v, _) = thread_turn(&follow("你好，A"), all_on());
    assert_eq!(v["thread"], serde_json::json!({ "type": "create" }), "第一段不能接 r2: {v}");
    let (v, _) = thread_turn(&follow("你好，B"), all_on());
    assert_eq!(v["thread"]["previous_message_id"], "msg_r2", "第二段照常接上: {v}");
}

/// 客户端把上一条回复改了再发下一条：正文 A 改成 B、工具入参改了（id 不变）、thinking 改了，
/// 都得 `create`——`continue` 会让上游沿用存档里的 A，改动被静默丢掉。原样带回照常
/// `continue`；把 thinking 整块丢掉也算原样（上游线程里那份本来就在）。
#[test]
fn sim_threads_create_when_the_client_edited_the_last_reply() {
    use crate::proxy::session_link::CcSessionLink;
    let user1 = serde_json::json!({ "role": "user", "content": "线程测试·改回复 hi" });
    let reply = |thinking: Option<&str>, text: &str, command: &str| {
        let mut blocks = Vec::new();
        if let Some(t) = thinking {
            blocks.push(serde_json::json!({ "type": "thinking", "thinking": t, "signature": "s" }));
        }
        blocks.push(serde_json::json!({ "type": "text", "text": text }));
        blocks.push(bash_use("toolu_e", command));
        serde_json::Value::Array(blocks)
    };
    let (_, p) = thread_turn(&thread_body("claude-opus-5-5", serde_json::json!([user1])), all_on());
    CcSessionLink::record_thread(
        &p.unwrap(),
        "msg_E",
        vec!["toolu_e".into()],
        reply_of(reply(Some("先看"), "我看一下", "ls")),
        100,
    );
    let follow = |content: serde_json::Value| {
        thread_body(
            "claude-opus-5-5",
            serde_json::json!([
                user1,
                { "role": "assistant", "content": content },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_e", "content": "a.txt" },
                ] },
            ]),
        )
    };
    let create = serde_json::json!({ "type": "create" });
    for (label, content) in [
        ("改了正文", reply(Some("先看"), "我改过了", "ls")),
        ("改了工具入参", reply(Some("先看"), "我看一下", "pwd")),
        ("改了 thinking", reply(Some("改过"), "我看一下", "ls")),
    ] {
        let (v, _) = thread_turn(&follow(content), all_on());
        assert_eq!(v["thread"], create, "{label}: {v}");
    }
    for (label, content) in [
        ("原样带回", reply(Some("先看"), "我看一下", "ls")),
        ("丢掉 thinking", reply(None, "我看一下", "ls")),
    ] {
        let (v, _) = thread_turn(&follow(content), all_on());
        assert_eq!(v["thread"]["previous_message_id"], "msg_E", "{label}: {v}");
    }
}

/// 来访自己在工具定义和历史消息上标了断点：前面几步按 4 个分预算，带 `thread` 时上游只收 3 个
/// （`Found 4` 那条 400）。先摘 tools 上的、再摘历史消息上的，system 两个与末条那个留着。
#[test]
fn sim_threads_cap_breakpoints_at_three() {
    let cc = serde_json::json!({ "type": "ephemeral" });
    let mut body = thread_body(
        "claude-opus-5-5",
        serde_json::json!([
            { "role": "user", "content": [{ "type": "text", "text": "线程测试·断点 第一问", "cache_control": cc }] },
            { "role": "assistant", "content": [{ "type": "text", "text": "好" }] },
            { "role": "user", "content": [{ "type": "text", "text": "第二问", "cache_control": cc }] },
        ]),
    );
    body["tools"] = serde_json::json!([{
        "name": "lookup",
        "description": "d",
        "input_schema": { "type": "object" },
        "cache_control": cc,
    }]);
    let (v, _) = thread_turn(&body, all_on());
    assert_eq!(v["thread"], serde_json::json!({ "type": "create" }), "{v}");
    assert_eq!(crate::proxy::count_cache_control(&v), 3, "{v}");
    assert!(
        v["tools"].as_array().unwrap().iter().all(|t| t.get("cache_control").is_none()),
        "tools 上的先摘: {v}"
    );
    let msgs = v["messages"].as_array().unwrap();
    assert!(msgs.last().unwrap()["content"][0].get("cache_control").is_some(), "末条那个留着: {v}");

    // 来访只带自己的 system、一个断点都没标：基座、其余、第五块、末条正好 4 个，摘第五块那个。
    let mut plain = thread_body(
        "claude-opus-5-5",
        serde_json::json!([{ "role": "user", "content": "线程测试·断点 只带 system" }]),
    );
    plain["system"] = "你是助手".into();
    let (v, _) = thread_turn(&plain, all_on());
    assert_eq!(v["thread"], serde_json::json!({ "type": "create" }), "{v}");
    assert_eq!(crate::proxy::count_cache_control(&v), 3, "{v}");
    let sys = v["system"].as_array().unwrap();
    assert_eq!(sys.len(), 5, "{v}");
    assert!(sys[2].get("cache_control").is_some(), "基座留着: {v}");
    assert!(sys[3].get("cache_control").is_some(), "其余留着: {v}");
    assert!(sys[4].get("cache_control").is_none(), "第五块的摘掉: {v}");
}

/// 工具 schema 里叫 `cache_control` 的参数、`tool_use.input` 里的同名字段都是业务数据：既不算
/// 断点，也不能被摘。按递归计数会把参数定义当成第 4 个断点摘掉，schema 只剩
/// `required:["cache_control"]` 加 `additionalProperties:false`，再也满足不了。
#[test]
fn sim_threads_cap_leaves_business_cache_control_fields_alone() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "cache_control": { "type": "string" } },
        "required": ["cache_control"],
        "additionalProperties": false,
    });
    let mut body = thread_body(
        "claude-opus-5-5",
        serde_json::json!([
            { "role": "user", "content": "线程测试·断点 业务字段" },
            { "role": "assistant", "content": [{
                "type": "tool_use", "id": "toolu_cc", "name": "set_cache",
                "input": { "cache_control": "no-store" },
            }] },
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_cc", "content": "ok" },
            ] },
        ]),
    );
    body["tools"] = serde_json::json!([{
        "name": "set_cache",
        "description": "d",
        "input_schema": schema,
    }]);
    let (v, _) = thread_turn(&body, all_on());
    assert_eq!(v["thread"], serde_json::json!({ "type": "create" }), "{v}");
    let tool = v["tools"].as_array().unwrap().iter().find(|t| t["name"] == "set_cache");
    assert_eq!(tool.unwrap()["input_schema"], schema, "schema 原样: {v}");
    // `messages[1]` 是首轮补的环境说明，assistant 那条在它之后。
    assert_eq!(v["messages"][2]["content"][0]["input"]["cache_control"], "no-store", "{v}");
    let sys = v["system"].as_array().unwrap();
    assert!(sys.iter().filter(|b| b.get("cache_control").is_some()).count() == 2, "{v}");
    // 末条（`<total_tokens>` 提醒）照常带断点：业务字段不占预算，消息断点不该被跳过。
    let last = v["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "system", "{v}");
    assert!(last["content"][0].get("cache_control").is_some(), "末条该补断点: {v}");
    assert_eq!(crate::proxy::count_cache_control(&v), 3, "{v}");

    // 直接裁：3 个真断点 + 2 处业务字段，一个都不摘。
    let cc = serde_json::json!({ "type": "ephemeral" });
    let mut w = serde_json::json!({
        "tools": [{ "name": "t", "input_schema": {
            "type": "object", "properties": { "cache_control": { "type": "string" } },
        } }],
        "system": [{ "type": "text", "text": "a", "cache_control": cc }],
        "messages": [
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": "x", "name": "t", "input": { "cache_control": cc } },
            ] },
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "x", "content": [
                    { "type": "text", "text": "r", "cache_control": cc },
                ] },
                { "type": "text", "text": "y", "cache_control": cc },
            ] },
        ],
    });
    let before = w.clone();
    assert_eq!(super::cap_thread_breakpoints(&mut w), 0);
    assert_eq!(w, before);
}

/// 只摘超出的那几个；一个都不超时原样。顺序：tools → 顶层 → 非末尾消息 → system（从后往前）→ 末条。
#[test]
fn cap_thread_breakpoints_strips_in_order() {
    let cc = serde_json::json!({ "type": "ephemeral" });
    let mut v = serde_json::json!({
        "cache_control": cc,
        "system": [{ "type": "text", "text": "a", "cache_control": cc }],
        "messages": [
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "t", "content": [
                    { "type": "text", "text": "x", "cache_control": cc },
                ] },
            ] },
            { "role": "user", "content": [{ "type": "text", "text": "y", "cache_control": cc }] },
        ],
    });
    let untouched = v.clone();
    let mut three = v.clone();
    three.as_object_mut().unwrap().shift_remove("cache_control");
    assert_eq!(super::cap_thread_breakpoints(&mut three), 0);

    assert_eq!(super::cap_thread_breakpoints(&mut v), 1);
    assert!(v.get("cache_control").is_none(), "顶层先摘: {v}");
    assert_eq!(v["system"], untouched["system"]);
    assert_eq!(v["messages"], untouched["messages"]);

    // 再多两个：历史消息上的（含嵌套在 tool_result 里的）先走，末条留着。
    v["messages"][0]["content"][0]["cache_control"] = cc.clone();
    v["messages"][1]["content"]
        .as_array_mut()
        .unwrap()
        .insert(0, serde_json::json!({ "type": "text", "text": "z", "cache_control": cc }));
    assert_eq!(crate::proxy::count_cache_control(&v), 5);
    assert_eq!(super::cap_thread_breakpoints(&mut v), 2);
    assert!(v["messages"][0]["content"][0].get("cache_control").is_none(), "{v}");
    assert!(v["messages"][0]["content"][0]["content"][0].get("cache_control").is_none(), "{v}");
    assert!(v["messages"][1]["content"][0].get("cache_control").is_some(), "{v}");
    assert!(v["messages"][1]["content"][1].get("cache_control").is_some(), "末条留着: {v}");
    assert_eq!(v["system"], untouched["system"]);
    v["messages"][1]["content"][0].as_object_mut().unwrap().shift_remove("cache_control");

    // 历史上没得摘了才动 system，从后往前；末条始终留着。
    v["system"] = serde_json::json!([
        { "type": "text", "text": "a", "cache_control": cc },
        { "type": "text", "text": "b", "cache_control": cc },
        { "type": "text", "text": "c", "cache_control": cc },
    ]);
    assert_eq!(super::cap_thread_breakpoints(&mut v), 1);
    assert!(v["system"][1].get("cache_control").is_some(), "{v}");
    assert!(v["system"][2].get("cache_control").is_none(), "system 从后往前摘: {v}");
    assert!(v["messages"][1]["content"][1].get("cache_control").is_some(), "末条留着: {v}");
}

fn thread_body(model: &str, messages: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "model": model, "messages": messages, "max_tokens": 32000 })
}

/// 官方 2.1.285 同一会话（`cap/auto-2.1.285-20260930/00032` → `00033` → `00036`）：首条
/// `create` 带完整上下文；接得上的续轮 `continue`，只发新增消息，`system` 只剩 billing 一块、
/// 不带 `tools`，`thread` 与 `diagnostics` 同指上一条回复；键序同 `00244`。
#[test]
fn sim_threads_create_then_continue_with_only_new_messages() {
    use crate::proxy::session_link::CcSessionLink;
    let user1 = serde_json::json!({ "role": "user", "content": "线程测试·续轮 第一问" });
    let (v1, p1) =
        thread_turn(&thread_body("claude-opus-5-5", serde_json::json!([user1])), all_on());
    assert_eq!(v1["thread"], serde_json::json!({ "type": "create" }), "{v1}");
    assert!(v1["tools"].as_array().is_some_and(|t| !t.is_empty()));
    let msgs1 = v1["messages"].as_array().unwrap();
    assert_eq!(msgs1.len(), 2, "首轮只有用户那句与环境说明，不另追加提醒: {v1}");
    let tail = msgs1.last().unwrap();
    assert_eq!(tail["role"], "system");
    let block = &tail["content"][0];
    let text = block["text"].as_str().unwrap();
    assert!(text.starts_with("# Environment\n"), "{text}");
    assert!(
        text.contains("<total_tokens>15000000 tokens left</total_tokens>\n\nToday's date is "),
        "首轮的 1500 万整提醒并在环境说明里（00340）: {text}"
    );
    assert_eq!(
        block["cache_control"],
        serde_json::json!({ "type": "ephemeral", "ttl": "1h" }),
        "断点落在环境说明上"
    );
    let user_tail = &v1["messages"][0]["content"];
    assert!(
        user_tail.as_array().is_some_and(|b| b.iter().all(|x| x.get("cache_control").is_none())),
        "原末条的断点摘掉了: {v1}"
    );
    // 首轮回复 usage 共 40232（00032 那条），下一轮倒数从这里算。
    CcSessionLink::record_thread(
        &p1.expect("create 也要等回程提交"),
        "msg_A",
        vec!["toolu_1".into()],
        reply_of(serde_json::json!([
            { "type": "text", "text": "我看一下" },
            bash_use("toolu_1", "ls"),
        ])),
        40232,
    );

    let assistant = serde_json::json!({ "role": "assistant", "content": [
            { "type": "text", "text": "我看一下" },
            { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls" } },
        ] });
    let result = serde_json::json!({ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt" },
        ] });
    let msgs2 = serde_json::json!([user1, assistant, result]);
    let (v2, p2) = thread_turn(&thread_body("claude-opus-5-5", msgs2.clone()), all_on());
    assert_eq!(
        v2["thread"],
        serde_json::json!({ "type": "continue", "previous_message_id": "msg_A" }),
        "{v2}"
    );
    assert_eq!(v2["diagnostics"], serde_json::json!({ "previous_message_id": "msg_A" }));
    let delta = v2["messages"].as_array().unwrap();
    assert_eq!(delta.len(), 2, "只发新增的那条加提醒: {v2}");
    assert_eq!(delta[0]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(
        delta[1]["content"][0]["text"], "<total_tokens>14959768 tokens left</total_tokens>",
        "对话首轮锚点是 0，工具续轮减去整段上下文（同 00276 的 40880）"
    );
    let sys = v2["system"].as_array().unwrap();
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0].as_object().unwrap().len(), 2, "billing 块只有 type / text: {v2}");
    assert!(sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"));
    assert!(v2.get("tools").is_none());
    let keys: Vec<&str> = v2.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "model",
            "messages",
            "system",
            "metadata",
            "max_tokens",
            "thinking",
            "context_management",
            "output_config",
            "thread",
            "diagnostics",
        ],
        "键序同官方续轮 00244"
    );

    // 再一轮：接着 continue 那条回复（纯文本、没有 tool_use）。
    let p2 = p2.expect("continue 也要提交");
    CcSessionLink::record_thread(&p2, "msg_B", Vec::new(), reply_of("只有 a.txt".into()), 40436);
    let mut msgs3 = msgs2.as_array().unwrap().clone();
    msgs3.push(serde_json::json!({ "role": "assistant", "content": "只有 a.txt" }));
    msgs3.push(serde_json::json!({ "role": "user", "content": "好，下一步" }));
    let (v3, p3) = thread_turn(&thread_body("claude-opus-5-5", msgs3.clone().into()), all_on());
    assert_eq!(v3["thread"]["previous_message_id"], "msg_B", "{v3}");
    assert_eq!(v3["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        v3["messages"][1]["content"][0]["text"],
        "<total_tokens>15000000 tokens left</total_tokens>",
        "新输入重新锚定（00039）"
    );

    // 新输入锚在 40436 上；接下来两次工具续轮：40606 → 用 170，41114 → 用 678（00040、00041
    // 的算法），上下文回落时只减不增。
    CcSessionLink::record_thread(
        &p3.unwrap(),
        "msg_C",
        vec!["toolu_2".into()],
        reply_of(serde_json::json!([bash_use("toolu_2", "pwd")])),
        40606,
    );
    let mut msgs4 = msgs3.clone();
    msgs4.push(serde_json::json!({ "role": "assistant", "content": [
            { "type": "tool_use", "id": "toolu_2", "name": "Bash", "input": { "command": "pwd" } },
        ] }));
    msgs4.push(serde_json::json!({ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_2", "content": "/tmp" },
        ] }));
    let (v4, p4) = thread_turn(&thread_body("claude-opus-5-5", msgs4.clone().into()), all_on());
    assert_eq!(
        v4["messages"][1]["content"][0]["text"],
        "<total_tokens>14999830 tokens left</total_tokens>"
    );
    CcSessionLink::record_thread(
        &p4.unwrap(),
        "msg_D",
        vec!["toolu_3".into()],
        reply_of(serde_json::json!([bash_use("toolu_3", "ls")])),
        40000,
    );
    let mut msgs5 = msgs4;
    msgs5.push(serde_json::json!({ "role": "assistant", "content": [
            { "type": "tool_use", "id": "toolu_3", "name": "Bash", "input": { "command": "ls" } },
        ] }));
    msgs5.push(serde_json::json!({ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_3", "content": "x" },
        ] }));
    let (v5, _) = thread_turn(&thread_body("claude-opus-5-5", msgs5.into()), all_on());
    assert_eq!(
        v5["messages"][1]["content"][0]["text"],
        "<total_tokens>14999830 tokens left</total_tokens>",
        "上下文回落不回涨（00244）"
    );
}

/// haiku 不带 `mid-conversation-system`：提醒写成 `<system-reminder>`——新输入插在用户那句
/// 前面（`cap/auto-2.1.285-20260930/00411`），工具续轮拼进 `tool_result` 正文末尾（`00412`）。
#[test]
fn sim_total_tokens_reminder_is_a_system_reminder_on_haiku() {
    use crate::proxy::session_link::CcSessionLink;
    let user1 = serde_json::json!({ "role": "user", "content": "线程测试·haiku 第一问" });
    let (v1, p1) = thread_turn(
        &thread_body("claude-haiku-4-5-20251001", serde_json::json!([user1])),
        all_on(),
    );
    let blocks = v1["messages"][0]["content"].as_array().unwrap();
    assert_eq!(v1["messages"].as_array().unwrap().len(), 1, "不追加 system 消息: {v1}");
    // 首轮：环境说明各段、`<total_tokens>`、日期，都是提醒块，排在用户那句前面（00303）；
    // 提醒只有环境说明里那一份。
    assert_eq!(blocks.len(), 7, "{v1}");
    assert!(blocks[0]["text"].as_str().unwrap().starts_with("<system-reminder>\n# Environment\n"));
    assert_eq!(
        blocks[4]["text"],
        "<system-reminder>\n<total_tokens>15000000 tokens left</total_tokens>\n</system-reminder>"
    );
    let tokens = blocks.iter().filter(|b| b["text"].as_str().unwrap().contains("<total_tokens>"));
    assert_eq!(tokens.count(), 1, "{v1}");
    assert!(blocks[5]["text"].as_str().unwrap().starts_with("<system-reminder>\nToday's date is "));
    assert_eq!(blocks[6]["text"], "线程测试·haiku 第一问");
    assert!(blocks[6].get("cache_control").is_some(), "断点仍在用户那句上");
    CcSessionLink::record_thread(
        &p1.unwrap(),
        "msg_H",
        vec!["toolu_h".into()],
        reply_of(serde_json::json!([bash_use("toolu_h", "ls")])),
        30000,
    );

    let msgs = serde_json::json!([
        user1,
        { "role": "assistant", "content": [
            { "type": "tool_use", "id": "toolu_h", "name": "Bash", "input": { "command": "ls" } },
        ] },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_h", "content": "a.txt" },
        ] },
    ]);
    let (v2, _) = thread_turn(&thread_body("claude-haiku-4-5-20251001", msgs), all_on());
    assert_eq!(v2["thread"]["type"], "continue", "{v2}");
    let delta = v2["messages"].as_array().unwrap();
    assert_eq!(delta.len(), 1);
    assert_eq!(
        delta[0]["content"][0]["content"],
        "a.txt\n\n<system-reminder>\n<total_tokens>14970000 tokens left</total_tokens>\n</system-reminder>"
    );
}

/// 接不上的一律 `create`：回复的 tool_use id 对不上、重新生成（历史与上一轮一样长）、改了更早的
/// 历史、换了模型、来访指定了 `tool_choice`、上一条 `continue` 失败（线程作废）。
#[test]
fn sim_threads_fall_back_to_create_when_the_history_does_not_follow() {
    use crate::proxy::session_link::CcSessionLink;
    let user1 = serde_json::json!({ "role": "user", "content": "线程测试·回退 第一问" });
    let first = thread_body("claude-sonnet-5-5", serde_json::json!([user1]));
    let (_, p1) = thread_turn(&first, all_on());
    CcSessionLink::record_thread(
        &p1.unwrap(),
        "msg_R1",
        vec!["toolu_real".into()],
        reply_of(serde_json::json!([bash_use("toolu_real", "ls")])),
        1000,
    );
    let assistant = |id: &str| {
        serde_json::json!({ "role": "assistant", "content": [
                { "type": "tool_use", "id": id, "name": "Bash", "input": { "command": "ls" } },
            ] })
    };
    let result = |id: &str| {
        serde_json::json!({ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": id, "content": "x" },
            ] })
    };
    let create = serde_json::json!({ "type": "create" });

    let wrong_id = serde_json::json!([user1, assistant("toolu_other"), result("toolu_other")]);
    let (v, _) = thread_turn(&thread_body("claude-sonnet-5-5", wrong_id), all_on());
    assert_eq!(v["thread"], create, "tool_use id 对不上: {v}");

    let (v, _) = thread_turn(&first, all_on());
    assert_eq!(v["thread"], create, "重新生成");

    let good = serde_json::json!([user1, assistant("toolu_real"), result("toolu_real")]);
    let (v, _) = thread_turn(&thread_body("claude-opus-5-5", good.clone()), all_on());
    assert_eq!(v["thread"], create, "换了模型");

    let mut forced = thread_body("claude-sonnet-5-5", good.clone());
    forced["tool_choice"] = serde_json::json!({ "type": "tool", "name": "Bash" });
    let (v, p) = thread_turn(&forced, all_on());
    assert_eq!(v["thread"], create, "指定了 tool_choice: {v}");
    assert!(p.is_some_and(|p| !p.is_continue()), "按 create 提交");

    let edited = serde_json::json!([
        { "role": "user", "content": "线程测试·回退 改过的第一问" },
        assistant("toolu_real"),
        result("toolu_real"),
    ]);
    let (v, _) = thread_turn(&thread_body("claude-sonnet-5-5", edited), all_on());
    assert_eq!(v["thread"], create, "改了更早的历史");

    let (v, p) = thread_turn(&thread_body("claude-sonnet-5-5", good.clone()), all_on());
    assert_eq!(v["thread"]["type"], "continue", "对照组接得上: {v}");
    CcSessionLink::drop_thread(&p.unwrap());
    let (v, _) = thread_turn(&thread_body("claude-sonnet-5-5", good), all_on());
    assert_eq!(v["thread"], create, "上一条 continue 失败后线程作废");
}

/// fable-5-1 在 2.1.285 一条 `thread` 都不发（`cap/auto-2.1.285-20260930/00383`、`00554`），2.1.291 起
/// 也发了（`cap/auto-2.1.291-20261006-full/00464` 首轮 `create`）——模拟路径用 2.1.291 表，跟着写；
/// `<total_tokens>` 提醒两版都带；开关关着时两样都不写。
#[test]
fn sim_threads_follow_the_version_for_fable_5_1_and_respect_the_switch() {
    let msgs = serde_json::json!([{ "role": "user", "content": "线程测试·豁免" }]);
    let (v, p) = thread_turn(&thread_body("claude-fable-5-1", msgs.clone()), all_on());
    assert_eq!(v["thread"]["type"], "create", "2.1.291 的 fable-5-1 发: {v}");
    let note = v["messages"][1]["content"][0]["text"].as_str().unwrap();
    assert!(
        note.starts_with("# Environment\n")
            && note.contains("<total_tokens>15000000 tokens left</total_tokens>"),
        "首轮提醒并在环境说明里照带: {note}"
    );
    assert!(p.is_some_and(|p| !p.is_continue()), "按 create 提交");
    let (v, _) = thread_turn(&thread_body("claude-fable-5", msgs.clone()), all_on());
    assert_eq!(v["thread"]["type"], "create", "fable-5 发: {v}");
    let off = store::ForwardFlags { sim_message_threads: false, ..all_on() };
    let (v, p) = thread_turn(&thread_body("claude-opus-5-5", msgs), off);
    assert!(v.get("thread").is_none() && p.is_none(), "{v}");
}

/// `cch` 真值算法的回归锚点：这些 `(body, cch)` 对取自 `claude-cli/2.1.289` 的 Bun HTTP
/// 出口层（把带 `cch=00000;` 占位符的 body 真正发一遍、读它回填的值），是独立于我们实现
/// 的权威真值。每条各打一个规范化规则：
/// - `max_tokens` 在中段、`fallbacks`、`fallback_credit_token` 剥干净后都回到 `plain`；
/// - `max_tokens` 在队尾（后无逗号、吃前逗号）与「本就没 model」同值；
/// - `all_fields` 三个字段都剥、`stream` 留下；`two_models` 顶层与 advisor 两处 model 值都清空。
#[test]
fn cch_matches_official_egress() {
    let bh = "x-anthropic-billing-header: cc_version=2.1.289.a; cc_entrypoint=cli; cch=00000;";
    let sys = format!("[{{\"type\":\"text\",\"text\":\"{bh}\"}}]");
    let msgs = "[{\"role\":\"user\",\"content\":\"hi\"}]";
    let cases: &[(&str, String)] = &[
        (
            "1ee24",
            format!("{{\"model\":\"claude-opus-5-5\",\"system\":{sys},\"messages\":{msgs}}}"),
        ),
        (
            "1ee24",
            format!(
                "{{\"model\":\"claude-opus-5-5\",\"max_tokens\":128000,\"system\":{sys},\"messages\":{msgs}}}"
            ),
        ),
        ("bc961", format!("{{\"system\":{sys},\"messages\":{msgs},\"max_tokens\":64000}}")),
        (
            "1ee24",
            format!(
                "{{\"model\":\"claude-opus-5-5\",\"system\":{sys},\"messages\":{msgs},\"fallbacks\":[\"claude-sonnet-5-5\"]}}"
            ),
        ),
        (
            "1ee24",
            format!(
                "{{\"model\":\"claude-opus-5-5\",\"fallback_credit_token\":\"tok_abc123\",\"system\":{sys},\"messages\":{msgs}}}"
            ),
        ),
        (
            "389a3",
            format!(
                "{{\"model\":\"claude-opus-5-5\",\"max_tokens\":32000,\"system\":{sys},\"messages\":{msgs},\"stream\":true,\"fallbacks\":[\"claude-sonnet-5-5\"],\"fallback_credit_token\":\"tok_x\"}}"
            ),
        ),
        (
            "9164d",
            format!(
                "{{\"model\":\"claude-opus-5-5\",\"system\":{sys},\"tools\":[{{\"type\":\"advisor_20260301\",\"name\":\"advisor\",\"model\":\"claude-opus-5-5\"}}],\"messages\":{msgs}}}"
            ),
        ),
        ("bc961", format!("{{\"system\":{sys},\"messages\":{msgs}}}")),
    ];
    for (want, body) in cases {
        assert_eq!(&super::compute_cch(body.as_bytes()), want, "body={body}");
    }
}

/// [`apply_cch`] 限定在 billing header 里定位并回填 cch：占位符与自带真值都重算，
/// billing header 之外（用户正文）的 `cch=…` 一律不碰，没有 billing header 时不动。
#[test]
fn apply_cch_scoped_to_billing_header() {
    // messages 排在 system 之前、且用户正文里恰好含 cch=00000;（257/263 抓包是这个键序）。
    let user = "cch=00000; 这是用户正文不该被动";
    // billing header 的 cch 取不同初值，body 其余部分完全一致。
    let body_with = |billing_cch: &str| {
        format!(
            "{{\"model\":\"claude-opus-5-5\",\"messages\":[{{\"role\":\"user\",\"content\":\"{user}\"}}],\"system\":[{{\"type\":\"text\",\"text\":\"x-anthropic-billing-header: cc_version=2.1.289.a; cc_entrypoint=cli; cch={billing_cch};\"}}]}}"
        )
    };

    let mut bytes = body_with("00000").into_bytes();
    let cch = super::apply_cch(&mut bytes).expect("billing header 里有 cch");
    let out = String::from_utf8(bytes).unwrap();
    assert_ne!(cch, "00000", "billing cch 被算成真值");
    // billing header 的 cch 被填成真值；用户正文里的 cch=00000; 原样保留。
    assert!(out.contains(&format!("cch={cch};")), "billing 回填: {out}");
    assert!(out.contains(user), "用户正文未被动: {out}");
    assert_eq!(out.matches("cch=00000;").count(), 1, "只剩用户正文那一处占位: {out}");

    // billing 自带真值（非 00000）时，body 其余一致，重算应得到同一个 cch。
    let mut rb = body_with("dde8e").into_bytes();
    assert_eq!(super::apply_cch(&mut rb).as_deref(), Some(cch.as_str()), "自带真值被重算");

    // 没有 billing header 时不动（真实 CC 非订阅、或 billing 关着）。
    let mut nobh = br#"{"messages":[{"role":"user","content":"cch=00000; x"}]}"#.to_vec();
    assert!(super::apply_cch(&mut nobh).is_none(), "无 billing header 则不改");
}

/// [`finalize_cch`] 三档策略：算真值（与 [`apply_cch`] 同值）；不算且是 luban 的占位符时
/// 填随机值（不留 `00000`）；不算且是来访自带的则原样保留。
#[test]
fn finalize_cch_strategies() {
    let body_with = |cch: &str| {
        format!(
            "{{\"system\":[{{\"type\":\"text\",\"text\":\"x-anthropic-billing-header: cc_version=2.1.289.a; cc_entrypoint=cli; cch={cch};\"}}],\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}"
        )
    };
    let mut want = body_with("00000").into_bytes();
    let real = super::apply_cch(&mut want).unwrap();

    // 开：自带值与占位符都按出站字节重算。
    for (init, ours) in [("dde8e", false), ("00000", true)] {
        let mut b = body_with(init).into_bytes();
        assert_eq!(super::finalize_cch(&mut b, true, ours).as_deref(), Some(real.as_str()));
        assert_eq!(b, want);
    }

    // 关、来访自带：原样保留。
    let mut b = body_with("dde8e").into_bytes();
    assert!(super::finalize_cch(&mut b, false, false).is_none());
    assert_eq!(b, body_with("dde8e").into_bytes());

    // 关、luban 的占位符：随机值，形状合法且逐请求变化。
    let mut seen = std::collections::HashSet::new();
    for _ in 0..20 {
        let mut b = body_with("00000").into_bytes();
        let cch = super::finalize_cch(&mut b, false, true).unwrap();
        assert!(String::from_utf8(b).unwrap().contains(&format!("cch={cch};")));
        seen.insert(cch);
    }
    assert!(seen.len() > 1, "随机值不该 20 次全同: {seen:?}");
}

/// 前置改写（剥 prefill、thinking 降级等）动过 body、[`rewrite_body_out`] 自己又没改任何东西时，
/// 来访自带的 cch 已与 body 不符：`cch_real_recompute` 开着必须重算，不能原样早退；关着则原样
/// 放行。自带值本就对得上的体照旧零改写早退。
#[test]
fn rewrite_recomputes_stale_real_cch() {
    let body_with = |cch: &str, content: &str| {
        format!(
            "{{\"model\":\"claude-opus-5-5\",\"system\":[{{\"type\":\"text\",\"text\":\"x-anthropic-billing-header: cc_version=2.1.289.a; cc_entrypoint=cli; cch={cch};\"}}],\"messages\":[{{\"role\":\"user\",\"content\":\"{content}\"}}]}}"
        )
    };
    // 客户端那份（cch 是它自己的真值），以及被前置改写动过内容、cch 没跟着变的那份。
    let mut orig = body_with("00000", "hi").into_bytes();
    let client_cch = super::apply_cch(&mut orig).unwrap();
    let mut want = body_with("00000", "hi there").into_bytes();
    let fresh = super::apply_cch(&mut want).unwrap();
    assert_ne!(client_cch, fresh);
    let stale = Bytes::from(body_with(&client_cch, "hi there"));

    let cred = test_cred();
    let off = store::ForwardFlags {
        spoof_identity: false,
        billing_cch: false,
        cch_real_recompute: false,
        strip_extra_fields: false,
        system_shape: false,
        eager_tool_streaming: false,
        flatten_tool_schemas: false,
        strip_empty_text: false,
        ..store::ForwardFlags::default()
    };
    let run = |body: &Bytes, flags: store::ForwardFlags| {
        super::rewrite_body_out(
            body,
            &cred,
            "fp",
            flags,
            None,
            None,
            None,
            false,
            None,
            false,
            false,
            None,
            None,
            crate::proxy::CcRequestKind::Main,
            None,
        )
    };

    let on = store::ForwardFlags { cch_real_recompute: true, ..off };
    let (out, v) = run(&stale, on);
    assert_eq!(String::from_utf8(out.to_vec()).unwrap(), String::from_utf8(want).unwrap());
    assert!(v.is_some(), "重算过就交回与出站字节同构的 Value");

    // 关着：原样放行（自带值保留）。
    let (out, _) = run(&stale, off);
    assert_eq!(out, stale);

    // 自带值本就对得上：开着也零改写早退。
    let intact = Bytes::from(orig);
    let (out, v) = run(&intact, on);
    assert_eq!(out, intact);
    assert!(v.is_none());
}

/// 用户正文里原样引用**整条** `x-anthropic-billing-header: …cch=00000;…`（在 system 之前）
/// 也不会被误认：按 JSON 结构定位只认 `system[0].text` 那个真字段，正文里那段引号是 `\"`、
/// 不是键结构。回填只落在 billing header，正文原样保留。
#[test]
fn apply_cch_ignores_quoted_header_in_user_text() {
    // 用户把一条看起来一模一样的 billing header 塞进正文（system 之前）。
    let spoof = "x-anthropic-billing-header: cc_version=2.1.289.a; cc_entrypoint=cli; cch=00000;";
    let body = format!(
        "{{\"messages\":[{{\"role\":\"user\",\"content\":\"看这个 {spoof} 冒充\"}}],\"system\":[{{\"type\":\"text\",\"text\":\"{spoof}\"}}]}}"
    );
    let mut bytes = body.into_bytes();
    let cch = super::apply_cch(&mut bytes).expect("定位到 system[0] 的 billing header");
    let out = String::from_utf8(bytes).unwrap();
    // 正文里那条保持 cch=00000;，只有 system[0] 的被填成真值。
    assert_eq!(out.matches("cch=00000;").count(), 1, "正文那条占位符未被动: {out}");
    assert!(out.contains(&format!("看这个 {spoof} 冒充")), "正文整体未被改: {out}");
    let sys_pos = out.find("\"system\"").unwrap();
    assert!(out[sys_pos..].contains(&format!("cch={cch};")), "真 billing 被回填: {out}");
}

/// billing header 文本里出现裸 `cch=`（后面不是合法的 `<5 位 hex>;`，例如紧跟字符串收尾
/// 引号）时，绝不越界写那 5 位去破坏 JSON——没有合法 cch 段就当没有，返回 `None`。
#[test]
fn apply_cch_rejects_malformed_cch_segment() {
    // billing header 以裸 `cch=` 结尾（随即是字符串的收尾引号），后面没有 5 位 hex + 分号。
    let bh = "x-anthropic-billing-header: cc_version=2.1.289.a; cc_entrypoint=cli; cch=";
    let body = format!(
        "{{\"system\":[{{\"type\":\"text\",\"text\":\"{bh}\"}}],\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}"
    );
    let original = body.clone();
    let mut bytes = body.into_bytes();
    assert!(super::apply_cch(&mut bytes).is_none(), "无合法 cch 段则不动");
    assert_eq!(String::from_utf8(bytes).unwrap(), original, "字节逐字节未变，JSON 未被破坏");

    // 短到 `cch=12;`（只有 2 位）也不认——必须恰好 5 位 hex 加分号。
    let bh2 = "x-anthropic-billing-header: cc_version=2.1.289.a; cch=12;";
    let body2 = format!(
        "{{\"system\":[{{\"type\":\"text\",\"text\":\"{bh2}\"}}],\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}"
    );
    let orig2 = body2.clone();
    let mut b2 = body2.into_bytes();
    assert!(super::apply_cch(&mut b2).is_none(), "非 5 位不认");
    assert_eq!(String::from_utf8(b2).unwrap(), orig2, "未破坏");
}

/// 全量回归：遍历本机 `cap/` 下所有抓包，对每条 `/v1/messages` 请求体**原样**跑一遍
/// [`apply_cch`]（它内部把 billing header 的 cch 归零、按整条 body 重算、回填），得到的值必须
/// 等于客户端自己写的那个真实 cch。这同时压到了 billing header 定位（而非全局搜索）与重算。
///
/// **`#[ignore]`：只在本地带着抓包手动跑**（`cargo test cch_matches_all_captures --
/// --ignored`）。`cap/` 不随仓库走（`.gitignore`），CI 上拿不到，常规 `cargo test` 不碰它；
/// 跨 CI 一致的真值锚点在 [`cch_matches_official_egress`]。
#[test]
#[ignore = "依赖本机 cap/ 抓包，本地手动跑"]
fn cch_matches_all_captures() {
    use std::path::Path;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("cap");
    if !root.is_dir() {
        eprintln!("cap/ 不在，跳过全量 cch 回归");
        return;
    }
    let re = regex_cch();
    let (mut matched, mut total) = (0usize, 0usize);
    let mut fails: Vec<String> = Vec::new();
    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                dirs.push(p);
                continue;
            }
            if !p.to_string_lossy().ends_with(".req.raw") {
                continue;
            }
            let Ok(raw) = std::fs::read(&p) else { continue };
            let Some(sep) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
            let head = &raw[..sep];
            let first = head.split(|&b| b == b'\r').next().unwrap_or(b"");
            if !first.windows(13).any(|w| w == b"/v1/messages?" || w == b"/v1/messages ")
                || first.windows(12).any(|w| w == b"count_tokens")
            {
                continue;
            }
            let body = &raw[sep + 4..];
            let Some((_, real)) = re(body) else { continue };
            total += 1;
            // 原样跑 apply_cch（内部归零+重算+回填），结果应等于客户端写的真实 cch。
            let mut out = body.to_vec();
            let got = super::apply_cch(&mut out);
            if got.as_deref().map(str::as_bytes) == Some(real.as_slice()) {
                matched += 1;
            } else if fails.len() < 8 {
                fails.push(format!(
                    "{}: real={} got={}",
                    p.file_name().unwrap().to_string_lossy(),
                    String::from_utf8_lossy(&real),
                    got.as_deref().unwrap_or("<None>")
                ));
            }
        }
    }
    assert!(total > 0, "cap/ 存在但没扫到任何带 cch 的 /v1/messages");
    assert_eq!(matched, total, "{matched}/{total} 命中；前几条失败：\n{}", fails.join("\n"));
}

/// 在 body 里找第一个 `cch=<5 位小写 hex>;`，返回（那 5 位的起始下标，那 5 位字节）。
fn regex_cch() -> impl Fn(&[u8]) -> Option<(usize, Vec<u8>)> {
    |body: &[u8]| {
        let needle = b"cch=";
        let mut i = 0;
        while i + 10 <= body.len() {
            if &body[i..i + 4] == needle {
                let hex = &body[i + 4..i + 9];
                if body[i + 9] == b';'
                    && hex.iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
                {
                    return Some((i + 4, hex.to_vec()));
                }
            }
            i += 1;
        }
        None
    }
}

/// 首轮环境说明的模型行：名字与知识截止按规范名查表，带日期的 id 照认；1M 会话名字加
/// `(1M context)`、id 加 `[1m]`（`cap/auto-2.1.291-20261006-full/00216`）；表外的只写 id；
/// 模型名里有空白之类的不补。
#[test]
fn env_note_model_line_follows_the_captured_table() {
    let line = super::env_model_line;
    assert_eq!(
        line("claude-opus-5-5", false).unwrap(),
        "You are powered by the model named Opus 5.5. The exact model ID is claude-opus-5-5. \
         Assistant knowledge cutoff is June 2026."
    );
    let one_m = "You are powered by the model named Opus 5.5 (1M context). The exact model ID is \
                 claude-opus-5-5[1m]. Assistant knowledge cutoff is June 2026.";
    assert_eq!(line("claude-opus-5-5", true).unwrap(), one_m, "头上带 context-1m");
    assert_eq!(line("claude-opus-5-5[1m]", false).unwrap(), one_m, "模型名自带 [1m]");
    assert_eq!(
        line("claude-haiku-4-5-20251001", false).unwrap(),
        "You are powered by the model named Haiku 4.5. The exact model ID is \
         claude-haiku-4-5-20251001. Assistant knowledge cutoff is February 2025."
    );
    assert!(line("claude-opus-5", false).unwrap().contains("named Opus 5. "), "不被 opus-5-5 截胡");
    assert!(line("claude-sonnet-5", false).unwrap().ends_with("cutoff is January 2026."));
    assert_eq!(
        line("claude-next-9", false).unwrap(),
        "You are powered by the model claude-next-9."
    );
    assert_eq!(line("claude opus\n", false), None);
    assert_eq!(line("", false), None);
}

/// opus 那族：首条用户消息之后一条 `role: system`，各段次序同 `00340`；精简工具时没有
/// scratchpad 一行与 artifact 三个技能（`cap/auto-2.1.291-20261006/00066`）；同一条再补一遍不重复。
/// **对着 `cap/auto-2.1.293-20261008-full` 逐段核首轮环境说明**：opus / sonnet / haiku-5-5 三个
/// 带 `mid-conversation-system` 的主线程（`00419`、`00256`、`00344`），按抓包机的 cwd 补一份，与官方
/// 那条 `role: system` 消息去掉模拟路径不注的几段后逐字节相同。不比的：MCP 那几段（2.1.293 新增的
/// 「tools just became available」、MCP 说明）、延迟工具那段、插件技能（名字带 `:` 的那几行）；
/// scratchpad 那行里的会话 id 与日期换成模拟这边的。抓包目录不在仓库里就跳过。
#[test]
fn env_note_matches_the_2_1_293_captures() {
    let dir = format!("{}/cap/auto-2.1.293-20261008-full", env!("CARGO_MANIFEST_DIR"));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("skipped: cap/auto-2.1.293-20261008-full not present");
        return;
    };
    let files: Vec<std::path::PathBuf> = entries.filter_map(|e| Some(e.ok()?.path())).collect();
    let cwd = "/private/tmp/claude-501/-Users-easayliu-Works-easay-luban/32e92005-bbe2-45cf-9aac-d4f538716b03/scratchpad/calc";
    for (cred, n) in [(5301, "00419"), (5302, "00256"), (5303, "00344")] {
        let path = files
            .iter()
            .find(|p| {
                p.file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|f| f.starts_with(n) && f.ends_with(".req.raw"))
            })
            .unwrap_or_else(|| panic!("{n}"));
        let raw = std::fs::read(path).unwrap();
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let official: serde_json::Value = serde_json::from_slice(&raw[sep + 4..]).unwrap();
        let model = official["model"].as_str().unwrap();
        let session = official["metadata"]["user_id"].as_str().unwrap();
        let session: serde_json::Value = serde_json::from_str(session).unwrap();
        let session = session["session_id"].as_str().unwrap();
        let otext = official["messages"][1]["content"][0]["text"].as_str().unwrap();

        let body = format!(
            r#"{{"model":"{model}","max_tokens":16,"messages":[{{"role":"user","content":"环境说明·{n}"}}]}}"#
        );
        let mut sim = crate::proxy::test_support::sim_for(&body);
        sim.env = Some(crate::proxy::SimEnv::from_cwd("/Users/easayliu", cwd));
        sim.trim_tools = false;
        let mut v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(super::insert_env_note(&mut v, &sim, cred).inserted, "{model}");
        assert_eq!(v["messages"][1]["role"], "system", "{model}");
        let text = v["messages"][1]["content"][0]["text"].as_str().unwrap();

        // 官方那份去掉模拟不注的段落：MCP 两种、延迟工具；技能清单只留内建的（不带 `:` 的）。
        let mut keep = Vec::new();
        let mut in_mcp = false;
        for para in otext.split("\n\n") {
            if para.starts_with("# MCP Server Instructions") {
                in_mcp = true;
            }
            if para.starts_with("The following skills are available") {
                in_mcp = false;
            }
            if in_mcp
                || para.starts_with("The following tools just became available")
                || para.starts_with("The following deferred tools are now available")
            {
                continue;
            }
            // 插件技能形如 `- commit-commands:commit: …`，名字里带 `:`。
            let plugin = |l: &str| {
                l.strip_prefix("- ")
                    .and_then(|s| s.split_once(": "))
                    .is_some_and(|(name, _)| name.contains(':'))
            };
            let para: Vec<&str> = para.lines().filter(|l| !plugin(l)).collect();
            keep.push(para.join("\n"));
        }
        let want = keep
            .join("\n\n")
            .replace(session, &sim.session_id)
            .replace("Today's date is 2026-10-08.", "");
        let got = text.split("Today's date is ").next().unwrap().to_string();
        assert_eq!(got, want, "{model}（{n}）");
        let line = config::CC_MODEL_IDENTITIES.iter().find(|(id, ..)| *id == model).unwrap().1;
        assert!(got.contains(&format!("You are powered by the model named {line}.")), "{model}");
    }
}

#[test]
fn env_note_is_a_system_message_after_the_first_prompt() {
    let body = r#"{"model":"claude-opus-5-5","max_tokens":16,"messages":[{"role":"user","content":"环境说明·opus"}]}"#;
    let mut sim = crate::proxy::test_support::sim_for(body);
    sim.env = Some(crate::proxy::SimEnv::from_cwd("/Users/sam", "/Users/sam/src/api"));
    // 精简与否随首轮钉住，两种各用一张凭证，算两段对话。
    for (trim, cred) in [(false, 4242), (true, 4245)] {
        sim.trim_tools = trim;
        let mut v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert!(super::insert_env_note(&mut v, &sim, cred).inserted);
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "system");
        let text = msgs[1]["content"][0]["text"].as_str().unwrap();
        let order = [
            "# Environment\nYou have been invoked in the following environment: \n - Primary \
             working directory: /Users/sam/src/api\n - Is a git repository: true\n - Platform: \
             darwin\n - Shell: zsh\n - OS Version: Darwin 27.0.0\n",
            "\n\nYou are powered by the model named Opus 5.5.",
            "\n\nAvailable agent types for the Agent tool:\n",
            "\n\nWhen you launch multiple agents",
            "\n\nThe following skills are available for use with the Skill tool:\n\n- dataviz:",
            "\n- security-review: Complete a security review of the pending changes on the current \
             branch\n\n<total_tokens>15000000 tokens left</total_tokens>\n\nToday's date is ",
        ];
        let mut at = 0;
        for part in order {
            let found = text[at..].find(part).unwrap_or_else(|| panic!("缺「{part}」: {text}"));
            at += found + part.len();
        }
        let scratch = format!(
            "Scratchpad directory: /private/tmp/claude-501/-Users-sam-src-api/{}/scratchpad",
            sim.session_id
        );
        assert_eq!(text.contains(&scratch), !trim, "trim={trim}: {text}");
        assert_eq!(text.contains("- artifact-design:"), !trim, "trim={trim}");
        assert!(text.contains("- update-config:") && !text.contains("commit-commands:"));
        assert!(!text.contains("ToolSearch") && !text.contains("# MCP Server"), "{text}");
        let again = v.clone();
        assert!(!super::insert_env_note(&mut v, &sim, cred).inserted, "已有一份就不再补");
        assert_eq!(v, again);
    }
}

/// haiku 那族：每段一个提醒块排在最前，客户端 system 挪进来的那块在它们之后、日期之前，日期
/// 紧贴用户正文（`cap/auto-2.1.291-20261006-full/00553`）；Explore 用 haiku 那版描述。
#[test]
fn env_note_on_haiku_is_reminders_before_the_prompt() {
    let body = r#"{"model":"claude-haiku-4-5-20251001","max_tokens":16,"messages":[{"role":"user","content":[{"type":"text","text":"<system-reminder>\nCodebase and user instructions are shown below.\n</system-reminder>"},{"type":"text","text":"环境说明·haiku"}]}]}"#;
    let mut sim = crate::proxy::test_support::sim_for(body);
    sim.env = Some(crate::proxy::SimEnv::from_cwd("/Users/sam", "/Users/sam/src/api"));
    let mut v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert!(super::insert_env_note(&mut v, &sim, 4243).inserted);
    assert_eq!(v["messages"].as_array().unwrap().len(), 1, "不追加 system 消息");
    let texts: Vec<&str> = v["messages"][0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["text"].as_str().unwrap())
        .collect();
    let heads = [
        "<system-reminder>\n# Environment\n",
        "<system-reminder>\nYou are powered by the model named Haiku 4.5.",
        "<system-reminder>\nAvailable agent types for the Agent tool:\n",
        "<system-reminder>\nThe following skills are available",
        "<system-reminder>\n<total_tokens>15000000 tokens left</total_tokens>\n</system-reminder>",
        "<system-reminder>\nCodebase and user instructions",
        "<system-reminder>\nToday's date is ",
        "环境说明·haiku",
    ];
    assert_eq!(texts.len(), heads.len(), "{texts:?}");
    for (t, h) in texts.iter().zip(heads) {
        assert!(t.starts_with(h), "「{h}」: {t}");
    }
    assert!(texts[2].contains("Explore: Fast read-only search agent"), "haiku 的 Explore 描述");
    assert!(texts[6].ends_with(".\n</system-reminder>\n"), "日期块末尾多一个换行: {:?}", texts[6]);
}

/// 同一段对话每轮补的是同一份：后续轮次它作为历史留在首条之后，与首轮逐字节相同，缓存与
/// message thread 的前缀才对得上。
#[test]
fn env_note_is_identical_on_every_turn() {
    let first = r#"{"model":"claude-sonnet-5-5","max_tokens":16,"messages":[{"role":"user","content":"环境说明·逐轮"}]}"#;
    let later = r#"{"model":"claude-sonnet-5-5","max_tokens":16,"messages":[{"role":"user","content":"环境说明·逐轮"},{"role":"assistant","content":"好"},{"role":"user","content":"再来"}]}"#;
    let mut sim = crate::proxy::test_support::sim_for(first);
    sim.env = Some(crate::proxy::SimEnv::from_cwd("/Users/sam", "/Users/sam/src/api"));
    let note = |body: &str| {
        let mut v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert!(super::insert_env_note(&mut v, &sim, 4244).inserted);
        v["messages"][1].clone()
    };
    let a = note(first);
    let b = note(later);
    assert_eq!(a, b);
}

/// 来访只给了记忆目录时，工作目录从项目段倒推，换算回去仍是同一个项目段；倒推不出家目录之下的
/// 路径就退回 `<home>/<最后一截>`。
#[test]
fn env_cwd_from_memory_dir_round_trips_to_the_same_slug() {
    use crate::proxy::SimEnv;
    let e = SimEnv::from_memory_dir("/Users/sam", "-Users-sam-src-my-api");
    assert_eq!(e.cwd, "/Users/sam/src/my/api");
    assert_eq!(SimEnv::from_cwd("/Users/sam", &e.cwd).slug, e.slug);
    assert_eq!(SimEnv::from_memory_dir("/Users/sam", "-private-tmp-x").cwd, "/Users/sam/x");
    assert_eq!(
        SimEnv::from_memory_dir("/Users/sam", "-Users-sam--hidden").cwd,
        "/Users/sam/hidden"
    );
}

/// 环境说明在 `messages` 里，不跟 system 第四块的开关：`simulate_full_system` 关掉时第四块没了，
/// 环境说明照补。不注官方工具的请求（一个工具都没声明、`fill_absent_tools` 关着）整段不补——
/// 它列的 Agent 类型与技能离不开那两个工具。
#[test]
fn env_note_follows_tool_injection_not_the_full_system_switch() {
    use crate::proxy::test_support::{all_on, detect_for, rewrite_body, test_cred};
    let run = |body: &str, flags: store::ForwardFlags| {
        let b = Bytes::from(body.to_string());
        let sim = detect_for(&b, flags).expect("走模拟");
        let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()
    };
    let has_note = |v: &serde_json::Value| {
        v["messages"][1]["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("# Environment\n"))
    };
    let body = r#"{"model":"claude-opus-5-5","max_tokens":16,"messages":[{"role":"user","content":"环境说明·开关"}]}"#;
    let no_rest = store::ForwardFlags { simulate_full_system: false, ..all_on() };
    let v = run(body, no_rest);
    assert!(has_note(&v), "第四块关着也补: {v}");
    assert_eq!(v["system"].as_array().unwrap().len(), 3, "第四块确实没补: {v}");

    let no_fill = store::ForwardFlags { fill_absent_tools: false, ..all_on() };
    let v = run(body, no_fill);
    assert!(v.get("tools").is_none() && !has_note(&v), "不注工具就不补: {v}");
    let with_tools = r#"{"model":"claude-opus-5-5","max_tokens":16,"tools":[{"name":"t","description":"d","input_schema":{"type":"object"}}],"messages":[{"role":"user","content":"环境说明·开关"}]}"#;
    assert!(has_note(&run(with_tools, no_fill)), "自己带了工具照补");
}

/// 中途切模型：首轮那份环境说明原样留在历史里（模型行仍写首轮的模型），切换那一轮另起一条
/// 模型说明，与这一轮的 `<total_tokens>` 并在一条 system 消息里（`cap/auto-2.1.285-20260930/00243`）。
/// 历史不变，倒数也接着上一轮算，不重置。
#[test]
fn env_note_survives_a_model_switch() {
    use crate::proxy::session_link::CcSessionLink;
    let user1 = serde_json::json!({ "role": "user", "content": "环境说明·切模型 第一问" });
    let (v1, p1) =
        thread_turn(&thread_body("claude-opus-5-5", serde_json::json!([user1])), all_on());
    let note1 = v1["messages"][1].clone();
    assert!(note1["content"][0]["text"].as_str().unwrap().contains("named Opus 5.5."));
    CcSessionLink::record_thread(
        &p1.unwrap(),
        "msg_S1",
        vec!["toolu_s".into()],
        reply_of(serde_json::json!([bash_use("toolu_s", "ls")])),
        40232,
    );
    let msgs = serde_json::json!([
        user1,
        { "role": "assistant", "content": [
            { "type": "tool_use", "id": "toolu_s", "name": "Bash", "input": { "command": "ls" } },
        ] },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_s", "content": "a.txt" },
        ] },
    ]);
    let (v2, _) = thread_turn(&thread_body("claude-sonnet-5-5", msgs), all_on());
    assert_eq!(v2["thread"]["type"], "create", "换了模型重新 create: {v2}");
    let out = v2["messages"].as_array().unwrap();
    let mut note2 = out[1].clone();
    note2["content"][0].as_object_mut().unwrap().shift_remove("cache_control");
    let mut want = note1.clone();
    want["content"][0].as_object_mut().unwrap().shift_remove("cache_control");
    assert_eq!(note2, want, "首轮那份逐字节不变");
    let tail = out.last().unwrap();
    assert_eq!(tail["role"], "system");
    assert_eq!(
        tail["content"][0]["text"],
        "You are powered by the model named Sonnet 5.5. The exact model ID is claude-sonnet-5-5. \
         Assistant knowledge cutoff is June 2026.\n\n<total_tokens>14959768 tokens left</total_tokens>",
        "模型说明与倒数并在一条里，倒数接着上一轮: {v2}"
    );
}

/// 不开 message thread 时没有 `<total_tokens>` 那条，模型说明在换模型那一轮单独补一条 system
/// 消息；再往后几轮完整历史整发一遍，上游线程里没留着那条，得在历史里原位重现——跳过的话，
/// 后面每一轮都只剩首轮那份、变成「切完模型又切回首轮那个模型」。
#[test]
fn model_notice_without_threads_is_its_own_message_on_the_switch_turn() {
    use crate::proxy::test_support::{all_on, detect_for, rewrite_body, test_cred};
    let flags = store::ForwardFlags { sim_message_threads: false, ..all_on() };
    let run = |model: &str, msgs: serde_json::Value| {
        let body = serde_json::json!({ "model": model, "max_tokens": 16, "messages": msgs });
        let b = Bytes::from(body.to_string());
        let sim = detect_for(&b, flags).expect("走模拟");
        let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()
    };
    let text = |m: &serde_json::Value| m["content"][0]["text"].as_str().unwrap().to_string();
    let u1 = serde_json::json!({ "role": "user", "content": "模型说明·无线程 第一问" });
    let v1 = run("claude-opus-5-5", serde_json::json!([u1]));
    assert!(text(&v1["messages"][1]).contains("named Opus 5.5."));

    let a1 = serde_json::json!({ "role": "assistant", "content": "好" });
    let u2 = serde_json::json!({ "role": "user", "content": "第二问" });
    let v2 = run("claude-sonnet-5-5", serde_json::json!([u1, a1, u2]));
    let out = v2["messages"].as_array().unwrap();
    assert_eq!(out.len(), 5, "{v2}");
    assert!(text(&out[1]).contains("named Opus 5.5."), "首轮那份不改");
    assert_eq!(out[4]["role"], "system");
    assert_eq!(
        text(&out[4]),
        "You are powered by the model named Sonnet 5.5. The exact model ID is claude-sonnet-5-5. \
         Assistant knowledge cutoff is June 2026."
    );

    // 第三轮：完整历史重发，切换说明必须补回原位——u2 之后、a2 之前。首轮那份仍写 Opus。
    let a2 = serde_json::json!({ "role": "assistant", "content": "好的" });
    let u3 = serde_json::json!({ "role": "user", "content": "第三问" });
    let v3 = run("claude-sonnet-5-5", serde_json::json!([u1, a1, u2, a2, u3]));
    let out = v3["messages"].as_array().unwrap();
    assert_eq!(out.len(), 7, "历史切换说明回到原位: {v3}");
    assert!(text(&out[1]).contains("named Opus 5.5."), "首轮那份不改");
    assert_eq!(out[2]["role"], "assistant", "a1 位置不动");
    assert_eq!(out[3]["role"], "user", "u2 位置不动");
    assert_eq!(out[4]["role"], "system", "u2 后面原位插切换说明");
    assert!(text(&out[4]).contains("named Sonnet 5.5."), "历史切换说明仍写 Sonnet");
    assert_eq!(out[5]["role"], "assistant", "切换说明之后是 a2");
    assert_eq!(out[6]["role"], "user", "末条是 u3");
}

/// haiku 那种不带 `mid-conversation-system` 的：模型说明裹成提醒块，新输入放在那条消息最前面
/// （`cap/auto-2.1.285-20260930/00411`），工具结果放在末尾（`tool_result` 得排最前）。
#[test]
fn model_notice_without_system_messages_is_a_reminder() {
    let mut v = serde_json::json!({ "messages": [{ "role": "user", "content": "问" }] });
    assert!(super::place_model_notice(&mut v, "M", false, true));
    assert_eq!(
        v["messages"][0]["content"],
        serde_json::json!([
            { "type": "text", "text": "<system-reminder>\nM\n</system-reminder>" },
            { "type": "text", "text": "问" },
        ])
    );
    let mut v = serde_json::json!({ "messages": [{ "role": "user", "content": [
        { "type": "tool_result", "tool_use_id": "t", "content": "ok" },
    ] }] });
    assert!(super::place_model_notice(&mut v, "M", false, false));
    assert_eq!(v["messages"][0]["content"][0]["type"], "tool_result");
    assert_eq!(v["messages"][0]["content"][1]["text"], "<system-reminder>\nM\n</system-reminder>");
    let mut v = serde_json::json!({ "messages": [{ "role": "assistant", "content": "答" }] });
    assert!(!super::place_model_notice(&mut v, "M", true, true), "末条不是 user 不补");
}

/// 跨模型族切模型：形态按**本轮**模型自带的 beta 走——首轮 Opus（带 `mid-conversation-system`）
/// 落 `role: system`，切到 Haiku（不带）跟着切成用户正文里的提醒块；首轮那份照 role:system
/// 原样发给 Haiku 会被上游 400 拒。两种形态之间 `<total_tokens>` 倒数不该重置：线程指纹按
/// 客户端真正发出的 `messages`（raw）算，与环境说明的形态无关。
#[test]
fn env_note_follows_the_current_models_form_across_family_switches() {
    use crate::proxy::session_link::CcSessionLink;
    let user1 = serde_json::json!({ "role": "user", "content": "环境说明·跨族切 第一问" });
    let (v1, p1) =
        thread_turn(&thread_body("claude-opus-5-5", serde_json::json!([user1])), all_on());
    assert_eq!(v1["messages"][1]["role"], "system", "首轮 Opus 的环境说明是 role:system");
    assert!(v1["messages"][1]["content"][0]["text"].as_str().unwrap().contains("named Opus 5.5."));
    CcSessionLink::record_thread(
        &p1.unwrap(),
        "msg_hx1",
        vec!["toolu_hx".into()],
        reply_of(serde_json::json!([bash_use("toolu_hx", "ls")])),
        40232,
    );
    let msgs = serde_json::json!([
        user1,
        { "role": "assistant", "content": [
            { "type": "tool_use", "id": "toolu_hx", "name": "Bash", "input": { "command": "ls" } },
        ] },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_hx", "content": "a.txt" },
        ] },
    ]);
    // 切到 Haiku：形态跟当前模型，环境说明走提醒块、不发 `role: system` 消息。
    let (v2, _) = thread_turn(&thread_body("claude-haiku-4-5-20251001", msgs), all_on());
    let m2 = v2["messages"].as_array().unwrap();
    for m in m2 {
        if m["role"] == "system" {
            let t = m["content"][0]["text"].as_str().unwrap_or_default();
            assert!(!t.starts_with("# Environment"), "Haiku 不该再出现 role:system 环境说明: {v2}");
        }
    }
    let first_text = m2[0]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        first_text.starts_with("<system-reminder>\n# Environment"),
        "环境说明塞进首条用户正文、是 Haiku 自己的形态: {first_text}",
    );
    // `<total_tokens>` 倒数接着上一轮算（14959768），形态变化不打断线程指纹。Haiku 没有合并
    // 到独立 system 消息里，倒数塞在末条用户的最后一个 `tool_result` 正文里（规则见
    // [`insert_total_tokens_reminder`]）；只要整条请求里找得到这个数就行。
    let dump = serde_json::to_string(&v2).unwrap();
    assert!(
        dump.contains("<total_tokens>14959768 tokens left"),
        "倒数接着上一轮，形态变化不该重置: {v2}",
    );
    assert!(dump.contains("named Haiku 4.5"), "末尾合并上切换说明: {v2}");
}

/// 关着 message thread 的那条路：切模型发生过之后再来一轮，完整历史重发时切换说明必须在
/// 原位重现——跳过的话，模型只看到首轮那份身份说明，等于告诉 Haiku 「你是 Opus」。
#[test]
fn historical_switch_notice_reappears_without_threads() {
    use crate::proxy::test_support::{all_on, detect_for, rewrite_body, test_cred};
    let flags = store::ForwardFlags { sim_message_threads: false, ..all_on() };
    let run = |model: &str, msgs: serde_json::Value| {
        let body = serde_json::json!({ "model": model, "max_tokens": 16, "messages": msgs });
        let b = Bytes::from(body.to_string());
        let sim = detect_for(&b, flags).expect("走模拟");
        let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()
    };
    let u1 = serde_json::json!({ "role": "user", "content": "历史切换·无线程 1" });
    let a1 = serde_json::json!({ "role": "assistant", "content": "好" });
    let u2 = serde_json::json!({ "role": "user", "content": "历史切换·无线程 2" });
    let a2 = serde_json::json!({ "role": "assistant", "content": "好的" });
    let u3 = serde_json::json!({ "role": "user", "content": "历史切换·无线程 3" });
    let _ = run("claude-opus-5-5", serde_json::json!([u1.clone()]));
    let _ = run("claude-sonnet-5-5", serde_json::json!([u1.clone(), a1.clone(), u2.clone()]));
    let v3 = run("claude-sonnet-5-5", serde_json::json!([u1, a1, u2, a2, u3]));
    let out = v3["messages"].as_array().unwrap();
    let roles: Vec<&str> = out.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        vec!["user", "system", "assistant", "user", "system", "assistant", "user"],
        "切换说明回到 u2 之后 a2 之前: {v3}",
    );
    assert!(
        out[1]["content"][0]["text"].as_str().unwrap().contains("named Opus 5.5."),
        "首轮那份仍写 Opus",
    );
    assert!(
        out[4]["content"][0]["text"].as_str().unwrap().contains("named Sonnet 5.5."),
        "历史切换说明写 Sonnet",
    );
}

/// 新旧模型切换：Opus 5.5（带 `mid-conversation-system`）→ 旧 Opus（不带）后，环境说明得随
/// 当前模型切成提醒块，不能再往出站 body 里塞 `role: system` 消息——塞了上游会 400 拒。
#[test]
fn env_note_drops_role_system_when_switching_to_an_older_model() {
    use crate::proxy::test_support::{all_on, detect_for, rewrite_body, test_cred};
    let run = |model: &str, msgs: serde_json::Value| {
        let body = serde_json::json!({ "model": model, "max_tokens": 16, "messages": msgs });
        let b = Bytes::from(body.to_string());
        let sim = detect_for(&b, all_on()).expect("走模拟");
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()
    };
    let u1 = serde_json::json!({ "role": "user", "content": "跨 beta·旧 Opus" });
    let a1 = serde_json::json!({ "role": "assistant", "content": "好" });
    let u2 = serde_json::json!({ "role": "user", "content": "继续" });
    let _ = run("claude-opus-5-5", serde_json::json!([u1.clone()]));
    // 旧 Opus（无 `mid-conversation-system` beta）这一轮：环境说明按本轮形态走，不是 role:system。
    let v2 = run("claude-opus-4-6", serde_json::json!([u1, a1, u2]));
    let m2 = v2["messages"].as_array().unwrap();
    for m in m2 {
        if m["role"] == "system" {
            let t = m["content"][0]["text"].as_str().unwrap_or_default();
            assert!(
                !t.starts_with("# Environment") && !t.starts_with("You are powered by"),
                "旧模型这一轮不该出现 role:system 的环境说明 / 模型说明: {v2}",
            );
        }
    }
    let first_text = m2[0]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        first_text.starts_with("<system-reminder>\n# Environment"),
        "环境说明塞进首条用户正文: {first_text}",
    );
}

/// 连续两条 user 消息（`[user, user]`）：环境说明不能插在它们中间——`role: system` 必须紧挨
/// assistant 之前或排在数组末尾，夹在两条 user 之间上游会 400。[`system_insert_pos`] 应把它推到
/// 数组末尾（没有 assistant 时的唯一合法位置）。
#[test]
fn env_note_lands_at_the_end_when_the_prompt_is_two_users_in_a_row() {
    use crate::proxy::test_support::{all_on, detect_for, rewrite_body, test_cred};
    let body = serde_json::json!({
        "model": "claude-opus-5-5",
        "max_tokens": 16,
        "messages": [
            { "role": "user", "content": "连续 user·第一条" },
            { "role": "user", "content": "连续 user·第二条" },
        ],
    });
    let b = Bytes::from(body.to_string());
    let sim = detect_for(&b, all_on()).expect("走模拟");
    let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let m = v["messages"].as_array().unwrap();
    assert!(m.len() >= 3, "环境说明至少让 messages 长 3 条: {v}");
    assert_eq!(m[0]["role"], "user");
    assert_eq!(m[1]["role"], "user", "两条 user 不该被 system 切开: {v}");
    // 环境说明落在最后一条 user 之后（或再被 `<total_tokens>` 这类后续插入推到更后面都算合法）。
    let any_env = m.iter().any(|x| {
        x["role"] == "system"
            && x["content"][0]["text"].as_str().is_some_and(|t| t.starts_with("# Environment"))
    });
    assert!(any_env, "环境说明仍要补、只是落在末尾: {v}");
    let env_idx = m
        .iter()
        .position(|x| {
            x["role"] == "system"
                && x["content"][0]["text"].as_str().is_some_and(|t| t.starts_with("# Environment"))
        })
        .unwrap();
    assert!(env_idx >= 2, "环境说明不在两条 user 之间: {v}");
}

/// 历史被裁短（compact / summarize）后，`pin.switch.turn` 可能大于当前 `msgs.len()`——原位重现
/// 无从插起，`pin_env` 把它当成「这一轮刚换了模型」处理（补一条当前模型说明到末尾），不该两种
/// 说明都丢、让 Sonnet 只收到首轮的 Opus 身份说明。
#[test]
fn compacted_history_still_carries_the_current_model_notice() {
    use crate::proxy::test_support::{all_on, detect_for, rewrite_body, test_cred};
    let flags = store::ForwardFlags { sim_message_threads: false, ..all_on() };
    let run = |model: &str, msgs: serde_json::Value| {
        let body = serde_json::json!({ "model": model, "max_tokens": 16, "messages": msgs });
        let b = Bytes::from(body.to_string());
        let sim = detect_for(&b, flags).expect("走模拟");
        let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()
    };
    let u1 = serde_json::json!({ "role": "user", "content": "压缩·第一问" });
    let a1 = serde_json::json!({ "role": "assistant", "content": "好" });
    let u2 = serde_json::json!({ "role": "user", "content": "压缩·第二问" });
    let a2 = serde_json::json!({ "role": "assistant", "content": "好的" });
    let u3 = serde_json::json!({ "role": "user", "content": "压缩·第三问" });
    let _ = run("claude-opus-5-5", serde_json::json!([u1.clone()]));
    // 第二轮在 msgs.len()=3 时换到 Sonnet；pin.switch 记成 (3, Sonnet)。
    let _ = run("claude-sonnet-5-5", serde_json::json!([u1.clone(), a1.clone(), u2.clone()]));
    let _ = run(
        "claude-sonnet-5-5",
        serde_json::json!([u1.clone(), a1.clone(), u2.clone(), a2.clone(), u3.clone()]),
    );
    // 第四轮客户端做了压缩，`msgs.len()` 回到 1：切换位置（3）已经超出当前消息数。
    let compacted = serde_json::json!({ "role": "user", "content": "压缩后汇总的首条" });
    let v = run("claude-sonnet-5-5", serde_json::json!([compacted]));
    let dump = serde_json::to_string(&v).unwrap();
    assert!(
        dump.contains("named Sonnet 5.5."),
        "压缩后仍要让当前模型看到自己的 Sonnet 身份说明，而不是只剩首轮的 Opus 身份: {v}",
    );
}

/// 历史里同一个签名块出现了两次（客户端把同一段贴了两遍），模拟路径又在前面插了一条环境说明：
/// 消息下标整体后移，但 assistant 那几轮没动，两块仍要各按位置还原成原始字节，JSON 转义
/// （反斜杠 u003c）不能被 serde 往返改成字面的 `<`。
#[test]
fn duplicate_signed_blocks_keep_their_encoding_after_a_system_message_is_inserted() {
    // 转义序列在运行时拼出来：源码里直接写，经过某些编辑工具会被提前解码成 `<`。
    let escaped = format!("a {}u003c b", '\\');
    let thinking = format!(r#"{{"type":"thinking","thinking":"{escaped}","signature":"SIGDUP"}}"#);
    let original = format!(
        r#"{{"messages":[{{"role":"user","content":"q1"}},{{"role":"assistant","content":[{thinking},{{"type":"text","text":"x"}}]}},{{"role":"user","content":"q2"}},{{"role":"assistant","content":[{thinking},{{"type":"text","text":"y"}}]}},{{"role":"user","content":"q3"}}]}}"#
    );
    let mut v: serde_json::Value = serde_json::from_str(&original).unwrap();
    v["messages"].as_array_mut().unwrap().insert(
        1,
        serde_json::json!({ "role": "system", "content": [{ "type": "text", "text": "env" }] }),
    );
    let out =
        super::preserve_thinking_encoding(original.as_bytes(), serde_json::to_vec(&v).unwrap());
    let out = String::from_utf8(out).unwrap();
    assert_eq!(out.matches(escaped.as_str()).count(), 2, "{out}");
    assert!(!out.contains("a < b"), "{out}");
}

/// 开着工具名混淆：上一轮回复的指纹记的是上游看到的假名，客户端下一轮带回来的是真名。线程
/// 指纹得按换过名的算，工具续轮才接得上（`continue`），不然每轮都退回 `create` 重发整段上下文。
#[test]
fn sim_threads_continue_through_obfuscated_tool_names() {
    use crate::proxy::session_link::CcSessionLink;
    let tools = serde_json::json!([{
        "name": "lookup",
        "description": "look something up",
        "input_schema": { "type": "object", "properties": { "q": { "type": "string" } } },
    }]);
    let map = super::build_tool_name_map(Some(&serde_json::json!({ "tools": tools })))
        .expect("自定义工具要混淆");
    let fake = map.forward["lookup"].clone();
    let turn = |msgs: serde_json::Value| {
        let body = serde_json::json!({
            "model": "claude-opus-5-5", "max_tokens": 32000, "tools": tools, "messages": msgs,
        });
        let raw = Bytes::from(body.to_string());
        let sim = sim_for(&body.to_string());
        let out = super::rewrite_body_out(
            &raw,
            &test_cred(),
            "fp",
            all_on(),
            Some(&sim),
            None,
            None,
            false,
            Some(&map),
            true,
            true,
            None,
            None,
            crate::proxy::CcRequestKind::Main,
            None,
        )
        .0;
        (serde_json::from_slice::<serde_json::Value>(&out).unwrap(), sim.take_thread())
    };
    let user1 = serde_json::json!({ "role": "user", "content": "线程测试·混淆工具名" });
    let (v1, p1) = turn(serde_json::json!([user1]));
    assert_eq!(v1["thread"]["type"], "create", "{v1}");
    let call = |name: &str| serde_json::json!({ "type": "tool_use", "id": "toolu_lk", "name": name, "input": { "q": "x" } });
    CcSessionLink::record_thread(
        &p1.unwrap(),
        "msg_LK",
        vec!["toolu_lk".into()],
        reply_of(serde_json::json!([call(&fake)])),
        1000,
    );
    let (v2, _) = turn(serde_json::json!([
        user1,
        { "role": "assistant", "content": [call("lookup")] },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_lk", "content": "ok" },
        ] },
    ]));
    assert_eq!(
        v2["thread"],
        serde_json::json!({ "type": "continue", "previous_message_id": "msg_LK" }),
        "{v2}"
    );
}

/// Opus 开场后换 Sonnet、这一轮是连着两条 user：环境说明只能落在末尾，换模型说明并进那一条，
/// 不能因为末条是 system 就丢掉——丢了 Sonnet 只看到首轮的 Opus 身份。线程开与关两条路都要有。
#[test]
fn model_notice_merges_into_a_trailing_env_note() {
    use crate::proxy::test_support::{detect_for, rewrite_body};
    for threads in [false, true] {
        let flags = store::ForwardFlags { sim_message_threads: threads, ..all_on() };
        let run = |model: &str, msgs: serde_json::Value| {
            let body = serde_json::json!({ "model": model, "max_tokens": 16, "messages": msgs });
            let b = Bytes::from(body.to_string());
            let sim = detect_for(&b, flags).expect("走模拟");
            let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
            serde_json::from_slice::<serde_json::Value>(&out).unwrap()
        };
        let u1 =
            serde_json::json!({ "role": "user", "content": format!("连发换模型·{threads} 1") });
        let u2 = serde_json::json!({ "role": "user", "content": "连发换模型 2" });
        let _ = run("claude-opus-5-5", serde_json::json!([u1]));
        let v = run("claude-sonnet-5-5", serde_json::json!([u1, u2]));
        let out = v["messages"].as_array().unwrap();
        let roles: Vec<&str> = out.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "user", "system"], "threads={threads}: {v}");
        let text = out[2]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("# Environment") && text.contains("named Opus 5.5."), "{text}");
        assert!(
            text.ends_with(
                "\n\nYou are powered by the model named Sonnet 5.5. The exact model ID is \
                 claude-sonnet-5-5. Assistant knowledge cutoff is June 2026."
            ),
            "threads={threads}: {text}"
        );
    }
}

/// 换过模型之后客户端删改了历史：按旧消息条数算的下标会指到别的消息上。仍找得到换模型那条
/// 用户消息就在它后面重现；找不到就当这一轮刚换，补在末尾——两种都不能悄悄丢掉。
#[test]
fn historical_switch_survives_edited_history() {
    use crate::proxy::test_support::{detect_for, rewrite_body};
    let flags = store::ForwardFlags { sim_message_threads: false, ..all_on() };
    let run = |model: &str, msgs: serde_json::Value| {
        let body = serde_json::json!({ "model": model, "max_tokens": 16, "messages": msgs });
        let b = Bytes::from(body.to_string());
        let sim = detect_for(&b, flags).expect("走模拟");
        let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()
    };
    let text = |m: &serde_json::Value| m["content"][0]["text"].as_str().unwrap().to_string();
    let sonnet = "named Sonnet 5.5.";
    for (tag, edit) in [("删掉 a1", 0), ("换掉 u2", 1)] {
        let u1 = serde_json::json!({ "role": "user", "content": format!("删改历史·{tag} 1") });
        let a1 = serde_json::json!({ "role": "assistant", "content": "好" });
        let u2 = serde_json::json!({ "role": "user", "content": "删改历史 2" });
        let a2 = serde_json::json!({ "role": "assistant", "content": "好的" });
        let u3 = serde_json::json!({ "role": "user", "content": "删改历史 3" });
        let _ = run("claude-opus-5-5", serde_json::json!([u1]));
        let _ = run("claude-sonnet-5-5", serde_json::json!([u1, a1, u2]));
        if edit == 0 {
            // [u1, u2, a2, u3]：旧下标 2 现在是 a2。u2 仍在，说明跟在它后面（并进紧随的环境说明）。
            let v = run("claude-sonnet-5-5", serde_json::json!([u1, u2, a2, u3]));
            let out = v["messages"].as_array().unwrap();
            let roles: Vec<&str> = out.iter().map(|m| m["role"].as_str().unwrap()).collect();
            assert_eq!(roles, ["user", "user", "system", "assistant", "user"], "{v}");
            assert!(text(&out[2]).contains("named Opus 5.5.") && text(&out[2]).ends_with(&format!(
                "{sonnet} The exact model ID is claude-sonnet-5-5. Assistant knowledge cutoff is \
                 June 2026."
            )), "{v}");
        } else {
            // [u1, a1, ux, ax, u3]：u2 已经不在，补在末尾。
            let ux = serde_json::json!({ "role": "user", "content": "改过的第二问" });
            let v = run("claude-sonnet-5-5", serde_json::json!([u1, a1, ux, a2, u3]));
            let out = v["messages"].as_array().unwrap();
            let roles: Vec<&str> = out.iter().map(|m| m["role"].as_str().unwrap()).collect();
            assert_eq!(
                roles,
                ["user", "system", "assistant", "user", "assistant", "user", "system"]
            );
            assert!(text(&out[6]).contains(sonnet), "{v}");
            assert!(!text(&out[1]).contains(sonnet) && !text(&out[3]).contains(sonnet));
        }
    }
}

/// 客户端回传的历史里夹着无签名的空 thinking 块：出站会剥掉它（上游必拒），上游那条回复里也
/// 从来没有它。线程指纹得按剥过的算，正常的工具续轮才接得上。
#[test]
fn sim_threads_continue_when_the_client_adds_an_empty_thinking_block() {
    use crate::proxy::session_link::CcSessionLink;
    let user1 = serde_json::json!({ "role": "user", "content": "线程测试·空 thinking" });
    let (_, p1) =
        thread_turn(&thread_body("claude-opus-5-5", serde_json::json!([user1])), all_on());
    CcSessionLink::record_thread(
        &p1.unwrap(),
        "msg_ET",
        vec!["toolu_et".into()],
        reply_of(serde_json::json!([bash_use("toolu_et", "ls")])),
        1000,
    );
    let msgs = serde_json::json!([
        user1,
        { "role": "assistant", "content": [
            { "type": "thinking", "thinking": "" },
            { "type": "text", "text": "" },
            bash_use("toolu_et", "ls"),
        ] },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_et", "content": "a.txt" },
        ] },
    ]);
    let (v2, _) = thread_turn(&thread_body("claude-opus-5-5", msgs), all_on());
    assert_eq!(
        v2["thread"],
        serde_json::json!({ "type": "continue", "previous_message_id": "msg_ET" }),
        "{v2}"
    );
}

/// 换模型那一轮来访以 `role: system` 收尾：指令式写法（`content: []` 带 `output_config`）原样
/// 留在最后、模型说明另起一条排在它前面；字符串形态的普通 system 就并进去。两条路都不能丢说明。
#[test]
fn model_notice_survives_a_trailing_client_system_message() {
    use crate::proxy::test_support::{detect_for, rewrite_body};
    let directive = serde_json::json!({
        "role": "system", "output_config": { "effort": "low" }, "content": [],
    });
    let plain = serde_json::json!({ "role": "system", "content": "客户端收尾的 system" });
    let notice = "You are powered by the model named Sonnet 5.5. The exact model ID is \
                  claude-sonnet-5-5. Assistant knowledge cutoff is June 2026.";
    for threads in [false, true] {
        for (tag, tail) in [("指令", &directive), ("字符串", &plain)] {
            let flags = store::ForwardFlags {
                sim_message_threads: threads,
                hoist_system_role: false,
                ..all_on()
            };
            let run = |model: &str, msgs: serde_json::Value| {
                let body =
                    serde_json::json!({ "model": model, "max_tokens": 16, "messages": msgs });
                let b = Bytes::from(body.to_string());
                let sim = detect_for(&b, flags).expect("走模拟");
                let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
                serde_json::from_slice::<serde_json::Value>(&out).unwrap()
            };
            let u1 = serde_json::json!({
                "role": "user", "content": format!("收尾 system·{tag}·{threads} 1"),
            });
            let a1 = serde_json::json!({ "role": "assistant", "content": "好" });
            let u2 = serde_json::json!({ "role": "user", "content": "收尾 system 2" });
            let _ = run("claude-opus-5-5", serde_json::json!([u1]));
            let v = run("claude-sonnet-5-5", serde_json::json!([u1, a1, u2, tail]));
            let out = v["messages"].as_array().unwrap();
            let last = out.last().unwrap();
            let ctx = format!("{tag} threads={threads}: {v}");
            if tag == "指令" {
                assert_eq!(last["content"], serde_json::json!([]), "指令原样留在最后: {ctx}");
                assert_eq!(last["output_config"], directive["output_config"], "{ctx}");
                let text = out[out.len() - 2]["content"][0]["text"].as_str().unwrap();
                assert!(text.starts_with(notice), "说明另起一条、排在指令前: {ctx}");
            } else {
                // 末条断点会把字符串形态改成块数组，两种都认。
                let text = last["content"]
                    .as_str()
                    .or_else(|| last["content"][0]["text"].as_str())
                    .unwrap();
                assert!(text.starts_with("客户端收尾的 system\n\n"), "{ctx}");
                assert!(text.contains(notice), "说明并进去: {ctx}");
            }
        }
    }
}

/// 提升只搬普通 system：指令式那条（`content: []` 带 `output_config`）的意义全在消息级字段上，
/// 搬过去只剩一个空数组，等于连同客户端中途调的 effort 一起删了。它留在原位，上游哪儿都收；
/// 带正文的拆开，正文提升、`output_config` 留成原位的指令。
#[test]
fn hoisting_leaves_system_directives_in_place() {
    let directive = serde_json::json!({
        "role": "system", "output_config": { "effort": "low" }, "content": [],
    });
    let mut v = serde_json::json!({ "messages": [
        { "role": "system", "content": "be brief" },
        { "role": "user", "content": "hi" },
        directive,
        { "role": "assistant", "content": "ok" },
        { "role": "user", "content": "go on" },
    ] });
    assert!(super::hoist_system_role_messages(&mut v));
    assert_eq!(v["system"], serde_json::json!([{ "type": "text", "text": "be brief" }]));
    let msgs = v["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "system", "assistant", "user"], "{v}");
    assert_eq!(msgs[1], directive, "指令原样留着");

    // 带正文又带 `output_config` 的拆成两半：正文提升，原位留一条只有 `output_config` 的指令。
    // 别的消息级字段不跟着留。
    let mut split = serde_json::json!({ "messages": [
        { "role": "system", "output_config": { "effort": "low" }, "clear_at": 3, "content": "be brief" },
        { "role": "user", "content": "hi" },
    ] });
    assert!(super::hoist_system_role_messages(&mut split));
    assert_eq!(split["system"], serde_json::json!([{ "type": "text", "text": "be brief" }]));
    assert_eq!(
        split["messages"][0],
        serde_json::json!({ "role": "system", "output_config": { "effort": "low" }, "content": [] }),
        "{split}"
    );
    assert_eq!(split["messages"][1]["role"], "user");

    let mut only =
        serde_json::json!({ "messages": [{ "role": "user", "content": "hi" }, directive] });
    let before = only.clone();
    assert!(!super::hoist_system_role_messages(&mut only), "只有指令时什么都不搬");
    assert_eq!(only, before);
}

/// 模拟路径上开头夹着一条指令：客户端那段长 system 要挪进首条**用户**消息、环境说明跟在它后面，
/// 指令本身一个字节都不动。不跳过它的话，正文会塞进指令里，指令就成了一条开头的普通 system。
/// 严格检查开（默认，不提升）与关（提升）两种都要成立。
#[test]
fn simulation_skips_a_leading_system_directive() {
    use crate::proxy::test_support::{detect_for, rewrite_body};
    let directive = serde_json::json!({
        "role": "system", "output_config": { "effort": "low" }, "content": [],
    });
    for strict in [true, false] {
        let flags = store::ForwardFlags { reject_openai_shape: strict, ..all_on() };
        let body = serde_json::json!({
            "model": "claude-opus-5-5",
            "max_tokens": 16,
            "system": [{ "type": "text", "text": "客户端规则。".repeat(400) }],
            "messages": [directive, { "role": "user", "content": format!("开头指令·{strict}") }],
        });
        let b = Bytes::from(body.to_string());
        let sim = detect_for(&b, flags).expect("走模拟");
        let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        let ctx = format!("strict={strict}: {v}");
        assert_eq!(msgs[0], directive, "指令原样: {ctx}");
        assert_eq!(msgs[1]["role"], "user", "{ctx}");
        let first = msgs[1]["content"][0]["text"].as_str().unwrap();
        assert!(first.contains("客户端规则。"), "长 system 进了首条用户消息: {ctx}");
        assert!(
            msgs[2]["content"][0]["text"].as_str().unwrap().starts_with("# Environment"),
            "环境说明跟在首条用户消息后面: {ctx}"
        );
    }
}

/// 走一遍模拟改写，返回出站体。
fn sim_run(
    model: &str,
    messages: serde_json::Value,
    flags: store::ForwardFlags,
) -> serde_json::Value {
    use crate::proxy::test_support::{detect_for, rewrite_body};
    let body = serde_json::json!({ "model": model, "max_tokens": 32, "messages": messages });
    let b = Bytes::from(body.to_string());
    let sim = detect_for(&b, flags).expect("走模拟");
    serde_json::from_slice(&rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None)).unwrap()
}

/// 只管一轮的 system（`clear_at: "next_user_message"`）在修补模式下留在原位：提升上去就成了
/// 永久指令。开头那种上游本来就不收，照旧提升。
#[test]
fn hoisting_keeps_turn_scoped_system_messages_in_place() {
    let scoped = serde_json::json!({
        "role": "system", "content": "临时提醒", "clear_at": "next_user_message",
    });
    let flags = store::ForwardFlags { reject_openai_shape: false, ..all_on() };
    let out = sim_run(
        "claude-sonnet-5-5",
        serde_json::json!([{ "role": "user", "content": "临时 system·提升" }, scoped]),
        flags,
    );
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs.last().unwrap(), &scoped, "原样留在最后: {out}");
    assert!(!out["system"].to_string().contains("临时提醒"), "没进顶层 system");

    let mut leading =
        serde_json::json!({ "messages": [scoped, { "role": "user", "content": "hi" }] });
    assert!(super::hoist_system_role_messages(&mut leading));
    assert_eq!(leading["messages"].as_array().unwrap().len(), 1, "开头那种照旧提升");
}

/// 换模型说明与 `<total_tokens>` 不并进只管一轮的 system：并进去下一轮就跟着失效，模型只剩
/// 首轮的 Opus 身份。官方是单独一条排在它前面，断点也在那一条上，`clear_at` 那条仍是末条、
/// 字符串正文、不带断点（`cap/auto-2.1.291-20261006-full/00465`）。线程开与关都一样。
#[test]
fn notices_stay_out_of_turn_scoped_system_messages() {
    for threads in [false, true] {
        let flags = store::ForwardFlags { sim_message_threads: threads, ..all_on() };
        let u1 = serde_json::json!({ "role": "user", "content": format!("临时 system·换模型·{threads}") });
        let a1 = serde_json::json!({ "role": "assistant", "content": "a1" });
        let u2 = serde_json::json!({ "role": "user", "content": "u2" });
        let scoped = serde_json::json!({
            "role": "system", "content": "临时提醒", "clear_at": "next_user_message",
        });
        let _ = sim_run("claude-opus-5-5", serde_json::json!([u1]), flags);
        let v = sim_run("claude-sonnet-5-5", serde_json::json!([u1, a1, u2, scoped]), flags);
        let msgs = v["messages"].as_array().unwrap();
        let ctx = format!("threads={threads}: {v}");
        assert_eq!(msgs.last().unwrap(), &scoped, "临时那条原样、仍是末条: {ctx}");
        let before = &msgs[msgs.len() - 2];
        assert_eq!(before["role"], "system", "{ctx}");
        assert!(before.get("clear_at").is_none(), "{ctx}");
        assert!(before.to_string().contains("named Sonnet 5.5"), "说明在它前面那条里: {ctx}");
        if threads {
            assert!(before.to_string().contains("<total_tokens>"), "{ctx}");
            assert!(before["content"][0].get("cache_control").is_some(), "断点在它身上: {ctx}");
        }
    }
}

/// 只管一轮的 system 上游不许带断点：末条消息的断点落在它前一条上。
#[test]
fn turn_scoped_system_never_gets_a_cache_breakpoint() {
    let flags = store::ForwardFlags { sim_message_threads: false, ..all_on() };
    let out = sim_run(
        "claude-sonnet-5-5",
        serde_json::json!([
            { "role": "user", "content": "临时 system·断点" },
            { "role": "assistant", "content": "ready" },
            { "role": "user", "content": "next" },
            { "role": "system", "content": "临时提醒", "clear_at": "next_user_message" },
        ]),
        flags,
    );
    let msgs = out["messages"].as_array().unwrap();
    let scoped = msgs.last().unwrap();
    assert_eq!(scoped["clear_at"], "next_user_message");
    assert_eq!(scoped["content"], "临时提醒", "正文仍是字符串、没挂断点: {out}");
    let prev = &msgs[msgs.len() - 2];
    assert!(prev["content"].as_array().unwrap().last().unwrap().get("cache_control").is_some());
}

/// 工具参数里叫 `cache_control` 的是业务数据：客户端改了历史里那次调用的这个参数，线程得认出
/// 历史变了、重新 `create`，不能接着 `continue` 把改过的那条切掉。
#[test]
fn sim_threads_see_edits_to_a_business_cache_control_argument() {
    use crate::proxy::session_link::CcSessionLink;
    let tools = serde_json::json!([{
        "name": "mcp__audit__set_cache",
        "description": "set policy",
        "input_schema": { "type": "object", "properties": { "cache_control": { "type": "string" } } },
    }]);
    let run = |messages: serde_json::Value| {
        let body = serde_json::json!({
            "model": "claude-opus-5-5", "max_tokens": 32, "tools": tools, "messages": messages,
        });
        let raw = body.to_string();
        let sim = sim_for(&raw);
        let out = rewrite_body(&Bytes::from(raw), &test_cred(), "fp", all_on(), Some(&sim), None);
        (serde_json::from_slice::<serde_json::Value>(&out).unwrap(), sim.take_thread())
    };
    let call = |value: &str| {
        serde_json::json!({ "type": "tool_use", "id": "toolu_biz", "name": "mcp__audit__set_cache",
            "input": { "cache_control": value } })
    };
    let u1 = serde_json::json!({ "role": "user", "content": "线程测试·业务 cache_control" });
    let result = serde_json::json!({ "role": "user", "content": [
        { "type": "tool_result", "tool_use_id": "toolu_biz", "content": "ok" },
    ] });
    let (_, p1) = run(serde_json::json!([u1]));
    CcSessionLink::record_thread(
        &p1.unwrap(),
        "msg_biz1",
        vec!["toolu_biz".into()],
        reply_of(serde_json::json!([call("no-store")])),
        1000,
    );
    let (v2, p2) = run(
        serde_json::json!([u1, { "role": "assistant", "content": [call("no-store")] }, result]),
    );
    assert_eq!(v2["thread"]["type"], "continue", "{v2}");
    CcSessionLink::record_thread(
        &p2.unwrap(),
        "msg_biz2",
        Vec::new(),
        reply_of("ready".into()),
        2000,
    );
    let (edited, _) = run(serde_json::json!([
        u1,
        { "role": "assistant", "content": [call("max-age=3600")] },
        result,
        { "role": "assistant", "content": "ready" },
        { "role": "user", "content": "next" },
    ]));
    assert_eq!(edited["thread"]["type"], "create", "改过的历史得整段重发: {edited}");
}

/// 客户端 system 文本块带的 `citations` 出站前去掉：上游不收在 system 里（2026-10-07 实测 400
/// `Citations are only allowed on top-level messages text blocks.`）。单块、多块合并、长文本挪进
/// 首条用户消息三条路都一样，用户消息里那份文档不动。
#[test]
fn system_citations_are_dropped() {
    let citations = serde_json::json!([{
        "type": "char_location", "cited_text": "abc", "document_index": 0, "document_title": "T",
        "start_char_index": 0, "end_char_index": 3,
    }]);
    let run = |system: serde_json::Value| {
        let body = serde_json::json!({ "model": "claude-opus-5-5", "max_tokens": 32, "system": system,
            "messages": [{ "role": "user", "content": [
                { "type": "document", "source": { "type": "text", "media_type": "text/plain", "data": "abc" },
                  "citations": { "enabled": true } },
                { "type": "text", "text": "引用测试" },
            ] }] });
        let flags = store::ForwardFlags { sim_message_threads: false, ..all_on() };
        let b = Bytes::from(body.to_string());
        let sim = crate::proxy::test_support::detect_for(&b, flags).unwrap();
        let out = rewrite_body(&b, &test_cred(), "fp", flags, Some(&sim), None);
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()
    };
    for system in [
        serde_json::json!([{ "type": "text", "text": "abc", "citations": citations }]),
        serde_json::json!([
            { "type": "text", "text": "abc", "citations": citations },
            { "type": "text", "text": "另一段指令" },
        ]),
        serde_json::json!([
            { "type": "text", "text": format!("abc{}", "x".repeat(2000)), "citations": citations },
        ]),
    ] {
        let v = run(system);
        assert!(!v["system"].to_string().contains("char_location"), "{v}");
        let first = &v["messages"][0]["content"];
        assert!(!first.to_string().contains("char_location"), "{v}");
        assert_eq!(first.as_array().unwrap().iter().filter(|b| b["type"] == "document").count(), 1);
    }
}

/// 强制工具时只删手动预算那种 thinking（上游实测 400）；adaptive 在 Claude API 上是合法组合，
/// 留着。`any` 与 `tool` 同等对待。
#[test]
fn forced_tool_choice_only_drops_manual_thinking() {
    let tools = serde_json::json!([{ "name": "Bash", "input_schema": { "type": "object" } }]);
    let mut adaptive = serde_json::json!({ "model": "claude-opus-4-8", "thinking": { "type": "adaptive" },
        "tool_choice": { "type": "tool", "name": "Bash" }, "tools": tools, "messages": [] });
    super::strip_extra_fields(&mut adaptive, true);
    assert_eq!(adaptive["thinking"]["type"], "adaptive");
    for choice in [
        serde_json::json!({ "type": "any" }),
        serde_json::json!({ "type": "tool", "name": "Bash" }),
    ] {
        let mut manual = serde_json::json!({ "model": "claude-haiku-4-5",
            "thinking": { "type": "enabled", "budget_tokens": 2000 },
            "tool_choice": choice, "tools": tools, "messages": [] });
        assert!(super::strip_extra_fields(&mut manual, true));
        assert!(manual.get("thinking").is_none(), "{manual}");
    }
}

/// `tool_choice: any` 同样不补 thinking：haiku 补出来的是手动预算那种，上游实测 400
/// `Thinking may not be enabled when tool_choice forces tool use.`
#[test]
fn any_tool_choice_gets_no_injected_thinking() {
    let body = serde_json::json!({ "model": "claude-haiku-4-5-20251001", "max_tokens": 2048,
        "tool_choice": { "type": "any" },
        "tools": [{ "name": "mcp__audit__lookup", "input_schema": { "type": "object" } }],
        "messages": [{ "role": "user", "content": "强制任意工具" }] });
    let raw = body.to_string();
    let sim = sim_for(&raw);
    let v: serde_json::Value = serde_json::from_slice(&rewrite_body(
        &Bytes::from(raw),
        &test_cred(),
        "fp",
        all_on(),
        Some(&sim),
        None,
    ))
    .unwrap();
    assert!(v.get("thinking").is_none(), "{v}");
}

/// haiku `max_tokens: 1024` 时补出来的预算抬到下限 1024、与 `max_tokens` 相等。文档说预算须小于
/// `max_tokens`，但 2026-10-07 实测这一组合上游 200（出站带 interleaved-thinking），照此固定。
#[test]
fn haiku_budget_at_the_floor_matches_the_live_result() {
    let raw = serde_json::json!({ "model": "claude-haiku-4-5-20251001", "max_tokens": 1024,
        "messages": [{ "role": "user", "content": "预算下限" }] })
    .to_string();
    let sim = sim_for(&raw);
    let v: serde_json::Value = serde_json::from_slice(&rewrite_body(
        &Bytes::from(raw),
        &test_cred(),
        "fp",
        all_on(),
        Some(&sim),
        None,
    ))
    .unwrap();
    assert_eq!(v["thinking"]["budget_tokens"], 1024, "{v}");
    assert_eq!(v["max_tokens"], 1024);
}

/// 带 `tool_addition` / `tool_removal` 的 system 整条留在原位：顶层 `system` 只收文本块。文本与
/// 工具变更混在一条里的也一样。
#[test]
fn hoisting_leaves_tool_change_messages_in_place() {
    let removal = serde_json::json!({ "role": "system", "content": [
        { "type": "tool_removal", "name": "lookup" },
    ] });
    let mixed = serde_json::json!({ "role": "system", "content": [
        { "type": "text", "text": "工具有变化" },
        { "type": "tool_addition", "tool": { "name": "lookup", "input_schema": { "type": "object" } } },
    ] });
    let mut v = serde_json::json!({ "messages": [
        { "role": "user", "content": "hi" },
        removal,
        { "role": "assistant", "content": "ok" },
        { "role": "user", "content": "go on" },
        mixed,
    ] });
    let before = v.clone();
    assert!(!super::hoist_system_role_messages(&mut v), "没有能提升的");
    assert_eq!(v, before);
}

/// 工具声明换了假名，按名字指它的协议位置也得跟着换：`tool_addition` / `tool_removal` 的
/// `tool_reference.name`、inline-tools 的 `tool_definition.definition.name`、ToolSearch 结果里的
/// `tool_reference.tool_name`、服务端工具搜索结果的 `tool_references[*].tool_name`。官方名不动；`input_schema` 的 `enum` / `default` 和工具结果正文里
/// 长得一样的对象是业务数据，不动。
#[test]
fn tool_references_follow_the_obfuscated_names() {
    let literal = serde_json::json!({ "type": "tool_reference", "name": "lookup" });
    let mut v = serde_json::json!({
        "tools": [
            { "name": "lookup", "input_schema": { "type": "object", "properties": {
                "target": { "enum": [literal], "default": literal },
            } } },
            { "name": "Bash" },
        ],
        "messages": [
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "t1", "content": [
                    { "type": "tool_reference", "tool_name": "lookup" },
                    { "type": "tool_reference", "tool_name": "Bash" },
                ] },
                { "type": "text", "text": "正文", "data": literal },
            ] },
            { "role": "assistant", "content": [
                { "type": "tool_search_tool_result", "tool_use_id": "srv1", "content": {
                    "type": "tool_search_tool_search_result",
                    "tool_references": [
                        { "type": "tool_reference", "tool_name": "lookup" },
                        { "type": "tool_reference", "tool_name": "Bash" },
                    ],
                } },
            ] },
            { "role": "system", "content": [
                { "type": "tool_addition", "tool": { "type": "tool_reference", "name": "lookup" } },
                { "type": "tool_addition", "tool": { "type": "tool_definition", "definition": {
                    "name": "lookup", "description": "新版",
                    "input_schema": { "type": "object", "properties": { "x": { "default": literal } } },
                } } },
                { "type": "tool_removal", "tool": { "type": "tool_reference", "name": "lookup" } },
            ] },
        ],
    });
    let map = build_tool_name_map(Some(&v)).unwrap();
    let fake = map.forward["lookup"].clone();
    assert!(super::apply_tool_names(&mut v, &map));
    assert_eq!(v["tools"][0]["name"], fake);
    let props = &v["tools"][0]["input_schema"]["properties"]["target"];
    assert_eq!(props["enum"][0], literal, "schema 字面量不动");
    assert_eq!(props["default"], literal);
    let refs = &v["messages"][0]["content"][0]["content"];
    assert_eq!(refs[0]["tool_name"], fake);
    assert_eq!(refs[1]["tool_name"], "Bash", "官方名不混淆");
    assert_eq!(v["messages"][0]["content"][1]["data"], literal, "正文里的不动");
    let server = &v["messages"][1]["content"][0]["content"]["tool_references"];
    assert_eq!(server[0]["tool_name"], fake, "服务端工具搜索的结果也换");
    assert_eq!(server[1]["tool_name"], "Bash");
    let changes = &v["messages"][2]["content"];
    assert_eq!(changes[0]["tool"]["name"], fake);
    let def = &changes[1]["tool"]["definition"];
    assert_eq!(def["name"], fake);
    assert_eq!(def["input_schema"]["properties"]["x"]["default"], literal);
    assert_eq!(changes[2]["tool"]["name"], fake);
}

/// 按 `chunk` 字节一块地喂 SSE 还原，收尾 flush，拼出客户端拿到的全部字节。
fn sse_restore(map: &crate::proxy::ToolNameMap, wire: &[u8], chunk: usize) -> String {
    let mut pending = Vec::new();
    let mut out = Vec::new();
    for c in wire.chunks(chunk) {
        out.extend_from_slice(&map.feed(&mut pending, c, true));
    }
    out.extend_from_slice(&map.flush(&mut pending, true));
    String::from_utf8(out).unwrap()
}

/// 回程只还原协议字段（`tool_use.name`、服务端搜索结果的 `tool_references[*].tool_name`）：
/// `server_tool_use.input` 的搜索模式、正文增量里提到的假名原样留着——客户端会把它们原样带回，
/// 请求侧换不回去。`error` 事件全文还原。逐字节分块喂与整段一致。
#[test]
fn restore_only_touches_protocol_name_fields() {
    let map =
        build_tool_name_map(Some(&serde_json::json!({ "tools": [{ "name": "lookup" }] }))).unwrap();
    let fake = map.forward["lookup"].clone();
    let wire = [
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv1","name":"tool_search_tool_regex","input":{}}}"#.to_string(),
        format!(r#"data: {{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":"{{\"pattern\": \"{fake}\"}}"}}}}"#),
        format!(r#"data: {{"type":"content_block_start","index":1,"content_block":{{"type":"tool_search_tool_result","tool_use_id":"srv1","content":{{"type":"tool_search_tool_search_result","tool_references":[{{"type":"tool_reference","tool_name":"{fake}"}}]}}}}}}"#),
        format!(r#"data: {{"type":"content_block_delta","index":2,"delta":{{"type":"text_delta","text":"I will call {fake} now"}}}}"#),
        format!(r#"data: {{"type":"content_block_start","index":3,"content_block":{{"type":"tool_use","id":"toolu_1","name":"{fake}","input":{{}}}}}}"#),
        "event: error".to_string(),
        format!(r#"data: {{"type":"error","error":{{"type":"invalid_request_error","message":"tool {fake} is broken"}}}}"#),
    ]
    .join("\n\n")
        + "\n\n";
    let whole = sse_restore(&map, wire.as_bytes(), wire.len());
    assert!(whole.contains(r#""tool_name":"lookup""#), "{whole}");
    assert!(whole.contains(r#""name":"lookup","input""#), "{whole}");
    assert!(whole.contains(&format!(r#"\"pattern\": \"{fake}\""#)), "搜索输入不动: {whole}");
    assert!(whole.contains(&format!("I will call {fake} now")), "正文不动: {whole}");
    assert!(whole.contains("tool lookup is broken"), "error 事件全文还原: {whole}");
    assert_eq!(sse_restore(&map, wire.as_bytes(), 1), whole, "分块还原与整段一致");
}

/// 整段 JSON（非流式透传、聚合出来的）按块还原：`tool_use.name` 换回真名，`input` 里恰好叫
/// `name` / `tool_name`、值等于假名的业务参数不动——流式下它走转义过的参数增量，本来也不动，
/// 两条路得一致。error 体全文还原；不含假名的原样交回。
#[test]
fn json_body_restore_keeps_tool_arguments() {
    let map =
        build_tool_name_map(Some(&serde_json::json!({ "tools": [{ "name": "lookup" }] }))).unwrap();
    let fake = map.forward["lookup"].clone();
    let msg = serde_json::json!({ "type": "message", "role": "assistant", "content": [
        { "type": "tool_use", "id": "toolu_1", "name": fake,
          "input": { "name": fake, "tool_name": fake } },
    ] });
    let body = serde_json::to_vec(&msg).unwrap();
    let out: serde_json::Value = serde_json::from_slice(&map.restore_json_body(&body)).unwrap();
    assert_eq!(out["content"][0]["name"], "lookup");
    assert_eq!(out["content"][0]["input"], msg["content"][0]["input"], "参数不动");
    let err = serde_json::json!({ "type": "error", "error": { "message": format!("tool {fake} failed") } });
    let out = String::from_utf8(map.restore_json_body(&serde_json::to_vec(&err).unwrap())).unwrap();
    assert!(out.contains("tool lookup failed"), "{out}");
    let plain = br#"{"type":"message","content":[]}"#;
    assert_eq!(map.restore_json_body(plain), plain.to_vec());
    // 非 SSE 时 feed 攒着，flush 一次处理。
    let mut pending = Vec::new();
    assert!(map.feed(&mut pending, &body, false).is_empty());
    let flushed: serde_json::Value =
        serde_json::from_slice(&map.flush(&mut pending, false)).unwrap();
    assert_eq!(flushed["content"][0]["name"], "lookup");
}

/// 端到端：上游回了一段带服务端工具搜索（输入里有假名）与工具调用的回复，客户端拿到还原后的
/// 那份、原样带回下一轮——线程照样接得上。
#[test]
fn sim_threads_continue_after_a_server_tool_search_reply() {
    use crate::proxy::session_link::CcSessionLink;
    let tools = serde_json::json!([{
        "name": "lookup", "description": "look up", "defer_loading": true,
        "input_schema": { "type": "object", "properties": { "q": { "type": "string" } } },
    }]);
    let map = super::build_tool_name_map(Some(&serde_json::json!({ "tools": tools }))).unwrap();
    let fake = map.forward["lookup"].clone();
    let turn = |msgs: serde_json::Value| {
        let body = serde_json::json!({
            "model": "claude-opus-5-5", "max_tokens": 32000, "tools": tools, "messages": msgs,
        });
        let raw = Bytes::from(body.to_string());
        let sim = sim_for(&body.to_string());
        let out = super::rewrite_body_out(
            &raw,
            &test_cred(),
            "fp",
            all_on(),
            Some(&sim),
            None,
            None,
            false,
            Some(&map),
            true,
            true,
            None,
            None,
            crate::proxy::CcRequestKind::Main,
            None,
        )
        .0;
        (serde_json::from_slice::<serde_json::Value>(&out).unwrap(), sim.take_thread())
    };
    let upstream_reply = serde_json::json!([
        { "type": "server_tool_use", "id": "srv1", "name": "tool_search_tool_regex",
          "input": { "pattern": fake } },
        { "type": "tool_search_tool_result", "tool_use_id": "srv1", "content": {
            "type": "tool_search_tool_search_result",
            "tool_references": [{ "type": "tool_reference", "tool_name": fake }],
        } },
        { "type": "tool_use", "id": "toolu_s", "name": fake, "input": { "q": "x" } },
    ]);
    let user1 = serde_json::json!({ "role": "user", "content": "线程测试·服务端搜索" });
    let (_, p1) = turn(serde_json::json!([user1]));
    CcSessionLink::record_thread(
        &p1.unwrap(),
        "msg_SRV",
        vec!["toolu_s".into()],
        reply_of(upstream_reply.clone()),
        1000,
    );
    // 客户端收到的是回程还原过的那份（非流式：整段 Message 按块还原）。
    let message =
        serde_json::json!({ "type": "message", "role": "assistant", "content": upstream_reply });
    let restored: serde_json::Value =
        serde_json::from_slice(&map.restore_json_body(&serde_json::to_vec(&message).unwrap()))
            .unwrap();
    let restored = restored["content"].clone();
    assert_eq!(restored[2]["name"], "lookup");
    assert_eq!(restored[0]["input"]["pattern"], fake.as_str());
    let (v2, _) = turn(serde_json::json!([
        user1,
        { "role": "assistant", "content": restored },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_s", "content": "ok" },
        ] },
    ]));
    assert_eq!(
        v2["thread"],
        serde_json::json!({ "type": "continue", "previous_message_id": "msg_SRV" }),
        "{v2}"
    );
}
