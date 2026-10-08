use crate::proxy::test_support::{ROLE_400, err_json};
use crate::proxy::{Bytes, StatusCode, store};

/// 测试用：一段上游拒答的整段 JSON 响应体（非流式），当作学规则时记下的回放体。
fn json_reply() -> store::LearnedReply {
    store::LearnedReply {
            sse: false,
            body: r#"{"id":"msg_r","type":"message","role":"assistant","model":"claude-opus-5","content":[],"stop_reason":"refusal","stop_sequence":null,"stop_details":{"type":"refusal","category":"cyber","explanation":"blocked"},"usage":{"input_tokens":12,"output_tokens":0}}"#.into()}
}

/// 拒绝日志的抑制：同一个键在窗口内只出一行，憋掉的条数记在下一行上，且**各键各算各的**。
///
/// 最后那条尤其要盯住：若两台设备共用一个计数，一台刷疯了会把另一台真正需要被看见的那行
/// 一起憋掉——日志里就此看不到第二台撞过限，而那正是排查时唯一的线索。
#[test]
fn rejection_logs_collapse_per_key_and_report_the_gap() {
    let log = crate::proxy::RejectionLog::default();

    // 首条立即出：撞限这件事本身不该等一个窗口才被看见。
    assert_eq!(crate::proxy::take_rejection_log_slot(&log, "device:a"), Some(0));
    // 窗口内的后续全憋着。
    for _ in 0..12 {
        assert_eq!(crate::proxy::take_rejection_log_slot(&log, "device:a"), None);
    }
    // 另一个键不受影响，自己也是立即出。
    assert_eq!(crate::proxy::take_rejection_log_slot(&log, "device:b"), Some(0));

    // 把 a 的「上次打印时刻」推到窗口之外，等价于等了 10 秒。
    {
        let mut map = log.lock();
        let (at, _) = map.get_mut("device:a").expect("a 该在表里");
        *at -= crate::proxy::REJECTION_LOG_WINDOW + std::time::Duration::from_secs(1);
    }
    assert_eq!(
        crate::proxy::take_rejection_log_slot(&log, "device:a"),
        Some(12),
        "憋掉的条数要交给下一行，否则「刷了多少」就没了"
    );
    // 交出去之后重新从 0 计，不该把同一批重复报一次。
    {
        let mut map = log.lock();
        let (at, _) = map.get_mut("device:a").unwrap();
        *at -= crate::proxy::REJECTION_LOG_WINDOW + std::time::Duration::from_secs(1);
    }
    assert_eq!(crate::proxy::take_rejection_log_slot(&log, "device:a"), Some(0));
}

/// 两条实测的形态类 400 原文（逐字），见 [`crate::proxy::ShapeProbe`]。
const EFFORT_400: &str = "This model does not support effort level 'xhigh'. \
                              Supported levels: high, low, max, medium.";

fn json_body(s: &str) -> Option<serde_json::Value> {
    serde_json::from_str(s).ok()
}

/// 请求体：带 effort 档位。
fn effort_req(model: &str, effort: &str) -> Option<serde_json::Value> {
    json_body(&format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"output_config":{{"effort":"{effort}"}}}}"#
    ))
}

/// 实测原文（列表截短）：被点名的类型不带引号，后半截还列着该模型**认**的一串类型。
const TOOL_TYPE_400: &str = "'claude-fable-5' does not support tool types: \
                                 computer_20250124. Did you mean one of advisor_20260301, \
                                 bash_20250124, browser_toolset_20260801, \
                                 text_editor_20250728, memory_20250818?";

/// 请求体：带一组 `tools[].type`。
fn tools_req(model: &str, types: &[&str]) -> Option<serde_json::Value> {
    let tools = types
        .iter()
        .map(|t| format!(r#"{{"type":"{t}","name":"{t}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    json_body(&format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"tools":[{tools}]}}"#
    ))
}

/// 请求体：`messages` 里混了个 `role: system`（litellm 那类客户端会这么发）。
/// 那条 role 跟在 user 之后：上游只对**对话中途**的 `role:"system"` 回 `ROLE_400`，
/// 开头那段回的是另一句（`use the top-level 'system' parameter`），见 `first_turn_index`。
fn role_req(model: &str, role: &str) -> Option<serde_json::Value> {
    json_body(&format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}},{{"role":"{role}","content":"you are…"}}]}}"#
    ))
}

/// 学到的 `role 'system'` 只拦对话中途带 system、且出站时还留着它的请求：只在开头带
/// system 的（上游回的是另一句），以及出站前会被整条提升的，都不能跟着一起在本地拒掉。
#[test]
fn learned_system_role_does_not_block_leading_system_prompts() {
    let mem = crate::proxy::ShapeMemory::default();
    let mid = role_req("claude-haiku-4-5", "system");
    let learned = crate::proxy::remember_shape_rejection(
        &mem,
        Some("claude-haiku-4-5"),
        mid.as_ref(),
        &err_json(ROLE_400),
    );
    assert_eq!(learned.len(), 1, "中途那条该学到");
    let leading = json_body(
        r#"{"model":"claude-haiku-4-5","messages":[{"role":"system","content":"you are…"},{"role":"user","content":"hi"}]}"#,
    );
    assert!(
        crate::proxy::known_shape_rejection(
            &mem,
            Some("claude-haiku-4-5"),
            leading.as_ref(),
            false
        )
        .is_none()
    );
    assert!(
        crate::proxy::known_shape_rejection(&mem, Some("claude-haiku-4-5"), mid.as_ref(), false)
            .is_some()
    );
    // 出站前会被整条提升（hoist 开着、非 CC 形态）：上游看不到这个 role，不拦。
    assert!(
        crate::proxy::known_shape_rejection(&mem, Some("claude-haiku-4-5"), mid.as_ref(), true)
            .is_none()
    );
    // 指令式 system 提升不动它，照样送到上游：豁免不成立，照拦。
    let directive = json_body(
        r#"{"model":"claude-haiku-4-5","messages":[{"role":"user","content":"hi"},{"role":"system","output_config":{"effort":"low"},"content":[]}]}"#,
    );
    assert!(
        crate::proxy::known_shape_rejection(
            &mem,
            Some("claude-haiku-4-5"),
            directive.as_ref(),
            true
        )
        .is_some()
    );
    // 只管一轮的 system：开头那条会被提升走，出站没有 system 了，照常豁免；中途那条留在原位，
    // 不豁免。判据与提升本身共用。
    let leading_scoped = json_body(
        r#"{"model":"claude-haiku-4-5","messages":[{"role":"system","clear_at":"next_user_message","content":"tmp"},{"role":"user","content":"hi"},{"role":"system","content":"you are…"}]}"#,
    );
    assert!(
        crate::proxy::known_shape_rejection(
            &mem,
            Some("claude-haiku-4-5"),
            leading_scoped.as_ref(),
            true
        )
        .is_none()
    );
    let mid_scoped = json_body(
        r#"{"model":"claude-haiku-4-5","messages":[{"role":"user","content":"hi"},{"role":"system","clear_at":"next_user_message","content":"tmp"}]}"#,
    );
    assert!(
        crate::proxy::known_shape_rejection(
            &mem,
            Some("claude-haiku-4-5"),
            mid_scoped.as_ref(),
            true
        )
        .is_some()
    );
    // 严格模式（不提升）下中途只有一条空壳：出站前一律丢掉，上游看不到 system，不拦。
    let only_shell = json_body(
        r#"{"model":"claude-haiku-4-5","messages":[{"role":"user","content":"hi"},{"role":"system","content":[]}]}"#,
    );
    assert!(
        crate::proxy::known_shape_rejection(
            &mem,
            Some("claude-haiku-4-5"),
            only_shell.as_ref(),
            false
        )
        .is_none()
    );
    // 空壳在提升之前就被丢掉，带着 `clear_at` 也不算留下来的 system：照常豁免。
    let shell_scoped = json_body(
        r#"{"model":"claude-haiku-4-5","messages":[{"role":"user","content":"hi"},{"role":"system","clear_at":"next_user_message","content":[]},{"role":"system","content":"you are…"}]}"#,
    );
    assert!(
        crate::proxy::known_shape_rejection(
            &mem,
            Some("claude-haiku-4-5"),
            shell_scoped.as_ref(),
            true
        )
        .is_none()
    );
    // 带正文又带 `output_config` 的会拆出一条指令留在原位，同样不豁免。
    let split = json_body(
        r#"{"model":"claude-haiku-4-5","messages":[{"role":"user","content":"hi"},{"role":"system","output_config":{"effort":"low"},"content":"be brief"}]}"#,
    );
    assert!(
        crate::proxy::known_shape_rejection(&mem, Some("claude-haiku-4-5"), split.as_ref(), true)
            .is_some()
    );
}

/// 2026-10-02 线上原文（逐字）：说的是 system **摆在哪儿**，不是这个模型不收 system。
const SYSTEM_POSITION_400: &str = "messages.1: role 'system' must precede an 'assistant' \
        message or end the array; the directive-only form (content: [] with output_config) is \
        accepted at any position";

/// 位置约束的 400 不学：学了就是同模型带中途 system 的请求全被本地拒掉，位置对的也一样。
/// 库里旧版本学进去的同款，回填时不进表、交给调用方删掉。
#[test]
fn positional_system_400_is_not_learned_and_stale_rows_are_dropped() {
    let mem = crate::proxy::ShapeMemory::default();
    let body = role_req("claude-opus-5-5", "system");
    let learned = crate::proxy::remember_shape_rejection(
        &mem,
        Some("claude-opus-5-5"),
        body.as_ref(),
        &err_json(SYSTEM_POSITION_400),
    );
    assert!(learned.is_empty(), "位置约束不该学成形态规则");
    assert!(
        crate::proxy::known_shape_rejection(&mem, Some("claude-opus-5-5"), body.as_ref(), false)
            .is_none()
    );

    // 无条件的那句照学，两者互不干扰。
    let learned = crate::proxy::remember_shape_rejection(
        &mem,
        Some("claude-haiku-4-5"),
        role_req("claude-haiku-4-5", "system").as_ref(),
        &err_json(ROLE_400),
    );
    assert_eq!(learned.len(), 1);

    let row = |model: &str, message: &str| store::LearnedRejection {
        kind: "shape".into(),
        model: model.into(),
        field: "role".into(),
        value: "system".into(),
        message: message.into(),
        reply: None,
    };
    let (dep, empty) = Default::default();
    let seeded = crate::proxy::resync_learned_memories(
        &mem,
        &dep,
        &empty,
        vec![row("claude-opus-5-5", SYSTEM_POSITION_400), row("claude-haiku-4-5", ROLE_400)],
    );
    assert_eq!(seeded.shape, 1, "只回填无条件的那条");
    assert_eq!(seeded.stale.len(), 1);
    assert_eq!(seeded.stale[0].model, "claude-opus-5-5");
    assert!(
        crate::proxy::known_shape_rejection(&mem, Some("claude-opus-5-5"), body.as_ref(), false)
            .is_none()
    );
}

/// 学一次之后，同款「模型 + 取值」在本地就被拦下，回给客户端的是上游那句原话。
/// 两类样本走的是同一套机制，故一并验。
#[test]
fn rejects_a_learned_request_shape_locally() {
    let mem = crate::proxy::ShapeMemory::default();
    let hit = |body: &Option<serde_json::Value>, model: &str| {
        crate::proxy::known_shape_rejection(&mem, Some(model), body.as_ref(), false)
    };

    // 学之前一律放行：这张表只挡确定无疑的重复失败，不替上游做没有依据的判断。
    assert!(hit(&effort_req("claude-sonnet-5", "xhigh"), "claude-sonnet-5").is_none());
    assert!(hit(&role_req("claude-opus-4-6", "system"), "claude-opus-4-6").is_none());

    let learn = |model: &str, body: &Option<serde_json::Value>, msg: &str| {
        crate::proxy::remember_shape_rejection(&mem, Some(model), body.as_ref(), &err_json(msg));
    };
    learn("claude-sonnet-5", &effort_req("claude-sonnet-5", "xhigh"), EFFORT_400);
    learn("claude-opus-4-6", &role_req("claude-opus-4-6", "system"), ROLE_400);

    let (field, value, message) =
        hit(&effort_req("claude-sonnet-5", "xhigh"), "claude-sonnet-5").expect("该被拦下");
    assert_eq!((field, value.as_str()), ("effort", "xhigh"));
    assert_eq!(message, EFFORT_400, "回放上游那句原话，不自己造文案");

    let (field, value, message) =
        hit(&role_req("claude-opus-4-6", "system"), "claude-opus-4-6").expect("该被拦下");
    assert_eq!((field, value.as_str()), ("role", "system"));
    assert_eq!(message, ROLE_400);

    // 结论只对「学过的那个模型 + 那个取值」成立，不外溢。
    assert!(hit(&effort_req("claude-sonnet-5", "high"), "claude-sonnet-5").is_none());
    assert!(hit(&effort_req("claude-opus-5", "xhigh"), "claude-opus-5").is_none());
    assert!(hit(&role_req("claude-opus-4-6", "developer"), "claude-opus-4-6").is_none());
    assert!(hit(&role_req("claude-sonnet-5", "system"), "claude-sonnet-5").is_none());
    // 普通请求（只有 user/assistant、没写 effort）永远不进这张表的判定。
    assert!(hit(&role_req("claude-opus-4-6", "user"), "claude-opus-4-6").is_none());
}

/// 工具类型那条 400：**只学被点名的那一个**，后半截「你是不是想用」列出的合法类型一个
/// 都不学。按裸子串判就会把 `bash_20250124`、`text_editor_20250728` 一并学成「这个模型
/// 不收」——它们正是该模型认的类型，下一条普通 CC 请求就被本地拒死。
#[test]
fn learns_the_named_tool_type_but_never_the_suggested_ones() {
    let mem = crate::proxy::ShapeMemory::default();
    // 一条带 computer 工具的请求：另外两个类型在建议清单里也列着。
    let body = tools_req(
        "claude-fable-5",
        &["computer_20250124", "bash_20250124", "text_editor_20250728", "custom"],
    );
    // 学之前照常放行：第一次还是要发上去，规则是上游那条 400 自己喂出来的。
    assert!(
        crate::proxy::known_shape_rejection(&mem, Some("claude-fable-5"), body.as_ref(), false)
            .is_none()
    );
    crate::proxy::remember_shape_rejection(
        &mem,
        Some("claude-fable-5"),
        body.as_ref(),
        &err_json(TOOL_TYPE_400),
    );
    assert_eq!(mem.read().len(), 1, "只该学被点名的 computer_20250124 那一条");

    let (field, value, message) =
        crate::proxy::known_shape_rejection(&mem, Some("claude-fable-5"), body.as_ref(), false)
            .expect("第二次该在本地拦下");
    assert_eq!((field, value.as_str()), ("tool_type", "computer_20250124"));
    assert_eq!(message, TOOL_TYPE_400, "回放上游那句原话，不自己造文案");

    // 不带那个类型的请求照常放行：建议清单里的两个没被学进去。
    let others = tools_req("claude-fable-5", &["bash_20250124", "text_editor_20250728", "custom"]);
    assert!(
        crate::proxy::known_shape_rejection(&mem, Some("claude-fable-5"), others.as_ref(), false)
            .is_none()
    );
    // 结论也不外溢到别的模型——computer 工具在 opus 上照发。
    assert!(
        crate::proxy::known_shape_rejection(&mem, Some("claude-opus-5"), body.as_ref(), false)
            .is_none()
    );
    // 没有 tools 的请求永远不进这张表的判定。
    assert!(
        crate::proxy::known_shape_rejection(
            &mem,
            Some("claude-fable-5"),
            effort_req("claude-fable-5", "high").as_ref(),
            false
        )
        .is_none()
    );
}

/// 点名的是另一个版本号（这次发的是 `computer_20250124`）→ 不学。判据是逐项精确比，
/// 不是前缀或子串：`computer_20241022` 与 `computer_20250124` 是两个取值。
#[test]
fn learns_nothing_when_another_tool_type_is_named() {
    const OTHER_400: &str = "'claude-fable-5' does not support tool types: computer_20241022. \
                                 Did you mean one of bash_20250124, computer_20250124?";
    let mem = crate::proxy::ShapeMemory::default();
    let body = tools_req("claude-fable-5", &["computer_20250124", "custom"]);
    crate::proxy::remember_shape_rejection(
        &mem,
        Some("claude-fable-5"),
        body.as_ref(),
        &err_json(OTHER_400),
    );
    assert!(mem.read().is_empty(), "建议清单里出现过也不算被点名");
}

/// 不该学的几种 400：报错没提这个字段、提了字段但没逐字引用这次的取值、
/// 以及认不出模型名。判据是「字段名 + `'取值'` 共现」，缺一不记——记错的代价是
/// 本地把好请求拒了，比多发一次上游请求严重得多。
#[test]
fn learns_nothing_when_the_error_does_not_name_the_value() {
    let cases: &[(&str, &str)] = &[
        // 与形态无关的 400。
        ("claude-sonnet-5", "max_tokens: 200000 > 64000, which is the maximum allowed"),
        // 提了字段名，但引的是别的取值（这次发的是 xhigh）。
        ("claude-sonnet-5", "This model does not support effort level 'ultra'."),
        // 引到了取值，但通篇没提这个字段名。
        ("claude-sonnet-5", "unexpected value 'xhigh' somewhere else entirely"),
    ];
    for (model, msg) in cases {
        let mem = crate::proxy::ShapeMemory::default();
        let body = effort_req(model, "xhigh");
        crate::proxy::remember_shape_rejection(&mem, Some(model), body.as_ref(), &err_json(msg));
        assert!(mem.read().is_empty(), "不该学: {msg}");
        assert!(
            crate::proxy::known_shape_rejection(&mem, Some(model), body.as_ref(), false).is_none()
        );
    }

    // 认不出模型名 → 学不到东西（这条 400 照常透传，只是记不下来）。
    let mem = crate::proxy::ShapeMemory::default();
    let body = effort_req("claude-sonnet-5", "xhigh");
    crate::proxy::remember_shape_rejection(&mem, None, body.as_ref(), &err_json(EFFORT_400));
    assert!(mem.read().is_empty());
}

/// **条件句一条都不学**（实测原文，opus-5 的 thinking/effort 联动规则）：
/// `max` 并非一律不行，只是 thinking 关掉时不行。学成「一律拒」的话，下次客户端开着
/// thinking 正常发 `max` 就会被本地误拒——而上游本来会接受。
#[test]
fn never_learns_a_conditional_rejection() {
    const COND_400: &str = "output_config.effort 'max' is not supported when thinking is \
                                disabled on this model. Use effort 'high' or below, or enable thinking.";
    let mem = crate::proxy::ShapeMemory::default();
    let body = effort_req("claude-opus-5", "max");
    crate::proxy::remember_shape_rejection(
        &mem,
        Some("claude-opus-5"),
        body.as_ref(),
        &err_json(COND_400),
    );
    assert!(mem.read().is_empty(), "条件句不该进表: {COND_400}");
    // 于是开着 thinking 的那条请求照常放行，不会被本地误拒。
    assert!(
        crate::proxy::known_shape_rejection(&mem, Some("claude-opus-5"), body.as_ref(), false)
            .is_none()
    );

    // 无条件那两条不受影响——判据只挡「when/unless/without」这类前提词。
    let mem = crate::proxy::ShapeMemory::default();
    crate::proxy::remember_shape_rejection(
        &mem,
        Some("claude-sonnet-5"),
        effort_req("claude-sonnet-5", "xhigh").as_ref(),
        &err_json(EFFORT_400),
    );
    crate::proxy::remember_shape_rejection(
        &mem,
        Some("claude-opus-4-6"),
        role_req("claude-opus-4-6", "system").as_ref(),
        &err_json(ROLE_400),
    );
    assert_eq!(mem.read().len(), 2, "无条件的两条仍该学得到");
}

/// 记忆表封顶后不再插入：取值来自来访请求，增长是外部可控的。
#[test]
fn shape_memory_is_capped() {
    let mem = crate::proxy::ShapeMemory::default();
    for i in 0..crate::proxy::SHAPE_MEMORY_CAP + 10 {
        let role = format!("r{i}");
        let body = role_req("claude-opus-4-6", &role);
        let msg = format!("role '{role}' is not supported on this model");
        crate::proxy::remember_shape_rejection(
            &mem,
            Some("claude-opus-4-6"),
            body.as_ref(),
            &err_json(&msg),
        );
    }
    assert_eq!(mem.read().len(), crate::proxy::SHAPE_MEMORY_CAP);
}

// ── deprecated field 学习与剥离 ──────────────────────────────────

const TEMP_400: &str = "`temperature` is deprecated for this model.";

fn temp_req(model: &str) -> Option<serde_json::Value> {
    json_body(&format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"temperature":0.7}}"#
    ))
}

fn top_p_req(model: &str) -> Option<serde_json::Value> {
    json_body(&format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"top_p":0.9}}"#
    ))
}

/// 零输出请求类的归类与命中：只有「无 tools（按值算）+ 恰好一条用户消息 + 带 max_tokens」
/// 才归类；带 tools、多轮、换 max_tokens、换模型的都不命中——宁可多放一条，不误伤真业务。
#[test]
fn empty_reply_class_is_narrow_and_known_empty_reply_matches_only_the_same_class() {
    let mem = crate::proxy::EmptyReplyMemory::default();
    let body = |extra: &str| -> serde_json::Value {
        serde_json::from_str(&format!(
                r#"{{"model":"claude-fable-5","system":"s","messages":[{{"role":"user","content":"ping"}}],"max_tokens":16{extra}}}"#
            ))
            .unwrap()
    };
    let ping = body("");
    assert_eq!(
        crate::proxy::empty_reply_class(Some("claude-fable-5"), Some(&ping)),
        Some(("claude-fable-5".to_string(), 16))
    );
    // 归类不看 UA、不看 system、不看 stream：模拟路径改的是身份，改不了「问一句不回」。
    assert_eq!(
        crate::proxy::empty_reply_class(Some("claude-fable-5"), Some(&body(r#","stream":true"#))),
        Some(("claude-fable-5".to_string(), 16))
    );
    // `tools: []` / `null` 按没有算；真带了工具就不归类。
    assert!(crate::proxy::empty_reply_class(Some("m"), Some(&body(r#","tools":[]"#))).is_some());
    assert!(crate::proxy::empty_reply_class(Some("m"), Some(&body(r#","tools":null"#))).is_some());
    assert!(
        crate::proxy::empty_reply_class(
            Some("m"),
            Some(&body(r#","tools":[{"name":"Read","input_schema":{"type":"object"}}]"#))
        )
        .is_none()
    );
    // 多轮、没写 max_tokens、没有模型：不归类。
    let multi: serde_json::Value = serde_json::from_str(
            r#"{"model":"m","messages":[{"role":"user","content":"a"},{"role":"assistant","content":"b"},{"role":"user","content":"c"}],"max_tokens":16}"#,
        )
        .unwrap();
    assert!(crate::proxy::empty_reply_class(Some("m"), Some(&multi)).is_none());
    let no_cap: serde_json::Value =
        serde_json::from_str(r#"{"model":"m","messages":[{"role":"user","content":"a"}]}"#)
            .unwrap();
    assert!(crate::proxy::empty_reply_class(Some("m"), Some(&no_cap)).is_none());
    assert!(crate::proxy::empty_reply_class(None, Some(&ping)).is_none());

    // 没学过：一律放行。
    assert!(crate::proxy::known_empty_reply(&mem, Some("claude-fable-5"), Some(&ping)).is_none());
    crate::proxy::remember_empty_reply(&mem, "claude-fable-5", 16, "{}").unwrap();
    assert_eq!(
        crate::proxy::known_empty_reply(&mem, Some("claude-fable-5"), Some(&ping)),
        Some((16, "{}".to_string()))
    );
    // 换 max_tokens / 换模型 / 带 tools：都是另一类，不命中。
    let other_cap: serde_json::Value = serde_json::from_str(
            r#"{"model":"claude-fable-5","messages":[{"role":"user","content":"ping"}],"max_tokens":8192}"#,
        )
        .unwrap();
    assert!(
        crate::proxy::known_empty_reply(&mem, Some("claude-fable-5"), Some(&other_cap)).is_none()
    );
    assert!(crate::proxy::known_empty_reply(&mem, Some("claude-fable-5-1"), Some(&ping)).is_none());
    assert!(
        crate::proxy::known_empty_reply(
            &mem,
            Some("claude-fable-5"),
            Some(&body(r#","tools":[{"name":"Read","input_schema":{"type":"object"}}]"#))
        )
        .is_none()
    );
}

/// 学到的规则能落库再读回：两个 remember_* 返回新学到的条目（重复不算），
/// `seed_learned_memories` 把它们放回两张表，对不上探针/名单的脏行跳过。
#[test]
fn learned_rejections_round_trip_through_seed() {
    let shape = crate::proxy::ShapeMemory::default();
    let dep = crate::proxy::DeprecatedFieldMemory::default();
    let body = serde_json::json!({
        "model": "claude-opus-5", "temperature": 0.7,
        "output_config": {"effort": "xhigh"}, "messages": []
    });
    let learned_shape = crate::proxy::remember_shape_rejection(
        &shape,
        Some("claude-opus-5"),
        Some(&body),
        &err_json(EFFORT_400),
    );
    assert_eq!(learned_shape.len(), 1, "{learned_shape:?}");
    assert_eq!(
        (
            learned_shape[0].kind.as_str(),
            learned_shape[0].field.as_str(),
            learned_shape[0].value.as_str()
        ),
        ("shape", "effort", "xhigh")
    );
    // 再学一次同一条：表里已有，不再返回。
    assert!(
        crate::proxy::remember_shape_rejection(
            &shape,
            Some("claude-opus-5"),
            Some(&body),
            &err_json(EFFORT_400)
        )
        .is_empty()
    );
    let learned_dep = crate::proxy::remember_deprecated_field(
        &dep,
        Some("claude-opus-5"),
        Some(&body),
        &err_json(TEMP_400),
    );
    assert_eq!(learned_dep.len(), 1, "{learned_dep:?}");
    assert_eq!(
        (
            learned_dep[0].kind.as_str(),
            learned_dep[0].field.as_str(),
            learned_dep[0].value.as_str()
        ),
        ("deprecated", "temperature", "")
    );

    // 模拟重启：空表 + 从「库里」读回的行（多两条对不上的脏行）。
    let mut rows: Vec<store::LearnedRejection> =
        learned_shape.into_iter().chain(learned_dep).collect();
    rows.push(store::LearnedRejection {
        kind: "shape".into(),
        model: "m".into(),
        field: "no_such_probe".into(),
        value: "v".into(),
        message: String::new(),
        reply: None,
    });
    rows.push(store::LearnedRejection {
        kind: "deprecated".into(),
        model: "m".into(),
        field: "model".into(),
        value: String::new(),
        message: String::new(),
        reply: None,
    });
    // 零输出那类：一条正常的，一条 value 不是整数的脏行。
    let ping = serde_json::json!({
        "model": "claude-fable-5", "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let empty = crate::proxy::EmptyReplyMemory::default();
    let learned_empty =
        crate::proxy::remember_empty_reply(&empty, "claude-fable-5", 16, r#"{"content":[]}"#)
            .expect("首次学到");
    assert_eq!(
        (learned_empty.kind.as_str(), learned_empty.field.as_str(), learned_empty.value.as_str()),
        ("empty_reply", "max_tokens", "16")
    );
    assert!(
        crate::proxy::remember_empty_reply(&empty, "claude-fable-5", 16, "again").is_none(),
        "同一类第二次不算新学到"
    );
    rows.push(learned_empty);
    rows.push(store::LearnedRejection {
        kind: "empty_reply".into(),
        model: "m".into(),
        field: "max_tokens".into(),
        value: "sixteen".into(),
        message: String::new(),
        reply: None,
    });
    // 拒答那类：一条正常的；再加一条 v0.3.89 学错的（拒答被记成了请求类）——不回填、报成过期。
    let refused = crate::proxy::remember_refused_prompt(
        &empty,
        "claude-opus-5",
        "deadbeef",
        r#"{"stop_reason":"refusal"}"#,
        json_reply(),
    )
    .expect("首次学到");
    assert_eq!(
        (refused.kind.as_str(), refused.field.as_str(), refused.value.as_str()),
        ("refusal", "prompt_sha", "deadbeef")
    );
    rows.push(refused);
    let stale = store::LearnedRejection {
        kind: "empty_reply".into(),
        model: "claude-opus-5".into(),
        field: "max_tokens".into(),
        value: "65536".into(),
        message: r#"{"content":[],"stop_reason":"refusal","stop_details":{"category":"cyber"}}"#
            .into(),
        reply: None,
    };
    rows.push(stale.clone());
    // 0.3.98 之前学的拒答规则：没存上游响应体，回放不出来——同样不回填、报成过期。
    let legacy_refusal = store::LearnedRejection {
        kind: "refusal".into(),
        model: "claude-opus-5".into(),
        field: "prompt_sha".into(),
        value: "0ld".into(),
        message: "[cyber] stop_details={}".into(),
        reply: None,
    };
    rows.push(legacy_refusal.clone());
    // 按应用学的：一条正常的（带体），一条没体的旧行（报成过期）。
    let app_row = crate::proxy::remember_app_refusal(
        &empty,
        "claude-opus-5",
        "5y5",
        "[cyber] x",
        json_reply(),
    )
    .expect("首次学到");
    assert_eq!(
        (app_row.kind.as_str(), app_row.field.as_str(), app_row.value.as_str()),
        ("app_refusal", "system_sha", "5y5")
    );
    assert!(
        crate::proxy::remember_app_refusal(&empty, "claude-opus-5", "5y5", "again", json_reply())
            .is_none(),
        "同一应用第二次不算新学到"
    );
    rows.push(app_row);
    let legacy_app = store::LearnedRejection {
        kind: "app_refusal".into(),
        model: "claude-opus-5".into(),
        field: "system_sha".into(),
        value: "0ldapp".into(),
        message: "[cyber] stop_details={}".into(),
        reply: None,
    };
    rows.push(legacy_app.clone());
    let shape2 = crate::proxy::ShapeMemory::default();
    let dep2 = crate::proxy::DeprecatedFieldMemory::default();
    let empty2 = crate::proxy::EmptyReplyMemory::default();
    assert_eq!(
        crate::proxy::seed_learned_memories(&shape2, &dep2, &empty2, rows),
        crate::proxy::SeededMemories {
            shape: 1,
            deprecated: 1,
            empty_reply: 1,
            refusal: 1,
            app_refusal: 1,
            stale: vec![stale, legacy_refusal, legacy_app]
        }
    );
    let opus_ping = serde_json::json!({
        "model": "claude-opus-5", "max_tokens": 65536,
        "messages": [{"role": "user", "content": "anything"}]
    });
    assert!(
        crate::proxy::known_empty_reply(&empty2, Some("claude-opus-5"), Some(&opus_ping)).is_none(),
        "学错的那条不回填：同形态的正常请求不受连坐"
    );
    let refused_body = serde_json::json!({
        "model": "claude-opus-5", "messages": [{"role": "user", "content": "x"}]
    });
    let digest = crate::proxy::prompt_digest(&refused_body).unwrap();
    crate::proxy::remember_refused_prompt(&empty2, "claude-opus-5", &digest, "{}", json_reply())
        .unwrap();
    let hit =
        crate::proxy::known_refused_prompt(&empty2, Some("claude-opus-5"), Some(&refused_body))
            .expect("逐字相同的提示词命中");
    assert_eq!(hit.verdict, "{}");
    assert_eq!(hit.reply, json_reply(), "命中时拿到的是学规则时上游那次的原样体");
    // 提示词改一个字、或换个模型：不命中。
    let other_body = serde_json::json!({
        "model": "claude-opus-5", "messages": [{"role": "user", "content": "y"}]
    });
    assert!(
        crate::proxy::known_refused_prompt(&empty2, Some("claude-opus-5"), Some(&other_body))
            .is_none()
    );
    assert!(
        crate::proxy::known_refused_prompt(&empty2, Some("claude-sonnet-5"), Some(&refused_body))
            .is_none()
    );
    // system 也进哈希：同一条 messages 换 system 是另一条提示词。
    let with_sys = serde_json::json!({
        "system": "s", "model": "claude-opus-5", "messages": [{"role": "user", "content": "x"}]
    });
    assert_ne!(crate::proxy::prompt_digest(&with_sys), Some(digest.clone()));
    let refusal_row = store::LearnedRejection {
        kind: "refusal".into(),
        model: "claude-opus-5".into(),
        field: "prompt_sha".into(),
        value: digest,
        message: String::new(),
        reply: None,
    };
    assert!(crate::proxy::forget_learned_memory(&shape2, &dep2, &empty2, &refusal_row));
    assert!(
        crate::proxy::known_refused_prompt(&empty2, Some("claude-opus-5"), Some(&refused_body))
            .is_none()
    );
    // 按应用学的那条回填了：同模型 + 同 system 命中，换 system / 换模型不命中，删得掉。
    let app_body = |sys: &str| serde_json::json!({"model": "claude-opus-5", "system": sys, "messages": [{"role": "user", "content": "anything"}]});
    let sys_a = "you are app A";
    let sha_a = crate::proxy::app_system_digest(&app_body(sys_a)).unwrap();
    crate::proxy::remember_app_refusal(&empty2, "claude-opus-5", &sha_a, "[cyber]", json_reply())
        .unwrap();
    let hit =
        crate::proxy::known_app_refusal(&empty2, Some("claude-opus-5"), Some(&app_body(sys_a)))
            .expect("同模型 + 同 system 命中");
    assert_eq!(hit.reply, json_reply());
    assert!(
        crate::proxy::known_app_refusal(
            &empty2,
            Some("claude-opus-5"),
            Some(&app_body("you are app B"))
        )
        .is_none()
    );
    assert!(
        crate::proxy::known_app_refusal(&empty2, Some("claude-sonnet-5"), Some(&app_body(sys_a)))
            .is_none()
    );
    let app_rule = store::LearnedRejection {
        kind: "app_refusal".into(),
        model: "claude-opus-5".into(),
        field: "system_sha".into(),
        value: sha_a,
        message: String::new(),
        reply: None,
    };
    assert!(crate::proxy::forget_learned_memory(&shape2, &dep2, &empty2, &app_rule));
    assert!(
        crate::proxy::known_app_refusal(&empty2, Some("claude-opus-5"), Some(&app_body(sys_a)))
            .is_none()
    );
    // 没有 system 的请求没有应用身份：不学也不判。
    assert_eq!(crate::proxy::app_system_digest(&serde_json::json!({"messages": []})), None);
    assert_eq!(
        crate::proxy::app_system_digest(&serde_json::json!({"system": "", "messages": []})),
        None
    );
    assert_eq!(
        crate::proxy::app_system_digest(&serde_json::json!({"system": [], "messages": []})),
        None
    );
    // system 的字串形态与单块数组形态是两份不同的 system。
    assert_ne!(
        crate::proxy::app_system_digest(&app_body(sys_a)),
        crate::proxy::app_system_digest(
            &serde_json::json!({"system": [{"type": "text", "text": sys_a}]})
        )
    );
    let (max_tokens, excerpt) =
        crate::proxy::known_empty_reply(&empty2, Some("claude-fable-5"), Some(&ping))
            .expect("零输出规则应已回填");
    assert_eq!((max_tokens, excerpt.as_str()), (16, r#"{"content":[]}"#));
    // 控制台单条删除：删得掉、删过就不再命中；对不上的行返回 false。
    let row = store::LearnedRejection {
        kind: "empty_reply".into(),
        model: "claude-fable-5".into(),
        field: "max_tokens".into(),
        value: "16".into(),
        message: String::new(),
        reply: None,
    };
    assert!(crate::proxy::forget_learned_memory(&shape2, &dep2, &empty2, &row));
    assert!(!crate::proxy::forget_learned_memory(&shape2, &dep2, &empty2, &row));
    assert!(
        crate::proxy::known_empty_reply(&empty2, Some("claude-fable-5"), Some(&ping)).is_none()
    );
    let hit =
        crate::proxy::known_shape_rejection(&shape2, Some("claude-opus-5"), Some(&body), false)
            .expect("形态规则应已回填");
    assert_eq!((hit.0, hit.1.as_str()), ("effort", "xhigh"));
    assert!(crate::proxy::has_learned_deprecated_field(&dep2, Some("claude-opus-5"), Some(&body)));
    assert!(
        !crate::proxy::has_learned_deprecated_field(&dep2, Some("claude-sonnet-5"), Some(&body)),
        "别的模型不受影响"
    );
    let no_temp = serde_json::json!({ "model": "claude-opus-5", "messages": [] });
    assert!(
        !crate::proxy::has_learned_deprecated_field(&dep2, Some("claude-opus-5"), Some(&no_temp)),
        "请求里没带那个字段就不算"
    );
}

/// 学到拒答时上游那次的原样体（SSE 或整段 JSON），按来访这次要的形态回放：形态一致逐字节
/// 原样，不一致才在两种形态间转换，而且转换是可逆的——JSON 展成的 SSE 再聚合回来是同一条
/// Message；残缺的 SSE 拼不出整段 JSON 时不回放（`None`，调用方照常转发）。
#[tokio::test]
async fn replays_the_recorded_refusal_in_the_shape_the_request_asks_for() {
    async fn parts(resp: crate::proxy::Response) -> (StatusCode, axum::http::HeaderMap, String) {
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024).await.unwrap();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }
    const SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"stop_details\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_sequence\":null,\"stop_details\":{\"type\":\"refusal\",\"category\":\"cyber\",\"explanation\":\"blocked\"}},\"usage\":{\"output_tokens\":0}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let sse = store::LearnedReply { sse: true, body: SSE.into() };
    let json = json_reply();

    // 形态一致：状态 200、content-type 跟形态走、体逐字节原样。
    let (status, headers, body) =
        parts(crate::proxy::replay_refusal(&sse, true).expect("SSE → 流式")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type").unwrap(), crate::proxy::SSE_CONTENT_TYPE);
    assert_eq!(body, SSE);
    let (status, headers, body) =
        parts(crate::proxy::replay_refusal(&json, false).expect("JSON → 非流式")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type").unwrap(), "application/json");
    assert_eq!(body, json.body);

    // SSE 学的、这次要非流式：聚合成整段 Message，判决字段都在。
    let (status, headers, body) =
        parts(crate::proxy::replay_refusal(&sse, false).expect("SSE → 非流式")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type").unwrap(), "application/json");
    let msg: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(msg["id"], "msg_s");
    assert_eq!(msg["stop_reason"], "refusal");
    assert_eq!(msg["stop_details"]["category"], "cyber");
    assert_eq!(msg["content"], serde_json::json!([]));
    assert_eq!(msg["usage"]["input_tokens"], 12);
    assert_eq!(msg["usage"]["output_tokens"], 0);

    // JSON 学的、这次要流式：展成 SSE，再用聚合器收回来必须是同一条 Message。
    let (status, headers, body) =
        parts(crate::proxy::replay_refusal(&json, true).expect("JSON → 流式")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type").unwrap(), crate::proxy::SSE_CONTENT_TYPE);
    assert!(body.starts_with("event: message_start\ndata: {\"type\":\"message_start\""), "{body}");
    assert!(body.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"), "{body}");
    let mut agg = crate::proxy::SseAggregator::default();
    agg.feed(body.as_bytes());
    let crate::proxy::Aggregated::Message(back) = agg.finish() else {
        panic!("展开的 SSE 应能聚合")
    };
    let want: serde_json::Value = serde_json::from_str(&json.body).unwrap();
    assert_eq!(back, want, "JSON → SSE → JSON 往返无损");

    // 带正文的 Message 也能展开再收回（回放路径学不到这种，但转换本身得是对的）。
    let rich = serde_json::json!({
        "id": "msg_c", "type": "message", "role": "assistant", "model": "claude-opus-5",
        "content": [
            {"type": "thinking", "thinking": "hmm", "signature": "sig"},
            {"type": "text", "text": "hi"},
            {"type": "tool_use", "id": "tu_1", "name": "Bash", "input": {"command": "ls"}},
            {"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}}
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 2}
    });
    let mut agg = crate::proxy::SseAggregator::default();
    agg.feed(crate::proxy::message_to_sse(&rich).unwrap().as_bytes());
    let crate::proxy::Aggregated::Message(back) = agg.finish() else { panic!("应能聚合") };
    assert_eq!(back, rich);

    // 残缺的 SSE（没有 message_stop）拼不出整段 JSON：不回放。
    let broken = store::LearnedReply {
        sse: true,
        body: SSE
            .trim_end_matches("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
            .into(),
    };
    assert!(crate::proxy::replay_refusal(&broken, false).is_none());
    // 形态一致时不看内容，原样发（学的那头保证过流是完整收尾的）。
    assert!(crate::proxy::replay_refusal(&broken, true).is_some());
    // 不是对象 / 没有 content 的 JSON 展不成 SSE。
    let bogus = store::LearnedReply { sse: false, body: "[]".into() };
    assert!(crate::proxy::replay_refusal(&bogus, true).is_none());
    assert!(crate::proxy::message_to_sse(&serde_json::json!({"id": "x"})).is_none());
}

/// [`UsageSniffer::refusal_reply`]：原样全文只在「没超上限、没见过输出内容块」时留着；
/// 超上限清空不学，见过 `content_block_start`（`fallback` 标记不算）也不学。
#[test]
fn sniffer_keeps_the_full_reply_only_while_it_is_replayable() {
    // 正常的小体：原样。
    let mut st = crate::proxy::UsageSniffer::new(false, false);
    st.feed(br#"{"id":"msg_1","content":[],"#);
    st.feed(br#""stop_reason":"refusal","usage":{"output_tokens":0}}"#);
    st.finish();
    assert_eq!(
        st.refusal_reply(),
        Some(store::LearnedReply {
            sse: false,
            body:
                r#"{"id":"msg_1","content":[],"stop_reason":"refusal","usage":{"output_tokens":0}}"#
                    .into()
        })
    );
    // 超上限：清空并标记，之后再喂也不攒。
    let mut st = crate::proxy::UsageSniffer::new(true, false);
    st.feed(&vec![b'x'; crate::proxy::REFUSAL_REPLY_BYTES]);
    st.feed(b"y");
    assert!(st.reply_overflow);
    assert!(st.reply.is_empty());
    st.feed(b"z");
    assert!(st.reply.is_empty());
    assert!(st.refusal_reply().is_none());
    // 见过输出内容块：不学，且此后不再拷字节（省内存）。
    let mut st = crate::proxy::UsageSniffer::new(true, false);
    st.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"output_tokens\":1}}}\n\n");
    let before = st.reply.len();
    st.feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
    let after = st.reply.len();
    assert!(after > before, "含首个内容块的那一块还在缓冲里");
    st.feed(b"event: content_block_delta\ndata: {}\n\n");
    assert_eq!(st.reply.len(), after, "见过输出块后不再攒");
    assert!(st.refusal_reply().is_none());
    // fallback 切换标记不算输出块：照常攒。
    let mut st = crate::proxy::UsageSniffer::new(true, false);
    st.feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"fallback\",\"from\":{\"model\":\"a\"},\"to\":{\"model\":\"b\"}}}\n\n");
    assert!(st.refusal_reply().is_some_and(|r| r.sse));
    // 一个字节都没收到 / opaque：没有。
    assert!(crate::proxy::UsageSniffer::new(true, false).refusal_reply().is_none());
    let mut st = crate::proxy::UsageSniffer::new(false, true);
    st.feed(b"{}");
    assert!(st.refusal_reply().is_none());
    // 不是 UTF-8：没有。
    let mut st = crate::proxy::UsageSniffer::new(false, false);
    st.feed(&[0xff, 0xfe, b'{', b'}']);
    assert!(st.refusal_reply().is_none());
}

/// 拒答格不设上限：学到的条数越过 [`SHAPE_MEMORY_CAP`] 照样进表、照样命中（此前套用那个
/// 上限，实测 2 小时 512 条 `reasoning_extraction` 撞满后新的就学不进了）；重复的不重学。
/// [`clear_learned_memory_kind`] 只清指定种类、别的表不动，种类名对不上什么都不动。
#[test]
fn refused_prompts_are_unbounded_and_cleared_per_kind() {
    let shape = crate::proxy::ShapeMemory::default();
    let dep = crate::proxy::DeprecatedFieldMemory::default();
    let empty = crate::proxy::EmptyReplyMemory::default();
    let n = crate::proxy::SHAPE_MEMORY_CAP + 10;
    for i in 0..n {
        assert!(
            crate::proxy::remember_refused_prompt(
                &empty,
                "claude-opus-5",
                &format!("{i:016x}"),
                "[cyber]",
                json_reply(),
            )
            .is_some(),
            "第 {i} 条也要学进去"
        );
    }
    assert!(
        crate::proxy::remember_refused_prompt(
            &empty,
            "claude-opus-5",
            &format!("{:016x}", 0),
            "[cyber]",
            json_reply(),
        )
        .is_none(),
        "重复的不重学"
    );
    assert_eq!(empty.read().prompts.len(), n);
    let last = serde_json::json!({"messages": [{"role": "user", "content": "x"}]});
    let digest = crate::proxy::prompt_digest(&last).unwrap();
    crate::proxy::remember_refused_prompt(
        &empty,
        "claude-opus-5",
        &digest,
        "[cyber]",
        json_reply(),
    )
    .unwrap();
    assert!(
        crate::proxy::known_refused_prompt(&empty, Some("claude-opus-5"), Some(&last)).is_some()
    );
    // 回填同样不设上限。
    let rows: Vec<store::LearnedRejection> = (0..n)
        .map(|i| store::LearnedRejection {
            kind: "refusal".into(),
            model: "claude-opus-5".into(),
            field: "prompt_sha".into(),
            value: format!("{i:016x}"),
            message: "[cyber]".into(),
            reply: Some(json_reply()),
        })
        .collect();
    let seeded = crate::proxy::resync_learned_memories(&shape, &dep, &empty, rows);
    assert_eq!(seeded.refusal, n);
    assert!(seeded.stale.is_empty());
    // 按种类清空：只清拒答格。
    crate::proxy::remember_empty_reply(&empty, "claude-fable-5", 16, "{}").unwrap();
    dep.write().insert(("claude-opus-5".into(), crate::proxy::FALLBACKS_FIELD.into()), "m".into());
    assert!(!crate::proxy::clear_learned_memory_kind(&shape, &dep, &empty, "bogus"));
    assert_eq!(crate::proxy::learned_memory_len(&shape, &dep, &empty), n + 2);
    crate::proxy::remember_app_refusal(&empty, "claude-opus-5", "app", "[cyber]", json_reply())
        .unwrap();
    assert_eq!(crate::proxy::learned_memory_len(&shape, &dep, &empty), n + 3);
    assert!(crate::proxy::clear_learned_memory_kind(&shape, &dep, &empty, "app_refusal"));
    assert!(empty.read().apps.is_empty());
    assert_eq!(crate::proxy::learned_memory_len(&shape, &dep, &empty), n + 2);
    assert!(crate::proxy::clear_learned_memory_kind(&shape, &dep, &empty, "refusal"));
    assert!(empty.read().prompts.is_empty());
    assert_eq!(empty.read().classes.len(), 1);
    assert_eq!(dep.read().len(), 1);
    assert!(crate::proxy::clear_learned_memory_kind(&shape, &dep, &empty, "empty_reply"));
    assert!(empty.read().classes.is_empty());
    assert!(crate::proxy::clear_learned_memory_kind(&shape, &dep, &empty, "deprecated"));
    assert_eq!(crate::proxy::learned_memory_len(&shape, &dep, &empty), 0);
}

/// [`prompt_digest`]：`tools` 与 `tool_choice` 也进哈希——同一段文字配不同工具集是不同的
/// 请求；缺失与显式 `[]` 也不同。
#[test]
fn prompt_digest_covers_tools_and_tool_choice() {
    let base = serde_json::json!({
        "model": "claude-opus-5", "messages": [{"role": "user", "content": "x"}]
    });
    let d0 = crate::proxy::prompt_digest(&base).unwrap();
    let mut with_tools = base.clone();
    with_tools["tools"] = serde_json::json!([{"name": "Bash", "input_schema": {"type": "object"}}]);
    let d1 = crate::proxy::prompt_digest(&with_tools).unwrap();
    assert_ne!(d0, d1, "带 tools 是另一条");
    let mut other_tools = with_tools.clone();
    other_tools["tools"][0]["name"] = serde_json::json!("Read");
    assert_ne!(d1, crate::proxy::prompt_digest(&other_tools).unwrap(), "换个工具是另一条");
    let mut with_choice = with_tools.clone();
    with_choice["tool_choice"] = serde_json::json!({"type": "auto"});
    assert_ne!(d1, crate::proxy::prompt_digest(&with_choice).unwrap(), "tool_choice 也算");
    let mut empty_tools = base.clone();
    empty_tools["tools"] = serde_json::json!([]);
    assert_ne!(d0, crate::proxy::prompt_digest(&empty_tools).unwrap(), "显式 [] 与缺失不同");
    // 同一条重算稳定。
    assert_eq!(d1, crate::proxy::prompt_digest(&with_tools).unwrap());
    // 没有 messages 的不算。
    assert!(crate::proxy::prompt_digest(&serde_json::json!({"system": "s"})).is_none());
}

/// v0.3.89 学错的 empty_reply 行：文案里的 `"stop_reason":"refusal"` 不论有没有空白、
/// 字段顺序如何，都判为 stale、不回填。
#[test]
fn stale_refusal_rows_are_detected_regardless_of_whitespace() {
    let row = |message: &str| store::LearnedRejection {
        kind: "empty_reply".into(),
        model: "claude-opus-5".into(),
        field: "max_tokens".into(),
        value: "65536".into(),
        message: message.into(),
        reply: None,
    };
    let rows = vec![
        row(r#"{"content":[],"stop_reason":"refusal"}"#),
        row(r#"{"content": [], "stop_reason": "refusal", "stop_details": null}"#),
        row(
            "event: message_delta\ndata: {\"type\": \"message_delta\", \"delta\": {\"stop_reason\" : \"refusal\"}}",
        ),
        // 真正的零输出（end_turn）：照常回填。
        row(r#"{"content":[],"stop_reason":"end_turn","usage":{"output_tokens":0}}"#),
    ];
    let shape = crate::proxy::ShapeMemory::default();
    let dep = crate::proxy::DeprecatedFieldMemory::default();
    let empty = crate::proxy::EmptyReplyMemory::default();
    let seeded = crate::proxy::seed_learned_memories(&shape, &dep, &empty, rows);
    assert_eq!(seeded.stale.len(), 3, "三种写法都判为学错的拒答");
    assert_eq!(seeded.empty_reply, 1, "end_turn 那条照常回填");
    // 同键的 stale 行都被挑出来后，表里只剩 end_turn 那条（同键 or_insert 只留第一条）。
    assert_eq!(empty.read().classes.len(), 1);
}

/// [`resync_learned_memories`]：按传入的行整体重建三张表——库里没有的（过期被删的）从
/// 内存里消失，库里有的回来；[`learned_memory_len`] 前后可比。
#[test]
fn resync_learned_memories_drops_rows_missing_from_store() {
    let shape = crate::proxy::ShapeMemory::default();
    let dep = crate::proxy::DeprecatedFieldMemory::default();
    let empty = crate::proxy::EmptyReplyMemory::default();
    // 先各学一条。
    let probe = &crate::proxy::SHAPE_PROBES[0];
    shape.write().insert(("claude-opus-5".into(), probe.field, "v".into()), "m".into());
    dep.write().insert(("claude-opus-5".into(), crate::proxy::FALLBACKS_FIELD.into()), "m".into());
    crate::proxy::remember_empty_reply(&empty, "claude-fable-5", 16, "{}").unwrap();
    let refused = crate::proxy::remember_refused_prompt(
        &empty,
        "claude-opus-5",
        "deadbeef",
        "[cyber]",
        json_reply(),
    )
    .unwrap();
    assert_eq!(crate::proxy::learned_memory_len(&shape, &dep, &empty), 4);
    // 库里只剩拒答那一条（其余三条已过期被删）：重建后内存里也只剩它。
    let seeded = crate::proxy::resync_learned_memories(&shape, &dep, &empty, vec![refused.clone()]);
    assert_eq!(
        seeded,
        crate::proxy::SeededMemories {
            shape: 0,
            deprecated: 0,
            empty_reply: 0,
            refusal: 1,
            app_refusal: 0,
            stale: vec![]
        }
    );
    assert_eq!(crate::proxy::learned_memory_len(&shape, &dep, &empty), 1);
    assert!(shape.read().is_empty());
    assert!(dep.read().is_empty());
    assert!(empty.read().classes.is_empty());
    assert_eq!(
        empty
            .read()
            .prompts
            .get(&("claude-opus-5".into(), "deadbeef".into()))
            .map(|p| p.verdict.as_str()),
        Some("[cyber]")
    );
    // 库里空了：内存也空。
    let seeded = crate::proxy::resync_learned_memories(&shape, &dep, &empty, vec![]);
    assert_eq!(seeded, crate::proxy::SeededMemories::default());
    assert_eq!(crate::proxy::learned_memory_len(&shape, &dep, &empty), 0);
}

/// 已知模型（4.7+）即使没学过也会主动剥掉 sampling 参数。
#[test]
fn strips_sampling_for_known_models_without_learning() {
    let mem = crate::proxy::DeprecatedFieldMemory::default();
    for model in &[
        "claude-fable-5",
        "claude-opus-5",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-5",
    ] {
        let body = temp_req(model);
        let raw = Bytes::from(serde_json::to_vec(body.as_ref().unwrap()).unwrap());
        let out = crate::proxy::maybe_strip_deprecated(&mem, Some(model), body.as_ref(), raw);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("temperature").is_none(), "{model}: temperature 应该被主动剥掉");
        assert!(v.get("model").is_some(), "{model}: 不该动别的字段");
    }
}

/// 4.6 及更早的模型不在预置名单里，不应主动剥。
#[test]
fn does_not_strip_sampling_for_old_models() {
    let mem = crate::proxy::DeprecatedFieldMemory::default();
    for model in &["claude-opus-4-6", "claude-sonnet-4-6", "claude-haiku-4-5"] {
        let body = temp_req(model);
        let raw = Bytes::from(serde_json::to_vec(body.as_ref().unwrap()).unwrap());
        let out =
            crate::proxy::maybe_strip_deprecated(&mem, Some(model), body.as_ref(), raw.clone());
        assert_eq!(out, raw, "{model}: 不该主动剥");
    }
}

/// 对于不在预置名单的模型，学一次 400 之后才会剥；不同模型不受影响。
#[test]
fn strips_deprecated_field_after_learning() {
    let mem = crate::proxy::DeprecatedFieldMemory::default();
    // 用 4.6（不在预置名单里）测试学习流程。
    let body = temp_req("claude-opus-4-6");

    // 学之前不剥。
    let raw = Bytes::from(serde_json::to_vec(body.as_ref().unwrap()).unwrap());
    let out = crate::proxy::maybe_strip_deprecated(
        &mem,
        Some("claude-opus-4-6"),
        body.as_ref(),
        raw.clone(),
    );
    assert_eq!(out, raw, "学之前应该原样返回");

    // 喂一条 400。
    crate::proxy::remember_deprecated_field(
        &mem,
        Some("claude-opus-4-6"),
        body.as_ref(),
        &err_json(TEMP_400),
    );
    assert_eq!(mem.read().len(), 1);

    // 学过之后剥掉。
    let out =
        crate::proxy::maybe_strip_deprecated(&mem, Some("claude-opus-4-6"), body.as_ref(), raw);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v.get("temperature").is_none(), "temperature 应该被剥掉: {v}");
    assert!(v.get("model").is_some(), "不该动别的字段: {v}");
    assert!(v.get("messages").is_some(), "不该动 messages: {v}");

    // 不同模型不受影响（用 sonnet-4-6，也不在预置名单里）。
    let other_body = temp_req("claude-sonnet-4-6");
    let other_raw = Bytes::from(serde_json::to_vec(other_body.as_ref().unwrap()).unwrap());
    let out = crate::proxy::maybe_strip_deprecated(
        &mem,
        Some("claude-sonnet-4-6"),
        other_body.as_ref(),
        other_raw.clone(),
    );
    assert_eq!(out, other_raw, "不同模型不该被剥");
}

/// 不该学的几种 400：没有 `deprecated`、没有反引号引用字段名、请求里不含该字段。
#[test]
fn learns_nothing_from_unrelated_errors() {
    let cases: &[(&str, &str)] = &[
        // 普通 400，跟 deprecated 无关。
        ("claude-fable-5", "max_tokens: 200000 > 64000, which is the maximum allowed"),
        // 有 deprecated 但没用反引号引字段名。
        ("claude-fable-5", "temperature is deprecated for this model."),
        // 反引号包的不是请求里有的字段。
        ("claude-fable-5", "`top_k` is deprecated for this model."),
    ];
    for (model, msg) in cases {
        let mem = crate::proxy::DeprecatedFieldMemory::default();
        let body = temp_req(model);
        crate::proxy::remember_deprecated_field(&mem, Some(model), body.as_ref(), &err_json(msg));
        assert!(mem.read().is_empty(), "不该学: {msg}");
    }
}

/// `top_p` 也走同一套机制。
#[test]
fn learns_top_p_deprecated() {
    let mem = crate::proxy::DeprecatedFieldMemory::default();
    let body = top_p_req("claude-fable-5");
    crate::proxy::remember_deprecated_field(
        &mem,
        Some("claude-fable-5"),
        body.as_ref(),
        &err_json("`top_p` is deprecated for this model."),
    );
    assert_eq!(mem.read().len(), 1);
    let raw = Bytes::from(serde_json::to_vec(body.as_ref().unwrap()).unwrap());
    let out =
        crate::proxy::maybe_strip_deprecated(&mem, Some("claude-fable-5"), body.as_ref(), raw);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v.get("top_p").is_none(), "top_p 应该被剥掉: {v}");
}

/// 没有模型或没有请求体时安全地不学不剥。
#[test]
fn graceful_on_missing_model_or_body() {
    let mem = crate::proxy::DeprecatedFieldMemory::default();
    // model 为 None。
    crate::proxy::remember_deprecated_field(
        &mem,
        None,
        temp_req("x").as_ref(),
        &err_json(TEMP_400),
    );
    assert!(mem.read().is_empty());
    // body 为 None。
    crate::proxy::remember_deprecated_field(&mem, Some("x"), None, &err_json(TEMP_400));
    assert!(mem.read().is_empty());
    // 剥也一样安全。
    let raw = Bytes::from_static(b"{}");
    assert_eq!(crate::proxy::maybe_strip_deprecated(&mem, None, None, raw.clone()), raw);
}

/// [`record_app_request`]：按比例学——拒答至少 3 条且占该应用请求数三成以上才学；风暴应用
/// 几条就学到，固定 system 偶尔撞一次分类器的真人会话永远学不到；学到后不再计；正常回答
/// 只加分母；表满整体清掉。
#[test]
fn app_refusals_are_learned_by_ratio_not_by_a_single_hit() {
    let mem = crate::proxy::EmptyReplyMemory::default();
    let reply = json_reply();
    let hit = |m: &crate::proxy::EmptyReplyMemory, sha: &str| {
        crate::proxy::record_app_request(
            m,
            "claude-opus-5",
            sha,
            Some(("[reasoning_extraction]", &reply)),
        )
    };
    let ok = |m: &crate::proxy::EmptyReplyMemory, sha: &str| {
        crate::proxy::record_app_request(m, "claude-opus-5", sha, None)
    };
    // 风暴应用：拒、答、拒、拒 → 第三条拒答时 3/4 = 75%，学到。
    assert!(hit(&mem, "storm").is_none());
    assert!(ok(&mem, "storm").is_none());
    assert!(hit(&mem, "storm").is_none(), "两条不够");
    let learned = hit(&mem, "storm").expect("第三条拒答、占 75%，学到");
    assert_eq!((learned.kind.as_str(), learned.value.as_str()), ("app_refusal", "storm"));
    assert_eq!(learned.reply, Some(reply.clone()));
    assert!(mem.read().apps.contains_key(&("claude-opus-5".to_string(), "storm".to_string())));
    // 学到之后不再计，也不重复学。
    assert!(hit(&mem, "storm").is_none());
    assert!(ok(&mem, "storm").is_none());
    // 真人会话：98 条正常、2 条拒答 → 2%，永远不学；再来一条拒答 3/101 也不学（比例不够）。
    for _ in 0..98 {
        assert!(ok(&mem, "agent").is_none());
    }
    assert!(hit(&mem, "agent").is_none());
    assert!(hit(&mem, "agent").is_none());
    assert!(hit(&mem, "agent").is_none(), "3 条但只占 3%，不学");
    assert!(!mem.read().apps.contains_key(&("claude-opus-5".to_string(), "agent".to_string())));
    assert_eq!(
        mem.read().app_counters.get(&("claude-opus-5".to_string(), "agent".to_string())),
        Some(&crate::proxy::AppCounter { total: 101, refused: 3 })
    );
    // 3 条拒答、3 条正常 = 50%：学。恰好 30% 也学（3/10）。
    for i in 0..3 {
        assert!(ok(&mem, "half").is_none(), "{i}");
        assert!(hit(&mem, "half").is_none() || i == 2);
    }
    assert!(mem.read().apps.contains_key(&("claude-opus-5".to_string(), "half".to_string())));
    for _ in 0..7 {
        ok(&mem, "edge");
    }
    assert!(hit(&mem, "edge").is_none());
    assert!(hit(&mem, "edge").is_none());
    assert!(hit(&mem, "edge").is_some(), "3/10 = 30% 恰好到线");
    // 每小时重建记忆表时计数器保留。
    let shape = crate::proxy::ShapeMemory::default();
    let dep = crate::proxy::DeprecatedFieldMemory::default();
    crate::proxy::resync_learned_memories(&shape, &dep, &mem, vec![]);
    assert!(mem.read().apps.is_empty(), "规则按库重建（库里没有）");
    assert_eq!(
        mem.read().app_counters.get(&("claude-opus-5".to_string(), "agent".to_string())),
        Some(&crate::proxy::AppCounter { total: 101, refused: 3 }),
        "计数器不随重建丢失"
    );
    // 表满：整体清掉重计。
    for i in 0..crate::proxy::APP_COUNTER_MAX_KEYS {
        ok(&mem, &format!("k{i}"));
    }
    assert!(mem.read().app_counters.len() <= crate::proxy::APP_COUNTER_MAX_KEYS);
    ok(&mem, "one-more");
    assert!(mem.read().app_counters.len() <= crate::proxy::APP_COUNTER_MAX_KEYS);
}

/// 瞬时限流交回客户端的 `retry-after` 必须是**指数**退避，且档位只随**墙钟**往上走。
///
/// 这一档不换号、也不把号挪出调度池，客户端拿到的就是一发 429——那么「下次什么时候再来」
/// 就是我们唯一还能影响拥堵的东西。固定值做不到「重试密度随失败次数下降」：一群客户端会
/// 按同一个节拍同时回来，正在拥堵的出口该塌还是塌；秒级重试更是直接把拥堵喂大。
///
/// 「随墙钟」那一半是后补的，见 [`crate::proxy::TRANSIENT_MAX_ATTEMPTS`]：这条用例曾经拿 1 毫秒
/// 间隔连打 8 发去断言整条阶梯，等于把「档位数的是并发度」这个 bug 冻进了测试里。
#[test]
fn transient_backoff_doubles_once_per_elapsed_window_and_decays_when_quiet() {
    let state = crate::proxy::TransientBackoff::default();
    let t0 = std::time::Instant::now();
    let secs = std::time::Duration::from_secs;
    let hit = |at: std::time::Instant| {
        let (wait, attempts) =
            crate::proxy::next_transient_backoff_at(&state, 1, "claude-opus-5", at);
        (wait.as_secs(), attempts)
    };

    // 一串的完整形状：2 → 4 → 8 → 16 → 32 → 60，第 6 档即「吞够了」，之后重新从 2 数起。
    // 封顶那一档就是上限本身：退避都涨到头还在撞，再吞下去只是让客户端一直吃 429。
    // 升档的时刻是**上一档等满**的时刻，故走完整条阶梯要 2+4+8+16+32=62 秒。
    let ladder: Vec<(u64, u32)> =
        [0, 2, 6, 14, 30, 62].iter().map(|s| hit(t0 + secs(*s))).collect();
    assert_eq!(
        ladder,
        vec![(2, 1), (4, 2), (8, 3), (16, 4), (32, 5), (60, 6)],
        "每等满一档才翻一倍，第 6 档到达上限"
    );
    assert_eq!(hit(t0 + secs(63)), (2, 1), "吞够了就地清零，下一发从头数起");
    assert_eq!(
        crate::proxy::TRANSIENT_MAX_ATTEMPTS,
        6,
        "上限必须正好落在退避封顶那一档上，否则 60 秒那一档要么白等要么根本走不到"
    );

    // 并发不吃档位：同一瞬间在飞的一批请求共用当前档位，一起拿 2 秒、一起算连撞第 1 档。
    // 线上那份日志里 6 条并发（`ttft_ms` 都在 230 上下）在 63 毫秒内撞完，按发数数就把
    // 6 格一次性吃光，于是这个号的这个模型被硬冷却挪出调度池，1.5 秒内一路点掉 5 个号。
    let burst: Vec<(u64, u32)> = (0..8)
        .map(|i| {
            let at = t0 + std::time::Duration::from_millis(i);
            let (wait, attempts) =
                crate::proxy::next_transient_backoff_at(&state, 3, "claude-opus-5", at);
            (wait.as_secs(), attempts)
        })
        .collect();
    assert_eq!(burst, vec![(2, 1); 8], "毫秒级的并发突发只能算连撞第 1 档");
    assert!(
        burst.iter().all(|(_, n)| *n < crate::proxy::TRANSIENT_MAX_ATTEMPTS),
        "并发突发绝不能触发「吞够了」——那会把这个号的这个模型硬冷却挪出调度池"
    );

    // 不认 `retry-after`、毫秒级重来的客户端照样要能把档位顶上去：锚点不刷新，档位按墙钟
    // 自己爬。没有这一条，「吞够了」那条逃生口对这类客户端永远走不到。
    let hammer = |at: std::time::Instant| {
        crate::proxy::next_transient_backoff_at(&state, 4, "claude-opus-5", at).1
    };
    let mut ms = 0u64;
    let mut peak = 0;
    while ms <= 62_000 {
        peak = peak.max(hammer(t0 + std::time::Duration::from_millis(ms)));
        ms += 200;
    }
    assert_eq!(peak, crate::proxy::TRANSIENT_MAX_ATTEMPTS, "连坏 62 秒就该判定这条路线走不通");

    // 别的账号、别的模型各算各的——一条路线拥堵不该让不相干的请求跟着等。
    assert_eq!(
        crate::proxy::next_transient_backoff_at(&state, 2, "claude-opus-5", t0).0.as_secs(),
        crate::proxy::TRANSIENT_BACKOFF_BASE_SECS,
        "另一个账号应从头数起"
    );
    assert_eq!(
        crate::proxy::next_transient_backoff_at(&state, 1, "claude-sonnet-5", t0).0.as_secs(),
        crate::proxy::TRANSIENT_BACKOFF_BASE_SECS,
        "同一个账号的另一个模型也应从头数起"
    );

    // 一档挂够久没能升上去 → 清零，从 2 秒重新数起。没有这条的话计数只增不减，几小时后
    // 偶发一次限流也会被判成「连撞第 9 档」，直接甩给客户端 60 秒。
    // 从**进入这一档的时刻**（上面那发 t0+63s）算起要够久，不是从 t0 算起。
    let later = t0 + secs(63) + crate::proxy::TRANSIENT_BACKOFF_RESET + secs(1);
    assert_eq!(hit(later), (2, 1), "这一档挂过重置窗口后应回到起点");
    // 刚清过零，等满这一档再撞才是这一串的第二档。
    assert_eq!(hit(later + secs(1)), (2, 1), "还没等满，仍是第 1 档");
    assert_eq!(hit(later + secs(2)), (4, 2), "等满 2 秒又撞上，这才是第 2 档");
}

/// 上游没给 `retry-after` 时，客户端实际拿到的退避序列。指数那一半几乎全被 30 秒的
/// 地板（[`DEFAULT_MODEL_COOLDOWN_SECS`]）吃掉：只有第 5、6 档才越过它。
/// [`next_transient_backoff`] 的注释里那句「第一次偶发限流几乎无感（2 秒）」在这条路上
/// 不成立。线上日志里那串 30/30/30/30/32/60 就是这么来的。
///
/// 但那串在线上是 63 毫秒内打完的——那是「档位数发数」的锅，现在它只能是 62 秒的产物；
/// 同一瞬间的一批并发从头到尾都是 30。两条一起断言，免得日后有人看着日志里的
/// 30/30/30/30/32/60 又把发数计数改回去。
#[test]
fn the_backoff_a_client_actually_sees_is_almost_flat() {
    let bare = crate::proxy::RateLimitInfo::from_headers(&crate::proxy::HeaderMap::new());
    let floor = bare.transient_cooldown();
    assert_eq!(floor.as_secs(), 30, "上游没给 retry-after 时的地板");

    let state = crate::proxy::TransientBackoff::default();
    let t0 = std::time::Instant::now();
    let seen = |cred_id, offsets: &[u64]| -> Vec<u64> {
        offsets
            .iter()
            .map(|ms| {
                let at = t0 + std::time::Duration::from_millis(*ms);
                let (wait, _) =
                    crate::proxy::next_transient_backoff_at(&state, cred_id, "claude-opus-5", at);
                floor.max(wait).as_secs()
            })
            .collect()
    };
    assert_eq!(
        seen(1, &[0, 2_000, 6_000, 14_000, 30_000, 62_000]),
        vec![30, 30, 30, 30, 32, 60],
        "熬满整条阶梯才与线上日志那串逐档对得上"
    );
    assert_eq!(
        seen(2, &[0, 5, 10, 13, 31, 63]),
        vec![30; 6],
        "线上那 63 毫秒内的 6 条并发，如今一律是第 1 档的 30 秒"
    );
}
