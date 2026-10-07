//! 出站形态清理：多余字段、tool_choice、顶层字段顺序、空块与 system 角色。

use super::*;

/// 剥掉官方客户端**从不发送**的顶层字段，返回是否改动过。只在
/// [`store::ForwardFlags::strip_extra_fields`] 开着时调用。
///
/// 判据逐条取自 `cap/raw/00006`（opus-5）与 `00009`（sonnet-5）两份直连抓包——两份的顶层键
/// 恒为 `model, messages, system, tools, metadata, max_tokens, thinking, context_management,
/// output_config, stream`，多一个就是白送的判据。
///
/// 目前三项：
///
/// 1. **`tool_choice`**：官方两份抓包里这个键**压根不存在**。但只删**等价于默认值**的那一种
///    （恰好只有 `{"type":"auto"}` 一个键）——`{"type":"tool", "name":…}`/`{"type":"any"}`
///    是客户端在强制选工具，`disable_parallel_tool_use` 也是它要的行为，删了就是改语义。
///    删掉的那种对模型零影响：`auto` 本来就是缺省。
///
/// 2. **`thinking.type == "disabled"`**：fable 族不支持显式关闭思考，会直接 400。
///    删掉整个 `thinking` 字段让上游走 adaptive 默认值。
///
///    **只对 fable 族删。** 这一条曾是无条件的，那是错的：`{"type":"disabled"}` 是
///    2.1.260 三个官方 profile（无工具 helper、标题生成、安全分类）的**正常形态**
///    （`cap/2.1.260/00024`、`cap/2.1.260-2/00058`、`cap/2.1.260/00019`，模型分别是 haiku
///    与 sonnet）。把它当成「官方从不发的多余字段」删掉，等于把一条官方形态的请求改成了
///    官方不产生的形态，还顺带把客户端「不要思考」的意图翻成了「随你」——那是要花钱的。
///
/// 3. **`thinking.display`**：2.1.251 及之前官方发的是裸的 `{"type":"adaptive"}`；**2.1.258 起
///    fable 族官方自己也发 `display:"updates"`**（`cap/2.1.258/00013`，配着
///    `thinking-display-updates-2026-08-18` beta）。故这一项由 `keep_display` 拨：来访本来
///    就是 CC 形态（真 CC 带什么 `display` 就发什么），或 `thinking` 整个是
///    [`ensure_thinking`] 按官方形态补的，都不剥；只剥第三方客户端自己写的 `display`。
///
///    **这一项有代价，不是零影响**：`display:"summarized"` 是客户端主动要思考摘要，剥掉之后
///    上游按缺省的 `omitted` 走，回程的 `thinking` 块文本为空，客户端那边的「思考过程」就空了。
///    功能不坏（块还在、签名照旧），只是看不到内容。拿「一条 400 直接打不通」换「思考摘要看不
///    到」是划算的，但划算不等于无损，故写在这里，并由开关兜底——不接受这个代价就关掉它。
///
/// **对真实 CC**：前两项本来就是空操作（官方不发 `tool_choice`、不发 `disabled`），第三项
/// 由调用方传 `keep_display = true` 跳过——2.1.258 起 `display` 是官方形态的一部分。
/// 判定要在模拟**之前**做（[`rewrite_body`] 里的 `cc_inbound`）：模拟一跑 body 就都是 CC 形态了。
pub(in crate::proxy) fn strip_extra_fields(v: &mut serde_json::Value, keep_display: bool) -> bool {
    let fable = v
        .get("model")
        .and_then(|m| m.as_str())
        .is_some_and(|m| m.to_ascii_lowercase().contains("fable"));
    let Some(obj) = v.as_object_mut() else { return false };
    let mut changed = false;
    if obj.get("tool_choice").is_some_and(is_default_tool_choice) {
        obj.remove("tool_choice");
        changed = true;
    }
    // `thinking.type == "disabled"`：fable 族不支持，直接 400。删掉整个 `thinking`
    // 字段让上游走 adaptive 默认值——客户端的意图（不要深度思考）近似保留，好过打不通。
    // 别的族**不动**：那是 2.1.260 三个官方辅助 profile 的正常形态，见函数文档第 2 项。
    if fable
        && obj.get("thinking").and_then(|t| t.get("type")).and_then(|t| t.as_str())
            == Some("disabled")
    {
        obj.remove("thinking");
        changed = true;
    }
    // tool_choice 强制工具调用（`tool` / `any`）时上游不收**手动预算**那种 thinking：2026-10-07
    // 实测 haiku-4-5 `enabled` + `any` 回 400 `Thinking may not be enabled when tool_choice forces
    // tool use.`。客户端同时发了两者时删 thinking 保 tool_choice：强制工具是客户端明确要的
    // 语义，thinking 可缺省。
    //
    // `adaptive` 不删：官方文档写明 Claude API 上强制工具不要求关思考（只有 Bedrock 要求配
    // `disabled`），Opus 4.7 / 4.8 / Opus 5 / Sonnet 5 上它是合法组合，删了等于替客户端关了思考。
    // Opus 5.5 / Sonnet 5.5 / Fable 5.1 本来就不收强制工具，删不删 thinking 都是 400。
    let forces_tool = matches!(
        obj.get("tool_choice").and_then(|tc| tc.get("type")).and_then(|t| t.as_str()),
        Some("tool" | "any")
    );
    let manual_thinking =
        obj.get("thinking").and_then(|t| t.get("type")).and_then(|t| t.as_str()) == Some("enabled");
    if forces_tool && manual_thinking {
        obj.remove("thinking");
        changed = true;
    }
    if let Some(thinking) = obj.get_mut("thinking").and_then(|t| t.as_object_mut()) {
        // CC 自己发的 / luban 按官方形态补的 `display` 照发；见函数文档第 3 项。
        if !keep_display && thinking.remove("display").is_some() {
            changed = true;
        }
        // thinking.type == "enabled" 时 budget_tokens 必须 >= 1024，否则上游 400。
        if thinking.get("type").and_then(|t| t.as_str()) == Some("enabled")
            && let Some(budget) = thinking.get("budget_tokens").and_then(|b| b.as_u64())
            && budget < 1024
        {
            thinking.insert("budget_tokens".into(), serde_json::Value::Number(1024.into()));
            changed = true;
        }
    }
    // thinking 开着时 temperature 必须是 1（上游强制），客户端设了别的值直接 400。
    // 删掉即可——默认值就是 1。判据同 ensure_context_management 那里的口径。
    let thinking_on = obj
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| matches!(t, "enabled" | "adaptive"));
    if thinking_on
        && obj.get("temperature").and_then(|t| t.as_f64()) != Some(1.0)
        && obj.remove("temperature").is_some()
    {
        changed = true;
    }
    // 同理 top_p：thinking 开着时上游要求「不传或 >= 0.95」（`top_p must be greater than or
    // equal to 0.95 or unset when thinking is enabled or in adaptive mode`）。这条是条件句，
    // 学习机制有意不学（见 `CONDITIONAL_MARKS`），只能在这里静态兜住。>= 0.95 的照发。
    // 非数字的取值也剥：上游一样 400，留着只是换一种死法。
    if thinking_on
        && obj.get("top_p").is_some_and(|p| !p.as_f64().is_some_and(|p| p >= 0.95))
        && obj.remove("top_p").is_some()
    {
        changed = true;
    }
    changed
}

/// 把 OpenAI 风格的 `tool_choice` 归一成 Anthropic 的对象形态，返回是否改动过。
///
/// Anthropic 只认 `{"type":"auto"|"any"|"tool"|"none", …}` 这一种对象；其它任何形态上游都回
/// 400 `tool_choice: Input should be an object`。OpenAI 兼容层与各类 SDK 常见的几种写法及其对应：
///
/// | 来访 | 出站 |
/// |---|---|
/// | `null` | 删掉（缺省） |
/// | `"auto"` | `{"type":"auto"}`（随后可被 [`strip_extra_fields`] 按缺省剥掉） |
/// | `"none"` | `{"type":"none"}` |
/// | `"required"` / `"any"` | `{"type":"any"}` |
/// | `{"type":"function","function":{"name":X}}` | `{"type":"tool","name":X}` |
/// | `{"type":"function"}`（没指定名字） | `{"type":"any"}` |
///
/// 认不出的形态**原样放行**，让上游报它自己的错——这里只翻译已知的方言，不替客户端猜。
/// 已经是 Anthropic 对象形态的一律不动（含 `disable_parallel_tool_use` 等附加键）。
pub(in crate::proxy) fn normalize_tool_choice(v: &mut serde_json::Value) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    let Some(tc) = obj.get("tool_choice") else { return false };
    let replacement = match tc {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(serde_json::json!({ "type": "auto" })),
            "none" => Some(serde_json::json!({ "type": "none" })),
            "required" | "any" => Some(serde_json::json!({ "type": "any" })),
            _ => return false,
        },
        serde_json::Value::Object(o)
            if o.get("type").and_then(|t| t.as_str()) == Some("function") =>
        {
            match o.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                Some(name) => Some(serde_json::json!({ "type": "tool", "name": name })),
                None => Some(serde_json::json!({ "type": "any" })),
            }
        }
        _ => return false,
    };
    match replacement {
        // `insert` 对已有键原位改值（`preserve_order`），键序不动。
        Some(val) => {
            obj.insert("tool_choice".into(), val);
        }
        None => {
            obj.remove("tool_choice");
        }
    }
    true
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

/// 上游要求 `text` 内容块的 `text` 字段非空（`text content blocks must be non-empty`），
/// 部分第三方客户端会发 `{"type":"text","text":""}` 的空块。
///
/// 此函数遍历 `messages`，从每条消息的 `content` 数组里剥掉空 text 块。
/// **安全守则**：剥完后若 content 变空则不动——空数组是另一种上游必拒的形态，
/// 不该把一种 400 换成另一种。
pub(in crate::proxy) fn strip_empty_text_blocks(v: &mut serde_json::Value) -> bool {
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for msg in msgs.iter_mut() {
        let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        let non_empty_count = content
            .iter()
            .filter(|blk| {
                let is_empty_text = blk.get("type").and_then(|t| t.as_str()) == Some("text")
                    && blk.get("text").and_then(|t| t.as_str()).is_some_and(|t| t.is_empty());
                !is_empty_text
            })
            .count();
        if non_empty_count == content.len() || non_empty_count == 0 {
            continue;
        }
        content.retain(|blk| {
            let is_empty_text = blk.get("type").and_then(|t| t.as_str()) == Some("text")
                && blk.get("text").and_then(|t| t.as_str()).is_some_and(|t| t.is_empty());
            !is_empty_text
        });
        changed = true;
    }
    if changed {
        tracing::info!("stripped empty text content blocks from messages");
    }
    changed
}

/// 丢掉 `content` 为**空壳**的 `role:"system"` 消息：空数组、空串、字段缺失或为 `null`、
/// 以及整条只有空 `text` 块的。返回是否确有丢掉的。
///
/// 上游对这种消息恒回 400（`messages.N: system content must contain at least one block`；
/// 全是空 text 块的那种是 `text content blocks must be non-empty`）。实跑里撞上它的是一条
/// `claude-cli/2.1.270 (external, claude-vscode, agent-sdk/0.3.270)` 的正经 CC 请求
/// （`req_grlwDAtQQpqvf54d`，透传、没走模拟）：官方在 `messages` 里合法使用 `role:"system"`
/// （deferred tools），这次那条是个空壳。
///
/// **不受 `hoist_system_role` 开关与「CC 形态跳过」那道豁免管**，理由是两者的取舍在这里都不
/// 成立：豁免是怕把官方合法的 `role:"system"` 提升掉、破坏形态，而空壳一个块都没有，不携带
/// 任何语义，留着必是一次 400、丢掉什么也不丢；开关管的是「要不要替第三方客户端把 system
/// 挪位置」，也与「上游必拒的形态」无关。只在上游本来就会拒的请求上动手，所以它不可能把一条
/// 本来能过的请求改坏。
///
/// **只碰 `role:"system"`**：空 content 的 user / assistant 消息同样会被上游拒，但删掉它们会
/// 改变轮次交替（末轮变成 assistant、整个 messages 变空……），那是另一回事，不在这里处理。
///
/// **指令式写法不算空壳**（[`is_system_directive`]）：`content: []` 带消息级 `output_config`，
/// 2.1.285 官方就这么发（`{"role":"system","output_config":{"effort":"high"},"content":[]}`），
/// 上游明说它「放在任何位置都收」。删掉它就是把客户端中途调的 effort 一并删了。
pub(in crate::proxy) fn drop_empty_system_messages(v: &mut serde_json::Value) -> bool {
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };
    let total = msgs.len();
    let mut dropped: Vec<String> = Vec::new();
    for (i, msg) in msgs.iter().enumerate() {
        if is_empty_system_shell(msg) {
            dropped.push(format!("{i}/{total}"));
        }
    }
    if dropped.is_empty() {
        return false;
    }
    msgs.retain(|msg| !is_empty_system_shell(msg));
    tracing::info!(
        count = dropped.len(),
        at = %dropped.join(", "),
        "dropped empty role:\"system\" messages: upstream rejects a system message with no content blocks"
    );
    true
}

/// 出站时会被 [`drop_empty_system_messages`] 丢掉的那种 `role:"system"` 空壳：`content` 缺失、
/// `null`、空串、空数组，或整条只有空 `text` 块；[`is_system_directive`] 除外。
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
        // 空数组，或整条只有空 `text` 块——后者 `strip_empty_text_blocks` 按约定不会去剥
        // （剥完会变空），留下来同样是一次 400。
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

/// 提升（[`hoist_system_role_messages`]）整条不动的 system：指令、带非文本块的、对话中途
/// （`mid`：首条 user/assistant 之后）只管一轮的。
fn hoisting_skips(msg: &serde_json::Value, mid: bool) -> bool {
    is_system_directive(msg) || has_non_text_blocks(msg) || (mid && is_turn_scoped_system(msg))
}

/// 提升之后 `messages` 里还留不留 `role:"system"`：整条不动的（[`hoisting_skips`]），或带
/// `output_config`、拆出一条指令留在原位的。形态记忆的豁免（`known_shape_rejection`）与提升本身
/// 共用这一条，位置条件也一样——开头那条只管一轮的会被提升走，中途的不会。
///
/// 判的是入站原件，所以先排除出站前就会被丢掉的空壳（[`is_empty_system_shell`]，
/// [`drop_empty_system_messages`] 在提升之前跑）；位置也按丢掉空壳之后的算。
pub(in crate::proxy) fn system_survives_hoisting(msgs: &[serde_json::Value]) -> bool {
    let kept: Vec<&serde_json::Value> = msgs.iter().filter(|m| !is_empty_system_shell(m)).collect();
    let first_turn = kept
        .iter()
        .position(|m| matches!(m.get("role").and_then(|r| r.as_str()), Some("user" | "assistant")))
        .unwrap_or(kept.len());
    kept.iter().enumerate().any(|(i, m)| {
        m.get("role").and_then(|r| r.as_str()) == Some("system")
            && (hoisting_skips(m, i >= first_turn) || m.get("output_config").is_some())
    })
}

/// `content` 里有不是 `text` 的块：`tool_addition` / `tool_removal` 之类只能待在消息里的东西。
pub(in crate::proxy) fn has_non_text_blocks(msg: &serde_json::Value) -> bool {
    msg.get("content").and_then(|c| c.as_array()).is_some_and(|blocks| {
        blocks.iter().any(|b| b.get("type").and_then(|t| t.as_str()) != Some("text"))
    })
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
/// 让上游判。出站会被丢掉的空壳（[`is_empty_system_shell`]）不算数，它们到不了上游。
///
/// 首条 user/assistant 之前的不归这里（[`first_turn_index`]）：上游回的是另一句，由
/// [`find_openai_marker`] 或提升（[`hoist_system_role_messages`]）处理。
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
            // 点名段里第一条会真正送到上游的；整段全是空壳则出站后这段不存在。
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

/// 出站前 [`hoist_system_role_messages`] 会不会跑：`hoist_system_role` 开着、`reject_openai_shape`
/// **关着**、且来访不是 CC 形态。
///
/// 严格检查开着时提升整个不跑：开头那段 system 在入口就被 [`find_openai_marker`] 拒了，能走到
/// 这里的只剩对话中途的，那是上游认的原生 system 消息（带着 `clear_at`、消息级 `output_config`
/// 这类只在原位才有意义的字段，作用范围也从它所在的位置起算），挪到顶层就改了语义。
///
/// 改写（[`rewrite_body_out`]）与入口处的形态记忆豁免（`known_shape_rejection` 的
/// `system_hoisted`）都从这里取，口径不许分叉；后者另外还要叠上 billable——非计费路径
/// （`count_tokens` 等）出站根本不改写。
pub(in crate::proxy) fn hoists_system_role(flags: &store::ForwardFlags, cc_shaped: bool) -> bool {
    flags.hoist_system_role && !flags.reject_openai_shape && !cc_shaped
}

/// 把 `messages` 里 `role:"system"` 的消息提升到顶层 `system` 字段。
///
/// litellm 等第三方客户端采用 OpenAI 格式，把 system 指令放在 `messages` 数组里
/// （`{"role":"system","content":"..."}`）。上游对开头那段恒 400（见 [`first_turn_index`]），
/// 跟在 user 之后的新模型认、老模型不认（`role 'system' is not supported on this model`）。
///
/// **对话中途的也一并提升**：挪到开头会改变它的位置，但那是修补路径本来的取舍——留在原位，
/// 老模型上就是一条修得好却没修的 400，还会被 [`remember_shape_rejection`] 学成规则，
/// 之后同模型带 system 的请求全在本地拒掉。
///
/// **指令式写法不提升**（[`is_system_directive`]）：它的 `content` 是空数组，全部意义在消息级
/// 的 `output_config` 上，提升只搬 content，等于把整条连同客户端中途调的 effort 一起删了。
/// 上游对它「放在任何位置都收」（[`misplaced_system_message`] 那句原话），留在原位即可。
///
/// **带正文又带 `output_config` 的拆成两半**：正文照常提升，原位留一条只有 `output_config` 的
/// 指令（`content: []`）。整条提升会把 effort 一起丢掉；整条留在原位，开头的那种上游必拒。
/// 两半的形态都实测过（2026-10-07，claude-sonnet-5-5）：开头的指令 200，提升出去的正文 200。
/// 别的消息级字段（`clear_at` 之类）不跟着留：只剩它们的空壳上游恒 400，带着它们的指令没实测过。
///
/// **只管一轮的也不提升**（[`is_turn_scoped_system`]）：提升上去就成了永久指令。首条
/// user/assistant 之前的那种除外——那个位置上游本来就不收，只能照旧提升。
///
/// **带非文本块的整条不提升**（[`has_non_text_blocks`]）：顶层 `system` 只收文本块，
/// `tool_addition` / `tool_removal`（工具中途上下线）这类只能待在消息的 `content` 里，搬上去
/// 就是一条必拒的请求。文本与它们混在一条里的也整条留着，拆开会改变它们的相对位置。
///
/// 这一步只在严格检查（`reject_openai_shape`）**关着**时才跑（[`hoists_system_role`]）。严格
/// 检查默认开着，那时开头的 system 在入口就被拒，中途的原样转发，字段本来就不会丢。
///
/// 处理逻辑：
/// 1. 从 `messages` 里找出所有 `role:"system"` 的消息（上面那几种例外除外），按原序收集其 content。
/// 2. 将收集到的 content 块**前置**到顶层 `system`（已有则合并，没有则新建）。
/// 3. 从 `messages` 里移除这些消息；带 `output_config` 的换成原位的那条指令。
///
/// content 的形态：OpenAI 格式通常是纯字符串（`"content":"You are a helpful assistant"`），
/// 也可能是 Anthropic 格式的内容块数组。两种都处理。
pub(in crate::proxy) fn hoist_system_role_messages(v: &mut serde_json::Value) -> bool {
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    let mut hoisted_blocks: Vec<serde_json::Value> = Vec::new();
    let mut indices_to_remove: Vec<usize> = Vec::new();
    // 拆出来留在原位的指令：（下标，那条指令）。
    let mut directives: Vec<(usize, serde_json::Value)> = Vec::new();
    let first_turn = first_turn_index(msgs);
    for (i, msg) in msgs.iter().enumerate() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("system")
            || hoisting_skips(msg, i >= first_turn)
        {
            continue;
        }
        match msg.get("output_config") {
            Some(oc) => directives.push((
                i,
                serde_json::json!({ "role": "system", "output_config": oc, "content": [] }),
            )),
            None => indices_to_remove.push(i),
        }
        match msg.get("content") {
            Some(serde_json::Value::String(s)) => {
                if !s.is_empty() {
                    hoisted_blocks.push(serde_json::json!({"type": "text", "text": s}));
                }
            }
            Some(serde_json::Value::Array(arr)) => {
                hoisted_blocks.extend(arr.iter().cloned());
            }
            _ => {}
        }
    }
    if indices_to_remove.is_empty() && directives.is_empty() {
        return false;
    }
    // 先原位换成指令（下标不变），再移除其余的（倒序，避免索引偏移）。
    let msgs = v.get_mut("messages").and_then(|m| m.as_array_mut()).unwrap();
    for (i, directive) in &directives {
        msgs[*i] = directive.clone();
    }
    for &i in indices_to_remove.iter().rev() {
        msgs.remove(i);
    }
    // 合并到顶层 system：已有的内容追加在 hoisted 之后（system 消息在前、原有 system 在后）。
    if !hoisted_blocks.is_empty() {
        let existing: Vec<serde_json::Value> = match v.get_mut("system").map(|s| s.take()) {
            Some(serde_json::Value::String(s)) => {
                if s.is_empty() {
                    Vec::new()
                } else {
                    vec![serde_json::json!({"type": "text", "text": s})]
                }
            }
            Some(serde_json::Value::Array(arr)) => arr,
            _ => Vec::new(),
        };
        hoisted_blocks.extend(existing);
        v.as_object_mut()
            .unwrap()
            .insert("system".into(), serde_json::Value::Array(hoisted_blocks));
    }
    tracing::info!(
        removed = indices_to_remove.len(),
        split = directives.len(),
        "hoisted role:system messages to top-level system field"
    );
    true
}
