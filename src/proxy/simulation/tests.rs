use crate::proxy::test_support::{
    API_SHAPE_BODY, PLAIN_BODY, all_on, base_block, detect_for, detect_with, parsed,
    platform_headers, rewrite_body, sim_for, test_cred,
};
use crate::proxy::{Bytes, HeaderValue, build_forward_headers, config, header, store};

/// 普通请求 → 官方四块 system（sonnet 族，2.1.258 无 reporting）：billing / 身份句 /
/// 基座（global）/ 客户端原文。基座按模型族选，且 `system` 落在 `messages` 之后（官方 key 序）。
#[test]
fn simulates_official_system_for_plain_request() {
    let body = Bytes::from(
            r#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"hi"}],"system":"你是助手","max_tokens":8}"#
                .to_string(),
        );
    let sim = detect_for(&body, all_on()).unwrap();
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let s = String::from_utf8(out.to_vec()).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();

    assert_eq!(sys.len(), 5, "官方四块（sonnet 无 reporting）+ 客户端自己那块: {s}");
    assert!(
        sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"),
        "第 0 块应是 billing header: {s}"
    );
    assert!(sys[0]["text"].as_str().unwrap().contains("; cch="), "cch 要在 billing 段里");
    assert!(
        sys[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("x-anthropic-billing-header: cc_version=2.1.291.926; cc_entrypoint=cli;"),
        "模拟路径的 cc_version 取 profile 的版本，后缀按首句派生（`hi` 取不到第 4/7/20 位，\
             按 `000` 算）: {s}"
    );
    // 官方主线程每条新输入都带 cc_prompt_id，2.1.277 起后面跟 cc_turn_origin=human
    // （cap/2.1.277/00023），2.1.285 起再跟会话里的第几轮（cap/2.1.285/00030）。几条用例
    // 共用同一张会话表、同一个会话 id，轮次不一定从 1 起，只钉形态：两个数相等且不小于 1。
    let billing = sys[0]["text"].as_str().unwrap();
    let (_, tail) = billing.split_once("; cc_turn_origin=human; cc_prompt_index=").expect(&s);
    let (prompt, turn) = tail.split_once("; cc_turn_index=").expect(&s);
    assert_eq!(Some(prompt), turn.strip_suffix(';'), "{s}");
    assert!(prompt.parse::<u32>().is_ok_and(|n| n >= 1), "{s}");
    assert!(sys[0]["text"].as_str().unwrap().contains("; cc_prompt_id="), "{s}");
    assert_eq!(sys[1]["text"], config::CC_SYSTEM_IDENTITY, "第 1 块必须是那句身份声明");
    assert!(sys[1].get("cache_control").is_none(), "身份句不带断点（官方如此）");
    assert_eq!(sys[2]["text"], config::CC_SYSTEM_BASE, "2.1.277 起四族同一份基座");
    assert_eq!(sys[2]["cache_control"]["scope"], "global");
    // 第四块是官方「其余」段（模板填好占位），客户端自己的 system 不再占一块。
    let rest = sys[3]["text"].as_str().unwrap();
    assert!(rest.starts_with("Write code that reads like"), "2.1.285 第四块: {rest:.80}");
    assert!(!unfilled(rest), "占位要全填掉: {rest}");
    let env = crate::proxy::sim_env_for(&test_cred(), "fp");
    assert!(
        rest.contains(&format!(
            "You have a persistent file-based memory at `{}/.claude/projects/{}/memory/`.",
            env.home, env.slug
        )),
        "记忆目录随派生的假环境走: {rest}"
    );
    assert!(
        rest.ends_with("<total_tokens>15000000 tokens left</total_tokens>"),
        "第四块末尾那行照抓包，客户端 system 一个字都不掺: {rest}"
    );
    assert_eq!(sys[4]["text"], "你是助手", "客户端 system 单独占末块（0.3.154）: {s}");
    assert_eq!(
        v["output_config"],
        serde_json::json!({"effort": "high"}),
        "sonnet 主线程带 output_config.effort=high（cap/2.1.280/00033）: {s}"
    );
    assert_eq!(sys[3]["cache_control"]["type"], "ephemeral");
    assert!(sys[3]["cache_control"].get("scope").is_none(), "只有基座标 global");
    assert_eq!(sys[2]["cache_control"]["ttl"], "1h", "基座该带 ttl: {s}");
    assert_eq!(sys[3]["cache_control"]["ttl"], "1h", "末块也该带 ttl: {s}");
    // 客户端原 system 留在 system 里（单独的末块），首条用户消息一个字都没多。
    let first = v["messages"][0]["content"].as_array().unwrap();
    assert_eq!(first.len(), 1, "首条消息不再被塞东西: {s}");
    assert_eq!(first[0]["text"], "hi", "首条消息正文原样: {s}");
    assert!(!s.contains(crate::proxy::CLIENT_SYSTEM_REMINDER_LEAD), "短 system 不该挪进消息: {s}");
    assert!(!s.contains("system_instructions"), "自创标签一个都不能有: {s}");
    assert!(!s.contains("(see conversation)"), "没有占位块: {s}");

    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "model",
            "messages",
            "system",
            "tools",
            "metadata",
            "max_tokens",
            "output_config",
            "thread",
            "diagnostics"
        ],
        "key 序（来访没带 tools 也补在 system 之后；output_config 在 max_tokens 之后、\
             diagnostics 之前）: {s}"
    );
    assert_eq!(
        v["diagnostics"],
        serde_json::json!({"previous_message_id": serde_json::Value::Null}),
        "会话首条：字段在、值为 null（cap/2.1.277/00023）: {s}"
    );

    // 2.1.291：opus / sonnet / fable 同一份基座，haiku 换成长的那份；认不出的模型不猜。
    for m in [
        "claude-opus-5",
        "claude-fable-5-1",
        "claude-fable-5",
        "claude-sonnet-5",
        "claude-haiku-4-5-20251001",
        "claude-opus-4-6",
    ] {
        let sim = sim_for(&format!(r#"{{"model":"{m}","messages":[]}}"#));
        let want =
            if m.contains("haiku") { config::CC_SYSTEM_BASE_HAIKU } else { config::CC_SYSTEM_BASE };
        assert_eq!(sim.base, Some(want), "{m}");
        assert!(sim.rest.is_some(), "{m} 也补第四块");
    }
    let alien = sim_for(r#"{"model":"gpt-4o","messages":[]}"#);
    assert!(alien.base.is_none(), "认不出的模型不猜基座");
    assert!(alien.rest.is_none(), "也不补第四块");
}

/// billing 后缀按会话首条**非 meta** 用户文本派生（[`crate::proxy::cc_version_suffix`]），
/// 抓包里那几个「固定值」逐个用它们各自会话的首句复算回来：前面塞多少条
/// `<system-reminder>` / `<local-command-caveat>` 都不影响，`<command-name>` 不算 meta。
#[test]
fn billing_suffix_is_derived_from_the_first_real_user_text() {
    let reminder = "<system-reminder>\nAs you answer the user's questions, you can use the \
                        following context:\n</system-reminder>";
    let body = |texts: &[&str]| {
        let content: Vec<_> =
            texts.iter().map(|t| serde_json::json!({"type": "text", "text": t})).collect();
        serde_json::json!({
                "model": "claude-opus-5-5",
                "messages": [{"role": "user", "content": content}]})
    };
    let text =
        |texts: &[&str], ver: &str| crate::proxy::billing_header_text(&body(texts), Some(ver));

    // `cap/2.1.280/00021`：会话首句 `hilew`。
    assert!(text(&[reminder, "hilew"], "2.1.280").contains("cc_version=2.1.280.bc5;"));
    // `cap/2.1.277/00023`：两条 reminder 之后是 `审查下性能优化的问题`。
    assert!(
        text(&[reminder, reminder, "审查下性能优化的问题"], "2.1.277")
            .contains("cc_version=2.1.277.d56;")
    );
    // `cap/2.1.260/00018`：caveat 是 meta，斜杠命令那块不是。
    let caveat = "<local-command-caveat>Caveat: The messages below were generated by the \
                      user while running local commands.</local-command-caveat>";
    let cmd = "<command-name>/commit-commands:commit</command-name>";
    assert!(text(&[reminder, caveat, cmd], "2.1.260").contains("cc_version=2.1.260.bcd;"));
    // 2.1.258 那五份全是 `"hi"` 开头的会话。
    let hi = serde_json::json!({"model": "claude-opus-5", "messages": [{"role": "user", "content": "hi"}]});
    assert!(crate::proxy::billing_header_text(&hi, Some("2.1.258")).contains(".1e2;"));
    // 同一个首句换个版本，后缀跟着变：版本号参与摘要。
    assert!(!text(&[reminder, "hilew"], "2.1.277").contains(".bc5;"));
    // 只有 meta 块的首条消息按空串算（2.1.277 子代理的 `385` 就是空串的值）。
    assert!(text(&[reminder], "2.1.277").contains("cc_version=2.1.277.385;"));
}

/// uuid 形态校验的边界：只认 `8-4-4-4-12` 的小写 hex。
#[test]
fn session_id_must_look_like_a_uuid() {
    let ok = crate::proxy::looks_like_uuid;
    assert!(ok("d0c1fb05-9b19-4576-9465-e2b8a206dabf"));
    // 不看 version/variant 位：v7 之类同样是个正常的会话 id，没理由拦。
    assert!(ok("00000000-0000-0000-0000-000000000000"));
    assert!(!ok("D0C1FB05-9B19-4576-9465-E2B8A206DABF"), "大写不是官方形态");
    assert!(!ok("d0c1fb05-9b19-4576-9465-e2b8a206dab"), "末段少一位");
    assert!(!ok("d0c1fb05-9b19-4576-9465-e2b8a206dabff"), "末段多一位");
    assert!(!ok("d0c1fb05-9b19-4576-9465-e2b8a206dabf-x"), "多一段");
    assert!(!ok("d0c1fb05_9b19_4576_9465_e2b8a206dabf"), "分隔符不对");
    assert!(!ok("g0c1fb05-9b19-4576-9465-e2b8a206dabf"), "非 hex");
    assert!(!ok(""));
}

/// 超长客户端 system 搬不动时，**一个字节都不许改**。
///
/// 原来的实现先把末块换成 `(see conversation)` 占位、再去写 `messages[0]`，落点不可写
/// 时就直接 `return false`——客户端明确下的那段指令凭空消失，而调用方只看到一个
/// `false`，以为什么都没发生。
///
/// 有没有第四块都一样有活干：客户端 system 恒为独立的末块（0.3.154），超长就搬。
#[test]
fn long_client_system_is_relocated_atomically() {
    let long = "指令".repeat(1200); // 远超 MAX_CLIENT_SYSTEM_BYTES
    let no_rest = store::ForwardFlags { simulate_full_system: false, ..all_on() };
    // messages[0].content 是数字：既不是数组也不是字符串，搬不过去。
    let body = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 64000,
            "messages": [{"role": "user", "content": 42}],
            "system": long});
    let raw = Bytes::from(serde_json::to_vec(&body).unwrap());
    let sim = detect_for(&raw, no_rest).expect("该请求应走模拟路径");
    assert!(sim.rest.is_none());
    let mut v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert!(crate::proxy::simulate_system(
        &mut v,
        &sim,
        crate::proxy::CacheShape { global: true, ttl_1h: true }
    ));
    let before = v.clone();
    assert!(!crate::proxy::relocate_long_client_system(&mut v, &sim), "搬不动就该返回 false");
    assert_eq!(v, before, "搬不动时 body 必须原样不动，不能只剩一个占位块");
    let tail = v["system"].as_array().unwrap().last().unwrap();
    assert!(tail["text"].as_str().unwrap().contains("指令"), "客户端那段 system 还在: {tail}");

    // messages[0] 可写时照常搬走，末块换占位。
    let ok = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 64000,
            "messages": [{"role": "user", "content": "hi"}],
            "system": long});
    let raw = Bytes::from(serde_json::to_vec(&ok).unwrap());
    let mut v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert!(crate::proxy::simulate_system(
        &mut v,
        &sim,
        crate::proxy::CacheShape { global: true, ttl_1h: true }
    ));
    assert!(crate::proxy::relocate_long_client_system(&mut v, &sim));
    let tail = v["system"].as_array().unwrap().last().unwrap();
    assert_eq!(tail["text"], "(see conversation)", "末块换成占位");
    let first = v["messages"][0]["content"].as_str().unwrap();
    assert!(first.starts_with("<system-reminder>\n"), "内容搬到了首条消息: {first}");
    assert!(first.contains("指令"), "内容没丢");

    // 有第四块时一样：客户端 system 是第 5 块，超长照搬，第四块的官方正文一个字不掺。
    let sim = detect_for(&raw, all_on()).unwrap();
    assert!(sim.rest.is_some());
    let mut v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert!(crate::proxy::simulate_system(
        &mut v,
        &sim,
        crate::proxy::CacheShape { global: true, ttl_1h: true }
    ));
    assert_eq!(v["system"].as_array().unwrap().len(), 5, "{v}");
    assert!(v["system"][4]["text"].as_str().unwrap().contains("指令"), "客户端 system 占末块");
    assert!(
        v["system"][3]["text"].as_str().unwrap().ends_with("</total_tokens>"),
        "第四块是官方正文，一个字都没掺"
    );
    assert!(crate::proxy::relocate_long_client_system(&mut v, &sim), "超长该搬走");
    assert_eq!(v["system"][4]["text"], "(see conversation)", "末块换成占位");
    assert!(v["messages"][0]["content"].as_str().unwrap().contains("指令"), "内容没丢");

    // 落点不可写（content 是数字）：搬不动，末块留着客户端原文，body 一个字节不动。
    let raw = Bytes::from(serde_json::to_vec(&body).unwrap());
    let mut v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert!(crate::proxy::simulate_system(
        &mut v,
        &sim,
        crate::proxy::CacheShape { global: true, ttl_1h: true }
    ));
    let before = v.clone();
    assert_eq!(v["system"].as_array().unwrap().len(), 5, "{v}");
    assert!(!crate::proxy::relocate_long_client_system(&mut v, &sim), "搬不动就该返回 false");
    assert_eq!(v, before, "搬不动时 body 必须原样不动");
    assert!(v["system"][4]["text"].as_str().unwrap().contains("指令"), "客户端那段还在");
}

/// 没有 system 的请求同样成立：opus 族四块（billing / 身份句 / 基座 / 其余），与
/// `cap/2.1.260-2/00013` 逐块同形：基座 `{ttl:1h, scope:global}`，第四块 `{ttl:1h}`。
#[test]
fn simulates_system_when_client_sent_none() {
    let body = Bytes::from(PLAIN_BODY.to_string());
    let sim = sim_for(PLAIN_BODY);
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    assert_eq!(sys.len(), 4, "没有客户端 system 也是官方四块: {v}");
    assert_eq!(sys[2]["text"], config::CC_SYSTEM_BASE);
    assert_eq!(sys[2]["cache_control"]["scope"], "global");
    let rest = sys[3]["text"].as_str().unwrap();
    assert!(rest.starts_with("Write code that reads like"), "2.1.285 第四块");
    let env = crate::proxy::sim_env_for(&test_cred(), "fp");
    assert!(
        rest.contains(&format!("`{}/.claude/projects/{}/memory/`", env.home, env.slug)),
        "记忆目录随派生的 cwd 走: {rest}"
    );
    // 2.1.277 起第四块不再写工作目录、模型名、知识截止与 scratchpad 一节（2.1.280 记忆段里
    // 那句「scratchpad prose」是正文，不是那一节）。
    for gone in [
        "Primary working directory",
        "powered by the model",
        "knowledge cutoff",
        "# Scratchpad Directory",
    ] {
        assert!(!rest.contains(gone), "{gone} 在 2.1.277 起的第四块里已经没有了: {rest}");
    }
    assert!(!unfilled(rest), "{rest}");
    assert_eq!(sys[3]["cache_control"], serde_json::json!({"type": "ephemeral", "ttl": "1h"}));
    assert_eq!(
        v["output_config"],
        serde_json::json!({"effort": "high"}),
        "模拟路径 opus 按 high 发（官方默认 medium，见 config::CC_PROFILES）: {v}"
    );
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    let idx =
        |k: &str| keys.iter().position(|x| *x == k).unwrap_or_else(|| panic!("{k}: {keys:?}"));
    assert!(idx("context_management") < idx("output_config"), "{keys:?}");
    assert!(idx("output_config") < idx("diagnostics"), "{keys:?}");
    assert!(idx("diagnostics") < idx("stream"), "{keys:?}");
    // haiku 主线程官方不带 output_config（cap/2.1.277/00046）。
    let haiku = Bytes::from(
            r#"{"model":"claude-haiku-4-5-20251001","max_tokens":32000,"messages":[{"role":"user","content":"hi"}]}"#
                .to_string(),
        );
    let hs = detect_for(&haiku, all_on()).unwrap();
    let hv: serde_json::Value = serde_json::from_slice(&rewrite_body(
        &haiku,
        &test_cred(),
        "fp",
        all_on(),
        Some(&hs),
        None,
    ))
    .unwrap();
    assert!(hv.get("output_config").is_none(), "haiku 不带 output_config: {hv}");
    assert_eq!(
        hv["thinking"],
        serde_json::json!({"budget_tokens": 31999, "type": "enabled", "display": "updates"}),
        "haiku 的 thinking（cap/2.1.277/00046）: {hv}"
    );
    // 客户端自己写了 output_config 就不动。
    let own = Bytes::from(
            r#"{"model":"claude-opus-5","max_tokens":1024,"messages":[{"role":"user","content":"hi"}],"output_config":{"effort":"low"}}"#
                .to_string(),
        );
    let os = detect_for(&own, all_on()).unwrap();
    let ov: serde_json::Value =
        serde_json::from_slice(&rewrite_body(&own, &test_cred(), "fp", all_on(), Some(&os), None))
            .unwrap();
    assert_eq!(ov["output_config"], serde_json::json!({"effort": "low"}), "{ov}");
    assert_eq!(v["messages"][0]["content"][0]["text"], "hi", "没有客户端 system 就不动首条消息");

    // 开关关掉：回到「末块是客户端原文」的旧形态，没有客户端 system 就只有前三块。
    let off = store::ForwardFlags { simulate_full_system: false, ..all_on() };
    let sim = detect_for(&body, off).unwrap();
    assert!(sim.rest.is_none());
    let out = rewrite_body(&body, &test_cred(), "fp", off, Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["system"].as_array().unwrap().len(), 3, "开关关着只有前三块: {v}");
    let with_sys = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"}],"system":"你是助手"}"#
                .to_string(),
        );
    let sim = detect_for(&with_sys, off).unwrap();
    let out = rewrite_body(&with_sys, &test_cred(), "fp", off, Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    assert_eq!(sys.len(), 4);
    assert_eq!(sys[3]["text"], "你是助手", "开关关着客户端原文留在末块");
    assert_eq!(v["messages"][0]["content"][0]["text"], "hi");
}

/// 2.1.277 的 fable 不再单独带 `# Reporting outcomes` 块（`cap/2.1.277/00023`），与其余
/// 三族一样是四块 `[billing, 身份句, 基座, 其余]`；thinking 补成 `{adaptive, display:"updates"}`
/// 且 `display` 不被 `strip_extra_fields` 剥掉。
#[test]
fn simulates_fable_with_reporting_block_and_display_updates() {
    let body = concat!(
        r#"{"model":"claude-fable-5-1","max_tokens":64000,"#,
        r#""messages":[{"role":"user","content":"hi"}],"system":"你是助手"}"#
    );
    let reporting = |b: &str| sim_for(b).profile.system == config::CcSystemShape::IdentityReporting;
    let b = Bytes::from(body.to_string());
    let sim = sim_for(body);
    for (b, name) in [
        (body, "fable"),
        (PLAIN_BODY, "opus"),
        (r#"{"model":"claude-sonnet-5","messages":[]}"#, "sonnet"),
        (r#"{"model":"claude-haiku-4-5-20251001","messages":[]}"#, "haiku"),
    ] {
        assert!(!reporting(b), "2.1.277 起 {name} 族都不带 reporting 块");
    }
    let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    assert_eq!(sys.len(), 5, "fable 族也是官方四块 + 客户端那块: {v}");
    assert!(sys.iter().all(|b| b["text"] != config::CC_SYSTEM_REPORTING), "没有 reporting 块: {v}");
    assert_eq!(sys[2]["text"], config::CC_SYSTEM_BASE, "四族共用基座");
    assert_eq!(sys[2]["cache_control"]["scope"], "global");
    let rest = sys[3]["text"].as_str().unwrap();
    assert!(
        rest.starts_with("Before you start, say in a line"),
        "2.1.291 fable 第四块: {rest:.80}"
    );
    assert!(
        rest.contains("This iteration of Claude is Claude Fable 5.1"),
        "2.1.291 fable 又带自我介绍"
    );
    assert!(!unfilled(rest), "{rest}");
    assert_eq!(sys[3]["cache_control"], serde_json::json!({"type": "ephemeral", "ttl": "1h"}));
    assert_eq!(v["output_config"], serde_json::json!({"effort": "high"}), "{v}");
    assert!(
        sys[0]["text"].as_str().unwrap().starts_with(
            "x-anthropic-billing-header: cc_version=2.1.291.926; cc_entrypoint=cli; cch="
        ),
        "{v}"
    );
    assert!(rest.ends_with("</total_tokens>"), "第四块是官方正文，客户端的不掺进来");
    assert_eq!(sys[4]["text"], "你是助手", "客户端 system 单独占末块（0.3.154）");
    assert_eq!(v["messages"][0]["content"][0]["text"], "hi", "首条消息正文原样");
    assert_eq!(
        v["thinking"],
        serde_json::json!({"type": "adaptive", "display": "updates"}),
        "fable 的 thinking 形态（cap/2.1.260/00018）: {v}"
    );
    assert_eq!(
        v["fallbacks"],
        serde_json::Value::Null,
        "没给 fallbacks 字面量（该族 refusal fallback 开关关着）时不替用户开: {v}"
    );
    assert!(
        sim.profile.beta.contains("thinking-display-updates-2026-08-18"),
        "display:updates 要有对应 beta"
    );
}

/// 模拟路径要补官方那**第三个**缓存断点：`cap/raw` 八份抓包每条恰好 3 个，前两个在
/// `system`，第三个恒在最后一条消息的最后一块上（六份非 haiku 落在末尾那条 `role:"system"`
/// 消息，两份 haiku 没那条消息就落在 `user` 末块——规则是位置不是角色）。
/// 顺带把裸字符串 `content` 收成官方那样的块数组，否则断点无处可挂。
#[test]
fn simulated_body_carries_official_message_breakpoint() {
    // 只验断点落位：`<total_tokens>` 提醒会把末条的断点挪到它身上（另有用例），这里关掉它。
    let on = crate::store::ForwardFlags { sim_message_threads: false, ..all_on() };
    // 字符串 content：要转成块数组，断点落在末块。
    let body = Bytes::from(PLAIN_BODY.to_string());
    let sim = sim_for(PLAIN_BODY);
    let out = rewrite_body(&body, &test_cred(), "fp", on, Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let blocks = v["messages"][0]["content"].as_array().expect("content 该收成块数组");
    assert_eq!(blocks[0]["type"], "text", "转出来的该是官方那种文本块");
    assert_eq!(blocks[0]["text"], "hi", "正文一个字都不该变");
    assert_eq!(blocks.last().unwrap()["cache_control"]["type"], "ephemeral", "末块该有断点");
    // 消息这个断点不带 `scope`（官方只在基座标），但跟着开关带 `ttl`。
    assert!(blocks.last().unwrap()["cache_control"].get("scope").is_none(), "只有基座标 global");
    assert_eq!(blocks.last().unwrap()["cache_control"]["ttl"], "1h");

    // 多轮对话：断点只落在**最后一条**消息上，前面的不动。
    let multi = concat!(
        r#"{"model":"claude-opus-5","max_tokens":16,"messages":["#,
        r#"{"role":"user","content":"a"},{"role":"assistant","content":"b"},"#,
        r#"{"role":"user","content":"c"}]}"#
    );
    let b = Bytes::from(multi.to_string());
    let sim = sim_for(multi);
    let out = rewrite_body(&b, &test_cred(), "fp", on, Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    for (i, m) in msgs.iter().enumerate() {
        let last = m["content"].as_array().unwrap().last().unwrap();
        assert_eq!(
            last.get("cache_control").is_some(),
            i == 2,
            "断点只该在最后一条消息上，第 {i} 条不对: {v}"
        );
    }

    // 客户端自己标过就不再多标一个；总数封顶 4，满了不补。
    // （`ttl` 会由 [`crate::proxy::fill_cache_ttl`] 补齐——三个断点要么都有、要么都没有。）
    let mine = concat!(
        r#"{"model":"claude-opus-5","max_tokens":16,"messages":[{"role":"user","content":["#,
        r#"{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]}"#
    );
    let b = Bytes::from(mine.to_string());
    let sim = sim_for(mine);
    let out = rewrite_body(&b, &test_cred(), "fp", on, Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v["messages"][0]["content"][0]["cache_control"],
        serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
        "客户端那个断点只该补 ttl，不该多出 scope 或被换掉 type: {v}"
    );
    assert_eq!(v["messages"][0]["content"][0]["text"], "hi", "正文一个字都不该动: {v}");
    assert!(
        crate::proxy::count_cache_control(&v) <= crate::proxy::MAX_CACHE_BREAKPOINTS,
        "断点超上限: {v}"
    );

    // 非模拟路径**不新标断点**：CC 形态的来访自己就标好了第三个断点，替它再标一个只会
    // 多占预算。唯一会动的是给那个断点补 `ttl`（[`crate::proxy::fill_cache_ttl`]），正文与断点
    // 位置都不变。
    let cc = Bytes::from(API_SHAPE_BODY);
    let before: serde_json::Value = serde_json::from_slice(&cc).unwrap();
    let out = rewrite_body(&cc, &test_cred(), "fp", all_on(), None, None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        crate::proxy::count_cache_control_in(&v["messages"]),
        crate::proxy::count_cache_control_in(&before["messages"]),
        "非模拟路径不该给 messages 新加断点: {v}"
    );
    assert_eq!(
        v["messages"][0]["content"][0]["text"], before["messages"][0]["content"][0]["text"],
        "正文不该被动: {v}"
    );
    assert_eq!(
        v["messages"][0]["content"][0]["cache_control"],
        serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
        "来访那个断点该补上 ttl，与 system 两个保持一致: {v}"
    );

    // 末块是 `tool_result` 也标：官方样本 `cap/2.1.260/00025`、`00029` 里最后一条 user
    // 消息的末块就是 tool_result、带 `{type:ephemeral}`。agent 循环每一轮都以 tool_result
    // 收尾，不标它就等于整个循环的 messages 永远没有断点（`req_Fxs76cgvc57N5GNr`：77 条
    // 消息、10 万 token 裸算、cache_creation 为 0）。
    let tool_loop = concat!(
        r#"{"model":"claude-opus-5","max_tokens":16,"messages":["#,
        r#"{"role":"user","content":"ls"},"#,
        r#"{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"ls"}}]},"#,
        r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"a.txt"}]}]}"#
    );
    let b = Bytes::from(tool_loop.to_string());
    let sim = sim_for(tool_loop);
    let out = rewrite_body(&b, &test_cred(), "fp", on, Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let msgs = v["messages"].as_array().unwrap();
    let last = msgs.last().unwrap()["content"].as_array().unwrap().last().unwrap();
    assert_eq!(last["type"], "tool_result", "末块该还是 tool_result: {v}");
    assert_eq!(
        last["cache_control"],
        serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
        "tool_result 末块该被标上第三个断点: {v}"
    );
    assert_eq!(last["tool_use_id"], "tu_1", "tool_result 其余字段一个不动: {v}");
    assert_eq!(last["content"], "a.txt", "tool_result 正文一个字不动: {v}");
    assert!(
        msgs[1]["content"][0].get("cache_control").is_none(),
        "前一条的 tool_use 不该被标: {v}"
    );
    assert_eq!(
        crate::proxy::count_cache_control_in(&v["messages"]),
        1,
        "messages 里恰好一个断点: {v}"
    );

    // 末块不是**非空 text / tool_result** 时一律不标：`thinking` 那种块上游还要验签名，
    // 往它上面挂 cache_control 是拿能发的请求去赌没样本的组合；image 没有样本。
    for (label, tail) in [
        ("thinking 块", r#"{"type":"thinking","thinking":"想","signature":"AAAA"}"#),
        ("空 text 块", r#"{"type":"text","text":""}"#),
        (
            "image 块",
            r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA"}}"#,
        ),
    ] {
        let body = format!(
            r#"{{"model":"claude-opus-5","max_tokens":16,"messages":[{{"role":"user","content":"hi"}},{{"role":"assistant","content":[{tail}]}}]}}"#
        );
        let b = Bytes::from(body.clone());
        let sim = sim_for(&body);
        let out = rewrite_body(&b, &test_cred(), "fp", on, Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let last = v["messages"].as_array().unwrap().last().unwrap();
        let blk = last["content"].as_array().unwrap().last().unwrap();
        assert!(blk.get("cache_control").is_none(), "{label} 不该被标断点: {v}");
    }

    // 空串 content 不转成空 text 块（那种块上游会拒），原样留着。
    let empty = concat!(
        r#"{"model":"claude-opus-5","max_tokens":16,"#,
        r#""messages":[{"role":"user","content":"hi"},{"role":"assistant","content":""}]}"#
    );
    let b = Bytes::from(empty.to_string());
    let sim = sim_for(empty);
    let out = rewrite_body(&b, &test_cred(), "fp", on, Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["messages"][1]["content"], "", "空串不该被转成空 text 块: {v}");
}

/// 已经是 CC 形态的请求一个字节都不该多改——判据是 `system` 里那句身份声明，
/// 字符串形态与数组形态都认。
#[test]
fn cc_shaped_from_non_cc_client_is_simulated() {
    // 非 CC 客户端（无 UA / Go-http-client 等）抄了 CC 的 system 但没带 metadata.user_id：
    // 应走模拟，把 headers 统一换成官方形态，body 里那份身份声明由 strip_cc_preamble
    // 剥掉再重建。
    let cc_no_meta = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}},{{"type":"text","text":"user prompt"}}]}}"#,
        config::CC_SYSTEM_IDENTITY
    ));
    assert!(detect_for(&cc_no_meta, all_on()).is_some(), "非 CC 客户端抄了 system 应走模拟");
    let as_string = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","system":"{}","messages":[]}}"#,
        config::CC_SYSTEM_IDENTITY
    ));
    assert!(detect_for(&as_string, all_on()).is_some(), "字符串形态也应走模拟");

    // 带 metadata.user_id 但 UA 不是 claude-cli → 照样走模拟（只看 UA）。
    let cc_with_meta = Bytes::from(API_SHAPE_BODY);
    assert!(detect_for(&cc_with_meta, all_on()).is_some(), "非 CC UA 带 user_id 也走模拟");

    // 真正的 CC 客户端（UA 带 claude-cli/）+ CC 形态 body 不模拟——UA 不降级。
    let mut cc_ua = crate::proxy::HeaderMap::new();
    cc_ua
        .insert(header::USER_AGENT, HeaderValue::from_static("claude-cli/2.1.226 (external, cli)"));
    // 真 CC 的体：身份句 + 基座；抄了身份句却没基座的 cc_no_meta 在 CC UA 下也走模拟。
    let cc_full = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}},{},{{"type":"text","text":"user prompt"}}]}}"#,
        config::CC_SYSTEM_IDENTITY,
        base_block()
    ));
    assert!(detect_with(&cc_full, &cc_ua, all_on()).is_none(), "真 CC 客户端(无tools)不该走模拟");
    assert!(
        detect_with(&cc_no_meta, &cc_ua, all_on()).is_some(),
        "CC UA + 身份句但没基座 → 去掉基座的第三方，走模拟"
    );

    // CC UA + CC 形态 system + 含官方工具名 → 不模拟。
    let cc_with_tools = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}},{}],"tools":[{{"name":"Bash"}},{{"name":"custom_tool"}}]}}"#,
        config::CC_SYSTEM_IDENTITY,
        base_block()
    ));
    assert!(
        detect_with(&cc_with_tools, &cc_ua, all_on()).is_none(),
        "CC UA + CC 形态 + 有官方工具不该走模拟"
    );

    // CC UA 但 system 里既无身份声明也无 billing header → 冒用 UA，走模拟。
    // 这就是封号复盘里那批探活请求的形态：官方 UA + 官方格式 user_id + 自造的 3 块 system。
    let cc_ua_fake_shape = Bytes::from(
        r#"{"model":"claude-sonnet-5","system":[{"type":"text","text":"probe"},{"type":"text","text":"a"},{"type":"text","text":"b","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"hi"}],"max_tokens":1024,"temperature":1,"stream":true}"#,
    );
    assert!(
        detect_with(&cc_ua_fake_shape, &cc_ua, all_on()).is_some(),
        "CC UA + 非 CC 形态 system 应走模拟"
    );
    assert!(
        detect_with(&Bytes::from(PLAIN_BODY), &cc_ua, all_on()).is_some(),
        "CC UA + 没有 system 也应走模拟"
    );
    // billing header 块 + 子代理自己那段长提示词（身份句不同）算 CC 形态 → 不模拟。
    let cc_billing_only = Bytes::from(format!(
        r#"{{"model":"claude-haiku-4-5-20251001","system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.abcdef; cch=00000"}},{{"type":"text","text":"You are a search agent. {}"}}],"messages":[{{"role":"user","content":"hi"}}]}}"#,
        "x".repeat(1200)
    ));
    assert!(
        detect_with(&cc_billing_only, &cc_ua, all_on()).is_none(),
        "CC UA + billing header + 子代理长提示词不该走模拟"
    );

    // CC UA + billing header + 身份句、却**没有基座**（ban.log 里 claude-cli/2.1.165 那批：
    // 两块 system、tools: []、每台新设备同样四道题）→ 去掉基座的第三方，走模拟补全。
    let no_base = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.165.abcdef; cch=00000"}},{{"type":"text","text":"{}"}}],"messages":[{{"role":"user","content":"hi"}}],"max_tokens":10240,"stream":true,"tools":[],"thinking":{{"type":"adaptive"}}}}"#,
        config::CC_SYSTEM_IDENTITY
    ));
    assert!(
        detect_with(&no_base, &cc_ua, all_on()).is_some(),
        "CC UA + 身份句但没有基座的该走模拟"
    );
    // 同样两块 system 但 max_tokens=1 → 2.1.187 Claude Desktop 的 cache 预热，照旧透传。
    let prewarm = Bytes::from(format!(
        r#"{{"model":"claude-haiku-4-5-20251001","messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.187.abcdef"}},{{"type":"text","text":"{}"}}],"max_tokens":1}}"#,
        config::CC_SYSTEM_IDENTITY
    ));
    assert!(
        detect_with(&prewarm, &cc_ua, all_on()).is_none(),
        "max_tokens=1 的预热不该因为没基座而被模拟"
    );
    // 官方额度探测（`cap/2.1.260-2/00004`）：没有 system、没有 tools、haiku、max_tokens=1、
    // 正文 `quota`。它过不了 is_cc_shaped，必须单独放行——装成主线程会给它补 system、基座
    // 和工具，正是既定要求里禁止的。曾在收紧「CC 形态」条件时漏掉过一次。
    let quota = Bytes::from(
        r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"quota"}],"metadata":{"user_id":"{\"device_id\":\"832cb7e697190bc475b926c7994ef183a0f8a58e29818f182e11f924e1ea2870\",\"account_uuid\":\"a\",\"session_id\":\"d0c1fb05-9b19-4576-9465-e2b8a206dabf\"}"}}"#,
    );
    assert!(
        crate::proxy::is_quota_probe_shaped(&parsed(&quota).unwrap()),
        "测试体本身得是官方额度探测形态"
    );
    assert!(detect_with(&quota, &cc_ua, all_on()).is_none(), "官方额度探测不该被模拟成主线程");
    // 差一点就不是额度探测：正文不是 quota、或模型不是 haiku 4.5 全名 → 没有 system 又不是
    // 额度探测的，照旧走模拟。
    let not_quota = Bytes::from(
        r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert!(detect_with(&not_quota, &cc_ua, all_on()).is_some());
    let wrong_model = Bytes::from(
        r#"{"model":"claude-opus-5","max_tokens":1,"messages":[{"role":"user","content":"quota"}]}"#,
    );
    assert!(detect_with(&wrong_model, &cc_ua, all_on()).is_some());
    // 官方 Helper（`cap/2.1.260/00024`）：子代理 billing header + SDK 身份句两块，没有任何
    // 长块，haiku、tools: []、max_tokens 32000、流式。基座阈值对它例外，照旧透传。
    const SUB_BILLING: &str = "x-anthropic-billing-header: cc_version=2.1.260.d95; cc_entrypoint=cli; cch=ca354; cc_is_subagent=true; cc_prompt_id=bd224f00-5a91-4f15-b92f-0f47c663eae8;";
    let helper_body = |model: &str, tools: &str, max_tokens: u32, stream: &str, sdk_line: &str| {
        Bytes::from(format!(
            r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"{SUB_BILLING}"}},{{"type":"text","text":"{sdk_line}"}}],"tools":{tools},"max_tokens":{max_tokens},"thinking":{{"type":"disabled"}},"temperature":1{stream}}}"#
        ))
    };
    let helper = helper_body(
        "claude-haiku-4-5-20251001",
        "[]",
        32000,
        r#","stream":true"#,
        config::CC_SDK_AGENT_IDENTITY,
    );
    let mut helper_headers = cc_ua.clone();
    helper_headers.insert(
        "anthropic-beta",
        HeaderValue::from_str(config::cc_profile(config::CcProfileKind::HelperSubagentHaiku).beta)
            .unwrap(),
    );
    assert!(
        detect_with(&helper, &helper_headers, all_on()).is_none(),
        "官方 Helper 子代理没有长块也不该被模拟"
    );
    assert!(
        detect_with(&helper, &cc_ua, all_on()).is_some(),
        "同样的 system 与 body、头上没带 Helper 的 beta → 不是官方 Helper，走模拟"
    );
    // 只抄了 `cc_is_subagent=true` 这个标记、其余不是官方 Helper 取值的，例外不成立，
    // 照基座阈值走模拟——每一条都是曾经能绕过的写法。
    for (label, body) in [
        (
            "模型是 opus",
            helper_body(
                "claude-opus-5",
                "[]",
                32000,
                r#","stream":true"#,
                config::CC_SDK_AGENT_IDENTITY,
            ),
        ),
        (
            "带了工具",
            helper_body(
                "claude-haiku-4-5-20251001",
                r#"[{"name":"Bash"}]"#,
                32000,
                r#","stream":true"#,
                config::CC_SDK_AGENT_IDENTITY,
            ),
        ),
        (
            "max_tokens 不是 32000",
            helper_body(
                "claude-haiku-4-5-20251001",
                "[]",
                1024,
                r#","stream":true"#,
                config::CC_SDK_AGENT_IDENTITY,
            ),
        ),
        (
            "非流式",
            helper_body(
                "claude-haiku-4-5-20251001",
                "[]",
                32000,
                "",
                config::CC_SDK_AGENT_IDENTITY,
            ),
        ),
        (
            "第二块不是 SDK 身份句",
            helper_body(
                "claude-haiku-4-5-20251001",
                "[]",
                32000,
                r#","stream":true"#,
                config::CC_SYSTEM_IDENTITY,
            ),
        ),
    ] {
        assert!(detect_with(&body, &helper_headers, all_on()).is_some(), "{label}");
    }
    // 三块 system（多出一块短的）也不是 Helper：官方 Helper 恰好两块。
    let three_blocks = Bytes::from(format!(
        r#"{{"model":"claude-haiku-4-5-20251001","messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"{SUB_BILLING}"}},{{"type":"text","text":"{}"}},{{"type":"text","text":"extra"}}],"tools":[],"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"stream":true}}"#,
        config::CC_SDK_AGENT_IDENTITY
    ));
    assert!(detect_with(&three_blocks, &helper_headers, all_on()).is_some());
    // 身份写错的（device 不是 64 位 hex / session 不是 uuid）也进不了透传，走模拟重建身份：
    // 这就是 ban.log 里 `channel-test` 那批探活的去向。
    let bad_identity = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"{}"}},{}],"tools":[{{"name":"Bash"}}],"metadata":{{"user_id":"{{\"device_id\":\"channel-test\",\"account_uuid\":\"a\",\"session_id\":\"channel-test-claude-code\"}}"}}}}"#,
        config::CC_SYSTEM_IDENTITY,
        base_block()
    ));
    assert!(
        detect_with(&bad_identity, &cc_ua, all_on()).is_some(),
        "身份格式不合法的不当官方客户端，走模拟"
    );
    let mut bad_header = cc_ua.clone();
    bad_header.insert(
        crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
        HeaderValue::from_static("channel-test-claude-code"),
    );
    assert!(detect_with(&cc_full, &bad_header, all_on()).is_some(), "体合法、会话头非法的也走模拟");
    // 只抄一句 cc_is_subagent=true 却没有 billing header 前缀的更不算。
    let fake_sub = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"cc_is_subagent=true"}},{{"type":"text","text":"{}"}}]}}"#,
        config::CC_SYSTEM_IDENTITY
    ));
    assert!(detect_with(&fake_sub, &cc_ua, all_on()).is_some());

    // CC UA + 全是非官方工具名 → 视为冒用，走模拟。
    let spoofed_ua = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"tools":[{"name":"exec"},{"name":"read_file"},{"name":"web_search"}]}"#.to_string()
        );
    assert!(
        detect_with(&spoofed_ua, &cc_ua, all_on()).is_some(),
        "CC UA 但零官方工具应走模拟（冒用）"
    );

    // 模拟后 system 里不该有两份身份声明。
    let sim = detect_for(&cc_no_meta, all_on()).unwrap();
    let out = rewrite_body(&cc_no_meta, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    let id_count = sys
        .iter()
        .filter(|b| b.get("text").and_then(|t| t.as_str()) == Some(config::CC_SYSTEM_IDENTITY))
        .count();
    assert_eq!(id_count, 1, "身份声明只该出现一次: {v}");
    // 客户端的原始 prompt 不该丢：剥掉抄来的前两块之后，剩下的正文单独占末块。
    assert_eq!(sys.last().unwrap()["text"], "user prompt", "客户端原始 prompt 应保留: {v}");

    // 开关关掉、或 merge_beta 关掉时也不模拟。
    let plain = Bytes::from(PLAIN_BODY.to_string());
    let off = store::ForwardFlags { simulate_cc: false, ..all_on() };
    assert!(detect_for(&plain, off).is_none());
    let no_beta = store::ForwardFlags { merge_beta: false, ..all_on() };
    assert!(detect_for(&plain, no_beta).is_none());
    // 解析不了的请求体不 panic、也不模拟。
    assert!(detect_for(&Bytes::from_static(b"not json"), all_on()).is_none());
}

/// 客户端已经用满 4 个缓存断点时不再加——加了整条请求会被上游拒，那是把「形态更像」
/// 换成「根本发不出去」。断点在别处（tools）时同样算数。
#[test]
fn respects_cache_breakpoint_budget() {
    let t = |n: &str| format!(r#"{{"name":"{n}","cache_control":{{"type":"ephemeral"}}}}"#);
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"tools":[{},{},{},{}]}}"#,
        t("t0"),
        t("t1"),
        t("t2"),
        t("t3"),
    ));
    let sim = detect_for(&body, all_on()).unwrap();
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(crate::proxy::count_cache_control(&v), 4, "断点数不得超过 4: {v}");
    assert!(v["system"][2].get("cache_control").is_none(), "预算用完时基座不带断点");
    assert_eq!(v["system"][2]["text"], config::CC_SYSTEM_BASE);
    assert!(v["system"][3].get("cache_control").is_none(), "预算用完时第四块也不带断点");
    assert!(v["system"][3]["text"].as_str().unwrap().starts_with("Write code that reads like"));
}

/// 客户端把 `system` 拆成多块时并成**一块**——3+N 块会被上游判第三方应用、改扣超额池
/// （`Third-party apps now draw from your extra usage`）。并成的那一块跟在官方四块之后，
/// 官方那四块的正文一个字都不掺客户端的（0.3.154）。
///
/// 客户端那 4 个断点并成一个、不再各占预算：基座与第四块都该拿到断点。
#[test]
fn merges_client_system_blocks_into_one_trailing_block() {
    let blk = |t: &str| {
        format!(r#"{{"type":"text","text":"{t}","cache_control":{{"type":"ephemeral"}}}}"#)
    };
    let sys_json = format!("[{},{},{},{}]", blk("a"), blk("b"), blk("c"), blk("d"));
    let body =
        Bytes::from(format!(r#"{{"model":"claude-opus-5","messages":[],"system":{sys_json}}}"#));
    let sim = detect_for(&body, all_on()).unwrap();
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();

    assert_eq!(sys.len(), 5, "官方四块 + 并成一块的客户端 system: {v}");
    let rest = sys[3]["text"].as_str().unwrap();
    assert!(rest.starts_with("Write code that reads like"), "第四块开头是官方正文");
    assert!(rest.ends_with("</total_tokens>"), "第四块末尾照抓包，客户端的不掺进来");
    assert_eq!(sys[4]["text"], "a\n\nb\n\nc\n\nd", "四块并成一块、一个字都不丢: {v}");
    assert_eq!(sys[3]["cache_control"], serde_json::json!({"type": "ephemeral", "ttl": "1h"}));
    assert_eq!(sys[2]["text"], config::CC_SYSTEM_BASE);
    assert_eq!(sys[2]["cache_control"]["scope"], "global", "客户端断点腾出的预算该给基座");
    assert_eq!(sys[4]["cache_control"]["ttl"], "1h", "末块照官方在 system 末尾标一个断点");
    assert_eq!(crate::proxy::count_cache_control(&v), 3, "基座 + 第四块 + 末块: {v}");

    // 首条消息可写也不挪：这段不到 MAX_CLIENT_SYSTEM_BYTES，留在自己那一块里。
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}}]}}],"system":{sys_json}}}"#
    ));
    let sim = detect_for(&body, all_on()).unwrap();
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    assert_eq!(sys.len(), 5, "{v}");
    assert!(sys[3]["text"].as_str().unwrap().ends_with("</total_tokens>"), "第四块原样: {v}");
    assert_eq!(sys[4]["text"], "a\n\nb\n\nc\n\nd", "客户端 system 占末块: {v}");
    let first = v["messages"][0]["content"].as_array().unwrap();
    assert_eq!(first.len(), 1, "首条消息不再被塞东西: {v}");
    assert_eq!(first[0]["text"], "hi");

    // 超过 MAX_CLIENT_SYSTEM_BYTES 的由 relocate_long_client_system 挪进首条消息，
    // 末块原地留一行占位，整段裹成官方那种 system-reminder 插在首条消息最前。
    let long = "x".repeat(crate::proxy::MAX_CLIENT_SYSTEM_BYTES + 1);
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}}]}}],"system":"{long}"}}"#
    ));
    let sim = detect_for(&body, all_on()).unwrap();
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    assert_eq!(sys.len(), 5, "{v}");
    assert!(sys[3]["text"].as_str().unwrap().ends_with("</total_tokens>"), "第四块原样");
    assert_eq!(sys[4]["text"], "(see conversation)", "末块换成占位: {v}");
    let first = v["messages"][0]["content"].as_array().unwrap();
    assert_eq!(first.len(), 2, "长 system 作为一个 text 块插在首条消息最前: {v}");
    assert_eq!(
        first[0]["text"],
        format!(
            "<system-reminder>\n{}\n\n{long}\n</system-reminder>",
            crate::proxy::CLIENT_SYSTEM_REMINDER_LEAD
        ),
        "与官方首条用户消息第一块同形（cap/2.1.277/00023）"
    );
    assert_eq!(first[1]["text"], "hi");

    // 空块并不进来（发一个空文本块上游不收），也就不多出末块那一块。
    let empty = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"system":[{"type":"text","text":""},{"type":"text","text":"  "}]}"#
                .to_string(),
        );
    let sim = detect_for(&empty, all_on()).unwrap();
    let out = rewrite_body(&empty, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    assert_eq!(sys.len(), 4, "全空的块应丢掉，只剩官方四块: {v}");
    assert!(sys[3]["text"].as_str().unwrap().ends_with("</total_tokens>"), "{v}");
}

/// 自称 CC（`system` 里有那句身份声明）却发了 5 块以上的第三方客户端：
/// 非 CC 客户端现在走模拟，`strip_cc_preamble` 剥掉旧身份、`simulate_system` 补上新的。
#[test]
fn cc_shaped_from_non_cc_client_strips_and_rebuilds() {
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"h"}},{{"type":"text","text":"{}"}},{{"type":"text","text":"c"}},{{"type":"text","text":"d"}},{{"type":"text","text":"e","cache_control":{{"type":"ephemeral"}}}}]}}"#,
        config::CC_SYSTEM_IDENTITY
    ));
    assert!(detect_for(&body, all_on()).is_some(), "非 CC 客户端应走模拟");
    let sim = detect_for(&body, all_on()).unwrap();
    let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    // 身份声明只有一份（模拟补的那份），客户端原来那份已被 strip 掉。
    let id_count = sys
        .iter()
        .filter(|b| b.get("text").and_then(|t| t.as_str()) == Some(config::CC_SYSTEM_IDENTITY))
        .count();
    assert_eq!(id_count, 1, "身份声明应恰好一份: {v}");
    assert_eq!(sys[1]["text"], config::CC_SYSTEM_IDENTITY);

    // 5 块及以内不动结构：cap_system_blocks 的直接验证。
    let four = Bytes::from(API_SHAPE_BODY);
    let before: serde_json::Value = serde_json::from_slice(&four).unwrap();
    let mut after = before.clone();
    assert!(!crate::proxy::cap_system_blocks(&mut after), "3 块不该被改");
    assert_eq!(before, after);
}

/// [`crate::proxy::simulates_cc`] 与 [`crate::proxy::Simulation::detect`] 必须给出同一个答案——指纹在
/// detect 之前先用前者算出站 UA，两处判据一旦分叉，一条请求就会被算到另一台设备名下。
#[test]
fn simulates_cc_agrees_with_detect() {
    const CC_UA: &str = "claude-cli/2.1.260 (external, cli)";
    let cc_shaped = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","system":[{{"type":"text","text":"{}"}},{}],"messages":[]}}"#,
        config::CC_SYSTEM_IDENTITY,
        base_block()
    ));
    let plain = Bytes::from(PLAIN_BODY.to_string());
    let sim_off = store::ForwardFlags { simulate_cc: false, ..all_on() };

    let agrees = |body: &Bytes, headers: &crate::proxy::HeaderMap, flags: store::ForwardFlags| {
        let v = parsed(body);
        let from_cc = crate::proxy::trusted_cc_version(&crate::proxy::ua_of(headers)).is_some();
        let by_predicate = crate::proxy::simulates_cc(v.as_ref(), headers, from_cc, flags);
        let by_detect = detect_with(body, headers, flags).is_some();
        assert_eq!(by_predicate, by_detect, "两处判据必须同源");
        by_predicate
    };

    let cc_ua = platform_headers(Some(CC_UA));
    assert!(!agrees(&cc_shaped, &cc_ua, all_on()), "真 CC（UA + 形态都对）不模拟");
    assert!(agrees(&cc_shaped, &platform_headers(None), all_on()), "抄了形态没抄 UA 的走模拟");
    assert!(agrees(&plain, &cc_ua, all_on()), "抄了 UA 没抄形态的走模拟");
    assert!(!agrees(&plain, &cc_ua, sim_off), "开关关着一律不模拟");
}

/// [`simulation_reason`]：记的是三道判据里**第一道**没过的（UA → 身份 → 形态，形态内部
/// CC 形态 → 基座 → 工具）；全过与官方额度探测为 `None`；`detect` 把同一个原因带进
/// [`Simulation::reason`]，流水的 `sim_reason` 列与 SIMULATED 日志行都取它。
#[test]
fn simulation_reason_names_the_first_failed_check() {
    use crate::proxy::SimulationReason::*;
    const CC_UA: &str = "claude-cli/2.1.260 (external, cli)";
    let cc_ua = platform_headers(Some(CC_UA));
    let no_ua = platform_headers(None);
    let reason = |body: &str, headers: &crate::proxy::HeaderMap| {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let from_cc = crate::proxy::trusted_cc_version(&crate::proxy::ua_of(headers)).is_some();
        crate::proxy::simulation_reason(Some(&v), headers, from_cc, all_on())
    };
    let identity = |device: &str| {
        format!(
            r#""metadata":{{"user_id":"{{\"device_id\":\"{device}\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}}"}}"#
        )
    };
    let good_dev = "b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d";
    let full = format!(
        r#"{{"model":"claude-opus-5","max_tokens":32000,"system":[{{"type":"text","text":"{}"}},{}],"tools":[{{"name":"Bash","input_schema":{{"type":"object"}}}}],"messages":[],{}}}"#,
        config::CC_SYSTEM_IDENTITY,
        base_block(),
        identity(good_dev)
    );
    // 全对：不模拟。
    assert_eq!(reason(&full, &cc_ua), None);
    // UA 不是 CC：形态再对也是 not_cc_client。
    assert_eq!(reason(&full, &no_ua), Some(NotCcClient));
    // 身份写错（device 不是 64 位 hex）：identity_malformed，排在形态之前。
    let bad_identity = full.replace(good_dev, "not-a-device");
    assert_eq!(reason(&bad_identity, &cc_ua), Some(IdentityMalformed));
    // 没有身份句、没有 billing header：not_cc_shaped。
    let plain = format!(
        r#"{{"model":"claude-opus-5","max_tokens":1024,"system":"be brief","messages":[],{}}}"#,
        identity(good_dev)
    );
    assert_eq!(reason(&plain, &cc_ua), Some(NotCcShaped));
    // 只抄了身份句、没抄基座、也不是预热：no_base_prompt。
    let no_base = format!(
        r#"{{"model":"claude-opus-5","max_tokens":1024,"system":[{{"type":"text","text":"{}"}}],"messages":[],{}}}"#,
        config::CC_SYSTEM_IDENTITY,
        identity(good_dev)
    );
    assert_eq!(reason(&no_base, &cc_ua), Some(NoBasePrompt));
    // 同一份体 max_tokens=1 即 cache 预热：基座免检，不模拟。
    let prewarm = no_base.replace(r#""max_tokens":1024"#, r#""max_tokens":1"#);
    assert_eq!(reason(&prewarm, &cc_ua), None);
    // 身份句 + 基座都在，tools 却一个官方名都没有：tools_not_cc。
    let odd_tools = full.replace(r#""name":"Bash""#, r#""name":"my_tool""#);
    assert_eq!(reason(&odd_tools, &cc_ua), Some(ToolsNotCc));
    // 官方桌面端预热：max_tokens=1、没有 system（或只有一块几百字节的应用块）、没有
    // tools——不是 CC 形态也放行；带长 system 的 1 token 请求仍算第三方。
    let desktop_prewarm = format!(
        r#"{{"model":"claude-fable-5-1","max_tokens":1,"messages":[{{"role":"user","content":"warm"}}],{}}}"#,
        identity(good_dev)
    );
    assert_eq!(reason(&desktop_prewarm, &cc_ua), None);
    let desktop_prewarm_app = desktop_prewarm.replace(
        r#""max_tokens":1,"#,
        &format!(r#""max_tokens":1,"system":[{{"type":"text","text":"{}"}}],"#, "a".repeat(911)),
    );
    assert_eq!(reason(&desktop_prewarm_app, &cc_ua), None);
    let desktop_prewarm_tools = desktop_prewarm.replace(
            r#""max_tokens":1,"#,
            r#""max_tokens":1,"tools":[{"name":"mcp__ccd_session__spawn_task","input_schema":{"type":"object"}}],"#,
        );
    assert_eq!(
        reason(&desktop_prewarm_tools, &cc_ua),
        Some(NotCcShaped),
        "只有非官方名的 tools 不享预热例外，仍按不是 CC 形态走模拟"
    );
    let anonymous_prewarm = r#"{"model":"claude-fable-5-1","max_tokens":1,"messages":[{"role":"user","content":"warm"}]}"#;
    assert_eq!(
        reason(anonymous_prewarm, &cc_ua),
        Some(NotCcShaped),
        "不带身份的 1 token 请求不算桌面端预热"
    );
    let long_prewarm = desktop_prewarm.replace(
        r#""max_tokens":1,"#,
        &format!(r#""max_tokens":1,"system":"{}","#, "b".repeat(1500)),
    );
    assert_eq!(reason(&long_prewarm, &cc_ua), Some(NotCcShaped));
    assert_eq!(reason(&desktop_prewarm, &no_ua), Some(NotCcClient), "UA 不可信照样模拟");
    let prewarm_bad_id = desktop_prewarm.replace(good_dev, "nope");
    assert_eq!(reason(&prewarm_bad_id, &cc_ua), Some(IdentityMalformed));
    // 官方 WebSearch 子调用：一条用户消息、只有 web_search 这个 server tool 且被 tool_choice
    // 强制、system 只有一句搜索助手提示（可带 billing header）。没有基座也放行，否则会被
    // 重建成主线程体、换 UA 换设备换会话（`ban/luban-ban-13/14`）。
    const WS_TOOL: &str = r#"{"type":"web_search_20250305","name":"web_search","max_uses":8}"#;
    const WS_CHOICE: &str = r#""tool_choice":{"type":"tool","name":"web_search"},"#;
    const WS_PROMPT: &str = "You are an assistant for performing a web search tool use";
    let ws_block = |text: &str| {
        format!(r#"{{"type":"text","text":"{text}","cache_control":{{"type":"ephemeral"}}}}"#)
    };
    let web_search = |system: &str, tools: &str, choice: &str, messages: &str| {
        format!(
            r#"{{"model":"claude-opus-5","max_tokens":64000,"system":{system},"tools":[{tools}],{choice}"messages":{messages},{}}}"#,
            identity(good_dev)
        )
    };
    let one_msg = r#"[{"role":"user","content":"Perform a web search for the query: rust 1.90 release notes"}]"#;
    let ws_sys = format!("[{}]", ws_block(WS_PROMPT));
    let ws = web_search(&ws_sys, WS_TOOL, WS_CHOICE, one_msg);
    assert_eq!(reason(&ws, &cc_ua), None, "不带 billing header 的 WebSearch 子调用放行");
    let ws_billing_sys = format!(
        r#"[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.abc; cc_entrypoint=cli; cch=1e2f3;"}},{}]"#,
        ws_block(WS_PROMPT)
    );
    assert_eq!(
        reason(&web_search(&ws_billing_sys, WS_TOOL, WS_CHOICE, one_msg), &cc_ua),
        None,
        "带 billing header 的同样放行"
    );
    let ws_string_sys = format!(r#""{WS_PROMPT}""#);
    assert_eq!(
        reason(&web_search(&ws_string_sys, WS_TOOL, WS_CHOICE, one_msg), &cc_ua),
        None,
        "字符串形态的 system 一并认"
    );
    // 差一项都不算，仍按原判据走模拟：
    assert_eq!(
        reason(&web_search(&ws_sys, WS_TOOL, "", one_msg), &cc_ua),
        Some(NotCcShaped),
        "没有 tool_choice 强制"
    );
    assert_eq!(
        reason(&web_search(&ws_sys, WS_TOOL, r#""tool_choice":{"type":"auto"},"#, one_msg), &cc_ua),
        Some(NotCcShaped),
        "tool_choice 不是强制 web_search"
    );
    assert_eq!(
        reason(
            &web_search(
                &ws_sys,
                &format!(r#"{WS_TOOL},{{"name":"Bash","input_schema":{{"type":"object"}}}}"#),
                WS_CHOICE,
                one_msg
            ),
            &cc_ua
        ),
        Some(NotCcShaped),
        "多了别的工具"
    );
    assert_eq!(
        reason(
            &web_search(
                &ws_sys,
                r#"{"name":"web_search","input_schema":{"type":"object"}}"#,
                WS_CHOICE,
                one_msg
            ),
            &cc_ua
        ),
        Some(NotCcShaped),
        "同名却不是 server tool"
    );
    assert_eq!(
        reason(
            &web_search(
                &ws_sys,
                WS_TOOL,
                WS_CHOICE,
                r#"[{"role":"user","content":"search a"},{"role":"assistant","content":"ok"},{"role":"user","content":"search b"}]"#
            ),
            &cc_ua
        ),
        Some(NotCcShaped),
        "不止一条消息"
    );
    assert_eq!(
        reason(
            &web_search(&format!("[{}]", ws_block(&"w".repeat(1500))), WS_TOOL, WS_CHOICE, one_msg),
            &cc_ua
        ),
        Some(NotCcShaped),
        "system 是长块"
    );
    assert_eq!(
        reason(
            &web_search(&format!("[{}]", ws_block("be brief")), WS_TOOL, WS_CHOICE, one_msg),
            &cc_ua
        ),
        Some(NotCcShaped),
        "system 与搜索无关"
    );
    assert_eq!(
        reason(
            &web_search(
                &format!("[{},{}]", ws_block(WS_PROMPT), ws_block("and more")),
                WS_TOOL,
                WS_CHOICE,
                one_msg
            ),
            &cc_ua
        ),
        Some(NotCcShaped),
        "去掉 billing header 后不止一块"
    );
    assert_eq!(
        reason(
            &web_search(
                &format!("[{}]", ws_block(config::CC_SYSTEM_IDENTITY)),
                WS_TOOL,
                WS_CHOICE,
                one_msg
            ),
            &cc_ua
        ),
        Some(NoBasePrompt),
        "写了身份句就是主线程形态，按基座判"
    );
    assert_eq!(reason(&ws, &no_ua), Some(NotCcClient), "UA 不可信照样模拟");
    assert_eq!(reason(&ws.replace(good_dev, "nope"), &cc_ua), Some(IdentityMalformed));
    // 开关关着：什么原因都没有。
    let v: serde_json::Value = serde_json::from_str(&plain).unwrap();
    let sim_off = store::ForwardFlags { simulate_cc: false, ..all_on() };
    assert_eq!(crate::proxy::simulation_reason(Some(&v), &cc_ua, true, sim_off), None);
    // detect 带出同一个原因，标签与流水列一致。
    let sim = detect_with(&Bytes::from(plain.clone()), &cc_ua, all_on()).expect("走模拟");
    assert_eq!(sim.reason, NotCcShaped);
    assert_eq!(sim.reason.tag(), "not_cc_shaped");
    // 来访事实：块数、字节数、有没有身份句 / billing header、tools 数、max_tokens。
    let f = crate::proxy::inbound_facts(&serde_json::from_str::<serde_json::Value>(&full).unwrap());
    assert_eq!(
        (f.system_blocks, f.identity, f.billing_header, f.tools, f.max_tokens),
        (2, true, false, 1, Some(32000))
    );
    assert!(f.system_bytes > crate::proxy::CC_BASE_PROMPT_MIN_LEN);
    let f = crate::proxy::inbound_facts(&v);
    assert_eq!(
        (f.system_blocks, f.system_bytes, f.identity, f.tools),
        (1, "be brief".len(), false, 0)
    );
}

/// [`cc_identity_well_formed`]：三处身份都合法（或都没带）才算；头上的非法值**原样**看，
/// 不像 [`incoming_session_id`] 那样先过滤掉。这就是 `channel-test` 那类探活进不了透传、
/// 被送去模拟的那道门。
#[test]
fn identity_well_formed_checks_all_three_sources() {
    const DEV: &str = "4fef933b15e89f7060000573496ce0eab6e9f0d1cf43e31dd4c7dc1c6801cfb5";
    const SESS: &str = "8f79a3c7-1125-4096-a03d-feb0d4c10d52";
    let body = |dev: &str, sess: &str| -> serde_json::Value {
        serde_json::json!({
            "model": "claude-opus-5",
            "messages": [],
            "metadata": {"user_id": format!(r#"{{"device_id":"{dev}","account_uuid":"a","session_id":"{sess}"}}"#)}
        })
    };
    let ok = body(DEV, SESS);
    let none = crate::proxy::HeaderMap::new();
    assert!(crate::proxy::cc_identity_well_formed(&none, &ok));
    assert!(
        crate::proxy::cc_identity_well_formed(
            &none,
            &serde_json::json!({"model": "m", "messages": []})
        ),
        "三处都没带算合法（补身份是另一条路的事）"
    );
    assert!(!crate::proxy::cc_identity_well_formed(&none, &body("channel-test", SESS)));
    assert!(!crate::proxy::cc_identity_well_formed(&none, &body(DEV, "channel-test-claude-code")));
    // 扁平串格式的 device 段同样要 64 位 hex。
    let flat = serde_json::json!({"model": "m", "messages": [], "metadata": {"user_id": format!("user_abc_account_x_session_{SESS}")}});
    assert!(!crate::proxy::cc_identity_well_formed(&none, &flat));

    // 头上的非法值原样看：会话链那条路把它过滤掉了，这里不能跟着丢。
    let mut h = crate::proxy::HeaderMap::new();
    h.insert(
        crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
        HeaderValue::from_static("  channel-test-claude-code "),
    );
    assert_eq!(crate::proxy::incoming_session_id(&h, None), None, "对照：会话链那边会丢掉它");
    assert!(!crate::proxy::cc_identity_well_formed(&h, &ok), "体合法、头非法 → 不合法");
    let mut good = crate::proxy::HeaderMap::new();
    good.insert(
        crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
        HeaderValue::from_static(SESS),
    );
    assert!(crate::proxy::cc_identity_well_formed(&good, &ok));
    let mut blank = crate::proxy::HeaderMap::new();
    blank.insert(
        crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
        HeaderValue::from_static("   "),
    );
    assert!(crate::proxy::cc_identity_well_formed(&blank, &ok), "空白头当没带");

    // 内嵌 JSON 里 session_id / device_id 写了键却不是字串：extract_* 会当成「没带」，但官方
    // 恒为字串，这是抄错了——不合法。透传会把那个 null 原样留在出站 user_id 里。
    for (label, uid) in [
        (
            "session_id 为 null",
            format!(r#"{{"device_id":"{DEV}","account_uuid":"a","session_id":null}}"#),
        ),
        (
            "session_id 为数字",
            format!(r#"{{"device_id":"{DEV}","account_uuid":"a","session_id":42}}"#),
        ),
        (
            "session_id 为对象",
            format!(r#"{{"device_id":"{DEV}","account_uuid":"a","session_id":{{}}}}"#),
        ),
        (
            "session_id 为空串",
            format!(r#"{{"device_id":"{DEV}","account_uuid":"a","session_id":""}}"#),
        ),
        (
            "device_id 为 null",
            format!(r#"{{"device_id":null,"account_uuid":"a","session_id":"{SESS}"}}"#),
        ),
        (
            "device_id 为数字",
            format!(r#"{{"device_id":1,"account_uuid":"a","session_id":"{SESS}"}}"#),
        ),
    ] {
        let bad = serde_json::json!({"model": "m", "messages": [], "metadata": {"user_id": uid}});
        assert!(!crate::proxy::cc_identity_well_formed(&none, &bad), "{label}");
    }
    // 首尾空白也是抄错：extract_session_id 会 trim 后认成合法 uuid，这里不能跟着放。
    let padded = serde_json::json!({"model": "m", "messages": [], "metadata": {"user_id": format!(r#"{{"device_id":"{DEV}","account_uuid":"a","session_id":" {SESS} "}}"#)}});
    assert!(!crate::proxy::cc_identity_well_formed(&none, &padded), "带空白的 session_id 不合法");
    let padded_dev = serde_json::json!({"model": "m", "messages": [], "metadata": {"user_id": format!(r#"{{"device_id":"{DEV} ","account_uuid":"a","session_id":"{SESS}"}}"#)}});
    assert!(
        !crate::proxy::cc_identity_well_formed(&none, &padded_dev),
        "带空白的 device_id 不合法"
    );
    // 键干脆不写是「没带」，合法；内嵌 JSON 只有 account_uuid 也合法。
    let only_account = serde_json::json!({"model": "m", "messages": [], "metadata": {"user_id": r#"{"account_uuid":"a"}"#}});
    assert!(crate::proxy::cc_identity_well_formed(&none, &only_account));
}

/// 「是官方客户端」要 UA 与体两头都对得上：UA 自报 `claude-cli/<版本>` **且** `system`
/// 是 CC 形态（身份声明或 billing header 块，[`is_cc_shaped`]）。只有 UA、体不是 CC
/// 形态的请求是抄了 UA 的第三方——封号复盘里那批探活请求正是这样——走模拟；`metadata`
/// 与 session 头本身不构成跳过理由。
///
/// 真 CC 不模拟的代价仍然成立（换头会把它自报的 UA 换掉、`x-stainless-*` 换成抓包机器
/// 的取值），故 CC 形态的真客户端照旧原样转发。
#[test]
fn cc_client_needs_both_ua_and_cc_shape() {
    let plain = Bytes::from(PLAIN_BODY.to_string());

    // 1) metadata.user_id 在、但 UA 不是 claude-cli → 仍走模拟。
    let with_meta = Bytes::from(
            r#"{"model":"claude-opus-5","system":"you are a helpful bot","messages":[],"metadata":{"user_id":"{\"device_id\":\"d0\",\"account_uuid\":\"a0\",\"session_id\":\"11111111-1111-4111-8111-111111111111\"}"}}"#
                .to_string(),
        );
    assert!(detect_for(&with_meta, all_on()).is_some(), "非 CC UA 带 user_id 照样走模拟");

    // 2) user_id 是认不出的格式、UA 不是 claude-cli → 同样走模拟。
    let odd_meta = Bytes::from(
        r#"{"model":"claude-opus-5","messages":[],"metadata":{"user_id":"whatever-new-format"}}"#
            .to_string(),
    );
    assert!(detect_for(&odd_meta, all_on()).is_some(), "非 CC UA 带奇异 user_id 也走模拟");

    // 3) 只带 X-Claude-Code-Session-Id 头、UA 不是 claude-cli → 走模拟。
    let mut cc_header = crate::proxy::HeaderMap::new();
    cc_header.insert(
        crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
        HeaderValue::from_static("bc201916-d0bc-4b4e-adba-caf41fb58746"),
    );
    assert!(detect_with(&plain, &cc_header, all_on()).is_some(), "非 CC UA 带 session 头也走模拟");

    // 4) 裸请求、裸 UA → 走模拟。
    assert!(detect_for(&plain, all_on()).is_some(), "裸第三方请求仍应走模拟");

    // 5) UA 自报 `claude-cli/<版本>` 但体不是 CC 形态（没 system / 自造 system）→ 抄了
    //    UA 的第三方，**走模拟**。
    const VSCODE_UA: &str = "claude-cli/2.1.226 (external, claude-vscode, agent-sdk/0.3.226)";
    let mut cc_ua = crate::proxy::HeaderMap::new();
    cc_ua.insert(header::USER_AGENT, HeaderValue::from_static(VSCODE_UA));
    assert!(
        detect_with(&plain, &cc_ua, all_on()).is_some(),
        "只有 claude-cli UA、没有 CC 形态的该走模拟"
    );
    assert!(
        detect_with(&with_meta, &cc_ua, all_on()).is_some(),
        "claude-cli UA + 自造 system + 官方格式 user_id 仍该走模拟"
    );
    // 5b) UA 与形态都对上 → **不模拟**，且非模拟路径原样转发它自报的 UA。
    let cc_shaped = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","system":[{{"type":"text","text":"{}"}},{}],"messages":[]}}"#,
        config::CC_SYSTEM_IDENTITY,
        base_block()
    ));
    assert!(
        detect_with(&cc_shaped, &cc_ua, all_on()).is_none(),
        "claude-cli UA + CC 形态的不该走模拟"
    );
    let out = build_forward_headers(&cc_ua, "tok", all_on(), None, None);
    assert_eq!(
        out.get(header::USER_AGENT).and_then(|v| v.to_str().ok()),
        Some(VSCODE_UA),
        "非模拟路径必须原样转发客户端自报的 UA"
    );

    // 6) UA 里读不出 `claude-cli/<版本>` → 走模拟。
    let mut sdk_ua = crate::proxy::HeaderMap::new();
    sdk_ua.insert(header::USER_AGENT, HeaderValue::from_static("python-httpx/0.27.0"));
    assert!(detect_with(&plain, &sdk_ua, all_on()).is_some(), "第三方 UA 仍应走模拟");

    // 7) 模拟路径下客户端原有的 metadata.user_id 被剥掉、由 ensure_cc_metadata 重建，
    //    确保 session_id 与出站头同值。
    let sim = detect_for(&with_meta, all_on()).unwrap();
    let out = rewrite_body(&with_meta, &test_cred(), "fp", all_on(), Some(&sim), None);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let user_id = v["metadata"]["user_id"].as_str().expect("应重建 metadata.user_id");
    let inner: serde_json::Value = serde_json::from_str(user_id).unwrap();
    assert_eq!(
        inner["session_id"].as_str().unwrap(),
        &sim.session_id,
        "body 里的 session_id 必须与 sim.session_id 一致"
    );

    // 8) 真实 CC 客户端没带 `metadata.user_id` 时也要补——上游对无 metadata 的请求
    //    走更严的限流通道，不补会裸 429。
    assert!(
        crate::proxy::bare_session_id(&cc_ua, all_on(), None, true, false, &test_cred(), "fp")
            .is_some(),
        "真实 CC 客户端无 metadata 也应补身份"
    );
}

/// 基座资产是逐字节从抓包取出来的，别被编辑器/格式化工具动过。
#[test]
fn system_base_assets_are_verbatim() {
    assert_eq!(
        config::CC_SYSTEM_BASE.len(),
        1588,
        "2.1.277 四族同一份基座的字节数（cap/2.1.277/00023）"
    );
    assert_eq!(config::CC_SYSTEM_IDENTITY.len(), 57, "身份句字节数");
    assert_eq!(config::CC_SYSTEM_REPORTING.len(), 911, "reporting 块字节数");
    let base = config::CC_SYSTEM_BASE;
    assert!(base.starts_with("\nYou are an interactive agent"), "开头那个 \\n 是官方就有的");
    assert!(!base.ends_with('\n'), "结尾多出的换行是编辑器加的，官方没有");
    assert!(
        base.contains("Text inside <pasted_content> tags"),
        "2.1.277 相对 2.1.258 的 opus 短基座多的正是这一行 Harness 条目"
    );
    // 基座是「切点之前」那一段，锚点属于其余段，不该出现在基座里。
    for anchor in config::CC_SYSTEM_BASE_ANCHORS {
        assert!(!base.contains(anchor), "基座里不该有拆块锚点: {anchor}");
    }
    // 2.1.277 第四块的开头就是既有的那条锚点，透传路径拆 API-key 三块形态时能切对。
    assert!(config::CC_SYSTEM_BASE_ANCHORS.iter().any(|a| config::CC_SYSTEM_REST.starts_with(a)));
}

/// **对着 `cap/auto-2.1.291-20261006-full` 逐项核**：四族各造一条第三方请求（只有一句 `hi` 和一段
/// 客户端 system）走模拟路径，出站的 beta、`anthropic-dispatch-id`、顶层键序、`thinking`、
/// `context_management`、`thread`、基座、第四块、14 个内建工具都要与那个模型的官方主线程一致。
/// opus / sonnet / haiku 取默认权限模式的首轮（`00340`、`00253`、`00303`）；fable 只有 auto 模式的
/// （`00464`），它的 beta 去掉 auto 模式才有的 `afk-mode` 与 `dangerous-tool-use`、Bash 不比（auto
/// 那版少一句）。
///
/// 刻意不比的几项（各有出处）：`effort`（模拟三族一律 high）、auto 模式才有的 `safeguards`、
/// `max_tokens`（来访自己的）、`stream`。抓包目录不在仓库里（`.gitignore`），没有就跳过。
#[test]
fn simulated_main_threads_match_the_2_1_291_captures() {
    let dir = format!("{}/cap/auto-2.1.291-20261006-full", env!("CARGO_MANIFEST_DIR"));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("skipped: cap/auto-2.1.291-20261006-full not present");
        return;
    };
    let files: Vec<std::path::PathBuf> = entries.filter_map(|e| Some(e.ok()?.path())).collect();
    for (n, auto_mode) in [("00340", false), ("00253", false), ("00303", false), ("00464", true)] {
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
        let head = std::str::from_utf8(&raw[..sep]).unwrap();
        let official: serde_json::Value = serde_json::from_slice(&raw[sep + 4..]).unwrap();
        let model = official["model"].as_str().unwrap();
        let header = |name: &str| {
            head.lines().find_map(|l| l.strip_prefix(&format!("{name}: ")[..])).unwrap()
        };

        let body = Bytes::from(format!(
            r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"system":"你是助手","max_tokens":32000}}"#
        ));
        // 对照的是官方默认配置的抓包（14 个工具），关掉默认开着的精简。
        let full_tools = store::ForwardFlags { sim_trim_tools: false, ..all_on() };
        let sim = detect_for(&body, full_tools).unwrap();
        let out = rewrite_body(&body, &test_cred(), "fp", full_tools, Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let h = crate::proxy::build_forward_headers_for(
            &crate::proxy::HeaderMap::new(),
            "tok",
            full_tools,
            Some(&sim),
            None,
            Some(model),
            false,
            crate::proxy::BetaCtx::MAIN,
        );

        // beta：auto 模式那条去掉 auto 模式才有的两项。
        let want: Vec<&str> = header("anthropic-beta")
            .split(',')
            .filter(|b| {
                !(auto_mode && (b.starts_with("afk-mode-") || b.starts_with("dangerous-tool-use-")))
            })
            .collect();
        assert_eq!(h["anthropic-beta"], want.join(","), "{model}（{n}）beta");
        assert_eq!(h["anthropic-dispatch-id"], header("anthropic-dispatch-id"), "{model}");
        assert_eq!(h["x-claude-code-request-class"], "main", "{model}");
        let billing = v["system"][0]["text"].as_str().unwrap();
        let pid = h["x-claude-code-prompt-id"].to_str().unwrap();
        assert!(billing.contains(&format!("cc_prompt_id={pid};")), "{model}: 头与 billing 同值");
        assert_eq!(h["x-stainless-package-version"], header("X-Stainless-Package-Version"));

        // `thread`：2.1.291 四族首轮都是 `create`（fable-5-1 也是了），模拟出站与之一致。
        assert_eq!(v.get("thread"), official.get("thread"), "{model}（{n}）thread");
        // 顶层键序：官方键里去掉模拟不造的 safeguards，再与出站共有键比先后。
        let skip = ["safeguards", "stream"];
        let official_keys: Vec<&str> = official
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .filter(|k| !skip.contains(k))
            .collect();
        let out_keys: Vec<&str> = v
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .filter(|k| !skip.contains(k))
            .collect();
        assert_eq!(out_keys, official_keys, "{model}（{n}）键序");
        assert_eq!(v["thinking"], official["thinking"], "{model} thinking");
        assert_eq!(v["context_management"], official["context_management"], "{model}");
        assert_eq!(
            v["output_config"].get("effort").is_some(),
            official["output_config"].get("effort").is_some(),
            "{model}: 带不带 effort 跟官方走（haiku 不带）"
        );

        // system：billing / 身份 / 基座 / 第四块，第五块是客户端自己的。
        let sys = v["system"].as_array().unwrap();
        let osys = official["system"].as_array().unwrap();
        assert_eq!(sys.len(), 5, "{model}");
        let billing = sys[0]["text"].as_str().unwrap();
        assert!(billing.starts_with("x-anthropic-billing-header: cc_version=2.1.291."));
        assert!(billing.contains("; cc_turn_origin=human; cc_prompt_index="), "{billing}");
        for i in 1..=2 {
            assert_eq!(sys[i]["text"], osys[i]["text"], "{model} system[{i}]");
            assert_eq!(sys[i]["cache_control"], osys[i]["cache_control"], "{model} [{i}]");
        }
        assert_eq!(sys[3]["cache_control"], osys[3]["cache_control"], "{model}");
        let cap_env = crate::proxy::SimEnv::from_cwd(
            "/Users/easayliu",
            "/private/tmp/claude-501/-Users-easayliu-Works-easay-luban/0bf28fc4-0424-4927-8ef6-0ccf4214a28d/scratchpad/calc",
        );
        // 官方原文去掉模拟路径不收的两段：EndConversation 那段（haiku 那份没有）与末尾 WebSearch 那段。
        let mut official_rest = osys[3]["text"].as_str().unwrap().to_string();
        if let Some(end) = official_rest.find("EndConversation (deferred tool)") {
            let end_to = end + official_rest[end..].find("\n\n").unwrap() + 2;
            official_rest.replace_range(end..end_to, "");
        }
        let ws = official_rest.find("\n\nWebSearch takes a `mode`").unwrap();
        official_rest.truncate(ws);
        let template = super::cc_system_rest(model).unwrap();
        assert_eq!(
            crate::proxy::render_system_rest(template, &cap_env),
            official_rest,
            "{model}: 第四块按抓包机的环境回填后逐字节相同"
        );

        // 工具：注入的 14 个内建工具与官方那 14 个逐字节相同（顺序是官方的名字序）。
        let tools = v["tools"].as_array().unwrap();
        let otools = official["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 14, "{model}");
        for t in tools {
            let name = t["name"].as_str().unwrap();
            if auto_mode && name == "Bash" {
                continue;
            }
            let o = otools.iter().find(|o| o["name"] == name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(
                serde_json::to_string(t).unwrap(),
                serde_json::to_string(o).unwrap(),
                "{model}: {name}"
            );
        }
    }
}

/// 一个抓包目录里的全部 `/v1/messages`（含 `count_tokens`）当真 CC 来访过一遍：被送进模拟、
/// 被当探针、merge_beta 改了官方串的逐条记下，另给出每条的请求分类。目录不在返回 `None`。
#[allow(clippy::type_complexity)]
fn official_passthrough_report(
    sub: &str,
) -> Option<(usize, Vec<String>, Vec<(String, crate::proxy::CcRequestKind)>)> {
    official_passthrough_report_at(sub, (2, 1, 285))
}

/// 同上，来访自报的版本按 `version` 喂给 merge_beta。
#[allow(clippy::type_complexity)]
fn official_passthrough_report_at(
    sub: &str,
    version: (u64, u64, u64),
) -> Option<(usize, Vec<String>, Vec<(String, crate::proxy::CcRequestKind)>)> {
    let dir = format!("{}/cap/{sub}", env!("CARGO_MANIFEST_DIR"));
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.to_str().is_some_and(|f| f.ends_with(".req.raw")))
        .collect();
    files.sort();
    let mut seen = 0;
    let mut bad = Vec::new();
    let mut kinds = Vec::new();
    for path in files {
        let raw = std::fs::read(&path).unwrap();
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = std::str::from_utf8(&raw[..sep]).unwrap();
        if !head.lines().next().unwrap().contains("/v1/messages") {
            continue;
        }
        seen += 1;
        let name = path.file_name().unwrap().to_str().unwrap()[..5].to_string();
        let v: serde_json::Value = serde_json::from_slice(&raw[sep + 4..]).unwrap();
        let mut headers = crate::proxy::HeaderMap::new();
        for l in head.lines().skip(1) {
            let Some((k, val)) = l.split_once(": ") else { continue };
            if let (Ok(k), Ok(val)) = (
                crate::proxy::HeaderName::from_bytes(k.to_ascii_lowercase().as_bytes()),
                HeaderValue::from_str(val),
            ) {
                headers.insert(k, val);
            }
        }
        let beta_raw = headers
            .get("anthropic-beta")
            .and_then(|b| b.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let beta = crate::proxy::inbound_beta_list(&headers);
        let model = v["model"].as_str().unwrap_or_default();
        let reason = crate::proxy::simulation_reason(Some(&v), &headers, true, all_on());
        if let Some(r) = reason {
            bad.push(format!("{name} {model}: 被送进模拟（{}）", r.tag()));
        }
        let probe = crate::proxy::probe_signature(
            Some(&v),
            crate::proxy::extract_device_id(Some(&v)).as_deref(),
            &beta,
            true,
            false,
            || false,
        );
        if let Some(p) = probe {
            bad.push(format!("{name} {model}: 被当探针（{p:?}）"));
        }
        // 与 handler 同一条路：请求分类与体里那几项事实决定 merge_beta 补哪些。
        let kind = crate::proxy::CcRequestKind::of(&v, &beta);
        kinds.push((name.clone(), kind));
        let merged = crate::proxy::merge_beta_for(
            Some(&beta_raw),
            Some(model),
            Some(version),
            crate::proxy::BetaCtx::of(kind, &raw[sep + 4..], Some(&v)),
        );
        if !beta_raw.is_empty() && merged != beta_raw {
            bad.push(format!(
                "{name} {model}: merge_beta 改了官方串\n  官方 {beta_raw}\n  合并 {merged}"
            ));
        }
    }
    Some((seen, bad, kinds))
}

/// **2.1.285 官方各类请求当真 CC 来访过一遍**（`cap/2.1.285` 的全部 `/v1/messages`：主线程
/// 首轮与 thread 续轮、task 通知轮、SDK 子代理首轮与续轮、无工具 helper、子代理摘要、主线程
/// 分叉的 auxiliary、标题生成、额度探测）：都得原样透传（不进模拟）、不被当探针拒，
/// merge_beta 对官方那串一项不多。抓包目录不在仓库里就跳过。
#[test]
fn every_2_1_285_official_request_passes_through() {
    let Some((seen, bad, kinds)) = official_passthrough_report("2.1.285") else {
        eprintln!("skipped: cap/2.1.285 not present");
        return;
    };
    assert!(seen >= 30, "{seen}");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    // 请求分类：两种 thread 续轮不能落进「无工具 = helper」。
    use crate::proxy::CcRequestKind::*;
    let kind_of = |n: &str| kinds.iter().find(|(k, _)| k == n).map(|(_, k)| *k);
    for (n, want) in [
        ("00017", QuotaProbe),
        ("00038", Title),
        ("00113", Main),
        ("00115", Main),     // 主线程 thread 续轮
        ("00149", Main),     // task 通知那一轮，也是续轮
        ("00120", Subagent), // 子代理首轮
        ("00127", Subagent), // 子代理 thread 续轮
        ("00125", Helper),   // 无工具 helper
        ("00135", Subagent), // 子代理进度摘要：带工具、`cc_is_subagent`
    ] {
        assert_eq!(kind_of(n), Some(want), "{n}");
    }
}

/// **2.1.285 各种用法的官方请求**（`cap/auto-2.1.285-20260930`：auto / default / plan 权限
/// 模式、`-p` 打印模式、`--continue`、`/compact`、fast、各档 effort、1M、fable / sonnet /
/// haiku 主线程、Explore / general-purpose / Plan 子代理、WebSearch、`count_tokens`……）
/// 同样一条不进模拟、不被当探针、merge_beta 一项不多。
#[test]
fn every_auto_2_1_285_official_request_passes_through() {
    let Some((seen, bad, _)) = official_passthrough_report("auto-2.1.285-20260930") else {
        eprintln!("skipped: cap/auto-2.1.285-20260930 not present");
        return;
    };
    assert!(seen >= 100, "{seen}");
    assert!(bad.is_empty(), "{} 条：\n{}", bad.len(), bad.join("\n"));
}

/// **2.1.291 各种用法的官方请求**（`cap/auto-2.1.291-20261006-full`：auto / default / plan、`-p`、
/// `--continue`、`/compact`、各档 effort、1M、四族主线程、三种子代理、WebSearch / WebFetch、
/// `count_tokens`；`cap/auto-2.1.291-20261006`：四族关掉 Artifact 等三个工具前后）同样一条不进模拟、
/// 不被当探针、merge_beta 一项不多。
#[test]
fn every_auto_2_1_291_official_request_passes_through() {
    for sub in ["auto-2.1.291-20261006-full", "auto-2.1.291-20261006"] {
        let Some((seen, bad, _)) = official_passthrough_report_at(sub, (2, 1, 291)) else {
            eprintln!("skipped: cap/{sub} not present");
            continue;
        };
        assert!(seen >= 8, "{sub}: {seen}");
        assert!(bad.is_empty(), "{sub} {} 条：\n{}", bad.len(), bad.join("\n"));
    }
}

/// **2.1.285 新认的几种官方形态**，不靠抓包目录（它不进仓库，CI 上上面两条回放会跳过）：
/// 每种按 `cap/auto-2.1.285-20260930` 里的样子造最小的一条，正反各一。
#[test]
fn auto_2_1_285_official_shapes_are_recognized() {
    use super::{
        is_official_count_tokens, is_official_helper_request, is_official_thread_continuation,
        is_official_title_request, is_official_web_search_request, simulation_reason,
    };
    use crate::proxy::CcRequestKind::{self, *};
    use crate::proxy::{BetaCtx, merge_beta_for};
    let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
    let beta = |s: &str| s.split(',').map(str::to_string).collect::<Vec<_>>();
    let mut cc = crate::proxy::HeaderMap::new();
    cc.insert(header::USER_AGENT, HeaderValue::from_static("claude-cli/2.1.285 (external, cli)"));
    let with_beta = |b: &'static str| {
        let mut h = cc.clone();
        h.insert("anthropic-beta", HeaderValue::from_static(b));
        h
    };
    let billing =
        "x-anthropic-billing-header: cc_version=2.1.285.989; cc_entrypoint=cli; cch=821a0;";
    let identity = config::CC_SYSTEM_IDENTITY;

    // count_tokens：只有三个键 + token-counting beta → 透传、只补 oauth；多一个 max_tokens 就不是。
    let ct_beta = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                       context-management-2025-06-27,token-counting-2024-11-01";
    let ct =
        j(r#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"foo"}],"tools":[]}"#);
    assert_eq!(simulation_reason(Some(&ct), &with_beta(ct_beta), true, all_on()), None);
    assert_eq!(CcRequestKind::of(&ct, &beta(ct_beta)), CountTokens);
    let merged = merge_beta_for(
        Some(ct_beta),
        Some("claude-opus-5-5"),
        Some((2, 1, 285)),
        BetaCtx::of(CountTokens, ct.to_string().as_bytes(), Some(&ct)),
    );
    assert_eq!(merged, ct_beta, "count_tokens 那串一项不多");
    let mut not_ct = ct.clone();
    not_ct["max_tokens"] = serde_json::json!(1024);
    assert!(!is_official_count_tokens(&not_ct, &beta(ct_beta)));
    assert!(!is_official_count_tokens(&ct, &[]), "没有 token-counting beta 不算");

    // 续轮重发 tools：新增消息里有 tool_reference（ToolSearch 载入）+ mid-conversation-tool-changes。
    let threads = beta("message-threads-2026-08-12,mid-conversation-tool-changes-2026-07-01");
    let cont = |msg: &str| {
        j(&format!(
            r#"{{"model":"claude-opus-5-5","messages":[{msg}],"system":[{{"type":"text","text":"{billing}"}}],
                "tools":[{{"name":"Bash","description":"x","input_schema":{{}}}}],
                "thread":{{"type":"continue","previous_message_id":"msg_01A"}},"diagnostics":{{"previous_message_id":"msg_01A"}}}}"#
        ))
    };
    let loaded = cont(
        r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":[{"type":"tool_reference","tool_name":"WebSearch"}]},{"type":"text","text":"Tool loaded."}]}"#,
    );
    assert!(is_official_thread_continuation(&loaded, &threads));
    assert!(!is_official_thread_continuation(&loaded, &threads[..1]), "没声明 tool-changes");
    let mcp = cont(
        r#"{"role":"system","content":[{"type":"tool_addition","tool":{"type":"tool_reference","name":"mcp__x__y"}}]}"#,
    );
    assert!(is_official_thread_continuation(&mcp, &threads), "MCP 工具中途上线");
    let plain = cont(
        r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}"#,
    );
    assert!(!is_official_thread_continuation(&plain, &threads), "工具集没变却带 tools");

    // WebSearch 子调用：billing 后面多一句 CC 身份句也认；归 Auxiliary，不进主线程链。
    let ws = j(&format!(
        r#"{{"model":"claude-haiku-4-5-20251001","messages":[{{"role":"user","content":"Perform a web search for the query: x"}}],
            "system":[{{"type":"text","text":"{billing}"}},{{"type":"text","text":"{identity}"}},
            {{"type":"text","text":"You are an assistant for performing a web search tool use"}}],
            "tools":[{{"type":"web_search_20250305","name":"web_search"}}],"tool_choice":{{"type":"tool","name":"web_search"}}}}"#
    ));
    assert!(is_official_web_search_request(&ws));
    assert_eq!(CcRequestKind::of(&ws, &[]), Auxiliary);

    // 主线程 WebFetch 页面处理：[不带子代理标记的 billing, CC 身份句] + helper 的 body 与 beta。
    let helper_beta =
        config::cc_profile_rows(config::CcProfileKind::HelperSubagentHaiku).last().unwrap().beta;
    let fetch = j(&format!(
        r#"{{"model":"claude-haiku-4-5-20251001","messages":[{{"role":"user","content":"\nWeb page content:\n---\nExample"}}],
            "system":[{{"type":"text","text":"{billing}"}},{{"type":"text","text":"{identity}"}}],
            "tools":[],"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"stream":true}}"#
    ));
    assert!(is_official_helper_request(&fetch, &beta(helper_beta)));
    assert_eq!(CcRequestKind::of(&fetch, &beta(helper_beta)), Auxiliary);

    // 会话起名：380 字节的提示词也算标题一类；几十字节的不算。
    let naming = |prompt: &str| {
        j(&format!(
            r#"{{"model":"claude-haiku-4-5-20251001","messages":[{{"role":"user","content":"<conversation>x</conversation>"}}],
                "system":[{{"type":"text","text":"{billing}"}},{{"type":"text","text":"{identity}"}},{{"type":"text","text":"{prompt}"}}],
                "tools":[],"max_tokens":32000,"thinking":{{"type":"disabled"}},"stream":true}}"#
        ))
    };
    let so = beta(config::CC_BETA_STRUCTURED_OUTPUTS);
    assert!(is_official_title_request(&naming(&"k".repeat(380)), &so));
    assert!(!is_official_title_request(&naming("Generate a short name."), &so));

    // 分叉与预热的请求分类。
    let fork = |last: &str| {
        j(&format!(
            r#"{{"model":"claude-opus-5-5","messages":[{{"role":"user","content":[{{"type":"text","text":"{last}"}}]}}],
                "system":[{{"type":"text","text":"{billing}"}}],"tools":[{{"name":"Bash"}}]}}"#
        ))
    };
    assert_eq!(
        CcRequestKind::of(&fork("CRITICAL: Respond with TEXT ONLY. Do NOT call any tools."), &[]),
        Fork
    );
    assert_eq!(
        CcRequestKind::of(
            &fork("<system-reminder>This is a side question from the user.</system-reminder>"),
            &[]
        ),
        Fork
    );
    assert_eq!(
        CcRequestKind::of(&fork("The user stepped away and is coming back. Recap"), &[]),
        Fork
    );
    assert_eq!(CcRequestKind::of(&fork("fix the bug"), &[]), Main);
    let hi = j(&format!(
        r#"{{"model":"claude-opus-5-5","max_tokens":1,"system":[{{"type":"text","text":"{billing}"}},{{"type":"text","text":"{identity}"}}],
            "messages":[{{"role":"user","content":[{{"type":"text","text":"Hi","cache_control":{{"type":"ephemeral"}}}}]}}]}}"#
    ));
    assert_eq!(CcRequestKind::of(&hi, &[]), Prewarm);
    let hi_beta = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                       context-management-2025-06-27,prompt-caching-scope-2026-01-05";
    assert_eq!(
        merge_beta_for(
            Some(hi_beta),
            Some("claude-opus-5-5"),
            Some((2, 1, 285)),
            BetaCtx::of(Prewarm, hi.to_string().as_bytes(), Some(&hi))
        ),
        hi_beta,
        "/model 预热只补 oauth"
    );

    // extended-cache-ttl 跟着体里的 1h 走；display=omitted 不补 thinking-display-updates。
    let main_beta = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                         effort-2025-11-24,advanced-tool-use-2025-11-20,cache-diagnosis-2026-04-07";
    let no_ttl = BetaCtx { ttl_1h: false, ..BetaCtx::MAIN };
    let merged =
        merge_beta_for(Some(main_beta), Some("claude-opus-5-5"), Some((2, 1, 285)), no_ttl);
    assert!(!merged.contains("extended-cache-ttl"), "{merged}");
    assert!(
        merge_beta_for(Some(main_beta), Some("claude-opus-5-5"), Some((2, 1, 285)), BetaCtx::MAIN)
            .contains("extended-cache-ttl")
    );
    assert_eq!(
        crate::proxy::ensure_cache_ttl_beta(&merged),
        merged.replace("cache-diagnosis", "extended-cache-ttl-2025-04-11,cache-diagnosis"),
        "改写补出 1h 之后再补回原位"
    );
    let omitted = j(
        r#"{"thinking":{"type":"adaptive","display":"omitted"},"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#,
    );
    let ctx = BetaCtx::of(Main, omitted.to_string().as_bytes(), Some(&omitted));
    assert_eq!(ctx, BetaCtx { display_omitted: true, ..BetaCtx::MAIN });
    let fable = merge_beta_for(Some(main_beta), Some("claude-fable-5-1"), Some((2, 1, 285)), ctx);
    assert!(!fable.contains("thinking-display-updates"), "{fable}");
}

/// 第四块模板里 luban 自己的两个占位。只认这两个名字：官方正文里本来就有
/// `{{…}}` 一类的双花括号写法，不能拿 `{{` 当判据。
const REST_PLACEHOLDERS: [&str; 2] = ["{{cwd_slug}}", "{{home}}"];

/// 正文里还剩没填的占位。
fn unfilled(text: &str) -> bool {
    REST_PLACEHOLDERS.iter().any(|ph| text.contains(ph))
}

/// 第四块模板逐字节取自抓包：字节数钉住，占位齐全，抓包机的路径一个都不能留。2.1.291 有三份：
/// opus / sonnet（[`config::CC_SYSTEM_REST`]）、fable、haiku。
#[test]
fn system_rest_assets_are_verbatim() {
    // （模板，字节数，开头）：抓包原文去掉 EndConversation 那段（haiku 那份本来就没有）与
    // `<total_tokens>` 之后的 WebSearch 那段，再把记忆目录换成两个占位。
    for (asset, len, head) in [
        (config::CC_SYSTEM_REST, 4700, "Write code that reads like the surrounding code"),
        (
            config::CC_SYSTEM_REST_FABLE,
            10794,
            "Before you start, say in a line what you're about to do",
        ),
        (config::CC_SYSTEM_REST_HAIKU, 16720, "# Text output (does not apply to tool calls)"),
    ] {
        assert_eq!(asset.len(), len, "{head}");
        assert!(asset.starts_with(head), "{head}");
        for ph in REST_PLACEHOLDERS {
            assert_eq!(
                asset.matches(ph).count(),
                1,
                "{head}: 占位 {ph} 恰好出现一次（记忆目录那一处）"
            );
        }
        assert!(
            asset.contains("{{home}}/.claude/projects/{{cwd_slug}}/memory/`"),
            "{head}: 记忆目录"
        );
        assert!(
            asset.ends_with("<total_tokens>15000000 tokens left</total_tokens>"),
            "{head}: 末行"
        );
        for leak in ["easayliu", "opdash", "proxy_captures", "scratchpad"] {
            assert!(!asset.contains(leak), "{head}: 模板里不该留抓包机的 {leak}");
        }
        // 模拟路径不注 ToolSearch 与延迟池里的 WebSearch（见 `cc_tools_core`），依赖它们的那两段
        // 指令不能留：提示词让模型去调一个客户端没声明的工具，是提示词与工具集不成套。
        assert!(!asset.contains("EndConversation") && !asset.contains("ToolSearch"), "{head}");
        assert!(!asset.contains("WebSearch takes a `mode`"), "{head}");
        for gone in ["Primary working directory", "powered by the model", "knowledge cutoff"] {
            assert!(!asset.contains(gone), "{head}: {gone}");
        }
        for section in ["# Session-specific guidance", "# Environment", "# Context management"] {
            assert!(asset.contains(section), "{head}: {section}");
        }
    }
    let opus = config::CC_SYSTEM_REST;
    // 2.1.291 opus / sonnet 那份：记忆一节是 2.1.285 那种写法，少了 `<cc-memory>` 引用那句。
    assert!(opus.contains("# Memory") && !opus.contains("<cc-memory"));
    for gone in
        ["# Delivering work", "# Writing for the user", "This iteration of Claude", "# auto memory"]
    {
        assert!(!opus.contains(gone), "{gone}");
    }
    // fable 那份换回带自我介绍与两节写作说明的写法。
    let fable = config::CC_SYSTEM_REST_FABLE;
    for kept in [
        "This iteration of Claude is Claude Fable 5.1",
        "# Delivering work",
        "# Writing for the user",
    ] {
        assert!(fable.contains(kept), "{kept}");
    }
    // haiku 那份配长基座，记忆一节是 `# auto memory` 长写法。
    assert!(config::CC_SYSTEM_REST_HAIKU.contains("# auto memory"));
    assert_eq!(
        config::CC_SYSTEM_BASE_HAIKU.chars().count(),
        11050,
        "cap/auto-2.1.291-20261006-full/00303"
    );
    assert!(!config::CC_SYSTEM_BASE_HAIKU.contains("easayliu"));
}

/// 各族选各自那份模板，填完一个占位都不剩；按抓包机的取值回填，字节数是模板多 38（两个占位
/// 换成 `/Users/easayliu` 与那串 43 字节的项目段）。
#[test]
fn system_rest_renders_every_placeholder() {
    use crate::proxy::{SimEnv, cc_system_rest, render_system_rest};
    assert_eq!(
        SimEnv::from_cwd("/Users/easayliu", "/Users/easayliu/Works/easay/opdash").slug,
        "-Users-easayliu-Works-easay-opdash",
        "官方的项目段拼法（cap/2.1.277/00023）"
    );
    let cap = SimEnv::from_cwd("/Users/easayliu", "/private/tmp/proxy_captures/20260930_143352");
    assert_eq!(
        SimEnv::from_cwd("/Users/x", "/private/tmp/proxy_captures/20260904_170955").slug,
        "-private-tmp-proxy-captures-20260904-170955",
        "下划线也换成横线（cap/2.1.260-2/00013）"
    );
    for (m, template) in [
        ("claude-opus-5", config::CC_SYSTEM_REST),
        ("claude-sonnet-5-5", config::CC_SYSTEM_REST),
        ("claude-fable-5-1", config::CC_SYSTEM_REST_FABLE),
        ("claude-haiku-4-5-20251001", config::CC_SYSTEM_REST_HAIKU),
    ] {
        let t = cc_system_rest(m).expect(m);
        assert_eq!(t, template, "{m}");
        let out = render_system_rest(t, &cap);
        assert_eq!(out.len(), template.len() + 38, "{m}");
        assert!(!unfilled(&out), "{m} 有占位没填: {out}");
        assert!(
            out.contains("/Users/easayliu/.claude/projects/-private-tmp-proxy-captures-20260930-143352/memory/`"),
            "{m}"
        );
    }
    // fable-5 不能拿到「我是 Fable 5.1」：只把自我介绍段换成 Fable 5 那段，其余与 5.1 那份相同
    // （2.1.291 `function qIo` 按规范名挑这一段）。
    let fable5 = cc_system_rest("claude-fable-5").unwrap();
    assert!(fable5.contains(config::CC_FABLE_5_IDENTITY));
    assert!(!fable5.contains("Claude Fable 5.1"), "fable-5 的提示词里不该出现 5.1");
    assert_eq!(
        fable5.replacen(config::CC_FABLE_5_IDENTITY, config::CC_FABLE_5_1_IDENTITY, 1),
        config::CC_SYSTEM_REST_FABLE
    );
    for m in ["claude-fable-5-1", "claude-fable-5-1[1m]"] {
        assert_eq!(cc_system_rest(m), Some(config::CC_SYSTEM_REST_FABLE), "{m}");
    }
    // 基座：haiku 那份长的，其余三族同一份。
    assert_eq!(
        super::cc_system_base("claude-haiku-4-5-20251001"),
        Some(config::CC_SYSTEM_BASE_HAIKU)
    );
    for m in ["claude-opus-5-5", "claude-fable-5-1", "claude-sonnet-5-5"] {
        assert_eq!(super::cc_system_base(m), Some(config::CC_SYSTEM_BASE), "{m}");
    }
    // 认不出的模型不补第四块：落回「末块放客户端 system」的旧形态。
    assert!(cc_system_rest("gpt-4o").is_none());
    assert!(sim_for(r#"{"model":"gpt-4o","messages":[]}"#).rest.is_none());
    assert!(sim_for(PLAIN_BODY).rest.is_some());
}

/// message-threads 续轮按 `cap/2.1.277/00035` 六项逐项对：`thread` 两个字段、`diagnostics`
/// 同一个 id、单块 billing system、没有 `tools`、`message-threads` beta、messages 非空。
/// 只抄 `thread` 两个字段的、缺任何一项的都不算——那是「可信 UA + 合法身份 + 两个字段」
/// 就能把一条没基座没工具的请求原样透传的口子。
#[test]
fn thread_continuation_is_recognized_only_with_a_previous_message() {
    use crate::proxy::is_official_thread_continuation;
    let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
    let threads_beta = vec![config::CC_BETA_MESSAGE_THREADS.to_string()];
    // 只有 thread 两个字段、别的一样都没有：不算。
    assert!(!is_official_thread_continuation(
        &j(
            r#"{"model":"claude-sonnet-5","thread":{"type":"continue","previous_message_id":"msg_011CfBtz2HLGiHiC4riqiNAA"}}"#
        ),
        &threads_beta
    ));

    // 可信 UA + 合法身份 + 官方续轮形态 → 不模拟，透传。照 cap/2.1.277/00035 的样子造。
    let mut cc_ua = crate::proxy::HeaderMap::new();
    cc_ua
        .insert(header::USER_AGENT, HeaderValue::from_static("claude-cli/2.1.277 (external, cli)"));
    cc_ua.insert(
        "anthropic-beta",
        HeaderValue::from_static(
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 message-threads-2026-08-12",
        ),
    );
    let cont = Bytes::from(concat!(
        r#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"ok"}]},"#,
        r#"{"role":"system","content":[{"type":"text","text":"<system-reminder>\n<total_tokens>14981508 tokens left</total_tokens>\n</system-reminder>","cache_control":{"type":"ephemeral","ttl":"1h"}}]}],"#,
        r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.277.d56; cc_entrypoint=cli; cch=429dc; cc_prev_req=req_011CfBtz1vkPkP5eC61usqa2; cc_prompt_id=759919ef-90c7-4602-b855-792116e1b7e9; cc_turn_origin=human;"}],"#,
        r#""metadata":{"user_id":"{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"7fe47444-c834-44e0-b568-d61e07daa35e\"}"},"#,
        r#""max_tokens":64000,"thinking":{"type":"adaptive","display":"updates"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"#,
        r#""output_config":{"effort":"high"},"thread":{"type":"continue","previous_message_id":"msg_011CfBtz2HLGiHiC4riqiNAA"},"diagnostics":{"previous_message_id":"msg_011CfBtz2HLGiHiC4riqiNAA"},"stream":true}"#
    ));
    let official: serde_json::Value = serde_json::from_slice(&cont).unwrap();
    assert!(is_official_thread_continuation(&official, &threads_beta));
    assert!(detect_with(&cont, &cc_ua, all_on()).is_none(), "官方续轮不该被重建成主线程");
    // 也不该被当成一次性探针（有 system、没 tools、一条消息、新设备）。
    assert!(
        crate::proxy::probe_signature(
            Some(&official),
            Some("b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d"),
            &threads_beta,
            true,
            false,
            || false,
        )
        .is_none(),
        "官方续轮不是探针"
    );

    // 六项缺任何一项都不算续轮；这些体在可信 UA 下落回「去掉基座的第三方」走模拟。
    let broken: Vec<(&str, serde_json::Value)> = vec![
        ("去掉 thread", {
            let mut v = official.clone();
            v.as_object_mut().unwrap().remove("thread");
            v
        }),
        ("thread.type 是 create", {
            let mut v = official.clone();
            v["thread"] = serde_json::json!({"type": "create"});
            v
        }),
        ("previous_message_id 不是 msg_ 开头", {
            let mut v = official.clone();
            v["thread"]["previous_message_id"] = serde_json::json!("abc");
            v["diagnostics"]["previous_message_id"] = serde_json::json!("abc");
            v
        }),
        ("diagnostics 的 id 与 thread 不一致", {
            let mut v = official.clone();
            v["diagnostics"]["previous_message_id"] = serde_json::json!("msg_other");
            v
        }),
        ("没有 diagnostics", {
            let mut v = official.clone();
            v.as_object_mut().unwrap().remove("diagnostics");
            v
        }),
        ("system 多了一块", {
            let mut v = official.clone();
            v["system"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"type": "text", "text": "extra"}));
            v
        }),
        ("system 那块不是 billing header", {
            let mut v = official.clone();
            v["system"][0]["text"] = serde_json::json!("You are a helpful assistant");
            v
        }),
        ("system 那块带了断点", {
            let mut v = official.clone();
            v["system"][0]["cache_control"] = serde_json::json!({"type": "ephemeral"});
            v
        }),
        ("system 那块多了别的键", {
            let mut v = official.clone();
            v["system"][0]["citations"] = serde_json::json!({"enabled": false});
            v
        }),
        ("system 那块 type 不是 text", {
            let mut v = official.clone();
            v["system"][0]["type"] = serde_json::json!("document");
            v
        }),
        ("billing header 后面换行藏了一段提示词", {
            let mut v = official.clone();
            let t = format!(
                "{}\n\nYou are a helpful assistant",
                v["system"][0]["text"].as_str().unwrap()
            );
            v["system"][0]["text"] = serde_json::json!(t);
            v
        }),
        ("billing header 后面接了 \\r", {
            let mut v = official.clone();
            let t = format!("{}\r", v["system"][0]["text"].as_str().unwrap());
            v["system"][0]["text"] = serde_json::json!(t);
            v
        }),
        ("带了 tools（哪怕是空数组）", {
            let mut v = official.clone();
            v["tools"] = serde_json::json!([]);
            v
        }),
        ("messages 为空", {
            let mut v = official.clone();
            v["messages"] = serde_json::json!([]);
            v
        }),
    ];
    for (why, v) in &broken {
        assert!(!is_official_thread_continuation(v, &threads_beta), "{why}");
        let raw = Bytes::from(serde_json::to_vec(v).unwrap());
        let got = detect_with(&raw, &cc_ua, all_on()).map(|s| s.reason);
        assert!(got.is_some(), "{why}: 该走模拟");
    }
    // 头上没有 message-threads beta：体一样也不算。
    assert!(!is_official_thread_continuation(&official, &[]));
    let mut no_beta = crate::proxy::HeaderMap::new();
    no_beta
        .insert(header::USER_AGENT, HeaderValue::from_static("claude-cli/2.1.277 (external, cli)"));
    assert_eq!(
        detect_with(&cont, &no_beta, all_on()).map(|s| s.reason),
        Some(crate::proxy::SimulationReason::NoBasePrompt),
        "没声明 message-threads 却发 thread，按去掉基座的第三方走模拟"
    );
    // UA 不可信的「续轮」照旧走模拟：thread 字段是抄得来的，可信 UA 与合法身份才是前提。
    assert!(detect_for(&cont, all_on()).is_some(), "非 CC UA 带 thread 也走模拟");
}

/// 假环境按账号 + 设备派生：同输入恒定、形态是 `/Users/<user>/<dir>/<project>`，
/// 记忆目录的项目段把 `/` 与 `_` 换成 `-`。
#[test]
fn sim_env_is_derived_per_account_and_device() {
    let cred = test_cred();
    let a = crate::proxy::sim_env_for(&cred, "fp-a");
    assert_eq!(a, crate::proxy::sim_env_for(&cred, "fp-a"), "同账号同设备恒定");
    let parts: Vec<&str> = a.slug.split('-').collect();
    assert_eq!(parts.len(), 5, "-Users-<user>-<dir>-<project>: {}", a.slug);
    assert_eq!((parts[0], parts[1]), ("", "Users"));
    assert_eq!(a.home, format!("/Users/{}", parts[2]));
    assert!(a.slug.starts_with(&a.home.replace('/', "-")));
    assert!(!a.slug.contains("easayliu") && !a.slug.contains("proxy"));
    assert!(!a.slug.contains('_'), "派生路径里没有下划线: {}", a.slug);
    let other = crate::credentials::Credential {
        account_uuid: Some("00000000-0000-4000-8000-000000000001".into()),
        ..cred.clone()
    };
    let envs: std::collections::HashSet<String> =
        (0..64).map(|i| crate::proxy::sim_env_for(&other, &format!("fp-{i}")).slug).collect();
    assert!(envs.len() > 8, "64 台设备不该挤在一两个目录里: {envs:?}");
}

/// 来访自己写了工作目录时第四块就用它那份，认不出来才退回派生的假环境。
#[test]
fn client_working_directory_wins_over_the_derived_one() {
    use crate::proxy::{SimEnv, client_env};
    let env = |body: &str| client_env(&serde_json::from_str::<serde_json::Value>(body).unwrap());

    // 1. 来访自己那条记忆目录：家目录与项目段照抄，不倒推 cwd。
    assert_eq!(
        env(
            r#"{"system":"memory at `/Users/easayliu/.claude/projects/-Users-easayliu-Works-easay-luban/memory/`"}"#
        ),
        Some(SimEnv {
            home: "/Users/easayliu".into(),
            slug: "-Users-easayliu-Works-easay-luban".into(),
        })
    );
    // 2. 明写工作目录的那一行（官方 CC 写在首条用户消息的 `<env>` 里）。
    assert_eq!(
        env(
            r#"{"messages":[{"role":"user","content":"<env>\nWorking directory: /Users/sam/src/api\nIs git repo: Yes\n</env>"}]}"#
        ),
        Some(SimEnv::from_cwd("/Users/sam", "/Users/sam/src/api"))
    );
    // 3. system 正文里裸一条路径；下划线按官方写法换成横线。
    assert_eq!(
        env(r#"{"system":[{"type":"text","text":"repo lives at /home/dev/work/my_app."}]}"#),
        Some(SimEnv { home: "/home/dev".into(), slug: "-home-dev-work-my-app".into() })
    );
    // 用户问句里提到的目录不算「我在这儿干活」：裸路径只认 system。
    assert_eq!(
        env(r#"{"messages":[{"role":"user","content":"why is /Users/sam/src/api broken"}]}"#),
        None
    );
    // `/memory` 后面还有别的：`/memory-backup` 不是记忆目录。这条不认之后整段也没有
    // 别的来源——裸路径扫描同样不收带 `/.claude/` 的路径。
    assert_eq!(
        env(
            r#"{"system":"at /Users/easayliu/.claude/projects/-Users-easayliu-Works-easay-luban/memory-backup/x"}"#
        ),
        None
    );
    // 旧目录那行排在前面时不算数：标签必须是行首，`Previous working directory:` 不是。
    assert_eq!(
        env(
            r#"{"system":"Previous working directory: /Users/old/gone\nWorking directory: /Users/sam/src/api"}"#
        ),
        Some(SimEnv::from_cwd("/Users/sam", "/Users/sam/src/api"))
    );
    // 行首的列表符号不挡事（`<env>` 之外各家写法不一）。
    assert_eq!(
        env(r#"{"system":" - cwd: /Users/sam/src/api"}"#),
        Some(SimEnv::from_cwd("/Users/sam", "/Users/sam/src/api"))
    );
    // 认不出来的一律当没给，由 `sim_env_for` 兜底。
    for junk in [
        r#"{"system":"/Users/sam"}"#,
        r#"{"system":"C:\\Users\\sam\\src"}"#,
        r#"{"system":"cwd: /tmp/work"}"#,
        r#"{"system":"and/or, maybe"}"#,
        // 标签得是行首那个词：`not-cwd:` 不算（首条消息不走裸路径那条，故为 None）。
        r#"{"messages":[{"role":"user","content":"not-cwd: /Users/sam/src/api"}]}"#,
        r#"{"system":"/Users/sam/a/b/c/d/e/f/g/h/i"}"#,
        PLAIN_BODY,
    ] {
        assert_eq!(env(junk), None, "{junk}");
    }
}

/// 整条 detect：来访 system 里那条路径直接落进第四块的记忆目录，派生的那台机器不再出现。
#[test]
fn simulated_memory_path_follows_the_client_working_directory() {
    let body = Bytes::from(
        concat!(
            r#"{"model":"claude-opus-5","max_tokens":1024,"#,
            r#""messages":[{"role":"user","content":"hi"}],"#,
            r#""system":"Working directory: /Users/easayliu/Works/easay/luban"}"#
        )
        .to_string(),
    );
    let sim = detect_for(&body, all_on()).expect("第三方请求应判为需要模拟");
    let rest = sim.rest.as_deref().expect("第四块");
    assert!(
        rest.contains(
            "`/Users/easayliu/.claude/projects/-Users-easayliu-Works-easay-luban/memory/`"
        ),
        "{rest}"
    );
    let derived = crate::proxy::sim_env_for(&test_cred(), "fp");
    assert!(!rest.contains(&derived.slug), "派生的那台机器不该再出现: {rest}");
}

/// cc_version 后缀与官方客户端的算法对齐（逆向自 2.1.251）：
/// sha256("59cf53e54c78" + chars_at(4,7,20) + 自报版本).hex()[..3]
///
/// 2.1.258 的五份抓包用户消息都是 "hi"，全为 `1e2`，与算法结论一致。**2.1.260 已经
/// 证否了这套算法**：六个 profile 各有固定后缀（`222`/`bcd`/…），算法算出来的是 `11d`。
/// 故这条路只剩 [`crate::proxy::billing_header_text`]（给真实 CC 补 billing header）在用，
/// 模拟路径改从 [`config::CcProfile::billing_suffix`] 取。
#[test]
fn cc_version_suffix_matches_official_algorithm() {
    // 用户消息 "hi"（短于 5 字符），位置 4/7/20 全取不到 → "000"
    // sha256("59cf53e54c780002.1.258") 的前 3 个 hex = "1e2"
    let suffix = |v: &serde_json::Value| crate::proxy::cc_version_suffix(v, "2.1.258");
    let body: serde_json::Value = serde_json::json!({
            "model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(suffix(&body), "1e2", "短消息 'hi'（cap/2.1.258 五份全是 1e2）");
    // 版本参与摘要：换个版本号，同一条消息就是另一个后缀。给一个 2.1.258 的来访写
    // 2.1.260 的版本，连后缀都会跟着错。
    assert_ne!(
        crate::proxy::cc_version_suffix(&body, "2.1.260"),
        "1e2",
        "版本进摘要，换版本必换后缀"
    );

    // 消息足够长时取 text[4], text[7], text[20]
    let body2: serde_json::Value = serde_json::json!({
            "model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "abcdefghijklmnopqrstuvwxyz"}]});
    // chars = text[4]='e', text[7]='h', text[20]='u'
    assert_eq!(suffix(&body2).len(), 3, "始终 3 个 hex 字符");

    // content 是数组时取第一个 text 块
    let body3: serde_json::Value = serde_json::json!({
    "model": "claude-sonnet-5",
    "messages": [{"role": "user", "content": [
        {"type": "text", "text": "hi"}
    ]}]});
    assert_eq!(suffix(&body3), "1e2", "数组形式与字符串形式结果一致");

    // 没有 messages 时退化为全 0
    let empty: serde_json::Value = serde_json::json!({"model": "x"});
    assert_eq!(suffix(&empty), "1e2", "无消息退化为 '000' → 同 'hi'");
}
