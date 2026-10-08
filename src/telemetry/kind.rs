//! 请求在会话里的角色（[`Kind`]），以及按角色、版本、模型做的几样判定。

use super::*;

/// 这条请求在会话里扮演的角色，决定 `querySource` 与要不要发 turn 级事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// 主线程对话：带工具、非子代理。发完整事件链。
    Main,
    /// 子代理（billing header 里 `cc_is_subagent=true`）。
    Subagent,
    /// 一轮结束后客户端自己发的「猜下一句」请求：带完整工具与上下文，末条用户消息以
    /// `[SUGGESTION MODE:` 开头（`cap/2.1.260-2/00063`）。
    Suggestion,
    /// 会话标题生成：haiku、无工具、system 里有「You are naming a coding session」
    /// （`cap/2.1.260-2/00058`）。
    Title,
    /// 其余无工具的辅助调用（未识别的那类）。
    Helper,
    /// 子代理的摘要请求：子代理跑着的时候客户端隔一阵用它的上下文问一句「在干什么」
    /// （`cap/2.1.280/00174`、`cap/2.1.277` 五条），算辅助调用，但挂在子代理那条支线上。
    AgentSummary,
    /// WebFetch 取回网页后，拿一个无工具的 haiku 按提示处理页面内容（`querySource:
    /// web_fetch_apply`，`cap/2.1.285/00125`、`00134`、`00140`、`00144`）。请求头带着发起它的
    /// 子代理的 `agent-id`、`request-class: auxiliary`，与子代理摘要同样的两项；区别是它
    /// **没有工具**、`thinking: disabled`、不带断点。事件形态与标题那类侧查询相同：没有链、
    /// `previousRequestId`、`effort`、`turn_origin`、`queryOverheadMs`，也没有收尾那串。
    WebFetchApply,
    /// 离开一阵再回来时客户端从主线程分叉出来的回顾：带主线程那套工具与上下文，末条用户消息
    /// 是固定的「The user stepped away and is coming back. Recap…」（`querySource: away_summary`，
    /// `cap/2.1.280/00048`、`00088`，`cap/2.1.285/00097`、`00158`）。事件形态同猜下一句：链另起、
    /// 深度 = 主线程末条 + 2，不占输入号、没有 tether，收尾是 stop hook、turn、turn_end、fork
    /// 统计与 `away_summary_generate`。此前它被当成一次新的主线程输入报。
    AwaySummary,
    /// WebSearch 工具另发的那条：haiku、工具只有 `web_search`、强制调它（`querySource:
    /// web_search_tool`，`cap/auto-2.1.285-20260930/00056`）。事件形态同标题那类侧查询（没有链、
    /// 缺标记、没有 `queryOverheadMs`），权限模式跟主线程；工具搜索判定报 `no_tools_in_request`，
    /// 也没有工具集核对与 git 探测。此前它被当成一次新的主线程输入报，还顶掉了主线程的线程底本，
    /// 下一条续轮整个补错。
    WebSearchTool,
    /// 规划模式收尾时给会话起名（`querySource: rename_generate_name`，`00166`）：同标题，收尾是
    /// `tengu_agent_name_set{source: auto}`。
    RenameName,
    /// `/model` 选完模型后的「Hi」探测（`querySource: model_validation`，`00230` 等四条）：官方
    /// 只报一条字段极少的 `tengu_api_success`，别的一条都没有。
    ModelValidation,
    /// `/btw` 插问（`querySource: side_question`，`00238`）：从主线程分叉，链另起、深度 1，
    /// 不写缓存；收尾 stop hook、缓存诊断（`unavailable`）、turn、fork 统计、turn_end。
    SideQuestion,
    /// `/compact`（`request-class: compaction`，`querySource: compact`，`00175`）：同插问，链另起、
    /// 深度 1；收尾 stop hook、turn、turn_end、fork 统计（`reactive-compact`），再是压缩收尾那几条。
    Compact,
}

impl Kind {
    /// 侧查询（标题生成、页面处理、起名）与分叉出来的插问、压缩按 `default` 权限模式跑，
    /// 不管主线程是不是 auto（`cap/auto-2.1.285-20260930`：auto 会话里的 `/compact` 也报
    /// default）；WebSearch 那条跟主线程（同一会话报 auto）。
    pub(super) fn runs_in_default_mode(self) -> bool {
        matches!(
            self,
            Kind::Title
                | Kind::Helper
                | Kind::WebFetchApply
                | Kind::RenameName
                | Kind::ModelValidation
                | Kind::SideQuestion
                | Kind::Compact
        )
    }

    pub(super) fn query_source(self) -> &'static str {
        match self {
            Kind::Main => "repl_main_thread",
            Kind::Subagent => "agent:builtin:general-purpose",
            Kind::Suggestion => "prompt_suggestion",
            Kind::Title => "generate_session_title",
            Kind::Helper => "compact",
            Kind::AgentSummary => "agent_summary",
            Kind::WebFetchApply => "web_fetch_apply",
            Kind::AwaySummary => "away_summary",
            Kind::WebSearchTool => "web_search_tool",
            Kind::RenameName => "rename_generate_name",
            Kind::ModelValidation => "model_validation",
            Kind::SideQuestion => "side_question",
            Kind::Compact => "compact",
        }
    }
    pub(super) fn category(self) -> &'static str {
        match self {
            Kind::Main => "main",
            Kind::Subagent => "subagent",
            _ => "auxiliary",
        }
    }
    /// 带 `queryChainId` / `queryDepth` 的那几类；标题那类查询没有链。
    pub(super) fn has_chain(self) -> bool {
        matches!(
            self,
            Kind::Main
                | Kind::Subagent
                | Kind::Suggestion
                | Kind::AgentSummary
                | Kind::AwaySummary
                | Kind::SideQuestion
                | Kind::Compact
        )
    }
    /// 带完整系统提示词（`systemPromptSource` / `snapshotHash`、规范化前后的消息数那套算法）。
    pub(super) fn has_boundary(self) -> bool {
        self.has_chain()
    }
    /// 系统提示词里有静态/动态分界（`tengu_sysprompt_boundary_found`）。子代理那份没有：
    /// 报两条 `tengu_sysprompt_missing_boundary_marker{promptBlockCount: 6}`（`cap/2.1.277`
    /// 自定义子代理与 `cap/2.1.280` Explore 子代理共 58 次请求，全是 6）。
    pub(super) fn has_boundary_marker(self) -> bool {
        matches!(
            self,
            Kind::Main | Kind::Suggestion | Kind::AwaySummary | Kind::SideQuestion | Kind::Compact
        )
    }
    /// 从主线程分叉出来、不写缓存的那几类（`skipCacheWrite: true`）。
    pub(super) fn is_fork(self) -> bool {
        matches!(
            self,
            Kind::Suggestion
                | Kind::AgentSummary
                | Kind::AwaySummary
                | Kind::SideQuestion
                | Kind::Compact
        )
    }
    /// 没有链、没有 `queryOverheadMs` 的一次性侧查询（标题那一类）。
    pub(super) fn is_side_query(self) -> bool {
        matches!(
            self,
            Kind::Title
                | Kind::Helper
                | Kind::WebFetchApply
                | Kind::WebSearchTool
                | Kind::RenameName
        )
    }
    /// 子代理与它的摘要请求：事件挂在子代理那条支线上。
    pub(super) fn is_agent(self) -> bool {
        matches!(self, Kind::Subagent | Kind::AgentSummary)
    }
}

/// 子代理请求的 `querySource`：内置的是 `agent:builtin:<类型>`（`cap/2.1.280` Explore），
/// 自定义的是 `agent:custom`（`cap/2.1.277`，请求头 `agent-type: custom`）。没有请求头
/// （2.1.277 之前的客户端）按内置的 general-purpose 报。
pub(super) fn agent_query_source(agent_type: Option<&str>) -> String {
    match agent_type.filter(|t| !t.is_empty()) {
        Some("custom") => "agent:custom".to_string(),
        Some(t) => format!("agent:builtin:{t}"),
        None => "agent:builtin:general-purpose".to_string(),
    }
}

/// `2.1.260` 这种三段版本号是否不低于 `min`。
pub(super) fn version_at_least(v: &str, min: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parse(v) >= parse(min)
}

/// 2.1.285 起 `tengu_attachments` 在 `attachment_types` 之后多报每种附件的 token 估算（**字符串**
/// 数组，与 types 一一对应），有 `skill_listing` 时再报 `skill_listing_is_full: true`，最后是
/// 紧随其后那条请求的 `query_source`（`cap/2.1.285` 二十条，2.1.280 的十六条都没有）。
///
/// 估算按类型取抓包里的数：`total_tokens_reminder` 二十条恒为 23；其余随内容变，取首轮那条
/// （`00040`）的值——代理看不到客户端注入的附件正文，模板里的附件类型本来也是照抓包写死的。
/// `model` 那条随模型名变（`cap/auto-2.1.285-20260930`：opus 62、opus[1m] 71、sonnet 64、fable 63、
/// haiku 70，交互式与 `-p` 相同）。`-p`（`sdk`）的附件正文是 SDK 那套：环境 75、MCP 说明 1080，
/// 延迟工具 / 子代理清单 / 技能清单 haiku 是 1001 / 489 / 2168（`00448` 等四个会话），其余模型
/// 977 / 435 / 3406（`00562`、`00622` 等五个会话）。
pub(super) fn fill_attachment_estimates(
    meta: &mut Value,
    query_source: &str,
    sdk: bool,
    model: &str,
) {
    let Some(obj) = meta.as_object_mut() else { return };
    let types: Vec<String> = obj
        .get("attachment_types")
        .and_then(|t| t.as_array())
        .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    // haiku-5-5（2.1.293）的附件与 opus 同一套（`cap/auto-2.1.293-20261008-full` 的 haiku-5-5 会话：
    // 延迟工具 / 子代理 / 技能清单都与 opus 相同），只有模型那条是它自己的 63；长描述那套只剩 haiku-4.5。
    let haiku_5_5 = crate::config::cc_haiku_is_5_5_family(model);
    let haiku = model.contains("haiku") && !haiku_5_5;
    let model_estimate = if haiku_5_5 {
        "63"
    } else if haiku {
        "70"
    } else if model.contains("opus") && model.ends_with("[1m]") {
        "71"
    } else if model.contains("sonnet") {
        "64"
    } else if model.contains("fable") {
        "63"
    } else {
        "62"
    };
    let estimate = |t: &str| match (t, sdk) {
        ("model", _) => model_estimate,
        ("environment", true) => "75",
        ("deferred_tools_delta", true) => {
            if haiku {
                "1001"
            } else {
                "977"
            }
        }
        ("agent_listing_delta", true) => {
            if haiku {
                "489"
            } else {
                "435"
            }
        }
        ("mcp_instructions_delta", true) => "1080",
        ("skill_listing", true) => {
            if haiku {
                "2168"
            } else {
                "3406"
            }
        }
        (t, _) => match t {
            "environment" => "103",
            "deferred_tools_delta" => "1542",
            "agent_listing_delta" => "714",
            "mcp_instructions_delta" => "1576",
            "skill_listing" => "3690",
            "auto_mode" => "31",
            "auto_mode_exit" => "15",
            "task_reminder" => "13",
            _ => "23",
        },
    };
    let estimates: Vec<Value> = types.iter().map(|t| json!(estimate(t))).collect();
    obj.insert("attachment_token_estimates".into(), Value::Array(estimates));
    if types.iter().any(|t| t == "skill_listing") {
        obj.insert("skill_listing_is_full".into(), json!(true));
    }
    obj.insert("query_source".into(), json!(query_source));
}

/// 模型的默认 effort（`tengu_api_success.default_effort_level`）：opus-5-5、sonnet-5-5 与 haiku-5-5
/// 是 `medium`，opus-4-7 是 `xhigh`，其余带 effort 的都是 `high`（`cap/2.1.285` 11 个模型、
/// `cap/2.1.280` 的 opus-5-5[1m]；haiku-5-5 见 `cap/auto-2.1.293-20261008-full` 18 条 `medium` /
/// `is_default_effort: true`）。模拟请求自己发的是 high，那是另一回事，这里只报模型的默认档。
pub(super) fn default_effort_of(model: &str) -> &'static str {
    if model.starts_with("claude-opus-5-5")
        || model.starts_with("claude-sonnet-5-5")
        || model.starts_with("claude-haiku-5-5")
    {
        "medium"
    } else if model.starts_with("claude-opus-4-7") {
        "xhigh"
    } else {
        "high"
    }
}

/// 工具跑完那条 `tengu_feature_ok` 的名字：`tool_` + 工具名的 snake_case（`WebFetch` →
/// `tool_web_fetch`、`SubagentHandback` → `tool_subagent_handback`）。
pub(super) fn tool_feature_name(tool: &str) -> String {
    let mut out = String::from("tool_");
    for (i, c) in tool.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// 在 `after` 那个键后面依次插入 `pairs`（保序 Map）；`after` 不在就追加到末尾。给按版本
/// 多出来的字段落到官方位置用——`json!` 字面量里没法按条件插键。
pub(super) fn insert_after(v: &mut Value, after: &str, pairs: Vec<(&str, Value)>) {
    let Some(obj) = v.as_object_mut() else { return };
    let at = obj.keys().position(|k| k == after).map_or(obj.len(), |i| i + 1);
    for (i, (k, val)) in pairs.into_iter().enumerate() {
        obj.shift_insert(at + i, k.to_string(), val);
    }
}

/// billing header 的 `cc_turn_origin` 换成事件里的写法：下划线换连字符
/// （`task_notification` → `task-notification`，`cap/2.1.280/00180` 与同批事件），
/// `human` / `peer` 原样。
pub(super) fn turn_origin_of(header: &str) -> String {
    header.replace('_', "-")
}

/// `tengu_api_success.snapshotHash`：12 位 hex，整个会话恒定、换会话就变（`cap/2.1.277`
/// 与 `cap/2.1.280` 两个会话 `system[1]`、`system[2]` 逐字相同，hash 却不同，只有末块
/// 动态段不同）。官方算的是客户端内部那份快照，代理看不到；取动态段的 hash 再加盐派生，
/// 满足「会话内恒定、会话间不同」，又不与任何一块的原始 sha256 撞上。
pub(super) fn snapshot_hash_of(shape: &RequestShape) -> String {
    sha256_hex(format!("snapshot:{}", shape.dynamic_hash).as_bytes())[..12].to_string()
}

/// tether 引擎把模型固定成无状态发送的几个模型（`sentThreadType: "none"`）：
///
/// - `cap/2.1.277`：fable-5-1 为 true、opus-5 为 false；
/// - `cap/2.1.280`：fable-5-1 与 opus-5-5**[1m]** 为 true，sonnet / haiku 为 false；
/// - `cap/2.1.285`（11 个模型）：只有 fable-5-1 为 true——fable-5 为 false，**不带 [1m] 的**
///   opus-5-5 也是 false（那条实际建了线程，`00030` 体里 `thread: create`）。
///
/// 故按具体模型判：fable-5-1，与 1M 上下文的 opus-5-5。`model` 是带 `[1m]` 后缀的展示名。
pub(super) fn model_held_stateless(model: &str) -> bool {
    model.starts_with("claude-fable-5-1")
        || (model.starts_with("claude-opus-5-5") && model.ends_with("[1m]"))
}

/// 按上一条请求判这条的 tether 决策，规则取自 `cap/2.1.277`（主线程 11 条、子代理 26 条）
/// 与 `cap/2.1.280`（主线程 7 条）：
///
/// - 线程里没有上一条 → `create/first_request`，`prevMessageCount` 0；
/// - 模型 / beta / effort / 工具表 / thinking 任一变了 → `create/config_changed`，对应的
///   `changed*` 置真，线程轮数归 1、`deltaMessageCount` 0；
/// - 否则消息只多不少 → `continue/append`，轮数 +1、`deltaMessageCount` = 新增条数。
///
/// `changedLatchedHeaders` 只在**切到 haiku** 时为真（两份抓包切到 haiku 三次全是，
/// 从 haiku 切走两次都不是）；`changedSystem` 抓包里恒为 false（system 的动态段每轮都在
/// 变，官方显然不把它算进去）。配置没变、消息却变少（回退、压缩）抓包里没见过，按
/// `create` + `unclaimedHistoryChange` 报，原因名是推测的。
pub(super) fn tether_decide(
    prev: Option<&TetherThread>,
    shape: &RequestShape,
    model: &str,
    betas: &str,
    is_main: bool,
) -> Tether {
    let fresh = |decision, reason, prev_messages| Tether {
        decision,
        reason,
        changed_model: false,
        changed_tools: false,
        changed_betas: false,
        changed_latched: false,
        changed_thinking: false,
        changed_effort: false,
        turns: 1,
        prev_messages,
        delta: 0,
        echo: false,
    };
    let Some(p) = prev else { return fresh("create", "first_request", 0) };
    let changed_model = p.model != model;
    let changed_tools = p.tools_hash != shape.tools_hash;
    let changed_betas = p.betas != betas;
    let changed_thinking = p.thinking_type != shape.thinking_type;
    let changed_effort = p.effort != shape.effort;
    if changed_model || changed_tools || changed_betas || changed_thinking || changed_effort {
        return Tether {
            changed_model,
            changed_tools,
            changed_betas,
            changed_latched: model.contains("haiku") && !p.model.contains("haiku"),
            changed_thinking,
            changed_effort,
            echo: is_main,
            ..fresh("create", "config_changed", p.messages)
        };
    }
    if shape.messages_len >= p.messages {
        return Tether {
            turns: p.turns + 1,
            delta: shape.messages_len - p.messages,
            echo: true,
            ..fresh("continue", "append", p.messages)
        };
    }
    Tether { echo: is_main, ..fresh("create", "history_changed", p.messages) }
}

/// 按请求头与体的形态判出这条请求在会话里的角色。
pub(super) fn classify_kind(call: &ApiCall, shape: &RequestShape, is_agent: bool) -> Kind {
    if is_agent {
        // 子代理的摘要请求（「这个子代理刚才在干什么」）：`request-class: auxiliary`、
        // 带同一个 `agent-id`（`cap/2.1.280/00174`），事件报 `querySource: agent_summary`。
        // 同样带着子代理 `agent-id` 的 WebFetch 页面处理没有工具（`cap/2.1.285/00125`），
        // 摘要请求带着子代理那套工具（`00135`）。
        if call.agent.request_class.as_deref() == Some("auxiliary") {
            if shape.tools_count == 0 { Kind::WebFetchApply } else { Kind::AgentSummary }
        } else {
            Kind::Subagent
        }
    } else {
        // 官方每条请求都带 `x-claude-code-request-class`（`main` / `subagent` / `auxiliary` /
        // `compaction`）；再按体的形态细分辅助调用。没有这个头（模拟路径、老客户端）只看形态。
        let class = call.agent.request_class.as_deref();
        let tools = shape.tools_count;
        if class == Some("compaction") || (shape.compact && tools > 0) {
            Kind::Compact
        } else if shape.model_validation {
            Kind::ModelValidation
        } else if shape.away_summary && tools > 0 {
            Kind::AwaySummary
        } else if shape.side_question && tools > 0 {
            Kind::SideQuestion
        } else if shape.suggestion && tools > 0 {
            Kind::Suggestion
        } else if shape.title && tools == 0 {
            Kind::Title
        } else if shape.rename && tools == 0 {
            Kind::RenameName
        } else if shape.web_search_tool {
            Kind::WebSearchTool
        } else if shape.web_fetch_page {
            Kind::WebFetchApply
        } else if tools == 0 || class == Some("auxiliary") {
            // 认不出的辅助调用：宁可当无链的侧查询，也不能当主线程——那会换一轮输入、顶掉
            // 主线程的线程底本。
            Kind::Helper
        } else {
            Kind::Main
        }
    }
}
