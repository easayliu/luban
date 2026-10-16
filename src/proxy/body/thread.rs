//! 模拟路径的消息线程（message threads）：线程决策落到出站体、断点预算与消息指纹。

use super::*;

/// 模拟路径的 message thread（`message-threads-2026-08-12`），规则见
/// [`crate::proxy::ThreadState`]：
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
///
/// `env_note` 带这一轮换了模型时新模型那行（[`EnvNoted::model_notice`]）、更早一轮换过模型要在
/// 历史原位重现的那条（[`EnvNoted::historical_switch`]），以及这一轮的落位（`mid_conv_sys`）。
/// 落位按**当前**模型自带的 beta——跨模型族时切回不支持中途 system 消息的旧模型，
/// 按这一轮的 beta 自然就落成提醒块，不会给上游发它拒绝的 `role: system` 消息。
///
/// `raw_fps` 是**插环境说明、挪客户端 system 之前**的 `messages` 指纹，`raw_sys_shape` 是同一时刻
/// system 正文的指纹（[`thread_system_shape`]；见 [`rewrite_body_out`] 里采样）。线程前缀匹配要用
/// 这份，形态随模型漂来漂去、客户端 system 每轮都变也不打断上一轮的谱系。形态哈希里的其余几样
/// （模型、`tools`、`thinking`、`output_config`）按**最终出站**体算（[`thread_shape_of`]）：工具是
/// 注入之后才定下来的，提前采就看不见「这一轮不再注工具」这类变化，会错发 `continue` 让上游沿用
/// 首轮的工具。续轮（`continue`）上游线程里仍留着
/// 历史切换说明，这里不重补；`create` 把完整历史重发，得补上。模型说明与 `<total_tokens>`
/// 同在指纹算完之后才补。
pub(super) fn apply_sim_thread(
    v: &mut serde_json::Value,
    sim: &Simulation,
    cred_id: i64,
    env_note: &EnvNoted,
    raw_fps: &[ThreadMsg],
    raw_sys_shape: u64,
) -> bool {
    if !crate::proxy::simulation::sim_is_main_thread(sim) {
        return false;
    }
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default();
    let threads = crate::proxy::simulation::sim_uses_threads(sim, model);
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else { return false };
    if msgs.is_empty() || raw_fps.is_empty() {
        return false;
    }
    // 指纹用插环境说明之前的那份（`raw_fps`，调用方算好）：当前这轮的消息、历史里存的那条
    // 都按客户端真正发出的 `messages` 算，环境说明与历史切换说明进不去指纹。换了模型族形态
    // 跟着变也不会打断前缀匹配，`<total_tokens>` 倒数也接着上一轮。
    let fps = raw_fps;
    let regular_prompt = crate::telemetry::last_is_new_prompt_body(v);
    let key = CcSessionKey { cred_id, session_id: &sim.session_id };
    let shape = thread_shape_of(v, raw_sys_shape);
    let (mut decision, pending, tokens_left) = thread_decision(key, shape, fps, regular_prompt);
    let has_tool_choice = v.get("tool_choice").is_some_and(|c| !c.is_null());
    if !threads || (has_tool_choice && matches!(decision, ThreadDecision::Continue { .. })) {
        decision = ThreadDecision::Create;
    }
    let mut pending = match &decision {
        ThreadDecision::Create => pending.into_create(),
        ThreadDecision::Continue { .. } => pending,
    };
    let is_create = matches!(decision, ThreadDecision::Create);
    let Some(obj) = v.as_object_mut() else { return false };
    match decision {
        ThreadDecision::Create if !threads => {}
        ThreadDecision::Create => {
            obj.insert("thread".into(), serde_json::json!({ "type": "create" }));
        }
        ThreadDecision::Continue { from, previous_message_id } => {
            // `from` 是按 raw 指纹数的（见 `raw_fps` 的注释）；出站 `messages` 比它多了环境说明
            // 塞进去的那条（`mid_conv_sys` 分支）/ 不多（haiku 分支），drain 要加这个偏移。
            // 续轮不走 `insert_historical_switch`（上游线程里仍留着那条），不必再算它。
            let drop = from + env_note.added_msgs;
            if let Some(serde_json::Value::Array(m)) = obj.get_mut("messages") {
                m.drain(..drop.min(m.len()));
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
    // 续轮上游线程里仍留着历史里那几条换模型说明（它们已经作为前几轮的回复附近记录），这一轮
    // 的完整上下文只有新增那几条消息，不必补。`create` 把完整历史整发一遍，缺的那几条就得补回去。
    // 历史各轮的 `<total_tokens>` 同理：上游线程里有，`create` 重发完整历史就得原位补回。带 beta
    // 时换模型那一轮两者并在同一条 system 里（模型说明在前），交给 [`replay_history_notes`] 一并
    // 重现；不带时两者各自写进用户消息、互不相干，先补模型说明。
    let placed = !is_create || {
        let switch = env_note.historical_switch.as_ref();
        let reminders = pending.history_reminders();
        if env_note.mid_conv_sys {
            replay_history_notes(v, fps, reminders, true, switch)
                .unwrap_or_else(|| insert_historical_switch(v, switch, true))
        } else {
            let placed = insert_historical_switch(v, switch, false);
            replay_history_notes(v, fps, reminders, false, None);
            placed
        }
    };
    let model_notice = env_note.model_notice_after(placed);
    let mcs = env_note.mid_conv_sys;
    let model_notice = model_notice.as_deref();
    if let Some(notice) = model_notice.filter(|_| !mcs) {
        place_model_notice(v, notice, false, regular_prompt);
    }
    let msgs = v.get_mut("messages").and_then(|m| m.as_array_mut());
    if let Some(msgs) = msgs
        && insert_total_tokens_reminder(
            msgs,
            tokens_left,
            regular_prompt,
            mcs,
            model_notice.filter(|_| mcs),
        )
    {
        pending.note_reminder(TurnReminder { turn_len: fps.len(), tokens_left, regular_prompt });
    }
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
/// 能不能写中途 system 消息分两种（`mid_conv_sys`：带 `mid-conversation-system`，或 2.1.299 起只剩的
/// `-clear-at`，见 [`crate::proxy::simulation::sim_has_mid_conversation_system`]）：
///
/// - **能**（opus / sonnet / fable / haiku-5-5）：追加一条独立的 `role: system` 消息，末条消息的
///   缓存断点挪到它身上（`cap/auto-2.1.285-20260930/00033`、`00036`、`00405`）；
/// - **不能**（haiku-4.5 与老一代）：写成 `<system-reminder>`——工具续轮拼在最后一个 `tool_result` 正文的
///   末尾（`00412`），新输入则作为一个文本块插在用户那句话前面（`00411`）。
///
/// 官方首轮那条会把它与环境说明、日期并进同一条 system 消息（`00032`、`00349`），那一份由
/// [`insert_env_note`] 补：带 beta 的首轮末条是那条 system 消息、这里不再追加；haiku 首条用户消息里
/// 已有一块 `<total_tokens>` 提醒，这里同样跳过。
///
/// 返回这一轮是否写进了一条 `<total_tokens>`（只补了模型说明、或整个跳过的都是 `false`）：写了的
/// 才记进线程状态，`create` 时由 `replay_total_tokens_reminders` 在历史原位重现。
fn insert_total_tokens_reminder(
    msgs: &mut Vec<serde_json::Value>,
    tokens_left: u64,
    regular_prompt: bool,
    mid_conversation_system: bool,
    model_notice: Option<&str>,
) -> bool {
    let text = format!("<total_tokens>{tokens_left} tokens left</total_tokens>");
    // 带 beta：落在末尾那段指令与临时 system（`clear_at`）之前，它们原样留在最后——官方的
    // `<total_tokens>` 也排在 `clear_at` 那条前面、断点在它身上（`cap/auto-2.1.291-20261006-full/00465`），
    // 见 [`sticky_tail_start`]。那之前一条是 system（来访连发几条 user，环境说明或历史换模型说明
    // 只能落在末尾；或来访自己以普通 system 收尾）就并进去，环境说明里本来就有 `<total_tokens>`，
    // 只补模型说明；是 user 就另起一条，断点从它身上挪过来。
    if mid_conversation_system {
        let at = sticky_tail_start(msgs);
        let Some(prev) = at.checked_sub(1).map(|i| &mut msgs[i]) else { return false };
        let with_notice = |t: String| match model_notice {
            Some(notice) => format!("{notice}\n\n{t}"),
            None => t,
        };
        return match prev.get("role").and_then(|r| r.as_str()) {
            Some("system") => {
                // 并进环境说明的只有模型说明，`<total_tokens>` 环境说明里本来就有。
                let env = is_env_note_msg(prev);
                let text = match (env, model_notice) {
                    (true, Some(notice)) => notice.to_string(),
                    (true, None) => return false,
                    (false, _) => with_notice(text),
                };
                if !append_to_system(prev, &text) {
                    msgs.insert(at, system_text_msg(&text));
                }
                !env
            }
            Some("user") => {
                let mut block = serde_json::json!({ "type": "text", "text": with_notice(text) });
                if let Some(cc) = take_last_cache_control(prev) {
                    block["cache_control"] = cc;
                }
                msgs.insert(at, serde_json::json!({ "role": "system", "content": [block] }));
                true
            }
            _ => false,
        };
    }
    let Some(last) = msgs.last_mut() else { return false };
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    let wrapped = format!("<system-reminder>\n{text}\n</system-reminder>");
    // 首轮那份环境说明（[`insert_env_note`]）已经把这条并进了首条用户消息，不再补第二条。
    let has_one = last.get("content").and_then(|c| c.as_array()).is_some_and(|blocks| {
        blocks.iter().any(|b| {
            b.get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.starts_with("<system-reminder>\n<total_tokens>"))
        })
    });
    if has_one {
        return false;
    }
    let content = last.get_mut("content");
    let Some(content) = content else { return false };
    if let serde_json::Value::String(s) = content {
        let s = std::mem::take(s);
        *content = serde_json::json!([{ "type": "text", "text": s }]);
    }
    let Some(blocks) = content.as_array_mut() else { return false };
    let ty = |b: &serde_json::Value| b.get("type").and_then(|t| t.as_str()).map(str::to_string);
    if regular_prompt {
        let at = blocks.iter().rposition(|b| ty(b).as_deref() == Some("text")).unwrap_or(0);
        blocks.insert(at, serde_json::json!({ "type": "text", "text": format!("{wrapped}\n") }));
        return true;
    }
    if let Some(tr) = blocks.iter_mut().rev().find(|b| ty(b).as_deref() == Some("tool_result")) {
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
        return true;
    }
    false
}

/// `create` 重发完整历史时，把历史各轮补过的 `<total_tokens>` 提醒（[`TurnReminder`]）放回原位：
/// 每一条都照当时的样子、在那一轮末尾（它的回复之前）重跑一遍 [`insert_total_tokens_reminder`]。
/// 上游线程里每轮都有这么一条，官方线程过期后的 `create` 也原样带着
/// （`cap/auto-2.1.296-20261012-idle/00045`）；漏了，前缀从第一条提醒处就和缓存对不上。
///
/// 落法（`mid_conv_sys`）按这一轮的模型，见 [`TurnReminder`]。
///
/// `switch` 是历史里要重现的那条换模型说明（[`EnvNoted::historical_switch`]，只在带 beta 时传）：
/// 换模型那一轮当时是把说明与提醒拼成「说明\n\n提醒」一并写的（[`apply_sim_thread`]），这里照样
/// 一并写；那一轮没补提醒就在原位单独插（[`insert_historical_switch_in`]）。不能先插说明再补提醒：
/// 来访那一轮以自己的 `role: system` 收尾时说明会并进那一条，改了它的指纹就对不齐了，而且两样的
/// 先后会反过来。返回说明放下了没有（同 [`insert_historical_switch`]）；没跑（没有要补的提醒、对不齐）
/// 时返回 `None`，由调用方照旧单独补说明。
///
/// `raw` 是插环境说明之前的来访历史指纹（[`apply_sim_thread`] 的 `raw_fps`），拿来把「那一轮
/// 来访第几条」换算成出站下标：luban 自己插的只有 system（环境说明、模型说明），指纹对不上又是
/// system 的跳过；别的对不上的是被就地改过的（haiku 的提醒块塞进了用户正文），角色得对得上才
/// 算同一条。对不齐就整段不补，退回原来的形态。
fn replay_history_notes(
    v: &mut serde_json::Value,
    raw: &[ThreadMsg],
    reminders: &[TurnReminder],
    mid_conv_sys: bool,
    switch: Option<&(usize, String)>,
) -> Option<bool> {
    if reminders.is_empty() {
        return None;
    }
    let msgs = v.get_mut("messages").and_then(|m| m.as_array_mut())?;
    let mut at = Vec::with_capacity(raw.len());
    for (k, m) in msgs.iter().enumerate() {
        let Some(r) = raw.get(at.len()) else { break };
        let role = m.get("role").and_then(|r| r.as_str());
        if message_fingerprint(m) == r.fp {
            at.push(k);
        } else if role != Some("system") {
            if (role == Some("assistant")) != r.assistant {
                return None;
            }
            at.push(k);
        }
    }
    if at.len() != raw.len() {
        return None;
    }
    let is_assistant =
        |m: &serde_json::Value| m.get("role").and_then(|r| r.as_str()) == Some("assistant");
    let mut switch = switch;
    let mut placed = true;
    // 从后往前补，前面的下标不受后面插入的影响。那一轮之后的第一条应是它的回复，不是就跳过。
    for r in reminders.iter().rev() {
        let Some(&end) = at.get(r.turn_len).filter(|_| raw[r.turn_len].assistant) else {
            continue;
        };
        // 换模型说明落在这一轮之后（那一轮没补提醒）：轮到它时就地单独插，再往前的插入不影响它。
        if let Some(sw) = switch.filter(|(u, _)| *u >= end) {
            placed = insert_historical_switch_in(msgs, Some(sw), mid_conv_sys);
            switch = None;
        }
        // 换模型那一轮就是这一轮（说明之后到这一轮回复之间没有别的回复）：与提醒拼在一起写。
        let notice = match switch {
            Some((u, line)) if msgs[*u + 1..end].iter().all(|m| !is_assistant(m)) => {
                switch = None;
                Some(line.as_str())
            }
            _ => None,
        };
        let tail = msgs.split_off(end);
        insert_total_tokens_reminder(msgs, r.tokens_left, r.regular_prompt, mid_conv_sys, notice);
        msgs.extend(tail);
    }
    if let Some(sw) = switch {
        placed = insert_historical_switch_in(msgs, Some(sw), mid_conv_sys);
    }
    Some(placed)
}

/// 摘下一条消息里最后一个缓存断点并交出来（没有就 `None`）。官方的断点在末条消息上，追加一条
/// system 提醒之后末条换成了它，断点跟着挪过去，总数不变。
fn take_last_cache_control(m: &mut serde_json::Value) -> Option<serde_json::Value> {
    let blocks = m.get_mut("content")?.as_array_mut()?;
    blocks.iter_mut().rev().find_map(|b| b.as_object_mut().and_then(|o| o.remove("cache_control")))
}

/// 插环境说明之前那份 `messages` 的线程指纹（[`apply_sim_thread`] 的 `raw_fps`）。
///
/// 环境说明不进指纹，但 [`rewrite_body_out`] 后面对历史消息本身做的改动要先在快照上做一遍，与
/// 最终出站一致——回复指纹记的是上游看到的那份：目前只有工具名混淆（[`apply_tool_names`]），
/// 它只看块本身，与中间别的步骤无关，在快照上先做就与出站一致。
pub(super) fn thread_snapshot(
    messages: &serde_json::Value,
    tool_names: Option<&ToolNameMap>,
) -> Vec<ThreadMsg> {
    let mut snap = serde_json::json!({ "messages": messages });
    if let Some(map) = tool_names {
        apply_tool_names(&mut snap, map);
    }
    snap["messages"].as_array().map_or_else(Vec::new, |m| m.iter().map(thread_msg_of).collect())
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

/// 把「客户端 system 挪没挪进首条用户消息」并进 [`thread_system_shape`] 的结果：挪位开关中途切换时
/// 出站落点变了，上游线程里还是旧落点，形态不同就得重新 `create`。没在算线程（`shape == 0`）
/// 时原样返回。
pub(super) fn thread_shape_with_relocation(shape: u64, relocated: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    if shape == 0 {
        return shape;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    shape.hash(&mut h);
    relocated.hash(&mut h);
    h.finish()
}

/// 线程形态指纹：模型、system 正文（`sys_shape`，由 [`thread_system_shape`] 在挪客户端 system
/// 之前算好）、`tools`、`thinking`、`output_config`。后三样按传入的（最终出站）体算，全部去掉
/// `cache_control`，见 [`crate::proxy::session_link::ThreadPending`] 的 `shape`。
fn thread_shape_of(v: &serde_json::Value, sys_shape: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.get("model").and_then(|m| m.as_str()).unwrap_or_default().hash(&mut h);
    sys_shape.hash(&mut h);
    for key in ["tools", "thinking", "output_config"] {
        key.hash(&mut h);
        if let Some(x) = v.get(key) {
            hash_without_cache_control(x, &mut h);
        }
    }
    h.finish()
}

/// 线程形态指纹里 system 正文那一份：billing header 那块除外（`cch` 与会话链字段每条都变），
/// 去掉 `cache_control` 再算。要在挪客户端 system（[`relocate_long_client_system`]）**之前**采：
/// 挪位之后末块只剩占位，再算就看不见 system 变没变。
pub(super) fn thread_system_shape(v: &serde_json::Value) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
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
    h.finish()
}

fn fingerprint_without_cache_control(v: &serde_json::Value) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    hash_without_cache_control(v, &mut h);
    h.finish()
}

/// 按结构哈希一个 JSON 值，跳过 `cache_control` 键：断点每轮都挪到最后一条消息上，同一条
/// 消息这轮有、下轮没有，不能因此判成历史变了。
///
/// 只跳过 API 认断点的那几层（消息、内容块、工具定义，见 [`cache_slots`]）。`tool_use.input`
/// 与工具的 `input_schema` 里叫 `cache_control` 的是业务数据：客户端改了历史里某次调用的这个
/// 参数，跳过它就认不出历史变了，续轮把改过的那条切掉，上游接着用旧的。进了这两个键就整棵
/// 原样哈希。
fn hash_without_cache_control<H: std::hash::Hasher>(v: &serde_json::Value, h: &mut H) {
    hash_json(v, h, true);
}

fn hash_json<H: std::hash::Hasher>(v: &serde_json::Value, h: &mut H, strip: bool) {
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
                hash_json(x, h, strip);
            }
        }
        serde_json::Value::Object(o) => {
            5u8.hash(h);
            for (k, x) in o {
                if strip && k == "cache_control" {
                    continue;
                }
                k.hash(h);
                hash_json(x, h, strip && !matches!(k.as_str(), "input" | "input_schema"));
            }
            6u8.hash(h);
        }
    }
}
