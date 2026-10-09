//! 出站形态清理：多余字段、tool_choice、顶层字段顺序、空壳 system 消息。

/// 剥掉官方客户端**从不发送**的顶层字段，返回是否改动过。只在
/// [`store::ForwardFlags::strip_extra_fields`] 开着时调用。
///
/// 判据逐条取自 `cap/raw/00006`（opus-5）与 `00009`（sonnet-5）两份直连抓包——两份的顶层键
/// 恒为 `model, messages, system, tools, metadata, max_tokens, thinking, context_management,
/// output_config, stream`，多一个就是白送的判据。
///
/// 只管「官方从不发」的形态，**不修补客户端自己写错的参数**：与 thinking 冲突的
/// `temperature` / `top_p`、不足 1024 的 `budget_tokens`、强制工具配手动预算 thinking、fable
/// 族的 `thinking: disabled` 之类，原样发出，由上游回官方的 400。
///
/// 1. **`tool_choice`**：官方两份抓包里这个键**压根不存在**。但只删**等价于默认值**的那一种
///    （恰好只有 `{"type":"auto"}` 一个键）——`{"type":"tool", "name":…}`/`{"type":"any"}`
///    是客户端在强制选工具，`disable_parallel_tool_use` 也是它要的行为，删了就是改语义。
///    删掉的那种对模型零影响：`auto` 本来就是缺省。
///
/// 2. **`thinking.display`**：2.1.251 及之前官方发的是裸的 `{"type":"adaptive"}`；**2.1.258 起
///    fable 族官方自己也发 `display:"updates"`**（`cap/2.1.258/00013`，配着
///    `thinking-display-updates-2026-08-18` beta）。故这一项由 `keep_display` 拨：来访本来
///    就是 CC 形态（真 CC 带什么 `display` 就发什么），或 `thinking` 整个是
///    [`ensure_thinking`] 按官方形态补的，都不剥；只剥第三方客户端自己写的 `display`。
///
///    **这一项有代价，不是零影响**：`display:"summarized"` 是客户端主动要思考摘要，剥掉之后
///    上游按缺省的 `omitted` 走，回程的 `thinking` 块文本为空，客户端那边的「思考过程」就空了。
///    功能不坏（块还在、签名照旧），只是看不到内容。由开关兜底——不接受这个代价就关掉它。
///
/// **对真实 CC**：第一项本来就是空操作（官方不发 `tool_choice`），第二项由调用方传
/// `keep_display = true` 跳过——2.1.258 起 `display` 是官方形态的一部分。
/// 判定要在模拟**之前**做（[`rewrite_body`] 里的 `cc_inbound`）：模拟一跑 body 就都是 CC 形态了。
pub(in crate::proxy) fn strip_extra_fields(v: &mut serde_json::Value, keep_display: bool) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    let mut changed = false;
    if obj.get("tool_choice").is_some_and(is_default_tool_choice) {
        obj.remove("tool_choice");
        changed = true;
    }
    // CC 自己发的 / luban 按官方形态补的 `display` 照发；见函数文档第 2 项。
    if !keep_display
        && let Some(thinking) = obj.get_mut("thinking").and_then(|t| t.as_object_mut())
        && thinking.remove("display").is_some()
    {
        changed = true;
    }
    changed
}

/// thinking 开着时剥掉与它冲突的采样参数，返回是否改动过。只给 luban **自己注入**的 thinking
/// 用（[`ensure_thinking`]）：那是 luban 造出来的冲突，得自己收拾。客户端自己写的 thinking 与
/// 采样参数冲突不归这里，原样发出、由上游回 400。
///
/// - `temperature`：thinking 开着时上游强制为 1，别的值直接 400。删掉即可——默认值就是 1。
/// - `top_p`：上游要求「不传或 >= 0.95」（`top_p must be greater than or equal to 0.95 or
///   unset when thinking is enabled or in adaptive mode`）。这条是条件句，学习机制有意不学
///   （见 `CONDITIONAL_MARKS`），只能在这里静态兜住。>= 0.95 的照发；非数字的取值也剥：
///   上游一样 400，留着只是换一种死法。
pub(in crate::proxy) fn drop_sampling_conflicting_with_thinking(
    obj: &mut serde_json::Map<String, serde_json::Value>,
) -> bool {
    // 判据同 ensure_context_management 那里的口径。
    let thinking_on = obj
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| matches!(t, "enabled" | "adaptive"));
    if !thinking_on {
        return false;
    }
    let mut changed = false;
    if obj.get("temperature").and_then(|t| t.as_f64()) != Some(1.0)
        && obj.remove("temperature").is_some()
    {
        changed = true;
    }
    if obj.get("top_p").is_some_and(|p| !p.as_f64().is_some_and(|p| p >= 0.95))
        && obj.remove("top_p").is_some()
    {
        changed = true;
    }
    changed
}

/// `tool_choice` 是否等价于「不写这个字段」，即恰好只有 `{"type":"auto"}` 一个键。
/// 多带任何一个键（如 `disable_parallel_tool_use`）都是客户端在要一种非缺省行为，不能删。
pub(in crate::proxy) fn is_default_tool_choice(v: &serde_json::Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == 1 && o.get("type").and_then(|t| t.as_str()) == Some("auto"))
}

/// 把请求对象改成 profile 的顶层键序（[`config::CcProfile::body_key_order`]），
/// 并保证 `stream` 在最后。
///
/// **键序按 profile 分**：主线程那串（[`config::CC_BODY_ORDER_MAIN`]）套不到安全分类
/// （`max_tokens` 在第二位、`system` 在 `messages` 前）与额度探测（只有四个键）上，
/// 硬套出来的是官方从不产生的排列。
///
/// 不认识的字段可能有语义，不能丢；保留它们彼此的原始顺序，放在已知字段与 `stream`
/// 之间。本函数只在 [`Simulation`] 路径调用，真 CC 请求继续保留客户端的字节与顺序。
pub(in crate::proxy) fn align_cc_top_level_order(
    v: &mut serde_json::Value,
    order: &[&str],
) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    let before: Vec<String> = obj.keys().cloned().collect();
    let mut old = std::mem::take(obj);
    let mut ordered = serde_json::Map::new();

    for key in order {
        if let Some(value) = old.shift_remove(*key) {
            ordered.insert((*key).to_string(), value);
        }
    }
    let stream = old.shift_remove("stream");
    ordered.extend(old);
    if let Some(value) = stream {
        ordered.insert("stream".to_string(), value);
    }

    let changed = before.iter().map(String::as_str).ne(ordered.keys().map(String::as_str));
    *obj = ordered;
    changed
}

/// `role:"system"` 空壳：`content` 缺失、`null`、空串、空数组，或整条只有空 `text` 块；
/// [`is_system_directive`] 除外。
///
/// luban 不丢它，原样出站：上游对它回的是另一句 400（`system content must contain at least
/// one block` / `text content blocks must be non-empty`）。本地那两道按位置与按学到规则的拦截
/// （[`misplaced_system_role`]、`learned_rules::role_values`）都不把它算进去，交给上游自己说。
pub(in crate::proxy) fn is_empty_system_shell(msg: &serde_json::Value) -> bool {
    if msg.get("role").and_then(|r| r.as_str()) != Some("system") || is_system_directive(msg) {
        return false;
    }
    match msg.get("content") {
        // 字段缺失或写成 null。
        None | Some(serde_json::Value::Null) => true,
        // 空串。**不 trim**：一个空格在上游那边是合法的非空文本，判它为空就是替客户端
        // 删掉一条它认为有内容的消息。
        Some(serde_json::Value::String(s)) => s.is_empty(),
        // 空数组，或整条只有空 `text` 块：留下来同样是一次 400。
        Some(serde_json::Value::Array(arr)) => arr.iter().all(|blk| {
            blk.get("type").and_then(|t| t.as_str()) == Some("text")
                && blk.get("text").and_then(|t| t.as_str()).is_some_and(str::is_empty)
        }),
        // 别的形态（对象、数字……）不是本函数的事，交给上游去说。
        Some(_) => false,
    }
}

/// 指令式 `role:"system"`：`content` 恰为空数组、且带消息级 `output_config`。
///
/// 上游原话（2026-10-02 线上 400）：`the directive-only form (content: [] with output_config)
/// is accepted at any position`。口径逐字照搬——空串、缺 content 的不算，上游没说收。
pub(in crate::proxy) fn is_system_directive(msg: &serde_json::Value) -> bool {
    msg.get("role").and_then(|r| r.as_str()) == Some("system")
        && msg.get("content").and_then(|c| c.as_array()).is_some_and(|a| a.is_empty())
        && msg.get("output_config").is_some()
}

/// 只管一轮的 system（`clear_at: "next_user_message"`，beta `mid-conversation-system-clear-at`）：
/// 下一条 user 一出现它就不再渲染，但仍留在历史里。官方文档的限制：只能是文本、**不能带
/// `cache_control`**（断点放在它前一条上）。官方抓包里它恒为一段字符串，断点与 `<total_tokens>`
/// 都落在它前一条上（`cap/auto-2.1.291-20261006-full/00465`、`00471`）。
///
/// 往它里面并东西，并进去的也只活一轮；把它提升到顶层 system，它就成了永久指令。
pub(in crate::proxy) fn is_turn_scoped_system(msg: &serde_json::Value) -> bool {
    msg.get("role").and_then(|r| r.as_str()) == Some("system")
        && msg.get("clear_at").and_then(|c| c.as_str()) == Some("next_user_message")
}

/// 对话中途 `role:"system"` 摆错位置时上游那句 400 的原话，`{}` 处是消息下标。
///
/// 2026-10-02 线上实测（`claude-opus-5-5`）：
/// ```text
/// messages.1: role 'system' must precede an 'assistant' message or end the array; the
/// directive-only form (content: [] with output_config) is accepted at any position
/// ```
fn misplaced_system_message(index: usize) -> String {
    format!(
        "messages.{index}: role 'system' must precede an 'assistant' message or end the array; \
         the directive-only form (content: [] with output_config) is accepted at any position"
    )
}

/// 对话中途的 `role:"system"` 摆错了位置 → 上游那句原话（见 [`misplaced_system_message`]）。
///
/// 规则是上游报错自己写明的：中途的 system 必须紧挨在 assistant 之前，或者是数组最后一条；
/// 指令式写法（[`is_system_directive`]）放哪儿都行。官方抓包（2.1.260–2.1.285，七百余条中途
/// system）与之吻合，并补了一条：**连续几条 system 算一段**——`user → system →
/// system(clear_at) → assistant` 官方常发、上游收，所以判的是「这一段之后的第一条非 system」。
///
/// **只拒确定无疑的**：段后紧跟的是 `user` 才算错（`tool` 之类别的 role 交给上游去说）；段里
/// 夹着指令式 system 时不拒——「普通 system → 指令 → user」上游收不收没有实测，宁可放过去
/// 让上游判。空壳（[`is_empty_system_shell`]）不算数：上游对它回的是另一句。
///
/// 首条 user/assistant 之前的不归这里（[`first_turn_index`]）：上游回的是另一句，由
/// [`find_openai_marker`] 处理（它停用时原样交给上游）。
pub(in crate::proxy) fn misplaced_system_role(body: Option<&serde_json::Value>) -> Option<String> {
    let msgs = body?.get("messages")?.as_array()?;
    let role = |m: &serde_json::Value| m.get("role").and_then(|r| r.as_str()).map(str::to_owned);
    let mut i = first_turn_index(msgs);
    while i < msgs.len() {
        if role(&msgs[i]).as_deref() != Some("system") {
            i += 1;
            continue;
        }
        // 这一段连续 system 是 [i, end)。
        let end = (i..msgs.len())
            .find(|&j| role(&msgs[j]).as_deref() != Some("system"))
            .unwrap_or(msgs.len());
        let run = &msgs[i..end];
        let followed_by_user = end < msgs.len() && role(&msgs[end]).as_deref() == Some("user");
        if followed_by_user && !run.iter().any(is_system_directive) {
            // 点名段里第一条不是空壳的；整段全是空壳则交给上游，它回的是另一句。
            if let Some(k) = run.iter().position(|m| !is_empty_system_shell(m)) {
                return Some(misplaced_system_message(i + k));
            }
        }
        i = end;
    }
    None
}

/// `messages` 里首条 user/assistant 的下标（一条都没有时为长度）。
///
/// 在它之前的 `role:"system"` 是 OpenAI 那种开头系统提示词，上游恒 400（`messages.0: use the
/// top-level 'system' parameter for the initial system prompt`）；在它之后的上游是认的
/// （2026-09-30 实测：新模型 200，老模型回 `role 'system' is not supported on this model`）。
/// 判定（[`find_openai_marker`]）与形态记忆（`learned_rules::role_values`）共用这一条，
/// 口径不许分叉。
pub(in crate::proxy) fn first_turn_index(msgs: &[serde_json::Value]) -> usize {
    msgs.iter()
        .position(|m| matches!(m.get("role").and_then(|r| r.as_str()), Some("user" | "assistant")))
        .unwrap_or(msgs.len())
}
