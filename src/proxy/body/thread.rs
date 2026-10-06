//! 模拟路径的消息线程（message threads）：线程决策落到出站体、断点预算与消息指纹。

use super::*;

/// 模拟路径的 message thread（`message-threads-2026-08-12`），规则见
/// [`crate::proxy::session_link::ThreadState`]：
///
/// - 会话里这段对话的第一条，或历史接不上上一轮（客户端改了历史、重新生成、自己裁剪了上下文，
///   换了模型 / effort / system / tools，上一条失败或被取消）：`thread: {type: create}`，完整上下文。
///   `diagnostics` 照旧由 [`ensure_diagnostics`] 写会话上一条回复——与官方切模型后那条 `create`
///   同形（`cap/auto-2.1.285-20260930/00243`）。
/// - 接得上：`thread: {type: continue, previous_message_id}`，`messages` 只留新增的那几条，
///   `system` 只剩 billing header 一块（去掉断点，官方那块只有 `type` / `text`），去掉 `tools`，
///   `diagnostics.previous_message_id` 改成同一个 id（`00033`）。billing header 的 `cc_version`
///   后缀此前已按完整历史算好，与官方「续轮沿用首轮后缀」一致。
///
/// 只给官方会发 `thread` 的主线程：2.1.285 的 opus / sonnet / haiku 各代与 fable-5 都发，
/// **fable-5-1 一条都不发**（auto、非 auto、`-p` 都是，`00383`、`00554`，`cap/2.1.285/00039`）；
/// 2.1.291 起 fable-5-1 也发（`cap/auto-2.1.291-20261006-full/00464`），见
/// [`crate::proxy::simulation::sim_uses_threads`]。来访指定了 `tool_choice` 的只 `create`：续轮不带
/// `tools`，`tool_choice` 没有落脚处。
///
/// 这一轮的结论（等回程提交的那份）挂到 `sim` 上，由 `ReqLog` 取走（[`Simulation::take_thread`]）。
pub(super) fn apply_sim_thread(v: &mut serde_json::Value, sim: &Simulation, cred_id: i64) -> bool {
    if !crate::proxy::simulation::sim_is_main_thread(sim) {
        return false;
    }
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default();
    let threads = crate::proxy::simulation::sim_uses_threads(sim, model);
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else { return false };
    if msgs.is_empty() {
        return false;
    }
    // 指纹在插 `<total_tokens>` 提醒**之前**算：客户端下一轮带回来的历史里没有这条提醒（它只进了
    // 上游线程），把它算进去，下一轮的前缀就永远对不上。
    let fps: Vec<ThreadMsg> = msgs.iter().map(thread_msg_of).collect();
    let regular_prompt = crate::telemetry::last_is_new_prompt_body(v);
    let key = CcSessionKey { cred_id, session_id: &sim.session_id };
    let (mut decision, pending, tokens_left) =
        thread_decision(key, thread_shape_of(v), &fps, regular_prompt);
    let has_tool_choice = v.get("tool_choice").is_some_and(|c| !c.is_null());
    if !threads || (has_tool_choice && matches!(decision, ThreadDecision::Continue { .. })) {
        decision = ThreadDecision::Create;
    }
    let pending = match &decision {
        ThreadDecision::Create => pending.into_create(),
        ThreadDecision::Continue { .. } => pending,
    };
    let Some(obj) = v.as_object_mut() else { return false };
    match decision {
        ThreadDecision::Create if !threads => {}
        ThreadDecision::Create => {
            obj.insert("thread".into(), serde_json::json!({ "type": "create" }));
        }
        ThreadDecision::Continue { from, previous_message_id } => {
            if let Some(serde_json::Value::Array(m)) = obj.get_mut("messages") {
                m.drain(..from);
            }
            let billing = obj
                .get("system")
                .and_then(|s| s.as_array())
                .and_then(|a| a.first())
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                .filter(|t| t.starts_with("x-anthropic-billing-header:"))
                .map(str::to_string);
            match billing {
                Some(text) => {
                    obj.insert(
                        "system".into(),
                        serde_json::json!([{ "type": "text", "text": text }]),
                    );
                }
                None => {
                    obj.remove("system");
                }
            }
            obj.remove("tools");
            obj.insert(
                "thread".into(),
                serde_json::json!({ "type": "continue", "previous_message_id": previous_message_id }),
            );
            obj.insert(
                "diagnostics".into(),
                serde_json::json!({ "previous_message_id": previous_message_id }),
            );
        }
    }
    insert_total_tokens_reminder(
        v,
        tokens_left,
        regular_prompt,
        crate::proxy::simulation::sim_has_beta(sim, config::CC_BETA_MID_CONVERSATION_SYSTEM),
    );
    if threads {
        cap_thread_breakpoints(v);
    }
    align_cc_top_level_order(v, sim.profile.body_key_order);
    sim.set_thread(pending);
    true
}

/// 带 `thread` 时一条请求最多 3 个缓存断点：第 4 个名额留给上游自己标在对话末尾的那个，多了整条
/// 拒（`thread: a maximum of 3 blocks with cache_control may be provided when `thread` is set`）。
/// 官方 `create` 恒为 3 个（基座、其余、末条消息），`continue` 恒为 1 个
/// （`cap/auto-2.1.285-20260930` 里 25 条 `create`、55 条 `continue` 逐条数过）。
const MAX_THREAD_CACHE_BREAKPOINTS: usize = 3;

/// 把断点裁到 [`MAX_THREAD_CACHE_BREAKPOINTS`] 以内。前面那几步按 [`MAX_CACHE_BREAKPOINTS`]
/// 分配预算：来访自带 system 时 [`crate::proxy::simulate_system`] 在第五块（客户端那段）上也标一个，
/// 加上基座、其余与末条消息正好 4 个；来访自己在工具定义、历史消息上标的也会凑满。按「摘了最
/// 不心疼」的顺序摘：
///
/// 1. `tools` 上的：缓存前缀按 tools → system → messages 排，system 上有断点就已经盖住了它；
/// 2. 顶层的（自动缓存）：它标的正是对话末尾，与上游预留的那个重复；
/// 3. `messages` 里除最后一个之外的，从前往后；
/// 4. `system` 里的，**从后往前**：官方只标基座与其余两块，先摘第五块那个；
/// 5. 仍超（只剩末条消息上那个）才摘它。
///
/// 计数与摘除只看 API 认 `cache_control` 的位置（[`cache_slots`]）：工具 schema 里叫
/// `cache_control` 的参数、`tool_use.input` 里的同名字段都是业务数据，算进去会把它们当断点
/// 摘掉，schema 从此 `required` 了一个不存在的属性。
///
/// 返回摘掉了几个。
pub(super) fn cap_thread_breakpoints(v: &mut serde_json::Value) -> usize {
    let slots = cache_slots(v);
    let excess = slots.len().saturating_sub(MAX_THREAD_CACHE_BREAKPOINTS);
    if excess == 0 {
        return 0;
    }
    let of = |pick: fn(&CacheSlot) -> bool| slots.iter().copied().filter(pick);
    let mut msgs: Vec<CacheSlot> = of(|s| matches!(s, CacheSlot::Message(..))).collect();
    let last_msg = msgs.pop();
    let order = of(|s| matches!(s, CacheSlot::Tool(_)))
        .chain(of(|s| matches!(s, CacheSlot::Top)))
        .chain(msgs)
        .chain(of(|s| matches!(s, CacheSlot::System(_))).collect::<Vec<_>>().into_iter().rev())
        .chain(last_msg);
    let doomed: Vec<CacheSlot> = order.take(excess).collect();
    for slot in &doomed {
        if let Some(o) = slot.resolve(v) {
            o.shift_remove("cache_control");
        }
    }
    doomed.len()
}

/// 官方主线程每条请求末尾的 `<total_tokens>N tokens left</total_tokens>` 提醒（数怎么算见
/// [`crate::proxy::session_link::TotalTokens`]），只跟在 user 消息（新输入或工具结果）后面。落法随
/// `mid-conversation-system` beta 分两种：
///
/// - **带那项 beta**（opus / sonnet / fable）：追加一条独立的 `role: system` 消息，末条消息的
///   缓存断点挪到它身上（`cap/auto-2.1.285-20260930/00033`、`00036`、`00405`）；
/// - **不带**（haiku）：写成 `<system-reminder>`——工具续轮拼在最后一个 `tool_result` 正文的
///   末尾（`00412`），新输入则作为一个文本块插在用户那句话前面（`00411`）。
///
/// 官方首轮那条会把它与环境说明、日期并进同一条 system 消息（`00032`、`00349`），那是首轮附件
/// 整体的形态，这里只补单独这一条。
fn insert_total_tokens_reminder(
    v: &mut serde_json::Value,
    tokens_left: u64,
    regular_prompt: bool,
    mid_conversation_system: bool,
) {
    let text = format!("<total_tokens>{tokens_left} tokens left</total_tokens>");
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else { return };
    let Some(last) = msgs.last_mut() else { return };
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return;
    }
    if mid_conversation_system {
        let mut block = serde_json::json!({ "type": "text", "text": text });
        if let Some(cc) = take_last_cache_control(last) {
            block["cache_control"] = cc;
        }
        msgs.push(serde_json::json!({ "role": "system", "content": [block] }));
        return;
    }
    let wrapped = format!("<system-reminder>\n{text}\n</system-reminder>");
    let content = last.get_mut("content");
    let Some(content) = content else { return };
    if let serde_json::Value::String(s) = content {
        let s = std::mem::take(s);
        *content = serde_json::json!([{ "type": "text", "text": s }]);
    }
    let Some(blocks) = content.as_array_mut() else { return };
    let ty = |b: &serde_json::Value| b.get("type").and_then(|t| t.as_str()).map(str::to_string);
    if regular_prompt {
        let at = blocks.iter().rposition(|b| ty(b).as_deref() == Some("text")).unwrap_or(0);
        blocks.insert(at, serde_json::json!({ "type": "text", "text": format!("{wrapped}\n") }));
    } else if let Some(tr) =
        blocks.iter_mut().rev().find(|b| ty(b).as_deref() == Some("tool_result"))
    {
        match tr.get_mut("content") {
            Some(serde_json::Value::String(s)) => {
                s.push_str("\n\n");
                s.push_str(&wrapped);
            }
            Some(serde_json::Value::Array(parts)) => {
                parts.push(serde_json::json!({ "type": "text", "text": wrapped }));
            }
            _ => {
                tr["content"] = serde_json::Value::String(wrapped);
            }
        }
    }
}

/// 摘下一条消息里最后一个缓存断点并交出来（没有就 `None`）。官方的断点在末条消息上，追加一条
/// system 提醒之后末条换成了它，断点跟着挪过去，总数不变。
fn take_last_cache_control(m: &mut serde_json::Value) -> Option<serde_json::Value> {
    let blocks = m.get_mut("content")?.as_array_mut()?;
    blocks.iter_mut().rev().find_map(|b| b.as_object_mut().and_then(|o| o.remove("cache_control")))
}

/// 出站一条消息的线程指纹，见 [`ThreadMsg`]。
pub(in crate::proxy) fn thread_msg_of(m: &serde_json::Value) -> ThreadMsg {
    let assistant = m.get("role").and_then(|r| r.as_str()) == Some("assistant");
    let tool_use_ids = if assistant {
        m.get("content")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
            .filter_map(|b| b.get("id").and_then(|i| i.as_str()).map(str::to_string))
            .collect()
    } else {
        Vec::new()
    };
    // 回复指纹与回程嗅探器同一个算法（[`ReplyFp`]），按块序喂；字符串形态的 content 即一个
    // text 块。只有 assistant 要比，user 留缺省值。
    let mut reply = ReplyFp::default();
    if assistant {
        match m.get("content") {
            Some(serde_json::Value::String(s)) if !s.is_empty() => {
                reply.text(reply_text_fp(REPLY_TEXT_FP_INIT, s), &[]);
            }
            Some(serde_json::Value::Array(blocks)) => {
                for b in blocks {
                    let field = |k: &str| b.get(k).and_then(|t| t.as_str()).unwrap_or_default();
                    match b.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            let citations = b
                                .get("citations")
                                .and_then(|c| c.as_array())
                                .map_or(&[][..], |c| &c[..]);
                            if !field("text").is_empty() || !citations.is_empty() {
                                reply.text(
                                    reply_text_fp(REPLY_TEXT_FP_INIT, field("text")),
                                    citations,
                                );
                            }
                        }
                        Some("tool_use") => reply.tool_use(
                            field("id"),
                            field("name"),
                            b.get("input").unwrap_or(&serde_json::Value::Null),
                        ),
                        Some("thinking") => reply
                            .thinking(false, reply_text_fp(REPLY_TEXT_FP_INIT, field("thinking"))),
                        Some("redacted_thinking") => {
                            reply.thinking(true, reply_text_fp(REPLY_TEXT_FP_INIT, field("data")))
                        }
                        Some("fallback") => {}
                        _ => reply.block(b),
                    }
                }
            }
            _ => {}
        }
    }
    ThreadMsg { fp: message_fingerprint(m), assistant, tool_use_ids, reply }
}

/// 一条消息的线程指纹：去掉 `cache_control`，且字符串形态的 `content` 与等价的单个 `text` 块
/// 同指纹——[`align_message_shape`] 给末条消息补断点时会把字符串改成块数组，同一条消息在上一轮
/// 是末条（块数组）、这一轮不是（仍是字符串），不能因此判成历史变了。
fn message_fingerprint(m: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let Some(obj) = m.as_object() else { return fingerprint_without_cache_control(m) };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (k, x) in obj {
        if k == "cache_control" {
            continue;
        }
        k.hash(&mut h);
        match (k.as_str(), x) {
            ("content", serde_json::Value::String(s)) => hash_without_cache_control(
                &serde_json::json!([{ "type": "text", "text": s }]),
                &mut h,
            ),
            _ => hash_without_cache_control(x, &mut h),
        }
    }
    h.finish()
}

/// 线程形态指纹：模型、system 正文（billing header 那块除外——`cch` 与会话链字段每条都变）、
/// `tools`、`thinking`、`output_config`。全部去掉 `cache_control` 再算，见
/// [`crate::proxy::session_link::ThreadPending`] 的 `shape`。
fn thread_shape_of(v: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.get("model").and_then(|m| m.as_str()).unwrap_or_default().hash(&mut h);
    match v.get("system") {
        Some(serde_json::Value::Array(blocks)) => {
            for b in blocks {
                let billing = b
                    .get("text")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.starts_with("x-anthropic-billing-header:"));
                if !billing {
                    hash_without_cache_control(b, &mut h);
                }
            }
        }
        Some(other) => hash_without_cache_control(other, &mut h),
        None => {}
    }
    for key in ["tools", "thinking", "output_config"] {
        key.hash(&mut h);
        if let Some(x) = v.get(key) {
            hash_without_cache_control(x, &mut h);
        }
    }
    h.finish()
}

fn fingerprint_without_cache_control(v: &serde_json::Value) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    hash_without_cache_control(v, &mut h);
    h.finish()
}

/// 按结构哈希一个 JSON 值，跳过所有 `cache_control` 键：断点每轮都挪到最后一条消息上，同一条
/// 消息这轮有、下轮没有，不能因此判成历史变了。
fn hash_without_cache_control<H: std::hash::Hasher>(v: &serde_json::Value, h: &mut H) {
    use std::hash::Hash;
    match v {
        serde_json::Value::Null => 0u8.hash(h),
        serde_json::Value::Bool(b) => {
            1u8.hash(h);
            b.hash(h);
        }
        serde_json::Value::Number(n) => {
            2u8.hash(h);
            n.to_string().hash(h);
        }
        serde_json::Value::String(s) => {
            3u8.hash(h);
            s.hash(h);
        }
        serde_json::Value::Array(a) => {
            4u8.hash(h);
            a.len().hash(h);
            for x in a {
                hash_without_cache_control(x, h);
            }
        }
        serde_json::Value::Object(o) => {
            5u8.hash(h);
            for (k, x) in o {
                if k == "cache_control" {
                    continue;
                }
                k.hash(h);
                hash_without_cache_control(x, h);
            }
            6u8.hash(h);
        }
    }
}
