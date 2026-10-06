//! 静态事件模板（会话启动 / 每轮输入 / 每轮收尾）

use super::*;

/// 模板里的一条事件，见 `assets/cc_telemetry_template.json` 顶部的说明。
#[derive(Debug, serde::Deserialize)]
pub(super) struct TplEvent {
    /// 相对锚点的毫秒偏移。
    pub(super) off: i64,
    /// `event`（`ClaudeCodeInternalEvent`）或 `growth`（`GrowthbookExperimentEvent`）。
    #[serde(rename = "type")]
    pub(super) kind: String,
    /// 事件名；growth 那类是 `experiment_id`。
    pub(super) name: String,
    #[serde(default)]
    pub(super) meta: Value,
    /// 连续重复几条（`tengu_skill_loaded` 那串）。
    #[serde(default)]
    pub(super) repeat: Option<u32>,
    /// growth：`experiment_metadata.feature_id`。
    #[serde(default)]
    pub(super) feature: Option<String>,
    /// growth：`variation_id`。
    #[serde(default)]
    pub(super) var: Option<i64>,
    /// 这条事件的 `additional_metadata` 阶段，见 [`MetaStage`]。缺省即
    /// [`MetaStage::Prompt`]（三项都写），模板里只给另外两阶段的事件标了这个字段。
    #[serde(default)]
    pub(super) stage: MetaStage,
}

/// `additional_metadata` 的三个阶段。官方在会话早期**不写**后两项，逐阶段加上去
/// （`cap/2.1.260-2/00016` 一批 194 条：57 条带 `renderer_mode`，其中 34 条带
/// `cc_prompt_id`，其余 137 条只有 `subscription_type`）。
///
/// 给一条 `tengu_cli_flags`（进程刚起来、界面还没画、用户一个字都没输）写上
/// `renderer_mode:"default"` 和一个 `cc_prompt_id`，是官方从不产生的组合，而这类事件
/// 一个会话有近百条。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum MetaStage {
    /// 只有 `subscription_type`：进程启动到界面起来之间。
    Startup,
    /// `renderer_mode` + `subscription_type`：界面起来了，用户还没提交。
    Renderer,
    /// 三项齐全：用户已经提交，这一轮有 `cc_prompt_id` 了。
    #[default]
    Prompt,
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct Template {
    /// 进程启动到第一次提交之间的那串（锚点：首条 `tengu_api_query`）。
    pub(super) startup: Vec<TplEvent>,
    /// 会话第一次用户输入围绕 `tengu_api_query` 的那串。
    pub(super) prompt: Vec<TplEvent>,
    /// 第二次起每次用户输入的那串（少了首轮才有的样式/记忆加载，多了几条附件计算）。
    pub(super) prompt_next: Vec<TplEvent>,
    /// 只在会话第一次输入时多出来的那几条（首次拉 bootstrap / MCP 配置等）。
    pub(super) first_prompt: Vec<TplEvent>,
    /// 每轮结束围绕 `tengu_api_success` 的那串。
    pub(super) turn: Vec<TplEvent>,
    /// 会话第一轮结束后的版本检查那串。
    pub(super) first_turn: Vec<TplEvent>,
    /// 会话跑久了才有的后台任务（2.1.285：插件自动更新检查、保留期清理），`off` 相对进程启动；
    /// 会话里后来的请求越过那个时刻才补发，见 `background_done`。2.1.260 那份没有。
    #[serde(default)]
    pub(super) background: Vec<TplEvent>,
}

pub(super) static TEMPLATE: std::sync::LazyLock<Template> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("../assets/cc_telemetry_template.json"))
        .expect("assets/cc_telemetry_template.json must parse")
});

/// 2.1.285 的模板（`cap/2.1.285/00040`，整理规则见文件顶部的 `_comment`）。
///
/// 与 2.1.260 那份（[`TEMPLATE`]）差得很多，不是逐项改得过来的：启动段 159 条（2.1.260 是 98 条），
/// 多了 `managed_config_ready`、`signed_cache_identity`、`remote_settings_fetch`、
/// `model_catalog_primary` / `model_catalog_compare`、`nonblocking_stdout`、
/// `mcp_tools_commands_loaded`、`settings_watcher_start` 这些，bootstrap / MCP 配置那几条
/// 挪进了界面起来之后、第一次输入之前（2.1.260 在第一次输入之后）；首次输入多了上下文宣告、
/// 提醒折叠、工具入参回显、`artifact_*` 那一串；之后每次输入只剩六条；每轮收尾没有了
/// `tip_shown` 与每轮一次的 `time_shell`。
pub(super) static TEMPLATE_2_1_285: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_285.json"))
            .expect("assets/cc_telemetry_template_2_1_285.json must parse")
    });

/// 2.1.285 `-p` 打印模式的模板（`cap/auto-2.1.285-20260930/00445`、`00448`，整理规则见文件顶部的
/// `_comment`）：进程只跑一轮，启动段连着输入与附件一直到首条 api_query，收尾段一直到进程退出。
pub(super) static TEMPLATE_2_1_285_SDK: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_285_sdk.json"))
            .expect("assets/cc_telemetry_template_2_1_285_sdk.json must parse")
    });

/// 2.1.291 的两份模板（交互式 / `-p`）：以 2.1.285 那两份为底，按
/// `cap/auto-2.1.291-20261006-full` 补齐 meta 键、换掉过期的实验曝光（整理规则见文件顶部的
/// `_comment`，生成脚本是同目录 `_driver/build_tpl_291.py`）。事件条目与先后没变，变的是十几条
/// 事件的 meta：`signed_cache_shadow` 多 `verify_micros`、`tengu_init` 多一串 `has_*` 项目画像、
/// `model_catalog_primary` 去掉等待计时、插件那几条多了来源与改名前的标识、两个工具池变化事件
/// 多了回收保留的计数等。
pub(super) static TEMPLATE_2_1_291: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_291.json"))
            .expect("assets/cc_telemetry_template_2_1_291.json must parse")
    });

/// 2.1.291 `-p` 打印模式的模板，见 [`TEMPLATE_2_1_291`]。
pub(super) static TEMPLATE_2_1_291_SDK: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_291_sdk.json"))
            .expect("assets/cc_telemetry_template_2_1_291_sdk.json must parse")
    });

/// 出站版本用哪一份模板：2.1.291 起用 [`TEMPLATE_2_1_291`]，2.1.285 ~ 2.1.290 用
/// [`TEMPLATE_2_1_285`]（`-p` 打印模式各用对应的 SDK 那份），之前的仍是 2.1.260 那份。
pub(super) fn template_for(version: &str, sdk: bool) -> &'static Template {
    match (version_at_least(version, "2.1.291"), version_at_least(version, "2.1.285"), sdk) {
        (true, _, true) => &TEMPLATE_2_1_291_SDK,
        (true, _, false) => &TEMPLATE_2_1_291,
        (false, true, true) => &TEMPLATE_2_1_285_SDK,
        (false, true, false) => &TEMPLATE_2_1_285,
        _ => &TEMPLATE,
    }
}

/// 官方 Datadog 那份日志只收这几类事件（`cap/2.1.258` 四批 285 条与 `cap/2.1.260-1` 六批
/// 对照：`tengu_feature_ok` 全部、`tengu_api_success`、启动那几条，其余只进 event_logging）。
///
/// `tengu_api_error` 与 `tengu_feature_bad` 抓包里没出现过——那几个会话没有失败请求；
/// 它们在 2.1.260 自己那份 Datadog 白名单里，故一并收（失败收尾那两条事件由
/// [`Telemetry::process`] 直接推 Datadog，不经这张表）。
pub(super) const DD_EVENT_NAMES: &[&str] = &[
    "tengu_feature_ok",
    "tengu_feature_bad",
    "tengu_api_success",
    "tengu_api_error",
    "tengu_started",
    "tengu_timer",
    "tengu_init",
    "tengu_mcp_sdk_generation",
    "tengu_mcp_server_connection_succeeded",
    "tengu_exit",
    "tengu_tool_use_success",
    "tengu_bash_tool_command_executed",
];

/// 这个事件进不进 Datadog：[`DD_EVENT_NAMES`]，加上 2.1.280 起白名单多出来的几项（`cap/2.1.280`、
/// `cap/2.1.285` 的 Datadog 批次都有 `model_catalog_compare`、`policy_limits_fetch`、
/// `policy_limits_cache_state_at_first_prompt`；2.1.285 另有 `tool_use_granted_in_prompt_temporary`）。
pub(super) fn dd_listed(name: &str, version: &str) -> bool {
    DD_EVENT_NAMES.contains(&name)
        || (version_at_least(version, "2.1.280")
            && matches!(
                name,
                "tengu_model_catalog_compare"
                    | "tengu_policy_limits_fetch"
                    | "tengu_policy_limits_cache_state_at_first_prompt"
                    | "tengu_tool_use_granted_in_prompt_temporary"
            ))
}

/// 服务端下发的事件抽样配置（eval 的 `tengu_event_sampling_config`，`"source":"force"`，即对
/// 所有账号生效；`cap/auto-2.1.291-20261006-full/00017` 那份响应）：
///
/// | 抽样率 | 事件 |
/// |---|---|
/// | 0（不发） | `api_before_normalize`、`api_after_normalize`、`query_before_attachments`、`query_after_attachments`、`skill_file_changed` |
/// | 0.01 | `api_cache_breakpoints`、`sysprompt_block`、`sysprompt_boundary_found`、`sysprompt_missing_boundary_marker`、`session_file_read`、`plugin_name_collision` |
///
/// 官方客户端按它在发事件前抽：抽中的那条 meta 末尾多一项 `sample_rate`（同一批抓包里
/// `api_cache_breakpoints` 一个会话 73 次请求只剩 1 条，带着 `"sample_rate":0.01`），没抽中的
/// 整条不发。2.1.285 的抓包（2026-09-30）里这几条每次都发，那时还没下发这份配置——这里只给
/// 2.1.291 起的出站版本用，旧版本的回放与模板不受影响。
const EVENT_SAMPLING_2_1_291: &[(&str, f64)] = &[
    ("tengu_plugin_name_collision", 0.01),
    ("tengu_session_file_read", 0.01),
    ("tengu_skill_file_changed", 0.0),
    ("tengu_api_before_normalize", 0.0),
    ("tengu_api_after_normalize", 0.0),
    ("tengu_query_before_attachments", 0.0),
    ("tengu_query_after_attachments", 0.0),
    ("tengu_api_cache_breakpoints", 0.01),
    ("tengu_sysprompt_block", 0.01),
    ("tengu_sysprompt_boundary_found", 0.01),
    ("tengu_sysprompt_missing_boundary_marker", 0.01),
];

/// 按 [`EVENT_SAMPLING_2_1_291`] 抽这一条：`false` 即不发；抽中且在表里的，meta 末尾补上
/// `sample_rate`。不在表里的、或出站版本早于 2.1.291 的原样放行。
pub(super) fn sample_event(name: &str, version: &str, meta: &mut Value) -> bool {
    if !version_at_least(version, "2.1.291") {
        return true;
    }
    let Some(&(_, rate)) = EVENT_SAMPLING_2_1_291.iter().find(|(n, _)| *n == name) else {
        return true;
    };
    if rate <= 0.0 || rand::random::<f64>() >= rate {
        return false;
    }
    if let Some(o) = meta.as_object_mut() {
        o.insert("sample_rate".into(), json!(rate));
    }
    true
}

/// 模板占位符的取值。
pub(super) struct Subst<'a> {
    pub(super) version: &'a str,
    /// 展示模型名（`claude-opus-5[1m]`）。
    pub(super) model: &'a str,
    /// 用户设置里的模型别名（`opus[1m]`），见 [`model_setting`]。
    pub(super) model_setting: &'a str,
    pub(super) permission_mode: &'a str,
    /// 这个会话是 `--resume` 回来的（`tengu_timer{startup}.resumed`）。
    pub(super) resumed: bool,
    /// 第几次输入（`tengu_file_history_snapshot_success.snapshotCount`）。
    pub(super) prompt_index: u32,
    /// 正文里有延迟加载的工具（`defer_loading: true`）。没有就不发
    /// `tengu_deferred_tools_pool_change`、`tengu_attachments` 里也没有 `deferred_tools_delta`
    /// ——官方 API-key 端（全量声明、无 ToolSearch）正是这个形态（`cap/2.1.258-api/00002`、
    /// `00022`），而订阅端延迟形态两者都有（`cap/2.1.258/00020`、`00032`）。
    pub(super) deferred: bool,
    /// `tengu_tool_search_mode_decision` 的 meta，见 [`tool_search_decision`]。模板里那两条
    /// 的 meta 只是占位，一律以这份覆盖。
    pub(super) tool_search: Value,
    /// `-p` 打印模式（SDK 模板）：附件的 `query_source` 与估算按 SDK 那套报，见
    /// [`fill_attachment_estimates`]。
    pub(super) sdk: bool,
    /// 这个会话的模型是 haiku：2.1.291 起它的工具是长描述那一套，`tengu_context_size` 的工具
    /// 计数与其余三族不同（[`context_tool_counts`]）。
    pub(super) haiku: bool,
    /// 正文里关着的那几个用户可关的工具（[`RequestShape::tools_off`]）。`-p` 不看它：打印模式本来就
    /// 不带 Artifact / SendFeedback（不是用户关的），也没有关掉之后的样本。
    pub(super) tools_off: ToolsOff,
}

/// 2.1.291 `tengu_context_size` 里的 `non_mcp_tools_count` / `non_mcp_tools_tokens`：客户端本地
/// 按全部内建工具（含延迟池里的）估的，随模型族与用户关掉的工具变
/// （`cap/auto-2.1.291-20261006-full` 各会话、`cap/auto-2.1.291-20261006` 关前 / 关后各四个会话）。
/// `-p`（`sdk`）本来就不带 Artifact / SendFeedback，没有关掉之后的样本，照不关的报。
///
/// 三个全关：数 34 → 29（Artifact 连带 `ArtifactComments` / `ArtifactData` 少 3 个，另两个各 1 个），
/// token 13834 → 6983，少 6851；haiku 37 → 32、14305 → 7454，少的同样是 6851。只关其中几个没有
/// 样本，token 按三家工具声明的字符数分摊那 6851（Artifact 一家 34399 + 7438 + 12241，ListAgents
/// 1180，SendFeedback 5537，`cap/auto-2.1.291-20261006-full` 里的声明），三个全关时正好等于抓包值。
fn context_tool_counts(sdk: bool, haiku: bool, off: ToolsOff) -> (u64, u64) {
    let (count, tokens) = match (sdk, haiku) {
        (true, false) => return (26, 6399),
        (true, true) => return (30, 6900),
        (false, false) => (34u64, 13834u64),
        (false, true) => (37, 14305),
    };
    const ARTIFACT: u64 = 6094;
    const LIST_AGENTS: u64 = 133;
    const SEND_FEEDBACK: u64 = 6851 - ARTIFACT - LIST_AGENTS;
    let (mut c, mut t) = (count, tokens);
    if off.artifact {
        c -= 3;
        t -= ARTIFACT;
    }
    if off.list_agents {
        c -= 1;
        t -= LIST_AGENTS;
    }
    if off.send_feedback {
        c -= 1;
        t -= SEND_FEEDBACK;
    }
    (c, t)
}

/// 官方用户关掉某个工具用的环境变量，`set_env_vars` 里按名字排序与已有的并在一起。
fn off_env_vars(off: ToolsOff) -> Vec<&'static str> {
    let mut v = Vec::new();
    if off.artifact {
        v.push("CLAUDE_CODE_DISABLE_ARTIFACT");
    }
    if off.list_agents {
        v.push("CLAUDE_CODE_HARBOR_KITE");
    }
    if off.send_feedback {
        v.push("CLAUDE_CODE_SEND_FEEDBACK");
    }
    v
}

/// 关掉某个工具之后不再发的启动 / 首次输入事件（`cap/auto-2.1.291-20261006` 四族关前 / 关后对照）：
/// Artifact 那三条，与跨会话消息（ListAgents 那一套）的本地套接字绑定。
fn dropped_by(off: ToolsOff, name: &str) -> bool {
    match name {
        "tengu_artifact_five_class_asks"
        | "tengu_artifact_inherited_type_grant"
        | "tengu_artifact_text_variant" => off.artifact,
        "tengu_uds_startup_bind" => off.list_agents,
        _ => false,
    }
}

/// `tengu_tool_search_mode_decision` 的 meta，**跟着正文里实际启用的能力走**。
///
/// 官方三种取值（event_logging 抓包解出 `additional_metadata`）：
///
/// | 正文 | enabled | reason | mcpToolCount |
/// |---|---|---|---|
/// | 声明了 `ToolSearch`（订阅端延迟形态，`cap/2.1.258/00032`、`2.1.270/00020`，同时带 `defer_loading` 占位） | true | `tst_enabled` | 28 / 2，是**会话里** MCP 工具总数，不在正文里 |
/// | 有工具、没有 `ToolSearch`（API-key 端 34 个全量声明，`cap/2.1.258-api/00022`） | false | `not_registered` | 2，正好等于正文里 `mcp__*` 的个数 |
/// | 没有工具（haiku helper，`cap/2.1.260-2/00065`） | false | `no_tools_in_request` | 0 |
///
/// **判「已启用」看的是 `ToolSearch` 本身，不是 `defer_loading` 占位。** 延迟声明只是把工具
/// 藏起来，能不能搜出来靠的是 `ToolSearch` 这条工具；一条只带占位、不带它的请求，模型无处发起
/// 搜索，代理这边也没有替它执行搜索的能力（见 `crate::proxy::cc_tools_core` 为什么不注这一对），
/// 报 `tst_enabled` 就是在宣称一个没有的能力。官方样本里两者总是同时出现，所以这条判据对官方
/// 形态没有区别，只在「半抄」的正文上分得开。工具池那两条事件（`pool_change`、
/// `deferred_tools_delta`）仍跟着 `defer_loading` 声明走——池子是声明出来的，搜索是另一回事。
///
/// 此前只看「有没有工具」，有就报 `tst_enabled`：模拟路径注入的 14 个官方工具与真 CC
/// API-key 端的全量声明都没有 ToolSearch，遥测却在宣称延迟加载已启用——正文与遥测互相矛盾。
///
/// 延迟形态下 `mcpToolCount` 是客户端本地的数，正文里看不到，沿用模板那个 `2`（抓包里
/// 确有 2 的样本）；`not_registered` 只在 API-key 端观察到，订阅端没有「有工具却不延迟」的
/// 官方样本，对模拟路径这是**更自洽**的取值而不是已证的。
pub(super) fn tool_search_decision(
    shape: &RequestShape,
    checked_model: &str,
    agent: bool,
) -> Value {
    let (enabled, reason, mcp) = if shape.has_tool_search {
        (true, "tst_enabled", 2)
    } else if shape.tools_count == 0 || shape.web_search_tool {
        // WebSearch 那条只有一个 server tool，官方同样报「请求里没有工具」（`cap/auto-2.1.285-20260930/00056`）。
        (false, "no_tools_in_request", 0)
    } else if agent {
        // 子代理的工具表里没有 `ToolSearch`（claude-code-guide 只有 Bash / Read / WebFetch /
        // WebSearch）：`mcp_search_unavailable`、`mcpToolCount` 0（`cap/2.1.285` 八条）。带着
        // `ToolSearch` 的子代理（`cap/2.1.280` Explore）走上面那条 `tst_enabled`。
        (false, "mcp_search_unavailable", 0)
    } else {
        (false, "not_registered", shape.mcp_tools)
    };
    json!({
        "enabled": enabled,
        "mode": "tst",
        "reason": reason,
        "checkedModel": checked_model,
        "mcpToolCount": mcp,
        "mcpNonBlocking": false,
        "userType": "external"
    })
}

/// 展示模型名 → 用户设置里的写法：`claude-opus-5[1m]` → `opus[1m]`、`claude-fable-5-1` → `fable`。
pub(super) fn model_setting(display: &str) -> String {
    let bare = display.trim_end_matches("[1m]");
    let family = bare.strip_prefix("claude-").unwrap_or(bare);
    let family = family.split('-').next().unwrap_or(family);
    if display.ends_with("[1m]") { format!("{family}[1m]") } else { family.to_string() }
}

/// 把 meta 里的 `{{…}}` 占位符换成实际值（只动字符串）。
pub(super) fn substitute(v: &Value, s: &Subst<'_>) -> Value {
    match v {
        Value::String(text) if text.contains("{{") => Value::String(
            text.replace("{{version}}", s.version)
                .replace("{{model}}", s.model)
                .replace("{{model_setting}}", s.model_setting)
                .replace("{{permission_mode}}", s.permission_mode),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, s)).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, x)| (k.clone(), substitute(x, s))).collect())
        }
        other => other.clone(),
    }
}

/// 按模板造一串事件（与对应的 Datadog 日志），时间戳 = 锚点 + 偏移。
pub(super) fn emit_template<'a, F>(
    tpl: &[TplEvent],
    anchor: DateTime<Utc>,
    id: &Identity,
    ctx: F,
    resp_model: &str,
    subst: &Subst<'_>,
) -> (Vec<(DateTime<Utc>, Value)>, Vec<Value>)
where
    F: Fn(DateTime<Utc>) -> EventCtx<'a>,
{
    let mut events = Vec::with_capacity(tpl.len());
    let mut dd = Vec::new();
    for e in tpl {
        let t = anchor + chrono::Duration::milliseconds(e.off);
        for _ in 0..e.repeat.unwrap_or(1).max(1) {
            if e.kind == "growth" {
                events.push((
                    t,
                    id.growth_event(
                        t,
                        &e.name,
                        e.var.unwrap_or(0),
                        e.feature.as_deref().unwrap_or(&e.name),
                        subst.version,
                    ),
                ));
                continue;
            }
            // 2.1.280 起每轮的「猜下一句」由代码按这一轮的缓存情况报（见 `turn` 收尾那里），
            // 2.1.260 模板首轮那条 `early_conversation` 不再用。
            if e.name == "tengu_prompt_suggestion" && version_at_least(subst.version, "2.1.280") {
                continue;
            }
            // 延迟工具池相关的事件跟着正文走，见 [`Subst::deferred`]。
            if e.name == "tengu_deferred_tools_pool_change" && !subst.deferred {
                continue;
            }
            let mut meta = substitute(&e.meta, subst);
            if e.name == "tengu_tool_search_mode_decision" {
                meta = subst.tool_search.clone();
            }
            if e.name == "tengu_attachments"
                && !subst.deferred
                && let Some(types) = meta.get_mut("attachment_types").and_then(|t| t.as_array_mut())
            {
                types.retain(|t| t.as_str() != Some("deferred_tools_delta"));
            }
            // 模板取自一个 auto 模式的会话；不是 auto 的会话没有 `auto_mode` 那条附件。
            if e.name == "tengu_attachments"
                && subst.permission_mode != "auto"
                && let Some(types) = meta.get_mut("attachment_types").and_then(|t| t.as_array_mut())
            {
                types.retain(|t| t.as_str() != Some("auto_mode"));
            }
            // 模板里的附件事件都在主线程新输入之前：交互式紧随其后的是 `repl_main_thread`，
            // `-p` 是 `sdk`（`cap/auto-2.1.285-20260930/00448`）。
            if e.name == "tengu_attachments" && version_at_least(subst.version, "2.1.285") {
                let source = if subst.sdk { "sdk" } else { "repl_main_thread" };
                fill_attachment_estimates(&mut meta, source, subst.sdk, subst.model);
            }
            if e.name == "tengu_timer"
                && meta.get("event").and_then(|x| x.as_str()) == Some("startup")
                && let Some(obj) = meta.as_object_mut()
            {
                obj.insert("resumed".into(), Value::Bool(subst.resumed));
            }
            if e.name == "tengu_file_history_snapshot_success"
                && let Some(obj) = meta.as_object_mut()
            {
                obj.insert("snapshotCount".into(), Value::from(subst.prompt_index));
            }
            let v291 = version_at_least(subst.version, "2.1.291");
            let off = if subst.sdk { ToolsOff::default() } else { subst.tools_off };
            if v291 && dropped_by(off, &e.name) {
                continue;
            }
            if v291
                && e.name == "tengu_context_size"
                && let Some(o) = meta.as_object_mut()
            {
                let (count, tokens) = context_tool_counts(subst.sdk, subst.haiku, off);
                o.insert("non_mcp_tools_count".into(), json!(count));
                o.insert("non_mcp_tools_tokens".into(), json!(tokens));
            }
            // 用户用环境变量关掉了这几个工具：启动遥测按名字排序列出全部已设的 `CLAUDE_CODE_*`
            // 变量（可执行文件里 `HCo()` 那段：收集后 `sort()`，`cap/auto-2.1.291-20261006` 三个都关
            // 的四个会话正是这三个名字），关了哪个列哪个。
            if v291
                && off.any()
                && e.name == "tengu_startup_telemetry"
                && let Some(o) = meta.as_object_mut()
            {
                let mut names: Vec<String> = o
                    .get("set_env_vars")
                    .and_then(|x| x.as_str())
                    .map(|s| s.split(',').filter(|n| !n.is_empty()).map(str::to_string).collect())
                    .unwrap_or_default();
                names.extend(off_env_vars(off).into_iter().map(str::to_string));
                names.sort();
                names.dedup();
                o.insert("set_env_var_count".into(), json!(names.len()));
                o.insert("set_env_vars".into(), json!(names.join(",")));
            }
            if !sample_event(&e.name, subst.version, &mut meta) {
                continue;
            }
            let c = ctx(t);
            events.push((t, id.event_at(e.stage, &e.name, t, &c, meta.clone())));
            // 关掉 Artifact 的会话，启动计时那条之后紧跟一条「本会话 Artifact 已停用」（四族关掉的
            // 会话各一条，`mechanism: env`）。
            if v291
                && off.artifact
                && e.name == "tengu_timer"
                && meta.get("event").and_then(|x| x.as_str()) == Some("startup")
            {
                let disabled =
                    json!({ "mechanism": "env", "session_interactivity": "interactive" });
                events.push((
                    t,
                    id.event_at(
                        MetaStage::Startup,
                        "tengu_artifact_disabled_session",
                        t,
                        &c,
                        disabled,
                    ),
                ));
            }
            if dd_listed(&e.name, subst.version) {
                let mut flat = snake_flat(&meta);
                // Datadog 那份与事件那份差几项（`cap/2.1.280`、`cap/2.1.285`）：MCP 连接不带服务
                // 地址 / key 的 hash，`remote_managed_settings_pull` 的 `status` 在那边叫 `http_status`。
                if let Some(o) = flat.as_object_mut() {
                    if e.name == "tengu_mcp_server_connection_succeeded" {
                        o.shift_remove("mcp_server_base_url");
                        o.shift_remove("mcp_server_key_hash");
                    }
                    if meta.get("feature_name").and_then(|f| f.as_str())
                        == Some("remote_managed_settings_pull")
                        && let Some(v) = o.shift_remove("status")
                    {
                        o.insert("http_status".into(), v);
                    }
                }
                dd.push(id.dd_entry_at(e.stage, &e.name, &c, resp_model, flat));
            }
        }
    }
    (events, dd)
}

/// [`emit_template`] 的产物：带时间戳的事件与 Datadog 条目。
pub(super) type TplOutput = (Vec<(DateTime<Utc>, Value)>, Vec<Value>);
