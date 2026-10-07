//! 模拟路径首轮的环境说明（`# Environment` 那条）：官方每段对话开头都带，第三方来访没有。

use super::*;

/// 给模拟的主线程补上官方首轮那份环境说明：工作目录与平台、模型与知识截止、Agent 类型、技能
/// 清单、`<total_tokens>` 与日期。2.1.277 起这些都不在 `system` 里了，官方在会话首轮把它们作为
/// 首条用户消息的附件发出，之后每轮作为历史原样带着（`cap/auto-2.1.291-20261006-full/00217`）。
/// 模拟路径的来访每轮都发完整历史、却从来没有这一段，于是每轮都在同一个位置补同一份：
///
/// - **带 `mid-conversation-system`**（opus / sonnet / fable 新几代）：首条用户消息之后插一条
///   `role: system` 消息，各段空一行拼成一个文本块（`00340`、`00253`、`00464`）。首轮它就是末条，
///   末条断点随后由 [`align_message_shape`] 落在它身上，与官方同位；
/// - **不带**（haiku 与老一代）：每段各裹一个 `<system-reminder>` 块插到首条用户消息最前面，
///   日期那块单独放在用户正文之前、客户端 system 挪进来的那几块之后（`00303`、`00553`）。
///
/// 内容只写模拟路径确实注入的东西：不注 ToolSearch 与延迟池，就没有「deferred tools」那段；
/// 没有 MCP 服务器，就没有 MCP 说明；技能只列内建的（[`config::CC_ENV_SKILLS`]）；精简工具时
/// 去掉 scratchpad 一行与 artifact 三个技能，与官方加那三个环境变量后的形态相同。
///
/// 每轮逐字节相同才不会打断缓存与 message thread 的前缀比对：环境取 [`Simulation::env`]（按账号 +
/// 设备派生或来访自己写的，恒定）；`<total_tokens>` 是首轮那个数——首轮必是一次新输入，倒数从
/// 预算整数起；日期、模型行、Agent 那段用哪一版、精简与否都按对话钉在首轮（[`pin_env`]）。
///
/// **中途换模型不改这一份**：官方历史里那条仍写首轮的模型，换模型那一轮另起一条「You are
/// powered by …」（`cap/auto-2.1.285-20260930/00243`、`00411`）。这里同样只在那一轮交出新模型的
/// 那行（[`EnvNoted::model_notice`]），落法见 [`place_model_notice`] 与 [`apply_sim_thread`]。改写了
/// 历史，message thread 与 `<total_tokens>` 倒数按前缀找上一轮就找不到，倒数会被重置。
///
/// 它在 `messages` 里，**不跟** system 第四块的开关 `simulate_full_system`：模拟主线程都补。唯一的
/// 前提是这条请求会注入官方工具——Agent 类型与技能清单说的是 `Agent` / `Skill` 两个工具，来访一个工具都
/// 没声明、[`inject_cc_tools`] 又不补的（开关 `fill_absent_tools` 关着，或强制调某个工具）就整段不补，
/// 判据与它逐条相同（[`injects_cc_tools`]）。
pub(in crate::proxy) fn insert_env_note(
    v: &mut serde_json::Value,
    sim: &Simulation,
    cred_id: i64,
) -> EnvNoted {
    let none = EnvNoted::default();
    if !crate::proxy::simulation::sim_is_main_thread(sim) {
        return none;
    }
    let Some(env) = sim.env.as_ref() else { return none };
    if !injects_cc_tools(v, sim.fill_absent_tools) {
        return none;
    }
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default();
    let Some(model_line) = env_model_line(model, sim.context_1m) else { return none };
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else { return none };
    // 首条**用户**消息：前面只允许夹指令式 system（[`is_system_directive`]，提升不动它、原样
    // 留在开头）；别的东西排在它前面就不是官方那种对话开头，不补。
    let Some(fu) = msgs.iter().position(|m| !is_system_directive(m)) else { return none };
    let first = &msgs[fu];
    if first.get("role").and_then(|r| r.as_str()) != Some("user") || has_env_note(msgs) {
        return none;
    }
    let mid_conv_sys =
        crate::proxy::simulation::sim_has_beta(sim, config::CC_BETA_MID_CONVERSATION_SYSTEM);
    // 换模型记在末条**用户**消息上：来访以 system（指令之类）收尾时末条不是那句话。
    let last_fp = msgs
        .iter()
        .rfind(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .map_or(0, |m| thread_msg_of(m).fp);
    let pin = pin_env(
        PinKey { cred_id, session_id: &sim.session_id, root: thread_msg_of(first).fp },
        PinFirst {
            model_line,
            haiku: sim.profile.kind == config::CcProfileKind::MainHaiku,
            trim: sim.trim_tools,
        },
        PinTurn {
            len: msgs.len(),
            opening: msgs[fu + 1..]
                .iter()
                .all(|m| m.get("role").and_then(|r| r.as_str()) == Some("system")),
            last_fp,
        },
    );
    let first = pin.first;
    let (model_notice, mut historical_switch) = match pin.switch {
        Some(sw) => locate_switch(msgs, sw),
        None => (None, None),
    };
    let sections = [
        env_head(env, &sim.session_id, first.trim),
        first.model_line,
        (if first.haiku { config::CC_ENV_AGENTS_HAIKU } else { config::CC_ENV_AGENTS }).to_string(),
        env_skills(first.trim).to_string(),
        format!(
            "<total_tokens>{} tokens left</total_tokens>",
            crate::proxy::session_link::TOTAL_TOKENS_BUDGET
        ),
    ];
    let date = pin.date;
    let mut noted = EnvNoted {
        inserted: true,
        model_notice,
        historical_switch: None,
        mid_conv_sys,
        added_msgs: 0,
    };
    // 落位按**这一轮**模型自带的 beta，而非首轮那份：切回不支持 `mid-conversation-system` 的旧模型
    // （opus 5.5 → opus 4.6）后，照首轮形态补一条 `role: system` 下去上游会 400。历史指纹跨模型族
    // 不漂的那层保证挪到 [`apply_sim_thread`]：fps 用 insert 之前的 `msgs` 算（见 `raw_fps`），
    // 环境说明换形态不影响前缀匹配。
    //
    // 位置也不能一股脑地插在 msgs[1]：末块到 `role: system` 之间必须紧跟一个 assistant，否则上游
    // 的那条「system must be before an assistant or at the end of the array」直接拒。
    // [`system_insert_pos`] 从首条用户之后找到第一个合法落点，没有就落在数组末尾。
    if mid_conv_sys {
        let text = format!("{}\n\nToday's date is {date}.", sections.join("\n\n"));
        let at = system_insert_pos(msgs, fu + 1);
        msgs.insert(
            at,
            serde_json::json!({ "role": "system", "content": [{ "type": "text", "text": text }] }),
        );
        noted.added_msgs = 1;
        // 换模型那条用户消息排在环境说明之后的，下标跟着后移一位。
        if let Some((idx, _)) = historical_switch.as_mut()
            && *idx >= at
        {
            *idx += 1;
        }
        noted.historical_switch = historical_switch;
        return noted;
    }
    noted.historical_switch = historical_switch;
    let Some(content) = msgs[fu].get_mut("content") else { return none };
    if let serde_json::Value::String(s) = content {
        let s = std::mem::take(s);
        *content = serde_json::json!([{ "type": "text", "text": s }]);
    }
    let Some(blocks) = content.as_array_mut() else { return none };
    blocks.splice(0..0, sections.iter().map(|s| reminder_block(s)));
    let at = blocks.iter().position(|b| !is_reminder_block(b)).unwrap_or(blocks.len());
    blocks.insert(
        at,
        text_block_bare(&format!(
            "<system-reminder>\nToday's date is {date}.\n</system-reminder>\n"
        )),
    );
    noted
}

/// [`insert_env_note`] 的结果。
#[derive(Debug, Default)]
pub(in crate::proxy) struct EnvNoted {
    /// 补了那份环境说明。
    pub(in crate::proxy) inserted: bool,
    /// 这一轮换了模型时新模型那行「You are powered by …」，否则 `None`。开着 message thread 时与这一轮
    /// 的 `<total_tokens>` 并在一起（[`apply_sim_thread`]），关着时当场补（[`place_model_notice`]）。
    pub(in crate::proxy) model_notice: Option<String>,
    /// 更早的某一轮已经换过模型、这一轮不是：那条模型说明得在历史里的原位重现，不然这段请求
    /// 把假成那一轮没换过。`(触发换模型那条用户消息在出站 `messages` 里的下标, 新模型那行)`，
    /// 下标已按指纹核对过、也已算上环境说明插进去的那条（[`locate_switch`]）。续轮
    /// （[`ThreadDecision::Continue`]）时上游线程里仍留着那条，重发只有新增消息，不用重补；
    /// `create` 与关着线程时把完整历史重发，就得补上。见 [`insert_historical_switch`]。
    pub(in crate::proxy) historical_switch: Option<(usize, String)>,
    /// 这一轮环境说明的落位：当前模型带 `mid-conversation-system` beta（opus / sonnet / fable）
    /// 时为 `true`——每条模型说明都作为独立 `role: system` 消息补；不带（haiku、不支持这项的旧
    /// 模型）时为 `false`——写成用户消息里的 `<system-reminder>` 块。按**本轮**模型取值，模型族
    /// 之间切换时跟着切——不然切回不支持这项的旧模型会被上游 400 拒。
    pub(in crate::proxy) mid_conv_sys: bool,
    /// 环境说明给 `messages` 加了多少条新 msg（0 或 1）：`mid_conv_sys` 分支补了一条 `role: system`
    /// 消息、算 1；haiku 分支改的是首条用户消息的 `content`、算 0。message thread 的 `continue` 分支
    /// 按 raw 指纹比对出要丢的消息条数，再加上这个偏移才是出站 `messages` 里真正要 `drain` 的长度。
    pub(in crate::proxy) added_msgs: usize,
}

impl EnvNoted {
    /// 这一轮要在末尾补的模型说明：本来就是换模型那一轮的那条；历史里那条没能在原位重现
    /// （`placed` 为假，见 [`insert_historical_switch`]）时改补它，好让模型仍看到当前身份。
    pub(in crate::proxy) fn model_notice_after(&self, placed: bool) -> Option<String> {
        match &self.historical_switch {
            Some((_, line)) if !placed => Some(line.clone()),
            _ => self.model_notice.clone(),
        }
    }
}

/// 把换模型那一轮的模型说明补到末条用户消息上，`mid_conversation_system` 选落法：
///
/// - 带那项 beta：末尾追加一条 `role: system`（`cap/auto-2.1.285-20260930/00243`；开着 message thread
///   时它与 `<total_tokens>` 并在一条里，不走这里）；
/// - 不带（haiku 等）：裹成 `<system-reminder>`，新输入插在那条消息最前面（`00411`）；工具结果
///   那种放在末尾——`tool_result` 块得排在最前面，上游才认得它接的是哪次调用。
///
/// 带 beta 时落在末尾那段指令与临时 system（[`sticky_tail_start`]）之前：那几条原样留在最后。
/// 它们前一条是普通 system（来访连发几条 user、环境说明只能落在末尾，或来访自己以 system 收尾）
/// 就并进那一条，是 user 就另起一条，是 assistant 不补、返回 `false`。
pub(in crate::proxy) fn place_model_notice(
    v: &mut serde_json::Value,
    notice: &str,
    mid_conversation_system: bool,
    regular_prompt: bool,
) -> bool {
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else { return false };
    if mid_conversation_system {
        let at = sticky_tail_start(msgs);
        let Some(prev) = at.checked_sub(1).map(|i| &mut msgs[i]) else { return false };
        let role = prev.get("role").and_then(|r| r.as_str()).map(str::to_owned);
        match role.as_deref() {
            Some("system") if append_to_system(prev, notice) => {}
            Some("system" | "user") => msgs.insert(at, system_text_msg(notice)),
            _ => return false,
        }
        return true;
    }
    let Some(last) = msgs.last_mut() else { return false };
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    let Some(content) = last.get_mut("content") else { return false };
    if let serde_json::Value::String(s) = content {
        let s = std::mem::take(s);
        *content = serde_json::json!([{ "type": "text", "text": s }]);
    }
    let Some(blocks) = content.as_array_mut() else { return false };
    if regular_prompt {
        blocks.insert(0, reminder_block(notice));
    } else {
        blocks.push(reminder_block(notice));
    }
    true
}

/// `m` 是 `role: system` 时把 `text` 并进去，返回 `true`：字符串形态接在末尾、块数组接在最后一个
/// 文本块末尾（都空一行），没有文本块就追加一块。官方首轮也是把环境说明、模型与 `<total_tokens>`
/// 并在同一条 system 里。
///
/// 不是 system、或是这两种时不动，返回 `false`：
/// - 指令式写法（`content: []` 带 `output_config`，[`is_system_directive`]）：往指令里塞文字就
///   不再是那条指令了，上游对它「放在任何位置都收」的豁免也跟着没了；
/// - 只管一轮的（`clear_at`，[`is_turn_scoped_system`]）：并进去的也只活一轮，下一轮模型就看
///   不到了。官方的 `<total_tokens>` 也是单独一条，排在它前面（`cap/auto-2.1.291-20261006-full/00465`）。
pub(super) fn append_to_system(m: &mut serde_json::Value, text: &str) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("system")
        || is_system_directive(m)
        || is_turn_scoped_system(m)
    {
        return false;
    }
    match m.get_mut("content") {
        Some(serde_json::Value::String(s)) => {
            s.push_str("\n\n");
            s.push_str(text);
        }
        Some(serde_json::Value::Array(blocks)) => {
            match blocks.iter_mut().rev().find_map(|b| match b.get_mut("text") {
                Some(serde_json::Value::String(s)) => Some(s),
                _ => None,
            }) {
                Some(s) => {
                    s.push_str("\n\n");
                    s.push_str(text);
                }
                None => blocks.push(serde_json::json!({ "type": "text", "text": text })),
            }
        }
        _ => return false,
    }
    true
}

/// 末尾那段「原样留在最后」的 system 从哪儿开始：指令（[`is_system_directive`]）与只管一轮的
/// （[`is_turn_scoped_system`]）。luban 补在末尾的东西都落在这段之前——并不进它们，也不该压到
/// 它们后面：官方的 `<total_tokens>` 就排在 `clear_at` 那条前面（`cap/auto-2.1.291-20261006-full/00465`）。
/// 一段连着的 system 之后是数组末尾，位置合法。没有这样的尾巴时是 `msgs.len()`。
pub(super) fn sticky_tail_start(msgs: &[serde_json::Value]) -> usize {
    msgs.iter()
        .rposition(|m| !is_system_directive(m) && !is_turn_scoped_system(m))
        .map_or(0, |i| i + 1)
}

/// 一条只有一个文本块的 `role: system` 消息。
pub(super) fn system_text_msg(text: &str) -> serde_json::Value {
    serde_json::json!({ "role": "system", "content": [{ "type": "text", "text": text }] })
}

/// 这条消息是 [`insert_env_note`] 补的那条 `role: system` 环境说明。
pub(super) fn is_env_note_msg(m: &serde_json::Value) -> bool {
    m.get("role").and_then(|r| r.as_str()) == Some("system")
        && m.get("content")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.starts_with(ENV_NOTE_HEAD))
}

const ENV_NOTE_HEAD: &str = "# Environment\nYou have been invoked";

fn reminder_block(s: &str) -> serde_json::Value {
    text_block_bare(&format!("<system-reminder>\n{s}\n</system-reminder>"))
}

/// 这段历史里已经有一份环境说明（同一条请求改写了两遍之类）：某条 `role: system` 环境说明
/// （首条之后第一个合法落点，不一定是 msgs[1]），或首条用户消息开头那个提醒块。
fn has_env_note(msgs: &[serde_json::Value]) -> bool {
    let reminder = msgs
        .iter()
        .find(|m| !is_system_directive(m))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str());
    msgs.iter().any(is_env_note_msg)
        || reminder.is_some_and(|t| {
            t.strip_prefix("<system-reminder>\n").is_some_and(|t| t.starts_with(ENV_NOTE_HEAD))
        })
}

/// `<system-reminder>` 开头的文本块：harness 塞在用户正文前面的那几块。
fn is_reminder_block(b: &serde_json::Value) -> bool {
    b.get("type").and_then(|t| t.as_str()) == Some("text")
        && b.get("text")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.starts_with("<system-reminder>"))
}

/// 环境说明开头那段（[`config::CC_ENV_HEAD`]）填好占位。scratchpad 目录是官方的
/// `/private/tmp/claude-<uid>/<项目段>/<会话 id>/scratchpad`（`cap/auto-2.1.291-20261006/00031`：
/// 项目段同记忆目录那个、会话 id 与 `X-Claude-Code-Session-Id` 同值），macOS 首个用户 uid 是 501。
fn env_head(env: &SimEnv, session_id: &str, trim_tools: bool) -> String {
    let scratchpad = if trim_tools {
        String::new()
    } else {
        format!(
            " - Scratchpad directory: /private/tmp/claude-501/{}/{session_id}/scratchpad — always \
             use it for temporary files (intermediate results, scripts, outputs that don't belong \
             in the project) instead of `/tmp` or other system temp directories; it is \
             session-specific, isolated from the project, and can generally be used without \
             permission prompts. Only use `/tmp` if the user explicitly asks.\n",
            env.slug
        )
    };
    config::CC_ENV_HEAD
        .replace("{{cwd}}", &env.cwd)
        .replace("{{os_release}}", config::CC_OS_RELEASE)
        .replace("{{scratchpad}}", &scratchpad)
}

/// 技能清单：精简工具时去掉 [`config::CC_ENV_TRIMMED_SKILLS`] 那三条（各占一行）。
fn env_skills(trim_tools: bool) -> &'static str {
    static TRIMMED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        config::CC_ENV_SKILLS
            .split('\n')
            .filter(|line| {
                !config::CC_ENV_TRIMMED_SKILLS.iter().any(|n| line.starts_with(&format!("- {n}:")))
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    if trim_tools { TRIMMED.as_str() } else { config::CC_ENV_SKILLS }
}

/// 「You are powered by the model named …」那一行。名字与知识截止查
/// [`config::CC_MODEL_IDENTITIES`]（规范名逐字相等，或后面只跟日期）；1M 会话名字后面加
/// `(1M context)`、id 后面加 `[1m]`（`00216`）。表里没有的模型照官方兜底只写 id。模型名里有
/// 不该出现的字符（空白、换行……）时返回 `None`，整段不补。
pub(in crate::proxy) fn env_model_line(model: &str, context_1m: bool) -> Option<String> {
    let lower = model.trim().to_ascii_lowercase();
    let bare = lower.strip_suffix("[1m]").unwrap_or(&lower);
    let one_m = context_1m || bare.len() < lower.len();
    if bare.is_empty()
        || !bare.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
    {
        return None;
    }
    let id = if one_m { format!("{bare}[1m]") } else { bare.to_string() };
    let hit = config::CC_MODEL_IDENTITIES.iter().find(|(k, ..)| {
        bare == *k || bare.strip_prefix(k).is_some_and(|rest| rest.starts_with("-20"))
    });
    Some(match hit {
        Some((_, name, cutoff)) => {
            let name = if one_m { format!("{name} (1M context)") } else { name.to_string() };
            format!(
                "You are powered by the model named {name}. The exact model ID is {id}. \
                 Assistant knowledge cutoff is {cutoff}."
            )
        }
        None => format!("You are powered by the model {id}."),
    })
}

/// [`pin_env`] 的键：（凭证，会话 id，首条消息指纹）。会话 id 按账号复用（[`Simulation::detect`]），
/// 同一个 id 下先后几段对话各有各的开头。
struct PinKey<'a> {
    cred_id: i64,
    session_id: &'a str,
    root: u64,
}

/// [`pin_env`] 看的这一轮：消息条数、是不是开场（首条用户消息之后只有 system）、末条用户
/// 消息的指纹。
struct PinTurn {
    len: usize,
    opening: bool,
    last_fp: u64,
}

/// 环境说明里随首轮定下、之后不再变的几样：模型那行、Agent 那段用不用 haiku 版、精简与否。
/// 落位（`mid_conv_sys`）**不**钉在这里——它必须跟着这一轮的模型走，不然切回不支持的旧模型会
/// 收到上游 400。
#[derive(Debug, Clone)]
struct PinFirst {
    model_line: String,
    haiku: bool,
    trim: bool,
}

/// [`pin_env`] 交出的：首轮那几样、日期、这一轮要不要补模型说明、更早几轮换过模型时需要在
/// 历史原位重现的那条。
struct Pinned {
    first: PinFirst,
    date: String,
    /// 最近一次换模型，[`locate_switch`] 拿它定位在这条请求里的哪儿。
    switch: Option<Switch>,
}

/// 一次换模型：那一轮来访的消息条数、那一轮末条用户消息（触发换模型的那句）的指纹、新模型那行。
#[derive(Debug, Clone)]
struct Switch {
    turn: usize,
    fp: u64,
    line: String,
}

/// 一段对话钉住的环境说明取值。
struct EnvPin {
    first: PinFirst,
    date: String,
    /// 最近一次见到的模型那行：这一轮的不同于它，就是换了模型。
    current: String,
    /// 最近一次换模型。同一轮重发（换号、重试）照样交出模型说明；更早几轮才算历史。多次换模型
    /// 只记最近一次——真实使用场景里一段对话里多半只换一次模型，再追到前面那条属于复杂边角，
    /// 先不做。
    switch: Option<Switch>,
    seen: std::time::Instant,
}

/// 环境说明按**对话**钉住：官方在会话开头取一次日期与模型，此后作为历史原样带着。逐轮现取会让
/// 跨过零点、或中途换了模型的对话在半路改写历史，缓存、message thread 与 `<total_tokens>` 倒数的
/// 前缀都对不上。
///
/// `now` 是这一轮现取的值；对话第一次见、或一天没再见到（按新对话算）时它就是首轮那份。之后
/// 只比模型那行：变了即这一轮换了模型，记下这一轮的消息条数与末条用户消息的指纹（[`PinTurn`]）。
/// 这次换模型落在这条请求的哪儿由 [`locate_switch`] 按指纹去找。时区取 luban 所在机器的本地时间
/// ——官方写的也是那台机器的本地日期。
///
/// 开场那一轮（`opening`，首轮重发/新对话开场）且模型与原钉的那条对不上时按新对话算、整条
/// 重置：会话 id 按缓存前缀 + 首条用户消息派生（[`crate::proxy::body::sim_session_key`]），两段
/// 不同的对话完全可以撞到同一个键，不重置就会把前一段的 pin.first 当成这段的首轮模型。
///
/// 表的上限是 [`PIN_CAP`]：满了先清一天没见的，仍超过一半就按最近见到的时间只留一半。清一次
/// 腾出一半的空，下次满之前不必再扫。
fn pin_env(key: PinKey<'_>, now: PinFirst, turn: PinTurn) -> Pinned {
    type Key = (i64, String, u64);
    static PINS: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<Key, EnvPin>>> =
        std::sync::LazyLock::new(Default::default);
    const IDLE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    let at = std::time::Instant::now();
    let fresh = |now: PinFirst| EnvPin {
        date: chrono::Local::now().format("%Y-%m-%d").to_string(),
        current: now.model_line.clone(),
        first: now,
        switch: None,
        seen: at,
    };
    let mut map = PINS.lock();
    if map.len() >= PIN_CAP {
        map.retain(|_, p| at.duration_since(p.seen) < IDLE);
        evict_oldest(&mut map, PIN_CAP / 2);
    }
    let pin = map
        .entry((key.cred_id, key.session_id.to_string(), key.root))
        .or_insert_with(|| fresh(now.clone()));
    let stale = at.duration_since(pin.seen) >= IDLE;
    let fresh_conversation = turn.opening && pin.first.model_line != now.model_line;
    if stale || fresh_conversation {
        *pin = fresh(now.clone());
    } else if pin.current != now.model_line {
        pin.current = now.model_line.clone();
        pin.switch = Some(Switch { turn: turn.len, fp: turn.last_fp, line: now.model_line });
    }
    pin.seen = at;
    Pinned { first: pin.first.clone(), date: pin.date.clone(), switch: pin.switch.clone() }
}

/// [`pin_env`] 那张表的容量上限。
const PIN_CAP: usize = 4096;

/// 按 `seen` 只留最近的 `keep` 条（同一时刻的并列可能多留几条）。
fn evict_oldest<K>(map: &mut std::collections::HashMap<K, EnvPin>, keep: usize) {
    if map.len() <= keep || keep == 0 {
        return;
    }
    let mut seen: Vec<std::time::Instant> = map.values().map(|p| p.seen).collect();
    let (_, cutoff, _) = seen.select_nth_unstable_by(keep - 1, |a, b| b.cmp(a));
    let cutoff = *cutoff;
    map.retain(|_, p| p.seen >= cutoff);
}

/// 最近一次换模型落在这条请求（插环境说明之前的 `msgs`）的哪儿：
///
/// - 触发它的那条用户消息就是末条、或之后只剩 system（换模型那一轮本身，或它的重发）→
///   这一轮交出模型说明；
/// - 在更早的位置 → `(下标, 那行)`，由 [`insert_historical_switch`] 在原位重现；
/// - 找不到（客户端删改了历史、compact 过）→ 原位无从插起，按「这一轮刚换了模型」补在末尾，
///   让模型仍然看到自己的当前身份。
///
/// 按指纹找而不是按当时的消息条数：删掉中间几条之后旧下标会指到别的消息上（甚至 assistant），
/// 那条说明要么插错地方、要么悄悄丢了。先试当时那个位置，对不上再从后往前找。
fn locate_switch(
    msgs: &[serde_json::Value],
    sw: Switch,
) -> (Option<String>, Option<(usize, String)>) {
    let is_it = |m: &serde_json::Value| {
        m.get("role").and_then(|r| r.as_str()) == Some("user") && thread_msg_of(m).fp == sw.fp
    };
    let idx = sw
        .turn
        .checked_sub(1)
        .filter(|&i| msgs.get(i).is_some_and(is_it))
        .or_else(|| msgs.iter().rposition(is_it));
    // 那条之后只剩 system（指令之类）也算这一轮。
    let later_turns = |i: usize| {
        msgs[i + 1..].iter().any(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"))
    };
    match idx {
        Some(i) if later_turns(i) => (None, Some((i, sw.line))),
        _ => (Some(sw.line), None),
    }
}

/// 把历史里那条模型说明补回它当时所在的位置。调用点：
///
/// - [`apply_sim_thread`] 的 `create` 分支（续轮上游线程里仍有，不用补）；
/// - 关着 message thread 时 [`crate::proxy::rewrite_body`] 走的那条路（完整历史每轮重发，必补）。
///
/// `switch.0` 是触发换模型那条用户消息在出站 `messages` 里的下标（[`locate_switch`] 核对过）。
/// 落位同 [`place_model_notice`]：`mid_conv_sys` 为 `true`（opus/sonnet/fable）时作为独立
/// `role: system` 消息插在那条用户消息之后的第一个合法落点，落点前一条已经是 system（环境说明
/// 之类）就并进去；为 `false`（haiku）时裹成 `<system-reminder>` 塞进那条用户消息，`tool_result`
/// 开头的那种放最后、其余放最前。
///
/// 那个下标上不是用户消息（中间又有步骤动了消息条数）时不补，返回 `false`，由调用方改按
/// 「这一轮刚换了模型」补在末尾。
pub(in crate::proxy) fn insert_historical_switch(
    v: &mut serde_json::Value,
    switch: Option<&(usize, String)>,
    mid_conv_sys: bool,
) -> bool {
    let Some((user_idx, notice)) = switch else { return true };
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else { return false };
    if msgs.get(*user_idx).and_then(|m| m.get("role")).and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    if mid_conv_sys {
        // 想插的位置如果夹在两条非 assistant 消息之间，上游会 400（system 必须紧挨 assistant，或在
        // 末尾）。[`system_insert_pos`] 从 want 开始找下一个合法落点。
        let at = system_insert_pos(msgs, user_idx + 1);
        if at > 0 && append_to_system(&mut msgs[at - 1], notice) {
            return true;
        }
        msgs.insert(
            at,
            serde_json::json!({
                "role": "system",
                "content": [{ "type": "text", "text": notice }],
            }),
        );
        return true;
    }
    let user = &mut msgs[*user_idx];
    let Some(content) = user.get_mut("content") else { return false };
    if let serde_json::Value::String(s) = content {
        let s = std::mem::take(s);
        *content = serde_json::json!([{ "type": "text", "text": s }]);
    }
    let Some(blocks) = content.as_array_mut() else { return false };
    let has_tool_result =
        blocks.iter().any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
    let block = reminder_block(notice);
    if has_tool_result {
        blocks.push(block);
    } else {
        blocks.insert(0, block);
    }
    true
}

/// 一条 `role: system` 消息能落的位置。上游规则：system 必须紧挨 assistant（排在它前面）
/// 或排在数组末尾，不能夹在两条非 assistant 消息之间（会 400：`system must be before an
/// assistant or at the end of the array`）。
///
/// `want` 是理想落位，`system_insert_pos` 返回不小于 `want` 的第一个合法落点：
/// - `want` 就是末尾（`== msgs.len()`）→ 直接返回。
/// - `msgs[want]` 是 assistant → `want` 合法，就在它前面落。
/// - 否则是另一条 user / system → 继续向后找 assistant，或退到数组末尾。
/// - 退到末尾时再往回让过末尾那段指令与临时 system（[`sticky_tail_start`]），但不早于 `want`：
///   它们原样留在最后。
///
/// 这个式子对 `want <= msgs.len()` 恒 well-defined。调用方自己保证 `want <= msgs.len()`。
fn system_insert_pos(msgs: &[serde_json::Value], want: usize) -> usize {
    let want = want.min(msgs.len());
    let mut at = want;
    while at < msgs.len() && msgs[at].get("role").and_then(|r| r.as_str()) != Some("assistant") {
        at += 1;
    }
    if at == msgs.len() { sticky_tail_start(msgs).max(want) } else { at }
}

#[cfg(test)]
mod tests {
    /// 表满了不止清一天没见的：近期条目照样按最近见到的时间只留一半，表不会无限长，下一次
    /// 清理要等再攒满一半。
    #[test]
    fn pin_table_evicts_down_to_half_when_full() {
        let base = std::time::Instant::now();
        let first = super::PinFirst { model_line: "m".into(), haiku: false, trim: false };
        let mut map: std::collections::HashMap<usize, super::EnvPin> = (0..super::PIN_CAP)
            .map(|i| {
                let pin = super::EnvPin {
                    first: first.clone(),
                    date: String::new(),
                    current: "m".into(),
                    switch: None,
                    seen: base + std::time::Duration::from_millis(i as u64),
                };
                (i, pin)
            })
            .collect();
        super::evict_oldest(&mut map, super::PIN_CAP / 2);
        assert_eq!(map.len(), super::PIN_CAP / 2);
        assert!(map.contains_key(&(super::PIN_CAP - 1)) && !map.contains_key(&0), "留最近的");
    }
}
