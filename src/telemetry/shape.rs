//! 从出站请求体里读出这条请求的形态（[`RequestShape`]）。

use super::*;

/// 上一条回复里的一次工具调用（续轮请求的倒数第二条 assistant 消息里的 `tool_use` 块）。
#[derive(Debug, Clone, Default)]
pub(super) struct ToolUse {
    pub(super) id: String,
    pub(super) name: String,
    /// 工具入参：请求历史里 assistant 那条的 `tool_use.input`，或（thread 续轮看不到历史）回复
    /// 流里记下的那份（[`ToolCall`]）。都没有是 `Null`。
    pub(super) input: Value,
    /// auto 模式的服务端判决，见 [`ToolCall::verdict`]。
    pub(super) verdict: Option<String>,
    /// `tool_result.is_error`：工具执行失败（Bash 非零退出、被用户拒绝……）。
    pub(super) is_error: bool,
    /// `tool_result` 正文的开头（至多 400 字符），认「被用户拒绝」「命令失败」用。
    pub(super) result_head: String,
    /// 见 [`ToolResultInfo::stripped_bytes`] / [`ToolResultInfo::image_b64`] /
    /// [`ToolResultInfo::media_blocks`]。
    pub(super) stripped_bytes: usize,
    pub(super) image_b64: usize,
    pub(super) media_blocks: usize,
    /// `input` 的 JSON 字符数。
    pub(super) input_len: usize,
    /// Bash 的 `command` 长度（其它工具为 0）。
    pub(super) command_len: usize,
    /// 对应 `tool_result` 的内容字符数。
    pub(super) result_len: usize,
    /// 结果太大被客户端落了盘、请求里只带预览（`<persisted-output>` 开头）时，预览里写的原始
    /// 大小（字节，按 `Output too large (58.9KB)` 还原）。见 [`persisted_original_size`]。
    pub(super) persisted_from: Option<usize>,
}

/// 被落盘的工具结果在请求里是一段预览：`<persisted-output>\nOutput too large (58.9KB). Full
/// output saved to: …`（`cap/2.1.285/00131`）。返回预览里写的原始大小（KB / MB 换成字节）；
/// 不是这个形态返回 `None`。官方那条 `tool_use_success.toolResultSizeBytes` 报的是原始大小
/// （60278，预览写 58.9KB），落盘本身另有一条 `tengu_tool_result_persisted`。
pub(super) fn persisted_original_size(content: &Value) -> Option<usize> {
    let text = match content {
        Value::String(t) => t.as_str(),
        Value::Array(blocks) => blocks.iter().find_map(|b| b.get("text")?.as_str())?,
        _ => return None,
    };
    let rest = text.strip_prefix("<persisted-output>")?;
    let after = rest.split("Output too large (").nth(1)?;
    let (num, tail) = after.split_at(after.find(|c: char| !(c.is_ascii_digit() || c == '.'))?);
    let n: f64 = num.parse().ok()?;
    let unit = if tail.starts_with("MB") {
        1024.0 * 1024.0
    } else if tail.starts_with("KB") {
        1024.0
    } else {
        1.0
    };
    Some((n * unit).round() as usize)
}

/// 从出站请求体里读出来的形态。
#[derive(Debug, Default)]
pub(super) struct RequestShape {
    pub(super) model: String,
    pub(super) messages_len: usize,
    /// 客户端启动时探额度的那条：haiku、`max_tokens: 1`、唯一一条消息就是 `quota`、无 system
    /// 无 tools（`cap/2.1.260-1/00004`、`00021`）。官方对它**不发任何 api 事件**（那个会话的
    /// 首批里只有真实对话那一条 `tengu_api_success`），所以遥测这边要跳过。
    pub(super) quota_probe: bool,
    /// 末条消息是用户新输入（而不是 tool_result 续轮）。
    pub(super) new_prompt: bool,
    /// 用户新输入的字符数。
    pub(super) prompt_len: usize,
    /// billing header 里的 `cc_prompt_id`。
    pub(super) cc_prompt_id: Option<String>,
    /// billing header 里的 `cc_prev_req`：**这条请求自己声明的上一条 request-id**。
    ///
    /// 与 `previousRequestId` 是同一件事——`cap/2.1.260-2` 那个会话三条续轮逐字相同
    /// （体里 `cc_prev_req=req_011CeiBW8Yx9…` ↔ 事件里 `previousRequestId` 同值），官方
    /// 两处出自同一个 `requestJournal`。故它是权威源：出站体已经这么发给上游了，遥测
    /// 再从会话状态另算一份，两份不一致时上游把请求体和事件批一 join 就能看出来。
    pub(super) cc_prev_req: Option<String>,
    /// billing header 里的 `cc_turn_origin`：这一轮是谁发起的（`human` / `peer` /
    /// `task_notification`…），见 [`turn_origin_of`]。
    pub(super) turn_origin: Option<String>,
    /// 顶层 `diagnostics.previous_message_id`：同理，是这条请求自己声明的上一条 message.id。
    /// 空值（`null`）与字段缺失都记 `None`。
    pub(super) diag_prev_message_id: Option<String>,
    pub(super) is_subagent: bool,
    pub(super) system_blocks: usize,
    pub(super) system_chars: usize,
    /// `system[0]`（billing header）的长度与 sha256。
    pub(super) sys0_len: usize,
    pub(super) sys0_hash: String,
    /// 倒数第二块 / 最后一块的长度（`tengu_sysprompt_boundary_found`）。
    pub(super) static_len: usize,
    pub(super) dynamic_len: usize,
    pub(super) tools_count: usize,
    pub(super) tools_chars: usize,
    pub(super) tools_hash: String,
    /// `{"Agent":3078,…}` 那串 JSON。
    pub(super) tool_lens: String,
    pub(super) deferred_tools: usize,
    /// 正文里**非延迟**的 `mcp__*` 工具数（`tengu_tool_search_mode_decision.mcpToolCount`
    /// 在非延迟形态下的取值：`cap/2.1.258-api` 报 2，正文里正好是两个 `mcp__ide__*`）。
    pub(super) mcp_tools: usize,
    /// 正文里声明了 `ToolSearch` 这个工具（非延迟）。它是延迟加载真正能用的前提：模型要靠它
    /// 把 `defer_loading` 的工具搜出来。只有占位声明、没有它，搜索无处发起。
    pub(super) has_tool_search: bool,
    pub(super) input_text_chars: usize,
    /// `estimatedInputTokens`，见 [`parse_shape`] 里的口径说明。
    pub(super) estimated_tokens: usize,
    pub(super) image_blocks: usize,
    pub(super) image_bytes: usize,
    pub(super) doc_blocks: usize,
    pub(super) doc_bytes: usize,
    pub(super) temperature: f64,
    pub(super) thinking_type: String,
    /// `output_config.effort`；侧查询（标题生成）不带。
    pub(super) effort: Option<String>,
    pub(super) fast_mode: bool,
    pub(super) permission_mode: &'static str,
    pub(super) cache_ttl_1h: bool,
    /// 体里有没有任何 `cache_control`（标题那类没有 → `cachingEnabled: false`）。
    pub(super) has_cache_control: bool,
    pub(super) api_system_messages: usize,
    /// 末条用户消息以 `[SUGGESTION MODE:` 开头。
    pub(super) suggestion: bool,
    /// 末条用户消息是离开回顾那句固定提示（见 [`Kind::AwaySummary`]）。
    pub(super) away_summary: bool,
    /// system 里有会话标题生成的指令。
    pub(super) title: bool,
    /// system 里有会话起名（kebab-case）的指令，见 [`Kind::RenameName`]。
    pub(super) rename: bool,
    /// 工具只有 `web_search` 这一个 server tool：WebSearch 工具另发的那条，见 [`Kind::WebSearchTool`]。
    pub(super) web_search_tool: bool,
    /// 无工具、唯一一条用户消息以 `Web page content:` 开头：WebFetch 取回网页后的页面处理。
    pub(super) web_fetch_page: bool,
    /// 有 `system`、无工具、`max_tokens: 1`：`/model` 选完模型后的「Hi」探测，见
    /// [`Kind::ModelValidation`]。额度探测没有 `system`，另见 `quota_probe`。
    pub(super) model_validation: bool,
    /// 末条用户消息里有 `/btw` 插问那段提醒，见 [`Kind::SideQuestion`]。
    pub(super) side_question: bool,
    /// 末条用户消息以 `/compact` 的「CRITICAL: Respond with TEXT ONLY」开头，见 [`Kind::Compact`]。
    pub(super) compact: bool,
    /// billing header 的 `cc_entrypoint=sdk-cli`：`-p` 打印模式，主线程报 `querySource: sdk`。
    pub(super) sdk: bool,
    /// 这一轮是 `!` 跑的 shell 命令（末条用户消息以 `<bash-input>` 开头）：报 `tengu_input_bash`
    /// 而不是 `tengu_input_prompt`，`turn_origin` 是 `unstamped`（`cap/auto-2.1.285-20260930/00191`）。
    pub(super) bash_input: bool,
    /// `!` 跑的那条命令与它的输出字数（`<bash-input>` / `<bash-stdout>`）。
    pub(super) bash_typed: Option<(String, usize)>,
    /// 请求里环境那段写的工作目录（`Primary working directory: …`）。
    pub(super) cwd: Option<String>,
    /// 环境那段的 `Is a git repository: true/false`。
    pub(super) git_repo: Option<bool>,
    /// 开场附上的 `gitStatus` 里 `Status:` 下面有没有改动（`M calc.py`、`?? x`），没有这段为 `None`。
    pub(super) git_dirty: Option<bool>,
    /// 这次新输入里夹着的后台任务通知（`[SYSTEM NOTIFICATION - NOT USER INPUT]` 那段提醒里的
    /// `<task-notification>…</task-notification>`，UTF-16 长度）：通知是在上一轮收尾时送到的，
    /// 没有单独发请求，官方当时就报了一条 `turn_origin: task-notification` 的输入
    /// （`cap/auto-2.1.285-20260930` 07:51:33.115，`prompt_length` 954 正是那一段）。
    pub(super) merged_notifications: Vec<usize>,
    /// 这次新输入之前，上一轮在权限弹框上被用户拒掉的工具（结果是「The user doesn't want to
    /// proceed…」那段），见 [`last_is_new_prompt`]。
    pub(super) rejected: Vec<ToolResultInfo>,
    /// 这一轮是提示词型斜杠命令起的头（`<command-message>init</command-message>`）：这一轮每条
    /// 主线程请求的 `tengu_api_success` 都带 `attributionSkill`（`00266`–`00268`）。
    pub(super) command_skill: Option<String>,
    /// `permission_mode` 取自请求体 `safeguards[].classifier_context.permission_mode`——客户端
    /// 自己写的，比从消息里的提示推断可靠（plan 模式也只在这里）。
    pub(super) permission_declared: bool,
    /// 续轮请求：上一条 assistant 消息里的工具调用，配上末条消息里的 tool_result 大小。
    pub(super) tool_uses: Vec<ToolUse>,
    /// 末条 user 消息里的 `tool_result`（`tool_use_id`、内容字符数）——前面**没有** assistant
    /// 的时候才记：message-threads 续轮的增量体就是这个样子，工具调用在上一条回复里，要由
    /// [`ThreadBase::fill`] 配回去。
    pub(super) orphan_results: Vec<ToolResultInfo>,
    pub(super) assistant_messages: usize,
    /// 最后一条 assistant 消息之后还有几条消息（没有 assistant 消息记 0）：
    /// `tengu_tether_live_outcome.deltaMessageCount`（`cap/2.1.277` 主线程续轮报 2、
    /// 子代理续轮报 1，与这个数逐条相等）。
    pub(super) after_last_assistant: usize,
    /// 缓存没盖住的尾段：最后一条带断点的消息之后还有几条、按块估的 token 数（[`tail_tokens_est`]）、
    /// 是否全是 `system` 角色。`messages` 里一个断点都没有时全为零。`tengu_api_success` 的
    /// `uncoveredTail*` 那组按它报，见那里。
    pub(super) tail_messages: usize,
    pub(super) tail_tokens_est: u64,
    pub(super) tail_all_system: bool,
    /// `system[1..]` 与整个 `tools` 数组各自的紧凑 JSON 长度（UTF-16 计）之和：续用线程时
    /// 省掉不发的那部分，`tengu_tether_live_outcome.omittedBytes`（`cap/2.1.277/00031`：
    /// 13184 + 72661 = 85845，与事件逐字节相等）。
    pub(super) omitted_bytes: usize,
    /// system 最后一块（会话级的动态段）的 sha256 前缀，`tengu_api_success.snapshotHash`
    /// 的来源，见 [`snapshot_hash_of`]。
    pub(super) dynamic_hash: String,
    /// 顶层 `thread.type`（`create` / `continue`），见 [`ThreadBase`]。
    pub(super) thread_type: Option<String>,
    /// 消息里出现过 auto 模式的进入/退出提示（`permission_mode` 是从消息里判的）。
    pub(super) auto_marker: bool,
    pub(super) device_id: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) account_uuid: Option<String>,
}

/// 内容块数组里 text 的字符数（tool_result 的 content 既可能是字符串也可能是块数组）。
pub(super) fn content_chars(c: &Value) -> usize {
    match c {
        Value::String(s) => js_len(s),
        // ToolSearch 的结果只有 `tool_reference` 块：官方按整段 JSON 的长度报
        // （`[{"type":"tool_reference","tool_name":"WebSearch"}]` 51，`cap/auto-2.1.285-20260930`）。
        Value::Array(blocks)
            if !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_reference")) =>
        {
            js_len(&c.to_string())
        }
        Value::Array(blocks) => {
            blocks.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).map(js_len).sum()
        }
        _ => 0,
    }
}

/// 续轮请求里上一条 assistant 消息的 `tool_use` 块，按 id 配上末条消息里的 `tool_result`。
/// 对话里最后两条**非 system** 消息（客户端把 total_tokens_reminder 之类的附件挂成一条
/// `role:"system"` 消息追在最后——`cap/2.1.260-2` 三条主线程请求的末条都是它，判「新输入」
/// 与「续轮」都得先跳过去）。返回 `(倒数第二条, 最后一条)`。
pub(super) fn last_two_non_system(messages: &[Value]) -> (Option<&Value>, Option<&Value>) {
    let mut it =
        messages.iter().rev().filter(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"));
    let last = it.next();
    let prev = it.next();
    (prev, last)
}

/// 末条 user 消息里的 `tool_result`，前一条不是 assistant 时（续轮增量体）才有意义，见
/// [`RequestShape::orphan_results`]。
pub(super) fn orphan_results_of(messages: &[Value]) -> Vec<ToolResultInfo> {
    let (prev, Some(last)) = last_two_non_system(messages) else { return vec![] };
    if prev.is_some_and(|p| p.get("role").and_then(|r| r.as_str()) == Some("assistant")) {
        return vec![];
    }
    tool_results_of(last)
}

/// 去掉结果末尾客户端追加的 `\n<system-reminder>…</system-reminder>`。只去它自己那个换行：
/// 命令输出本身的末行换行官方照算（`00412`：`…deletions(-)\n` 报 119）。
pub(super) fn strip_trailing_reminder(text: &str) -> &str {
    let mut t = text;
    while t.trim_end().ends_with("</system-reminder>")
        && let Some(at) = t.rfind("<system-reminder>")
    {
        t = t[..at].strip_suffix('\n').unwrap_or(&t[..at]);
    }
    t
}

/// 一条 `tool_result` 里遥测要的东西。
#[derive(Debug, Clone, Default)]
pub(super) struct ToolResultInfo {
    pub(super) id: String,
    /// 内容字符数。
    pub(super) len: usize,
    /// 见 [`ToolUse::persisted_from`]。
    pub(super) persisted: Option<usize>,
    pub(super) is_error: bool,
    /// 正文开头，见 [`ToolUse::result_head`]。
    pub(super) head: String,
    /// Read 结果去掉行号前缀后的字节数，见 [`read_content_bytes`]。
    pub(super) stripped_bytes: usize,
    /// 结果里图片块 base64 的总长（Read 读图片时 `sidecarFileBase64Bytes`）。
    pub(super) image_b64: usize,
    /// 结果里的图片块数（`toolResultMediaBlocks`）。
    pub(super) media_blocks: usize,
}

/// 一条消息里的全部 `tool_result`。
pub(super) fn tool_results_of(msg: &Value) -> Vec<ToolResultInfo> {
    msg.get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                .filter_map(|b| {
                    let id = b.get("tool_use_id")?.as_str()?.to_string();
                    let content = b.get("content");
                    let text = match content {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Array(blocks)) => blocks
                            .iter()
                            .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    let image_b64 = match content {
                        Some(Value::Array(blocks)) => blocks
                            .iter()
                            .filter_map(|x| x.get("source")?.get("data")?.as_str())
                            .map(str::len)
                            .sum(),
                        _ => 0,
                    };
                    // 客户端发请求时在结果末尾追一段 `<system-reminder>`（total_tokens 那类），官方量的是
                    // 工具自己的输出，不含它（`00412`：206 − 87 = 119）。
                    let tail = strip_trailing_reminder(&text);
                    let len = match content {
                        Some(Value::String(_)) => js_len(tail),
                        Some(c) => content_chars(c)
                            .saturating_sub(js_len(&text).saturating_sub(js_len(tail))),
                        None => 0,
                    };
                    let text = tail.to_string();
                    Some(ToolResultInfo {
                        id,
                        len,
                        persisted: content.and_then(persisted_original_size),
                        is_error: b.get("is_error").and_then(|e| e.as_bool()) == Some(true),
                        head: text.chars().take(400).collect(),
                        stripped_bytes: read_content_bytes(&text),
                        image_b64,
                        media_blocks: match content {
                            Some(Value::Array(blocks)) => blocks
                                .iter()
                                .filter(|x| x.get("type").and_then(|t| t.as_str()) == Some("image"))
                                .count(),
                            _ => 0,
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `toolInputSizeBytes`：客户端校验过的入参序列化后的长度（UTF-16 计，与 `bashCommandLen` 同一口径——
/// 含中文破折号的 Write 按字节数会多出 16）。Edit 的入参客户端会补上默认的 `"replace_all":false`
/// （官方 330 = 模型给的 310 + 20）。
pub(super) fn tool_input_len(name: &str, input: &Value) -> usize {
    let mut v = input.clone();
    if name == "Edit"
        && let Some(o) = v.as_object_mut()
        && !o.contains_key("replace_all")
    {
        o.insert("replace_all".into(), json!(false));
    }
    js_len(&v.to_string())
}

impl ToolUse {
    /// 按回复流里记下的那份（或历史里的 `tool_use` 块）补名字、入参与判决。
    pub(super) fn apply_call(&mut self, name: &str, input: &Value, verdict: Option<&String>) {
        self.name = name.to_string();
        if !input.is_null() {
            self.input = input.clone();
            self.input_len = tool_input_len(name, input);
            self.command_len = input.get("command").and_then(|c| c.as_str()).map_or(0, js_len);
        }
        if verdict.is_some() {
            self.verdict = verdict.cloned();
        }
    }
}

pub(super) fn tool_uses_of(messages: &[Value]) -> Vec<ToolUse> {
    let (Some(prev), Some(last)) = last_two_non_system(messages) else { return vec![] };
    if prev.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return vec![];
    }
    let results = tool_results_of(last);
    if results.is_empty() {
        return vec![];
    }
    prev.get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .filter_map(|b| {
                    let id = b.get("id")?.as_str()?.to_string();
                    let name = b.get("name")?.as_str()?.to_string();
                    let input = b.get("input").cloned().unwrap_or(Value::Null);
                    let command_len =
                        input.get("command").and_then(|c| c.as_str()).map_or(0, js_len);
                    let hit = results.iter().find(|r| r.id == id);
                    Some(ToolUse {
                        id,
                        name,
                        input_len: tool_input_len(b.get("name")?.as_str()?, &input),
                        command_len,
                        result_len: hit.map_or(0, |r| r.len),
                        persisted_from: hit.and_then(|r| r.persisted),
                        is_error: hit.is_some_and(|r| r.is_error),
                        result_head: hit.map(|r| r.head.clone()).unwrap_or_default(),
                        stripped_bytes: hit.map_or(0, |r| r.stripped_bytes),
                        image_b64: hit.map_or(0, |r| r.image_b64),
                        media_blocks: hit.map_or(0, |r| r.media_blocks),
                        verdict: None,
                        input,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn sha256_hex(data: &[u8]) -> String {
    crate::credentials::hex_lower(&Sha256::digest(data))
}

/// 失败请求的 `errorType`：把状态码 + 上游 `error.type`/文案归到官方那套分类里。
///
/// 官方那个判定（2.1.260 的 `ate()`）有四十来条分支，按顺序试；这里只保留一个转发代理
/// 真能看见的那些，顺序与官方一致——顺序要紧，比如 429 在 `>=400` 之前、`overloaded_error`
/// 在 `>=500` 之前。
///
/// 流内错误（`in_band`）官方走的是另一条分支：SDK 那边这类没有状态码、只有 `type`，报
/// `in_band_<type>`。
pub(super) fn error_kind(f: &CallFailure) -> String {
    let msg = f.message.to_lowercase();
    let etype = f.error_type.as_deref().unwrap_or("");
    if f.in_band {
        let t = if etype.is_empty() { "unknown" } else { etype };
        return format!("in_band_{}", camel_to_snake(t));
    }
    let Some(status) = f.status else {
        // 没有状态码 = 连接层就没走通。
        return "connection_error".to_string();
    };
    if status == 429 {
        return "rate_limit".to_string();
    }
    if status == 529 || etype == "overloaded_error" {
        return "server_overload".to_string();
    }
    if msg.contains("prompt is too long") || etype == "prompt_too_long_error" {
        return "prompt_too_long".to_string();
    }
    if status == 413 || msg.contains("request exceeds the maximum size") {
        return "request_too_large".to_string();
    }
    if status == 404 && etype == "not_found_error" && msg.contains("model: ") {
        return "model_not_found".to_string();
    }
    if status == 400 {
        for (needle, kind) in [
            ("could not process image", "image_unprocessable"),
            ("text content blocks must be non-empty", "empty_text_block"),
            ("text content blocks must contain non-whitespace text", "empty_text_block"),
            ("diagnostics.previous_message_id", "previous_message_id_invalid"),
            ("grammar compilation", "grammar_compile_error"),
            ("request body is not valid json", "request_body_invalid_json"),
        ] {
            if msg.contains(needle) {
                return kind.to_string();
            }
        }
        if msg.contains("signature") && msg.contains("thinking") {
            return "invalid_thinking_signature".to_string();
        }
    }
    if msg.contains("credit balance is too low") {
        return "credit_balance_low".to_string();
    }
    if msg.contains("oauth token has been revoked") || msg.contains("token has been revoked") {
        return "token_revoked".to_string();
    }
    if status == 401 || status == 403 {
        return "auth_error".to_string();
    }
    if status >= 500 {
        return "server_error".to_string();
    }
    if status >= 400 {
        return "client_error".to_string();
    }
    "unknown".to_string()
}

/// `tengu_feature_bad{api_request}` 的 `error_code`：官方对「重试都用光了」与「这类错误
/// 压根不重试」分别报 `api_request_retry_exhausted` 与 `api_request_non_retryable`
/// （可重试的状态码是 `{401,403,404,407,413,429}` 加 5xx 与连接层错误）。
pub(super) fn api_request_error_code(f: &CallFailure) -> &'static str {
    let retryable = match f.status {
        // 没有状态码（连接层）与流内错误都属于会重试的那类。
        None => true,
        Some(s) => matches!(s, 401 | 403 | 404 | 407 | 413 | 429) || s >= 500,
    };
    if retryable { "api_request_retry_exhausted" } else { "api_request_non_retryable" }
}

/// 一条消息的粗估 token 数：逐块 `round(长度 / 4)` 相加（长度按 UTF-16 计），text 取正文、
/// `tool_result` 取内容（数组形态取其中各 text 块）、`tool_use` 取入参 JSON，其余块不计。
/// 与官方 `resentTailTokensEst` / `sentOnceTailTokensEst` 逐条对得上（`cap/auto-2.1.285-20260930`）：
/// 猜下一句那条 1363 字符 → 341（28 条）；插问 1326 + 14 → 332 + 4 = 336（`00238`）；压缩
/// 690 + 6361 + 49 → 173 + 1590 + 12 = 1775（`00175`）；fable-5-1 主线程末尾那条 126 → 32（`00385`）。
pub(super) fn tail_tokens_est(m: &Value) -> u64 {
    let rough = |s: &str| (js_len(s) as f64 / 4.0).round() as u64;
    let text = |b: &Value| b.get("text").and_then(|t| t.as_str()).map_or(0, rough);
    match m.get("content") {
        Some(Value::String(s)) => rough(s),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| match b.get("type").and_then(|t| t.as_str()) {
                Some("text") => text(b),
                Some("tool_result") => match b.get("content") {
                    Some(Value::String(s)) => rough(s),
                    Some(Value::Array(inner)) => inner.iter().map(text).sum(),
                    _ => 0,
                },
                Some("tool_use") => b.get("input").map_or(0, |i| rough(&i.to_string())),
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

/// 官方那些「长度」都是 JavaScript 的 `String.length`，即 UTF-16 码元数：中文一个字算 1、
/// emoji 算 2，而不是 UTF-8 字节数或码点数（`cap/2.1.260-2/00057`：requestBodyChars 101459，
/// 同一份 body 的字节数是 101908）。
pub(super) fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// 内容块里 base64 数据的字节数估算（4 个字符 3 字节）。
pub(super) fn source_bytes(block: &Value) -> usize {
    block
        .get("source")
        .and_then(|s| s.get("data"))
        .and_then(|d| d.as_str())
        .map(|d| d.len() * 3 / 4)
        .unwrap_or(0)
}

/// 一条消息的 content：字符串或块数组。
pub(super) fn walk_content(content: &Value, shape: &mut RequestShape) {
    match content {
        Value::String(s) => shape.input_text_chars += js_len(s),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        shape.input_text_chars +=
                            b.get("text").and_then(|t| t.as_str()).map_or(0, js_len);
                    }
                    // 工具调用块按「工具名 + input 的 JSON」计：`cap/2.1.260-2` 续轮与猜下一句的
                    // inputTextCharLength 都比纯文本多 170 = "Bash"(4) + input JSON(166，与
                    // tool_use_success 的 toolInputSizeBytes 同值)。
                    Some("tool_use") => {
                        shape.input_text_chars +=
                            b.get("name").and_then(|n| n.as_str()).map_or(0, js_len);
                        if let Some(input) = b.get("input") {
                            shape.input_text_chars += js_len(&input.to_string());
                        }
                    }
                    Some("image") => {
                        shape.image_blocks += 1;
                        shape.image_bytes += source_bytes(b);
                    }
                    Some("document") => {
                        shape.doc_blocks += 1;
                        shape.doc_bytes += source_bytes(b);
                    }
                    Some("tool_result") => {
                        if let Some(c) = b.get("content") {
                            walk_content(c, shape);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// [`last_is_new_prompt`] 的整体 body 版本，给转发路径判「这条请求要不要换一个
/// `cc_prompt_id`」用（见 [`crate::proxy::CcSessionLink`]）：新的用户输入换一个，
/// 工具续轮沿用同一个。
///
/// 没有 `messages`（或不是数组）时算作新一轮——那种请求本来就不在任何一条链上。
pub fn last_is_new_prompt_body(body: &Value) -> bool {
    match body.get("messages").and_then(|m| m.as_array()) {
        Some(msgs) => last_is_new_prompt(msgs).0,
        None => true,
    }
}

/// 末条消息是不是一次新的用户输入：role 为 user，且 content 里没有 `tool_result`。
/// 用户打断工具调用后客户端补的那句，见 [`last_is_new_prompt`]。
pub(super) const INTERRUPTED_FOR_TOOL_USE: &str = "[Request interrupted by user for tool use]";
/// 用户在权限弹框上拒绝后，那个 `tool_result` 的开头。
pub(super) const USER_REJECTED_TOOL_USE: &str =
    "The user doesn't want to proceed with this tool use.";

pub(super) fn last_is_new_prompt(messages: &[Value]) -> (bool, usize) {
    let (_, Some(last)) = last_two_non_system(messages) else { return (true, 0) };
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return (false, 0);
    }
    match last.get("content") {
        Some(Value::String(s)) => (true, js_len(s)),
        Some(Value::Array(blocks)) => {
            // 用户在权限弹框上拒了工具、接着敲了下一句：工具结果（拒绝的那段话）与
            // `[Request interrupted by user for tool use]` 和新输入挤在一条消息里
            // （`cap/auto-2.1.285-20260930/00252`）。这是一次新输入，不是续轮。
            let interrupted = blocks.iter().any(|b| {
                b.get("text")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.trim_start().starts_with(INTERRUPTED_FOR_TOOL_USE))
            });
            if !interrupted
                && blocks
                    .iter()
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
            {
                return (false, 0);
            }
            // 用户敲的那段在 harness 注入的 system-reminder 之后，取最后一个 text 块。
            let len = blocks
                .iter()
                .rev()
                .find_map(|b| b.get("text").and_then(|t| t.as_str()))
                .map_or(0, js_len);
            (true, len)
        }
        _ => (true, 0),
    }
}

/// 解析 `metadata.user_id`（CC 内嵌 JSON 形态）。
pub(super) fn parse_user_id(v: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let Some(raw) = v.get("metadata").and_then(|m| m.get("user_id")).and_then(|u| u.as_str())
    else {
        return (None, None, None);
    };
    let Ok(inner) = serde_json::from_str::<Value>(raw) else { return (None, None, None) };
    let pick = |k: &str| {
        inner.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string)
    };
    (pick("device_id"), pick("session_id"), pick("account_uuid"))
}

pub(super) fn parse_shape(body: &[u8]) -> Option<RequestShape> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let mut shape = RequestShape {
        model: v.get("model")?.as_str()?.to_string(),
        temperature: v.get("temperature").and_then(|t| t.as_f64()).unwrap_or(1.0),
        thinking_type: v
            .get("thinking")
            .and_then(|t| t.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("disabled")
            .to_string(),
        effort: v
            .get("output_config")
            .and_then(|o| o.get("effort"))
            .and_then(|e| e.as_str())
            .map(str::to_string),
        fast_mode: v.get("speed").and_then(|s| s.as_str()) == Some("fast"),
        permission_mode: "default",
        has_cache_control: body.windows(15).any(|w| w == b"\"cache_control\""),
        ..Default::default()
    };
    (shape.device_id, shape.session_id, shape.account_uuid) = parse_user_id(&v);
    shape.thread_type =
        v.get("thread").and_then(|t| t.get("type")).and_then(|t| t.as_str()).map(str::to_string);
    // 这条请求自己声明的上一条 message.id（官方主线程每条都带，首轮是 `null`）。
    shape.diag_prev_message_id = v
        .get("diagnostics")
        .and_then(|d| d.get("previous_message_id"))
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .map(str::to_string);

    // system：块数、总长、首块（billing header）、末两块。
    if let Some(sys) = v.get("system") {
        let texts: Vec<&str> = match sys {
            Value::String(s) => vec![s.as_str()],
            Value::Array(blocks) => {
                blocks.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect()
            }
            _ => vec![],
        };
        shape.system_blocks = texts.len();
        shape.system_chars = texts.iter().map(|t| js_len(t)).sum();
        if let Some(first) = texts.first() {
            shape.sys0_len = js_len(first);
            shape.sys0_hash = sha256_hex(first.as_bytes());
            if first.starts_with("x-anthropic-billing-header:") {
                let field = |name: &str| -> Option<String> {
                    first
                        .split(';')
                        .map(str::trim)
                        .find_map(|kv| kv.strip_prefix(name))
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                };
                shape.cc_prompt_id = field("cc_prompt_id=");
                shape.cc_prev_req = field("cc_prev_req=");
                shape.turn_origin = field("cc_turn_origin=");
                shape.is_subagent = first.contains("cc_is_subagent=true");
                shape.sdk = field("cc_entrypoint=").as_deref() == Some("sdk-cli");
            }
        }
        let n = texts.len();
        if n >= 2 {
            shape.static_len = js_len(texts[n - 2]);
        }
        if let Some(last) = texts.last() {
            shape.dynamic_len = js_len(last);
            shape.dynamic_hash = sha256_hex(last.as_bytes())[..12].to_string();
        }
        if let Value::Array(blocks) = sys
            && blocks.len() > 1
        {
            shape.omitted_bytes += serde_json::to_string(&blocks[1..]).map_or(0, |j| js_len(&j));
        }
        if texts.iter().any(|t| t.contains("auto mode is active")) {
            shape.permission_mode = "auto";
        }
        shape.title = texts.iter().any(|t| t.contains("You are naming a coding session"));
        shape.rename = texts.iter().any(|t| t.contains("Generate a short kebab-case name"));
    }

    // tools：数量、JSON 长度、逐个长度表（整个工具对象的紧凑 JSON 长度：抓包里 Agent 3078 /
    // Bash 2352 与 `cap/2.1.260-2/00057` 逐个对得上）、hash、deferred 数。
    //
    // `toolSchemasHash` 是**长度表那串 JSON** 的 sha256 前 12 位，而不是工具数组的：无工具时
    // 官方报 `44136fa355b3`，正是 sha256("{}") 的前缀（`cap/2.1.260-2` 标题生成那条）。
    let mut lens = Map::new();
    if let Some(tools) = v.get("tools").and_then(|t| t.as_array()) {
        shape.tools_count = tools.len();
        shape.omitted_bytes += serde_json::to_string(tools).map_or(0, |j| js_len(&j));
        for t in tools {
            // 延迟加载的占位工具计入 `toolsCount` / `deferredToolsCount`，但不进长度表：官方
            // 16 个工具的表只有 15 项，`toolsCharLength` 也不含它（差的 204 正是占位那条）。
            if t.get("defer_loading").and_then(|d| d.as_bool()) == Some(true) {
                shape.deferred_tools += 1;
                continue;
            }
            if let Some(name) = t.get("name").and_then(|n| n.as_str()) {
                if name.starts_with("mcp__") {
                    shape.mcp_tools += 1;
                }
                if name == "ToolSearch" {
                    shape.has_tool_search = true;
                }
                lens.insert(name.to_string(), Value::from(js_len(&t.to_string())));
            }
        }
    }
    // `toolsCharLength` 是各工具长度之和（不含数组的方括号与逗号）：`cap/2.1.260-2` 74633。
    shape.tools_chars = lens.values().filter_map(|v| v.as_u64()).sum::<u64>() as usize;
    shape.tool_lens = Value::Object(lens).to_string();
    shape.tools_hash = sha256_hex(shape.tool_lens.as_bytes())[..12].to_string();

    // messages：条数、文本量、图片/文档、末条是否新输入、system 角色条数、续轮的工具调用。
    if let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) {
        shape.messages_len = msgs.len();
        shape.after_last_assistant = msgs
            .iter()
            .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .map_or(0, |i| msgs.len() - 1 - i);
        let has_breakpoint = |m: &Value| {
            m.get("content")
                .and_then(|c| c.as_array())
                .is_some_and(|b| b.iter().any(|x| x.get("cache_control").is_some()))
        };
        if let Some(bp) = msgs.iter().rposition(has_breakpoint) {
            let tail = &msgs[bp + 1..];
            shape.tail_messages = tail.len();
            shape.tail_tokens_est = tail.iter().map(tail_tokens_est).sum();
            shape.tail_all_system = !tail.is_empty()
                && tail.iter().all(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"));
        }
        for m in msgs {
            if let Some(c) = m.get("content") {
                walk_content(c, &mut shape);
            }
            match m.get("role").and_then(|r| r.as_str()) {
                Some("system") => shape.api_system_messages += 1,
                Some("assistant") => shape.assistant_messages += 1,
                _ => {}
            }
        }
        // `estimatedInputTokens`：opus/sonnet/fable 按 3 字符一个 token 向上取整（`cap/2.1.260-2`
        // 与 `cap/2.1.260-1` 六条全部精确相等：14203→4735、14422→4808、15163→5055、17539→5847、
        // 12938→4313、14051→4684；2.1.258 那版会多 1–2，不去模仿旧版）；haiku 按 4 字符一个
        // token 四舍五入（标题那条 221 → 55）。
        shape.estimated_tokens = estimate_tokens(&shape.model, shape.input_text_chars);
        // 2.1.280 起 auto 模式的说明不在 system 里，而是作为 system-reminder 挂在消息里，
        // 退出时再追一条「Exited Auto Mode」；以最后出现的那条为准（`cap/2.1.280`：00021
        // 只有进入 → auto，00038 / 00065 先进入后退出 → default，与事件里的 permissionMode
        // 逐条一致）。
        if let Some(auto) = auto_mode_of_messages(msgs) {
            shape.permission_mode = if auto { "auto" } else { "default" };
            shape.auto_marker = true;
        }
        (shape.new_prompt, shape.prompt_len) = last_is_new_prompt(msgs);
        // 客户端注入的一轮（同伴会话的消息、后台任务通知）：官方报的是里面那个元素的长度，不含
        // 「Another Claude session sent a message:」与系统通知那段前言（`cap/auto-2.1.285-20260930`
        // 四条：1156 / 1140 / 482 / 1814）。
        if shape.new_prompt
            && let (_, Some(last)) = last_two_non_system(msgs)
        {
            let text = match last.get("content") {
                Some(Value::String(s)) => Some(s.as_str()),
                Some(Value::Array(b)) => {
                    b.iter().rev().find_map(|x| x.get("text").and_then(|t| t.as_str()))
                }
                _ => None,
            };
            if let Some(t) = text {
                for tag in ["agent-message", "task-notification"] {
                    let close = format!("</{tag}>");
                    if let (Some(a), Some(b)) = (t.find(&format!("<{tag}")), t.find(&close)) {
                        if b > a {
                            shape.prompt_len = js_len(&t[a..b + close.len()]);
                        }
                        break;
                    }
                }
            }
        }
        if shape.new_prompt
            && let (_, Some(last)) = last_two_non_system(msgs)
        {
            shape.rejected = tool_results_of(last)
                .into_iter()
                .filter(|r| r.is_error && r.head.trim_start().starts_with(USER_REJECTED_TOOL_USE))
                .collect();
        }
        if shape.new_prompt
            && let Some(last) =
                msgs.iter().rev().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        {
            let first = match last.get("content") {
                Some(Value::String(s)) => Some(s.as_str()),
                Some(Value::Array(b)) => {
                    b.iter().find_map(|x| x.get("text").and_then(|t| t.as_str()))
                }
                _ => None,
            }
            .map(str::trim_start);
            shape.bash_input = first.is_some_and(|t| t.starts_with("<bash-input>"));
            if let Some(Value::Array(blocks)) = last.get("content") {
                shape.merged_notifications = blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .filter(|t| t.contains("[SYSTEM NOTIFICATION - NOT USER INPUT]"))
                    .filter_map(|t| {
                        let start = t.find("<task-notification>")?;
                        let end = t[start..].find("</task-notification>")? + start;
                        Some(js_len(&t[start..end + "</task-notification>".len()]))
                    })
                    .collect();
            }
            if shape.bash_input {
                let texts: Vec<&str> = match last.get("content") {
                    Some(Value::Array(b)) => {
                        b.iter().filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect()
                    }
                    Some(Value::String(s)) => vec![s.as_str()],
                    _ => vec![],
                };
                let between = |open: &str, close: &str| {
                    texts.iter().find_map(|t| {
                        let (_, rest) = t.split_once(open)?;
                        Some(rest.split_once(close).map_or(rest, |(x, _)| x).to_string())
                    })
                };
                if let Some(cmd) = between("<bash-input>", "</bash-input>") {
                    let out = between("<bash-stdout>", "</bash-stdout>").unwrap_or_default();
                    shape.bash_typed = Some((cmd, js_len(&out)));
                }
            }
            shape.command_skill = first
                .and_then(|t| t.strip_prefix("<command-message>"))
                .and_then(|t| t.split_once("</command-message>"))
                .map(|(name, _)| name.trim().to_string())
                .filter(|n| !n.is_empty());
        }
        if !shape.new_prompt {
            shape.tool_uses = tool_uses_of(msgs);
            if shape.tool_uses.is_empty() {
                shape.orphan_results = orphan_results_of(msgs);
            }
        }
        if let (_, Some(last)) = last_two_non_system(msgs)
            && last.get("role").and_then(|r| r.as_str()) == Some("user")
        {
            let text = match last.get("content") {
                Some(Value::String(s)) => Some(s.as_str()),
                Some(Value::Array(b)) => {
                    b.iter().rev().find_map(|x| x.get("text").and_then(|t| t.as_str()))
                }
                _ => None,
            };
            shape.suggestion =
                text.is_some_and(|t| t.trim_start().starts_with("[SUGGESTION MODE:"));
            shape.compact = text
                .is_some_and(|t| t.trim_start().starts_with("CRITICAL: Respond with TEXT ONLY"));
            // `/btw` 的提醒在问句**前面**那块（`cap/auto-2.1.285-20260930/00238`），末块是问句本身。
            shape.side_question = match last.get("content") {
                Some(Value::Array(b)) => b.iter().any(|x| {
                    x.get("text")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t.contains("This is a side question from the user"))
                }),
                _ => false,
            };
            shape.away_summary = text.is_some_and(|t| {
                t.trim_start().starts_with("The user stepped away and is coming back.")
            });
        }
        if msgs.len() == 1
            && shape.tools_count == 0
            && v.get("max_tokens").and_then(|m| m.as_u64()) == Some(1)
        {
            let text = match msgs[0].get("content") {
                Some(Value::String(s)) => Some(s.as_str()),
                Some(Value::Array(b)) if b.len() == 1 => b[0].get("text").and_then(|t| t.as_str()),
                _ => None,
            };
            shape.quota_probe = text == Some("quota");
        }
        shape.model_validation = shape.system_blocks > 0
            && shape.tools_count == 0
            && v.get("max_tokens").and_then(|m| m.as_u64()) == Some(1);
        shape.web_fetch_page = shape.tools_count == 0
            && msgs.len() == 1
            && match msgs[0].get("content") {
                Some(Value::String(s)) => s.trim_start().starts_with("Web page content:"),
                Some(Value::Array(b)) => b
                    .first()
                    .and_then(|x| x.get("text"))
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.trim_start().starts_with("Web page content:")),
                _ => false,
            };
    }
    shape.web_search_tool = v.get("tools").and_then(|t| t.as_array()).is_some_and(|t| {
        matches!(t.as_slice(), [tool] if tool.get("name").and_then(|n| n.as_str()) == Some("web_search")
            && tool.get("type").and_then(|x| x.as_str()).is_some_and(|x| x.starts_with("web_search_")))
    });
    // 客户端自己写的权限模式（auto / plan 模式的请求带 `safeguards`，default 模式不带）。
    if let Some(mode) = v
        .get("safeguards")
        .and_then(|s| s.as_array())
        .and_then(|s| {
            s.iter().find_map(|g| g.get("classifier_context")?.get("permission_mode")?.as_str())
        })
        .and_then(permission_mode_name)
    {
        shape.permission_mode = mode;
        shape.permission_declared = true;
    }
    // 缓存断点的 ttl：任一处断点写了 1h 就算 1h（工具入参里的同名字段不算）。
    shape.cache_ttl_1h = crate::proxy::has_cache_ttl_1h(&v);
    // 是不是 git 仓库、开场时工作区干不干净（`gitStatus` 那段的 `Status:`）。
    let find = |mark: &[u8]| body.windows(mark.len()).position(|w| w == mark);
    if let Some(at) = find(b"Is a git repository: ") {
        shape.git_repo = Some(body[at + 21..].starts_with(b"true"));
    }
    if let Some(at) = find(b"gitStatus") {
        let rest = &body[at..];
        const STATUS: &[u8] = b"\\n\\nStatus:\\n";
        if let Some(st) = rest.windows(STATUS.len()).position(|w| w == STATUS) {
            let after = &rest[st + STATUS.len()..];
            shape.git_dirty = Some(!after.starts_with(b"(clean)") && !after.starts_with(b"\\n"));
        }
    }
    // 工作目录：环境那段在 system 或首条用户消息的提醒里，原文里是一行。
    const CWD_MARK: &[u8] = b"Primary working directory: ";
    if let Some(at) = body.windows(CWD_MARK.len()).position(|w| w == CWD_MARK) {
        let rest = &body[at + CWD_MARK.len()..];
        let end = rest
            .windows(2)
            .position(|w| w == b"\\n" || w[0] == b'"')
            .unwrap_or(rest.len().min(512));
        shape.cwd = std::str::from_utf8(&rest[..end])
            .ok()
            .map(str::trim)
            .filter(|p| p.starts_with('/'))
            .map(str::to_string);
    }
    Some(shape)
}
