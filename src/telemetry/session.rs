//! 逐会话的状态：会话、子代理、线程、待发批次，以及 `process` 前半段推出的 [`TurnFacts`]。

use super::*;

/// 一个遥测会话（同一凭证 + 同一 `session_id`）跨请求要记住的东西。
pub(super) struct Session {
    /// 会话「进程」的起点：首条请求前几秒（客户端启动到第一次提交之间的那段）。
    pub(super) started_wall: SystemTime,
    pub(super) last_seen: Instant,
    /// 用户的第几次输入（事件里的 `prompt_index`）：同伴会话发来的（peer）不占号。
    pub(super) prompt_index: u32,
    /// 会话里一共有过几次新输入，**含** peer：判「会话首条」、选首轮模板、推客户端内部的
    /// 消息条数都用它——peer 那一轮也是客户端里实打实的一次提交与一条消息，只是不编号。
    pub(super) prompts_seen: u32,
    pub(super) prompt_id: String,
    pub(super) chain_id: String,
    /// 最近一条**主线程**请求的上游 request-id：`previousRequestId` 只串主线程那条链
    /// （标题生成那类侧查询不算，猜下一句那条也接在主线程后面），退出时的
    /// `cache_eviction_hint.last_request_id` 同样取它。
    pub(super) last_main_request_id: Option<String>,
    /// 最近一条主线程回复的 `message.id`（工具权限事件的 `messageID`）。
    pub(super) last_main_message_id: Option<String>,
    /// 最近一条主线程请求的 queryDepth（猜下一句的 depth = 它 + 2）。
    pub(super) last_main_depth: u32,
    /// 本轮下一条主线程请求的 queryDepth：新输入归零，每次 `tool_use` 续轮 +1。
    pub(super) turn_depth: u32,
    /// 本轮开始（用户提交）的时刻，`tengu_turn_end.duration_ms` 与首字上屏时长的起点。
    pub(super) turn_started: Option<SystemTime>,
    /// **上一轮**用户提交的时刻：`active_time.total{type:user}` 的窗口下界，见 `user_secs`。
    pub(super) prev_prompt_submit: Option<SystemTime>,
    /// 本轮已经出现过正文（`tengu_turn_first_text` 只发一次）。
    pub(super) turn_text_seen: bool,
    /// 本轮已执行的工具调用数。
    pub(super) turn_tool_calls: u32,
    /// 本轮主线程发过的 API 请求数与耗时之和（`-p` 收尾那条 `tengu_sdk_result` 的 `num_turns` 与
    /// `duration_api_ms`：`cap/auto-2.1.285-20260930/00790` → `00795` 一次输入两次请求，官方报
    /// 2 与 5734 + 3909 + 3 = 9646）。
    pub(super) turn_api_calls: u32,
    pub(super) turn_api_ms: i64,
    /// 会话里已经发过 `shell_snapshot_create`（首次 Bash 才有）。
    pub(super) shell_snapshot_done: bool,
    /// 会话里的首条主线程请求已经报过它那几条一次性事件（2.1.285：首字节时的
    /// `api_per_turn_effort` / `mcp_late_tool_additions` / `api_kept_reminder_clear_at`，收尾时的
    /// `tengu_wire_shape_recorded`）。
    pub(super) first_main_done: bool,
    /// 主线程那份会话级 beta（[`session_betas`] 过滤出站头）。子代理那条支线上除 API 调用本身
    /// 外的事件报的都是它，见 `betas_session` 的取法。
    pub(super) main_betas: String,
    /// 会话里已经发出去的主线程请求条数（上下文回放那对事件只跟前两条）。
    pub(super) main_requests: u32,
    /// 首次输入那段模板（`prompt` + `first_prompt`）已经发过：客户端注入的一轮（peer /
    /// task-notification）不套模板，第一次真正的用户输入才是「首次」。
    pub(super) first_prompt_tpl_done: bool,
    /// 模板 `background` 段已经补发到第几条。
    pub(super) background_done: usize,
    /// 最近几条请求的完成时刻（不分类别）。`timeSinceLastApiCallMs` 取「在这条之前完成的最近
    /// 一条」：并发时 `last_call_end` 可能是一条比这条晚结束的，差出负数就报不出来。
    pub(super) recent_ends: VecDeque<SystemTime>,
    /// 任一类请求最近一次结束的时刻（`timeSinceLastApiCallMs`）。
    pub(super) last_call_end: Option<SystemTime>,
    /// 上一条**主线程**请求的 token 总量（input + cache_read + cache_creation + output）：
    /// `messageTokens` 报的是「对话此刻的 token 数」，抓包里三条续轮逐一对得上。
    pub(super) prev_total_input: i64,
    /// 这一轮起头的提示词型斜杠命令，见 [`RequestShape::command_skill`]。
    pub(super) turn_skill: Option<String>,
    /// 会话第一条请求（被处理的那条）的发出时刻。
    pub(super) first_call_at: SystemTime,
    /// `/clear` 之前那个会话的 id，见 [`Identity::parent_session_id`]。
    pub(super) parent_session_id: Option<String>,
    /// `-p` 打印模式的会话。
    pub(super) sdk: bool,
    /// 在同一进程里被 `/clear` 换掉了：它不会有退出那一串（进程还在），[`Telemetry::gc`] 静默收掉。
    pub(super) cleared: bool,
    /// 每条线程（主线程 / 各子代理）上一条回复里的工具调用（[`ToolCall`]）：下一条请求带回它们的
    /// 结果时，工具事件按 id 取入参与判决。
    pub(super) reply_calls: HashMap<String, Vec<ToolCall>>,
    /// 这个会话里读过、写过的文件大小（路径 → 字节）：Edit / Write 的 `sidecarOriginalFileBytes`
    /// 要的是改之前那份有多大，代理只能从先前的 Read / Write / Edit 推。
    pub(super) file_sizes: HashMap<String, usize>,
    /// 最近一次在请求里看到的工作目录，见 [`RequestShape::cwd`]。thread 续轮的增量体里没有它。
    pub(super) cwd: Option<String>,
    /// 会话里到目前为止的用量合计：输入、输出、缓存读、缓存写（`tengu_auto_mode_decision` 的
    /// `session*Tokens`）。
    pub(super) usage_totals: [i64; 4],
    /// 见 [`RequestShape::git_repo`] / [`RequestShape::git_dirty`]：会话里最近一次看到的。
    pub(super) git_repo: Option<bool>,
    pub(super) git_dirty: Option<bool>,
    /// 上一条「猜下一句」出了建议（有正文）：`(request-id, 结束时刻, 建议的字数)`。用户接着敲了
    /// 别的，下一次输入时报 `tengu_prompt_suggestion{outcome: ignored}`（`cap/auto-2.1.285-20260930`
    /// 20 条）。
    pub(super) shown_suggestion: Option<(String, SystemTime, usize)>,
    /// 上一轮主线程被按 Esc 打断时那条回复的 `message.id`：下一次输入的 `tengu_input_prompt`
    /// 带 `interrupted_message_id`（`00264`）。
    pub(super) interrupted_message_id: Option<String>,
    /// 上一条主线程请求结束的时刻（夹带的后台任务通知就是那时送到的）。
    pub(super) last_main_end: Option<SystemTime>,
    /// 上一条主线程请求结束的时刻，**被 Esc 取消的也算**：2.1.291 `tether_live_outcome.threadIdleMs`
    /// 的时间锚点（`cap/auto-2.1.291-20261006-full/00354`：取消那条之后重建线程，idle 从取消那条
    /// 收尾算起）。与 [`Self::last_main_end`]、工具锚点（上一条有效回复里的工具调用）分开记。
    pub(super) last_main_anchor_end: Option<SystemTime>,
    /// 主线程最近一条**有效**（没被取消的）回复里有没有工具调用：`anchorHasToolCall` 的工具锚点。
    /// 与上面的时间锚点分开记；两样都只在本会话里算，`/clear` 换出来的新会话从空白起。
    pub(super) main_anchor_tool_call: bool,
    /// 刚做完一次 `/compact`，下一条主线程请求是压缩之后的第一条。
    pub(super) post_compact: bool,
    /// `default_model`：用户设置里的默认模型，取会话启动时那台设备的
    /// （[`State::device_default_model`]）。设备上还没记过的，取会话第一条**主线程**请求的展示
    /// 模型名——首条是侧查询（标题生成用的 haiku）时先记它占位，等主线程那条来了再换
    /// （`main_model_seen`）。`cap/auto-2.1.285-20260930`：`--model fable` 的会话报 opus，`/model
    /// haiku` 之后拉起的 `-p` 会话全报 haiku。
    pub(super) default_model: String,
    pub(super) main_model_seen: bool,
    pub(super) last_message_id: Option<String>,
    /// [`Self::last_message_id`] 那条请求的完成时刻：补发的、更早完成的请求不往回拨它。
    pub(super) last_message_end: Option<SystemTime>,
    pub(super) last_model: Option<String>,
    /// 上次报过的工具长度表 hash，主线程一组、侧查询一组各记各的：官方整个会话只有两条
    /// `tengu_tool_schema_sizes`（主线程 16 个工具一条、标题生成空表一条），第二轮主线程
    /// 不重发——两类查询的工具集互不覆盖。
    pub(super) tools_hash_main: Option<String>,
    pub(super) tools_hash_side: Option<String>,
    /// `claude_code.session.count` 已经报过。
    pub(super) counted: bool,
    /// 第一轮结束后的版本检查那串已经发过。
    pub(super) first_turn_done: bool,
    /// 这个会话的设备与账号身份、客户端版本、会话级 beta 串——保活要把空闲事件挂到真实会话
    /// 上时从这里取（见 [`Telemetry::latest_session`]）。版本与 beta 随每条请求刷新。
    pub(super) device_id: String,
    pub(super) account_uuid: String,
    pub(super) version: String,
    pub(super) betas: String,
    pub(super) subscription_type: String,
    /// 扣住等新一轮 prompt id 的侧查询：`(调用, 扣住时的上一条结束时刻, 扣住的时刻)`。
    /// 见 [`config::TELEMETRY_SIDE_QUERY_HOLD_SECS`]。
    pub(super) deferred: Vec<(ApiCall, Option<SystemTime>, Instant)>,
    /// 会话的系统提示词快照（`tengu_api_success.snapshotHash`）：首条带边界的请求记下
    /// （那条报 `systemPromptSource: live_recorded`），之后整个会话都报同一个值、
    /// `from_snapshot`。
    pub(super) snapshot_hash: Option<String>,
    /// 本会话已经报过 `tengu_sleepy_snowflake_applied` 的模型：官方每个模型只在头一次
    /// 被用于新输入时报一次（`cap/2.1.280` opus/fable/sonnet/haiku 各一条，再切回来不报）；
    /// 2.1.293 起标题生成（haiku-5-5）头一次也报，与主线程共用这张表。
    pub(super) sleepy_models: Vec<String>,
    /// 本会话已经报过 `tengu_heron_brook_applied`（haiku-5-5 主线程会话一条）。
    pub(super) heron_done: bool,
    /// 主线程的 tether 线程状态，见 [`TetherThread`]；子代理的各记在 [`AgentState`] 里。
    pub(super) tether_main: Option<TetherThread>,
    /// 每条线程上一条完整请求的形态，键是 `main` 或 `agent:<支线号>`，见 [`ThreadBase`]。
    pub(super) thread_bases: HashMap<String, ThreadBase>,
    /// 会话里的子代理，按支线号（`x-claude-code-agent-id`）分开记。
    pub(super) agents: HashMap<String, AgentState>,
    /// 最近一条回复里调了 `Agent` 工具的主线程请求：随后拉起的子代理首条报它为
    /// `invokingRequestId`。不能拿「最近一条主线程请求」代替——子代理是异步跑的，它的首条
    /// 回来之前主线程往往已经又完成了一条（`cap/2.1.280`：主线程 27.020 完成、子代理首条
    /// 28.759 才完成，而 invokingRequestId 指的是 22.953 那条）。
    pub(super) last_spawn_request_id: Option<String>,
    /// 主线程最近一条的权限模式。子代理的请求体里没有 auto 模式的提示（`cap/2.1.280`
    /// Explore 七条一条都没有），官方报的却是 `auto`——子代理沿用主线程的模式。
    pub(super) main_permission: &'static str,
    /// 主线程最近一条的 `output_config.effort`（haiku 没有）。子代理那条 `agent_tool_selected`
    /// 的 `session_effort` 报的是它（2.1.291）。
    pub(super) main_effort: Option<String>,
    /// 本轮的发起方（事件写法，如 `task-notification`）：新输入时从 billing header 取，
    /// 同一轮的续轮请求没带就沿用。
    pub(super) turn_origin: String,
}

/// 处理一条子代理请求时从 [`AgentState`] 抄出来的那几项（会话状态随后还要改，不能一直借着）。
pub(super) struct AgentView {
    pub(super) chain_id: String,
    pub(super) steps: u32,
    pub(super) last_request_id: Option<String>,
    pub(super) last_message_id: Option<String>,
    pub(super) prev_total: i64,
    pub(super) tools_hash: Option<String>,
    /// 只有支线首条才有。
    pub(super) invoking_request_id: Option<String>,
    /// 这条支线上一条请求结束的时刻，见 [`AgentState::last_end`]。
    pub(super) last_end: Option<SystemTime>,
}

/// 一个子代理（会话里的一条支线）跨请求要记住的东西。字段取值见 `cap/2.1.280` Explore
/// 子代理 a51764… 的 6 条请求与它的 1 条摘要请求。
#[derive(Default)]
pub(super) struct AgentState {
    /// 支线自己的 `queryChainId`：六条请求同一个，摘要请求另起。
    pub(super) chain_id: String,
    /// 已经完成的请求数：`queryDepth` = 2 + 它（2、3、4…），收尾的 `assistant_message_count`。
    pub(super) steps: u32,
    /// 首条请求发出的时刻：收尾 `turn_end.duration_ms` / `agent_tool_completed.duration_ms` 的起点。
    pub(super) started: Option<SystemTime>,
    /// 这条支线上一条请求结束的时刻（`tether_live_outcome.threadIdleMs` 的锚点）；首条没有。
    pub(super) last_end: Option<SystemTime>,
    /// 首条的用户提示字数（`agent_tool_completed.prompt_char_count`，抓包 481）。
    pub(super) prompt_chars: usize,
    /// 各续轮带回来的工具结果数之和（`total_tool_uses`）。
    pub(super) tool_uses: u32,
    /// 上一条的 request-id / message.id / 总 token（`previousRequestId`、工具事件的
    /// `messageID`、`messageTokens`）。
    pub(super) last_request_id: Option<String>,
    pub(super) last_message_id: Option<String>,
    pub(super) prev_total: i64,
    /// 支线自己的提示词快照（与主线程的不同，`cap/2.1.280` 为 `7d8050a24c2c`）。
    pub(super) snapshot_hash: Option<String>,
    pub(super) tether: Option<TetherThread>,
    /// 首条的 `invokingRequestId`（拉起它的那条主线程请求）。
    pub(super) invoking_request_id: Option<String>,
    /// 工具长度表报过的那份 hash（`tengu_tool_schema_sizes` 每条支线首次报一次）。
    pub(super) tools_hash: Option<String>,
    /// 子代理类型（请求头 `x-claude-code-agent-type`）。它的摘要请求头上只有 `agent-id`，
    /// 按类型定的取值（claude-code-guide 的 `dontAsk`）要从这里取。
    pub(super) agent_type: Option<String>,
}

/// 一条线程（主线程，或某个子代理）上一条**完整**请求的形态。
///
/// 2.1.277 起客户端对 sonnet / haiku 这类不被钉成无状态的模型走消息线程：首条
/// `thread: {"type":"create"}` 照常带全量，之后 `{"type":"continue", "previous_message_id"}`
/// 只发增量——system 只剩 billing 头那一块、没有 tools、消息只有上一条回复之后新增的那几条
/// （上一条 assistant 回复由服务端持有，不再回传）。可客户端自己的遥测报的是**它眼里的**
/// 整段对话：`cap/2.1.277` sonnet 那条续用请求体里 2 条消息，事件报 `messageCount` 8
/// （= 上一条的 5 + 体里 2 + 省掉的那条回复），`toolsCount` 照报 19。增量请求的这些量
/// 从这里补回来；没有它，增量请求会因为「没有工具」被当成辅助调用。
#[derive(Debug, Clone)]
pub(super) struct ThreadBase {
    pub(super) tools_count: usize,
    pub(super) tools_chars: usize,
    pub(super) tools_hash: String,
    pub(super) tool_lens: String,
    pub(super) deferred_tools: usize,
    pub(super) mcp_tools: usize,
    pub(super) has_tool_search: bool,
    pub(super) tools_off: ToolsOff,
    pub(super) system_blocks: usize,
    pub(super) system_chars: usize,
    pub(super) static_len: usize,
    pub(super) dynamic_len: usize,
    pub(super) dynamic_hash: String,
    pub(super) omitted_bytes: usize,
    pub(super) permission_mode: &'static str,
    pub(super) messages: usize,
    pub(super) assistant_messages: usize,
    pub(super) input_text_chars: usize,
    /// 这条请求的**回复**按 `inputTextCharLength` 口径的字数：下一条增量请求里省掉的正是它，
    /// 客户端报的输入长度却含它。
    pub(super) reply_chars: usize,
    /// 这条回复里的工具调用（工具名 → 入参字符数之和，按首次出现排序，即
    /// [`ApiCall::tool_use_lens`]）。下一条增量请求只带 `tool_result`，工具名要从这里配。
    pub(super) reply_tools: Vec<(String, usize)>,
}

impl ThreadBase {
    pub(super) fn of(
        s: &RequestShape,
        reply_chars: usize,
        reply_tools: Vec<(String, usize)>,
    ) -> Self {
        ThreadBase {
            tools_count: s.tools_count,
            tools_chars: s.tools_chars,
            tools_hash: s.tools_hash.clone(),
            tool_lens: s.tool_lens.clone(),
            deferred_tools: s.deferred_tools,
            mcp_tools: s.mcp_tools,
            has_tool_search: s.has_tool_search,
            tools_off: s.tools_off,
            system_blocks: s.system_blocks,
            system_chars: s.system_chars,
            static_len: s.static_len,
            dynamic_len: s.dynamic_len,
            dynamic_hash: s.dynamic_hash.clone(),
            omitted_bytes: s.omitted_bytes,
            permission_mode: s.permission_mode,
            messages: s.messages_len,
            assistant_messages: s.assistant_messages,
            input_text_chars: s.input_text_chars,
            reply_chars,
            reply_tools,
        }
    }

    /// 把一条增量请求的形态补成客户端视角的全量。auto 模式的提示在增量里没出现就沿用上一条的。
    pub(super) fn fill(&self, s: &mut RequestShape, auto_in_delta: bool) {
        // 工具集中途变了的续轮自己带着完整 `tools`（ToolSearch 刚载入工具、MCP 工具上线，
        // `cap/auto-2.1.285-20260930/00054`），以它为准；没带才沿用底本那份。
        if s.tools_count == 0 {
            s.tools_count = self.tools_count;
            s.tools_chars = self.tools_chars;
            s.tools_hash = self.tools_hash.clone();
            s.tool_lens = self.tool_lens.clone();
            s.deferred_tools = self.deferred_tools;
            s.mcp_tools = self.mcp_tools;
            s.has_tool_search = self.has_tool_search;
            s.tools_off = self.tools_off;
        }
        s.system_blocks = self.system_blocks;
        s.system_chars = self.system_chars;
        s.static_len = self.static_len;
        s.dynamic_len = self.dynamic_len;
        s.dynamic_hash = self.dynamic_hash.clone();
        s.omitted_bytes = self.omitted_bytes;
        if !auto_in_delta {
            s.permission_mode = self.permission_mode;
        }
        // 增量里没有 assistant 时，全量视角下它前面紧挨着上一条回复：末条 assistant 之后的条数
        // 就是增量的条数（`tether_live_outcome.deltaMessageCount`，`cap/2.1.285` 主线程续轮恒 2、
        // 子代理续轮恒 1，正是增量体的条数）。
        if s.assistant_messages == 0 {
            s.after_last_assistant = s.messages_len;
        }
        // 增量体里的 `tool_result` 配回上一条回复里的工具调用，续轮那串工具事件（放行、执行、
        // 成功、攒附件）才发得出来（`cap/2.1.285`：主线程 `00115`、`00121`，子代理 `00127` 等
        // 五条）。回复里只记了每个工具名的入参总长，按出现顺序配：结果条数与工具名个数相同就
        // 一一对上（`00136`：Read、WebFetch），只有一个工具名就全是它（`00131`：三条 WebFetch），
        // 多出来的归最后一个；同名多次调用的入参长度均分。
        if s.tool_uses.is_empty() && !s.orphan_results.is_empty() && !self.reply_tools.is_empty() {
            let names: Vec<&(String, usize)> = self.reply_tools.iter().collect();
            let pick = |i: usize| names[i.min(names.len() - 1)];
            let calls_of = |name: &str| {
                (0..s.orphan_results.len()).filter(|&i| pick(i).0 == name).count().max(1)
            };
            s.tool_uses = s
                .orphan_results
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let (name, input_total) = pick(i);
                    let input_len = input_total / calls_of(name);
                    ToolUse {
                        id: r.id.clone(),
                        name: name.clone(),
                        input_len,
                        // 入参里除 `command` 外还有 `description` 等几十字节，官方 Bash 的
                        // `bashCommandLen` 比入参总长短一截；拿不到原文，按总长估。回复流里记下了
                        // 原文的（[`ToolCall`]），[`Telemetry::process`] 随后按 id 换成真值。
                        command_len: if name == "Bash" { input_len.saturating_sub(40) } else { 0 },
                        result_len: r.len,
                        persisted_from: r.persisted,
                        is_error: r.is_error,
                        result_head: r.head.clone(),
                        stripped_bytes: r.stripped_bytes,
                        image_b64: r.image_b64,
                        media_blocks: r.media_blocks,
                        input: Value::Null,
                        verdict: None,
                    }
                })
                .collect();
        }
        s.messages_len += self.messages + 1;
        s.assistant_messages += self.assistant_messages + 1;
        s.input_text_chars += self.input_text_chars + self.reply_chars;
        s.estimated_tokens = estimate_tokens(&s.model, s.input_text_chars);
    }
}

/// 客户端 tether 引擎眼里的「上一条请求」：`tengu_tether_decision` 拿这条请求与它比，
/// 配置一样且消息只多不少就接着用（`continue/append`），配置变了就另起（`create/config_changed`）。
#[derive(Debug, Clone)]
pub(super) struct TetherThread {
    pub(super) model: String,
    pub(super) betas: String,
    pub(super) effort: Option<String>,
    pub(super) tools_hash: String,
    pub(super) thinking_type: String,
    pub(super) messages: usize,
    pub(super) turns: u32,
}

/// 一条请求的 tether 判定结果，事件链里 `tether_decision` / `echo_audit` / `live_outcome`
/// 三条共用。
pub(super) struct Tether {
    pub(super) decision: &'static str,
    pub(super) reason: &'static str,
    pub(super) changed_model: bool,
    pub(super) changed_tools: bool,
    pub(super) changed_betas: bool,
    pub(super) changed_latched: bool,
    pub(super) changed_thinking: bool,
    pub(super) changed_effort: bool,
    pub(super) turns: u32,
    pub(super) prev_messages: usize,
    pub(super) delta: usize,
    /// 线程里此前已有请求：只有这种才有 `tengu_tether_echo_audit`（主线程另起线程也算，
    /// 那是同一会话的回声；子代理只在续用时有）。
    pub(super) echo: bool,
}

/// 一个真实会话的身份快照，给保活复用：空闲的版本检查事件应当从**同一个会话**发出，
/// 而不是另造一台从不发 API 请求的幽灵设备。
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub device_id: String,
    pub account_uuid: String,
    /// 客户端版本（取自出站 UA）。
    pub version: String,
    /// 展示模型名（`claude-opus-5[1m]`）。
    pub model: String,
    /// 会话级 beta 串。
    pub betas: String,
    pub prompt_id: String,
    /// 会话「进程」的起点，算 `process.uptime` 用。
    pub started_wall: SystemTime,
}

/// 指标累积的最小单位：一条 API 调用。导出时按属性聚合。
#[cfg_attr(test, derive(Debug))]
pub(super) struct CallMetric {
    pub(super) session_id: String,
    pub(super) device_id: String,
    pub(super) account_uuid: String,
    pub(super) model: String,
    pub(super) category: &'static str,
    /// 没有 `output_config.effort` 的侧查询不带 `effort` 属性（抓包里标题生成那组没有）。
    pub(super) effort: Option<String>,
    /// 子代理的类型：子代理那组数据点多一个 `agent.name`（`cap/2.1.285/00157`：
    /// `query_source: subagent, agent.name: claude-code-guide`），落在 `effort` 之后。
    pub(super) agent_name: Option<String>,
    pub(super) cost: f64,
    pub(super) input: i64,
    pub(super) output: i64,
    pub(super) cache_read: i64,
    pub(super) cache_creation: i64,
    /// `active_time.total{type:cli}` 的贡献：只有以 `end_turn` 收尾的主线程请求算它的时长
    /// （`cap/2.1.260-2`：10.053s ≈ 3.779 + 6.338，中间那条 tool_use 收尾的 3.585 与两条侧查询
    /// 都不算；单请求会话 2.827 / 2.347 与 API 时长几乎相等）。
    pub(super) cli_secs: f64,
    /// `active_time.total{type:user}` 的贡献：新输入前用户敲字/思考的那段（上一轮结束到这次
    /// 提交，封顶 5s；首次输入取 0.9s——两份单轮会话是 0.878 / 1.118）。
    pub(super) user_secs: f64,
    /// 这条是该会话的第一条：带一条 `session.count`。
    pub(super) new_session: bool,
    /// 这个会话 id 此前已按退出收尾过，这次是 resume（`start_type: resume`）。
    pub(super) resumed: bool,
    /// 新进程 `--continue` 接上的（`start_type: continue`），见 [`State::process_starts`]。
    pub(super) continued: bool,
    /// 这条请求真的拿到了用量。失败的请求照样占 `session.count` 与 `active_time`（会话确实
    /// 起了、CLI 确实忙过），但**不进** `cost.usage` 与 `token.usage`——官方那两个计数器
    /// 是在成功回包时才加的，替它记一串 0 只会凭空多出一堆零值数据点。
    pub(super) usage: bool,
}

/// 一张凭证攒着还没发出去的东西。
#[derive(Default)]
pub(super) struct Pending {
    pub(super) version: String,
    pub(super) subscription_type: String,
    pub(super) events: Vec<(DateTime<Utc>, Value)>,
    pub(super) events_since: Option<Instant>,
    pub(super) dd: Vec<Value>,
    pub(super) dd_since: Option<Instant>,
    pub(super) metrics: Vec<CallMetric>,
    pub(super) metrics_since: Option<Instant>,
    /// 这个会话最近一条请求的身份与上下文：指标导出时要就地造一条
    /// `tengu_feature_ok{internal_metrics_export}`，用的就是这份。
    pub(super) identity: Option<Identity>,
    pub(super) model: String,
    pub(super) betas: String,
    pub(super) prompt_id: String,
    pub(super) started_wall: Option<SystemTime>,
    /// 退出收尾时指定的导出事件时间戳（排在 `lsp_shutdown` 之后、`cache_eviction_hint`
    /// 之前）；平时为 `None`，导出时取当下。
    pub(super) export_at: Option<DateTime<Utc>>,
}

/// `process` 前半段推出来、后半段拼事件与入队要用的那些量，与原先的局部变量一一对应。
pub(super) struct TurnFacts {
    pub(super) device_id: String,
    pub(super) session_id: String,
    pub(super) account_uuid: String,
    pub(super) version: String,
    pub(super) now: Instant,
    pub(super) continued_from: Option<(String, SystemTime)>,
    pub(super) cleared_from: Option<String>,
    pub(super) cleared_prev: Option<ClearedPrev>,
    pub(super) identity: Identity,
    pub(super) base_identity: Identity,
    pub(super) kind: Kind,
    pub(super) has_1m: bool,
    pub(super) display_model: String,
    pub(super) resp_model: String,
    pub(super) key: (i64, String),
    pub(super) is_new_session: bool,
    pub(super) resumed: bool,
    pub(super) continued: bool,
    pub(super) device_key: (i64, String),
    pub(super) git_outcome: &'static str,
    pub(super) turn_over: bool,
    pub(super) failed: bool,
    pub(super) aborted: bool,
    pub(super) emit_first_turn: bool,
    pub(super) is_main: bool,
    pub(super) new_prompt: bool,
    pub(super) turn_skill: Option<String>,
    pub(super) post_compaction: bool,
    pub(super) turn_origin: String,
    pub(super) prev_turn: (String, u32, Option<SystemTime>),
    pub(super) rejected_calls: Vec<(ToolCall, ToolResultInfo)>,
    pub(super) user_turn: bool,
    pub(super) notification_base: u32,
    pub(super) notifications: Vec<usize>,
    pub(super) prev_main_end: Option<SystemTime>,
    pub(super) prompt_id: String,
    pub(super) agent: Option<AgentView>,
    pub(super) previous_request_id: Option<String>,
    pub(super) prev_main_message_id: Option<String>,
    pub(super) prev_main_request_id: Option<String>,
    pub(super) prev_main_depth: u32,
    pub(super) prev_end: Option<SystemTime>,
    pub(super) time_since_last: Option<u64>,
    pub(super) message_tokens: i64,
    pub(super) default_model: String,
    pub(super) device_default_update: Option<String>,
    pub(super) model_changed: bool,
    pub(super) previous_message_id: Option<String>,
    pub(super) tools_changed: bool,
    pub(super) counted: bool,
    pub(super) started_wall: SystemTime,
    pub(super) main_chain: String,
    pub(super) chain_id: String,
    pub(super) query_depth: u32,
    pub(super) turn_started: DateTime<Utc>,
    pub(super) first_text_in_turn: bool,
    pub(super) first_text_interrupted: bool,
    pub(super) tool_calls_before: u32,
    pub(super) user_secs: f64,
    pub(super) shell_snapshot_first: bool,
    pub(super) prompt_index: u32,
    pub(super) prompt_seq: u32,
    pub(super) v270: bool,
    pub(super) v277: bool,
    pub(super) v280: bool,
    pub(super) v285: bool,
    pub(super) v291: bool,
    pub(super) v293: bool,
    /// 会话主线程最近一条的 effort，见 [`Session::main_effort`]。
    pub(super) main_effort: Option<String>,
    /// 同一条线（主线程 / 这个子代理）上一条回复里有工具调用（`tether_live_outcome.anchorHasToolCall`）。
    pub(super) anchor_tool_call: bool,
    /// 同一条线上一条请求结束的时刻（`tether_live_outcome.threadIdleMs`）：主线程含被取消的那条
    /// （[`Session::last_main_anchor_end`]），子代理是它自己那条支线的（[`AgentState::last_end`]），
    /// 首条没有。
    pub(super) anchor_end: Option<SystemTime>,
    pub(super) injected: bool,
    pub(super) first_prompt_tpl: bool,
    pub(super) background: std::ops::Range<usize>,
    pub(super) first_main: bool,
    pub(super) snapshot: Option<(&'static str, String)>,
    pub(super) sleepy: bool,
    /// 这条前面报一条 `tengu_heron_brook_applied`（2.1.293 haiku-5-5 会话的首次输入）。
    pub(super) heron: bool,
    pub(super) tether: Option<Tether>,
    pub(super) usage_before: [i64; 4],
    pub(super) ignored_suggestion: Option<(String, SystemTime, usize)>,
    pub(super) interrupted_message_id: Option<String>,
    pub(super) handback: Option<ToolCall>,
    pub(super) agent_done: Option<AgentState>,
    pub(super) ctx_model: String,
    pub(super) dd_model: String,
    pub(super) sess_turn_tools: u32,
    pub(super) sess_turn_api_calls: u32,
    pub(super) sess_turn_api_ms: i64,
    pub(super) session_cwd: Option<String>,
    pub(super) betas_full: String,
    pub(super) betas_own: String,
    pub(super) thread_step: Option<u32>,
    pub(super) betas_session: String,
}

impl Pending {
    /// 追加一串事件与 Datadog 条目，两路各记下最早攒进来的时刻。
    pub(super) fn push_batch(&mut self, (events, dd): TplOutput, now: Instant) {
        self.events_since.get_or_insert(now);
        self.events.extend(events);
        self.dd_since.get_or_insert(now);
        self.dd.extend(dd);
    }

    /// 会话的头一条常是标题生成那种不带环境段的侧查询，那时还不知道工作目录是不是 git 仓库；
    /// 等主线程那条说了，把还没发出去的那些补上 `vcs`（官方同一会话每条都带）。
    pub(super) fn backfill_vcs(&mut self, vcs: &'static str) {
        for (_, e) in self.events.iter_mut() {
            if let Some(env) = e.get_mut("event_data").and_then(|d| d.get_mut("env"))
                && env.get("vcs").is_none()
            {
                insert_after(env, "is_local_agent_mode", vec![("vcs", json!(vcs))]);
            }
        }
        for d in self.dd.iter_mut() {
            if d.get("vcs").is_none() && d.get("deployment_environment").is_some() {
                insert_after(d, "deployment_environment", vec![("vcs", json!(vcs))]);
            }
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) sessions: HashMap<(i64, String), Session>,
    /// 会话还没建起来就先完成的侧查询（标题生成之类，`cap/auto-2.1.291-20261006` 四族的
    /// `00064`→`00066`、`00135`→`00136`、`00204`→`00205`、`00274`→`00276`：标题先回来）。会话由
    /// 它建的话，启动模板就按一条没有工具的请求出，开关 `sim_trim_tools` 那几样（禁用的环境变量、
    /// `artifact_disabled_session`）再也补不上。先扣在这里，等这个会话第一条主线程请求把会话建好
    /// 再补发；扣太久（[`config::TELEMETRY_SIDE_QUERY_HOLD_SECS`]）没等到就照旧按它自己建会话。
    pub(super) presession: HashMap<(i64, String), Vec<(ApiCall, Instant)>>,
    /// 待发批次按 `(凭证, session_id)` 分开攒：真实客户端一个进程一个会话，各自往上报，
    /// **一个批次里只会有一个 `session_id` / `device_id`**。同一张凭证被几台设备同时用时，
    /// 合在一个 POST 里就是官方不会产生的混合批次。
    pub(super) pending: HashMap<(i64, String), Pending>,
    pub(super) org_uuid: HashMap<i64, String>,
    /// 已按「客户端退出」收尾的会话及收尾时刻（见 [`Telemetry::gc`]）。同一个 id 再来就是
    /// `claude --resume`：新进程、从头计数，但 `session.count` 报 `start_type: resume`。
    /// 保留 [`config::TELEMETRY_ENDED_SESSION_MEMORY_SECS`]。
    pub(super) ended: HashMap<(i64, String), Instant>,
    /// 每台设备（凭证 + device_id）用户设置里的默认模型，见 [`Session::default_model`]。
    /// `/model` 选完模型就写进设置（那条 `model_validation` 探测就是它），**之后**拉起的进程
    /// 才按新的报；进行中的会话仍报它启动时那个。没见过 `/model` 的设备以它第一条主线程请求的
    /// 模型为准。
    pub(super) device_default_model: HashMap<(i64, String), (String, Instant)>,
    /// 每台设备最近一次进程启动：启动时那条额度探测（`quota`）的会话 id 与时刻。官方客户端每拉起
    /// 一次进程都先发它，`/clear` 不发——据此分出三种会话形态（`cap/auto-2.1.285-20260930`）：
    /// 探测的会话 id 就是新会话的 → 新进程；新会话前没有新的探测、同一台设备上还有交互会话在跑 →
    /// 同一进程里 `/clear`；探测之后冒出来的是之前见过的会话 id → 新进程 `--continue` 接上了它
    /// （探测带的是进程启动时那个临时 id）。
    pub(super) process_starts: HashMap<(i64, String), (String, SystemTime)>,
}

impl Session {
    /// 会话的头一条请求到了：按它建一份初始状态。`id.parent_session_id` 就是 `/clear` 换掉的
    /// 那个会话（[`State::resolve_lineage`] 的 `cleared_from`），`cleared_prev` 是那个会话留下的。
    pub(super) fn fresh(
        call: &ApiCall,
        id: &Identity,
        now: Instant,
        cleared_prev: Option<&ClearedPrev>,
        device_default: Option<String>,
        display_model: &str,
    ) -> Session {
        Session {
            // 进程启动到首条请求：2.1.260 模板的启动段从 -1.3s 起、按 3.1s 算；2.1.285 那份启动段
            // 从 -6.2s 起（`cap/2.1.285/00040`：进程 06:34:08.6 起，扣掉信任对话框后约 6.2s 到首条
            // api_query）。
            started_wall: cleared_prev.map_or_else(
                || {
                    call.started_at
                        - Duration::from_millis(if id.sdk {
                            // `-p` 的启动段从 -4.4s 起（`00448`）。
                            4_450
                        } else if version_at_least(&id.version, "2.1.285") {
                            6_300
                        } else {
                            3_100
                        })
                },
                |p| p.started_wall,
            ),
            last_seen: now,
            prompt_index: 0,
            prompts_seen: 0,
            prompt_id: String::new(),
            chain_id: String::new(),
            last_main_request_id: None,
            last_main_message_id: None,
            last_main_depth: 0,
            turn_depth: 0,
            turn_started: None,
            prev_prompt_submit: None,
            turn_text_seen: false,
            turn_tool_calls: 0,
            turn_api_calls: 0,
            turn_api_ms: 0,
            shell_snapshot_done: false,
            first_main_done: false,
            main_betas: String::new(),
            main_requests: 0,
            first_prompt_tpl_done: false,
            // `/clear` 换的是会话不是进程：运行时长与后台任务的进度接着上一个会话的算。
            background_done: cleared_prev.map_or(0, |p| p.background_done),
            recent_ends: VecDeque::new(),
            last_call_end: None,
            prev_total_input: 0,
            default_model: device_default.clone().unwrap_or_else(|| display_model.to_string()),
            main_model_seen: device_default.is_some(),
            last_message_id: None,
            last_message_end: None,
            last_model: None,
            tools_hash_main: None,
            tools_hash_side: None,
            counted: false,
            first_turn_done: false,
            device_id: id.device_id.clone(),
            account_uuid: id.account_uuid.clone(),
            version: id.version.clone(),
            betas: String::new(),
            subscription_type: id.subscription_type.clone(),
            deferred: Vec::new(),
            snapshot_hash: None,
            sleepy_models: Vec::new(),
            heron_done: false,
            tether_main: None,
            thread_bases: HashMap::new(),
            agents: HashMap::new(),
            last_spawn_request_id: None,
            main_permission: "default",
            main_effort: None,
            turn_origin: "human".to_string(),
            turn_skill: None,
            first_call_at: call.started_at,
            parent_session_id: id.parent_session_id.clone(),
            sdk: id.sdk,
            cleared: false,
            reply_calls: HashMap::new(),
            file_sizes: HashMap::new(),
            cwd: None,
            usage_totals: [0; 4],
            git_repo: None,
            git_dirty: None,
            shown_suggestion: None,
            interrupted_message_id: None,
            post_compact: false,
            last_main_end: None,
            // `/clear` 换出来的会话也从空白起：首条没有锚点（`00356`：-1 / false）。
            last_main_anchor_end: None,
            main_anchor_tool_call: false,
        }
    }
}

/// `/clear` 换掉的那个会话留下的：最后一条主线程请求、最后结束时刻、进程起点、后台进度。
#[derive(Clone, Debug)]
pub(super) struct ClearedPrev {
    /// 旧会话最后一条主线程请求的 request-id（清空那几条事件报它）。
    pub(super) last_main_request_id: Option<String>,
    /// 旧会话最后一次请求完成的时刻。
    pub(super) last_call_end: Option<SystemTime>,
    /// 进程启动的时刻：`/clear` 不换进程，新会话沿用。
    pub(super) started_wall: SystemTime,
    /// 进程里已经补发过的后台任务数，新会话接着算。
    pub(super) background_done: usize,
    // tether 的两个锚点**不**从旧会话接：`/clear` 之后新会话的第一条主线程请求报
    // `create/no_continue_pointer`、`threadIdleMs: -1`、`anchorHasToolCall: false`
    // （`cap/auto-2.1.291-20261006-full/00356`，会话 `271f3652`）。
}

/// 这个会话与同一进程、同一设备上其他会话的关系，见 [`State::resolve_lineage`]。
pub(super) struct Lineage {
    /// 新进程 `--continue` 接上的：启动探测那个临时会话 id 与探测时刻。
    pub(super) continued_from: Option<(String, SystemTime)>,
    /// 同一进程里 `/clear` 换掉的那个会话。
    pub(super) cleared_from: Option<String>,
    pub(super) cleared_prev: Option<ClearedPrev>,
    pub(super) parent_session_id: Option<String>,
}

impl State {
    /// 会话形态：`/clear` 与 `--continue`，见 [`State::process_starts`]。
    /// 这条请求是不是 `--continue` 接上旧会话的新进程（只判，不改状态），见 [`Self::resolve_lineage`]。
    /// 返回启动探测那个临时会话 id 与探测时刻。
    pub(super) fn continue_marker(
        &self,
        call: &ApiCall,
        shape: &RequestShape,
        session_id: &str,
        device_id: &str,
    ) -> Option<(String, SystemTime)> {
        let skey = (call.cred_id, session_id.to_string());
        let dkey = (call.cred_id, device_id.to_string());
        let marker = self.process_starts.get(&dkey).cloned();
        // 同一台设备上可能同时跑着几个进程（另开一个终端、脚本里跑 `claude -p`），别的进程的启动
        // 探测不能把正在跑的会话当成 `--continue`。三样都满足才算：这个会话最后一次活动在探测之前、
        // 这一条不是 thread 续轮（新进程接不上旧进程的线程，`--continue` 之后首条是 create）、探测
        // 那个临时会话 id 没有自己发过请求（另一个进程会接着用它自己的 id）。
        marker.as_ref().and_then(|(probe_sid, at)| {
            let seen_before = self
                .sessions
                .get(&skey)
                .is_some_and(|s| s.last_call_end.unwrap_or(s.first_call_at) <= *at)
                || self.ended.contains_key(&skey);
            let probe_has_own_session =
                self.sessions.contains_key(&(call.cred_id, probe_sid.clone()));
            (probe_sid != session_id
                && !shape.sdk
                && seen_before
                && shape.thread_type.as_deref() != Some("continue")
                && !probe_has_own_session)
                .then(|| (probe_sid.clone(), *at))
        })
    }

    pub(super) fn resolve_lineage(
        &mut self,
        call: &ApiCall,
        shape: &RequestShape,
        session_id: &str,
        device_id: &str,
    ) -> Lineage {
        let skey = (call.cred_id, session_id.to_string());
        let dkey = (call.cred_id, device_id.to_string());
        let marker = self.process_starts.get(&dkey).cloned();
        let continued_from = self.continue_marker(call, shape, session_id, device_id);
        if continued_from.is_some() {
            // 新进程接上旧会话：会话里的一切从头来（与退出后 `--resume` 一样），指标报 `continue`。
            self.sessions.remove(&skey);
            self.ended.remove(&skey);
            self.process_starts.remove(&dkey);
        }
        let cleared_from: Option<String> = if continued_from.is_none()
            && !self.sessions.contains_key(&skey)
            && !shape.sdk
            && let Some((probe_sid, _)) = marker.as_ref()
            && probe_sid != session_id
        {
            // 被换掉的是这台设备上**最近在用**的那个交互会话。几个进程同时开着时，按探测时刻去挑，
            // 挑中的是最后启动的那个进程，而不是用户刚在里面敲 `/clear` 的那个。
            self.sessions
                .iter()
                .filter(|((c, sid), s)| {
                    *c == call.cred_id
                        && sid != session_id
                        && s.device_id == device_id
                        && !s.sdk
                        && !s.cleared
                })
                .max_by_key(|(_, s)| s.last_call_end)
                .map(|((_, sid), _)| sid.clone())
        } else {
            None
        };
        let cleared_prev = cleared_from.as_ref().and_then(|prev| {
            let p = self.sessions.get_mut(&(call.cred_id, prev.clone()))?;
            p.cleared = true;
            Some(ClearedPrev {
                last_main_request_id: p.last_main_request_id.clone(),
                last_call_end: p.last_call_end,
                started_wall: p.started_wall,
                background_done: p.background_done,
            })
        });
        let parent_session_id = cleared_from
            .clone()
            .or_else(|| self.sessions.get(&skey).and_then(|s| s.parent_session_id.clone()));
        Lineage { continued_from, cleared_from, cleared_prev, parent_session_id }
    }
}
