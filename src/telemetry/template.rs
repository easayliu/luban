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

/// 2.1.293 的两份模板（交互式 / `-p`）：以 2.1.291 那两份为底，按 `cap/auto-2.1.293-20261008-full`
/// 补齐 meta 键（生成脚本是同目录 `_driver/build_tpl_293.py`）。相对 2.1.291：`tengu_ochre_wren` 改成
/// 强制开、那条实验曝光没了；启动段多了 `org_config_gate_shadow`（组织配置那道闸的影子判定）与
/// `frontmatter_shadow_unknown_key` 各一条；`remote_settings_fetch` / `policy_limits_fetch` 多了
/// 预取与对冲那几项、`managed_config_ready` 多了四项策略画像、`tengu_started` 多一项实验标记；
/// `model_catalog_compare` 的选择器里多了 haiku-5-5（行数 11 → 12）。
pub(super) static TEMPLATE_2_1_293: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_293.json"))
            .expect("assets/cc_telemetry_template_2_1_293.json must parse")
    });

/// 2.1.293 `-p` 打印模式的模板，见 [`TEMPLATE_2_1_293`]。
pub(super) static TEMPLATE_2_1_293_SDK: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_293_sdk.json"))
            .expect("assets/cc_telemetry_template_2_1_293_sdk.json must parse")
    });

/// 2.1.296 的两份模板（交互式 / `-p`）：以 2.1.293 那两份为底，按 `cap/auto-2.1.296-20261013-full`
/// 生成（`_driver/build_tpl_296.py`）。相对 2.1.293：可执行文件里删掉的 `artifact_five_class_asks`、
/// `artifact_text_variant` 与两条 `signed_cache_identity` 去掉；`signed_cache_shadow` 换了两个键
/// （`identity_state` → `absence`、`states_lifetime`）；首轮多一条 `tengu_lucky_cerf_experiment_v3` 曝光。
pub(super) static TEMPLATE_2_1_296: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_296.json"))
            .expect("assets/cc_telemetry_template_2_1_296.json must parse")
    });

/// 2.1.296 `-p` 打印模式的模板，见 [`TEMPLATE_2_1_296`]。
pub(super) static TEMPLATE_2_1_296_SDK: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_296_sdk.json"))
            .expect("assets/cc_telemetry_template_2_1_296_sdk.json must parse")
    });

/// 2.1.299 的两份模板：以 2.1.296 那两份为底，按 `cap/auto-2.1.299-20261014-full` 生成
/// （`_driver/build_tpl_299.py`）。相对 2.1.296：`signed_cache_shadow` 多 `stored_copy`，
/// `remote_settings_fetch` 多 `cache_age_ms`、`stored_copy`、`stored_copy_gate`，`policy_limits_fetch`
/// 多后两个；首轮那条曝光换成正文开关的 `tengu_lucky_cerf_experiment_v4`（实际发哪几条照号的 eval）。
/// 退出那一串多出的 `tengu_resume_hint_printed` 不在模板里，见 `Telemetry::gc`。
pub(super) static TEMPLATE_2_1_299: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_299.json"))
            .expect("assets/cc_telemetry_template_2_1_299.json must parse")
    });

/// 2.1.299 `-p` 打印模式的模板，见 [`TEMPLATE_2_1_299`]。
pub(super) static TEMPLATE_2_1_299_SDK: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_299_sdk.json"))
            .expect("assets/cc_telemetry_template_2_1_299_sdk.json must parse")
    });

/// 2.1.300 的两份模板：以 2.1.299 那两份为底，按 `cap/auto-2.1.300-20261015-full` 生成
/// （`_driver/build_tpl_300.py`）。相对 2.1.299 只有 `remote_settings_fetch`、`policy_limits_fetch`、
/// `policy_limits_cache_state_at_first_prompt` 三条多了 `credential_road`（`"none"`，排在 `token_source`
/// 之后）。
pub(super) static TEMPLATE_2_1_300: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_300.json"))
            .expect("assets/cc_telemetry_template_2_1_300.json must parse")
    });

/// 2.1.300 `-p` 打印模式的模板，见 [`TEMPLATE_2_1_300`]。
pub(super) static TEMPLATE_2_1_300_SDK: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_300_sdk.json"))
            .expect("assets/cc_telemetry_template_2_1_300_sdk.json must parse")
    });

/// 2.1.303 的两份模板：以 2.1.300 那两份为底，按 `cap/auto-2.1.303-20261019-full` 生成
/// （`_driver/build_tpl_303.py`）。相对 2.1.300：`tengu_init` 在 `mcpClientCount` 之后多
/// `repoMcpAllowList` / `repoMcpDenyList` / `mcpConfigBlockedCount`（抓包 `false` / `false` / `0`）；
/// `-p` 那份另多一条 `tengu_fork_subagent_enabled`（这一版起 `-p` 默认开 fork，位置同交互式）。
/// 2.1.301 用的仍是 2.1.300 那两份（两版构造逐一相同）。
pub(super) static TEMPLATE_2_1_303: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_303.json"))
            .expect("assets/cc_telemetry_template_2_1_303.json must parse")
    });

/// 2.1.303 `-p` 打印模式的模板，见 [`TEMPLATE_2_1_303`]。
pub(super) static TEMPLATE_2_1_303_SDK: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_303_sdk.json"))
            .expect("assets/cc_telemetry_template_2_1_303_sdk.json must parse")
    });

/// 2.1.305 的两份模板：以 2.1.303 那两份为底，按 `cap/auto-2.1.305-20261021-full` 生成
/// （`_driver/build_tpl_305.py`）。相对 2.1.303 只动了启动期几条影子事件的 meta 键：`signed_cache_shadow`
/// 多 `age_s` / `cert_hours_left`，`org_config_gate_shadow` 换成 `settings_cache_age_ms` / `shadow_trigger`
/// 那套，`policy_limits_fetch` 多 `request_id_present`。2.1.306 沿用这两份：`cap/auto-2.1.306-20261021-full`
/// 与 2.1.305-full 比，luban 发的事件 meta 键都没变（零星几条多出的键是场景分支，两版 JS 逐字相同）。
/// 2.1.307 同样沿用：`cap/auto-2.1.307-20261022-full` 与 2.1.306-full 比，模板里那些事件的键与常量都没变，
/// 变的几处（摘要器收尾的 writer、auto 判决的 `skippedOnAllowRule`、压缩尝试的 `keepNoRound`、拒绝时的
/// `tengu_safety_event`）都在按请求现算的事件里，见 `TurnFacts::v307`。2.1.308 也沿用：两版内嵌 JS 除版本号 /
/// 构建时间外逐字相同，`cap/auto-2.1.308-20261022-full` 与 2.1.307-full 比多出的几个键都是场景分支。
/// 2.1.309、2.1.310、2.1.311 也沿用；2.1.310 的 `org_config_gate_shadow` 多五项，2.1.311 的 `tengu_init` 多
/// `config_dir_kind`、`<total_tokens>` 那档由 `client_data` 给出时多一条 `tengu_lapis_anchor_from_client_data`，都在
/// [`emit_template`] 里按版本补。
///
/// 这两份里 `org_config_gate_shadow` 跟服务端配置走的几项（`decision`、`exclusion_now`、`flag_*`、`grace_ttl_hours`、
/// 两侧 `*_reason`）按 `cap/auto-2.1.310-20261024-full` 交互式启动那 14 条的主流值改过（`pass` / `rollout_percent` /
/// 开、`disk`、1%、24h / `ok`、`none_configured`；2.1.309 抓包时还是关、0%、96h）。2.1.305 起这条等配置落定
/// （`shadow_trigger: settled`）才报，所以报的是当时的服务端状态。**2.1.293 ~ 2.1.303 那几份不改**：旧版在启动时、
/// 配置还没拉回来就先报，每版抓包 24 条里 23 条是 `blocked` + `fallback` + 两侧 `unavailable_pending`（带
/// `decision_reason` / `decision_resource`），那是旧客户端的时序、不是服务端状态，开关、比例、宽限期都是 fallback 下的默认值。
pub(super) static TEMPLATE_2_1_305: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_305.json"))
            .expect("assets/cc_telemetry_template_2_1_305.json must parse")
    });

/// 2.1.305 `-p` 打印模式的模板，见 [`TEMPLATE_2_1_305`]。
pub(super) static TEMPLATE_2_1_305_SDK: std::sync::LazyLock<Template> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_telemetry_template_2_1_305_sdk.json"))
            .expect("assets/cc_telemetry_template_2_1_305_sdk.json must parse")
    });

/// 出站版本用哪一份模板：2.1.305 起（含 2.1.306 ~ 2.1.311）用 [`TEMPLATE_2_1_305`]，2.1.303 ~ 2.1.304 用 [`TEMPLATE_2_1_303`]，2.1.300 ~ 2.1.302 用 [`TEMPLATE_2_1_300`]，2.1.299 用 [`TEMPLATE_2_1_299`]，2.1.296 ~ 2.1.298 用 [`TEMPLATE_2_1_296`]，2.1.293 ~ 2.1.295 用 [`TEMPLATE_2_1_293`]，2.1.291 ~ 2.1.292 用
/// [`TEMPLATE_2_1_291`]，2.1.285 ~ 2.1.290 用 [`TEMPLATE_2_1_285`]（`-p` 打印模式各用对应的 SDK
/// 那份），之前的仍是 2.1.260 那份。
pub(super) fn template_for(version: &str, sdk: bool) -> &'static Template {
    let at = |v: &str| version_at_least(version, v);
    if at("2.1.305") {
        return if sdk { &TEMPLATE_2_1_305_SDK } else { &TEMPLATE_2_1_305 };
    }
    if at("2.1.303") {
        return if sdk { &TEMPLATE_2_1_303_SDK } else { &TEMPLATE_2_1_303 };
    }
    if at("2.1.300") {
        return if sdk { &TEMPLATE_2_1_300_SDK } else { &TEMPLATE_2_1_300 };
    }
    if at("2.1.299") {
        return if sdk { &TEMPLATE_2_1_299_SDK } else { &TEMPLATE_2_1_299 };
    }
    if at("2.1.296") {
        return if sdk { &TEMPLATE_2_1_296_SDK } else { &TEMPLATE_2_1_296 };
    }
    match (at("2.1.293"), at("2.1.291"), at("2.1.285"), sdk) {
        (true, _, _, true) => &TEMPLATE_2_1_293_SDK,
        (true, _, _, false) => &TEMPLATE_2_1_293,
        (false, true, _, true) => &TEMPLATE_2_1_291_SDK,
        (false, true, _, false) => &TEMPLATE_2_1_291,
        (false, false, true, true) => &TEMPLATE_2_1_285_SDK,
        (false, false, true, false) => &TEMPLATE_2_1_285,
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

/// 服务端下发的 Datadog 抽样表（eval 的 `tengu_agile_glade`，`"source":"defaultValue"`，键是
/// `tengu_feature_ok` 的 `feature_name`）：2.1.293 ~ 2.1.306 全量抓包里 184 份 eval 一字不差，60 项都是
/// 0.01，`cap/auto-2.1.291-20261006-full/00017` 那时也已下发。
///
/// 客户端 2.1.293 起才照它抽（官方发 Datadog 前的 `re()`）：只抽 Datadog 那份，event_logging 照发；抽中的
/// 那条在字段末尾、`user_bucket` 之前多一项 `sample_rate`。抓包里表中这些 feature 的 event_logging /
/// Datadog 条数：2.1.291 1614 / 1614，2.1.293 1900 / 17，2.1.306 2004 / 13，DD 那几条都带 `0.01`。
const DD_FEATURE_SAMPLING_2_1_293: &[(&str, f64)] = &[
    ("api_request", 0.01),
    ("tool_bash", 0.01),
    ("hook_post_tool_use", 0.01),
    ("hook_pre_tool_use", 0.01),
    ("permission_auto_approve_config", 0.01),
    ("git_bare_repo_gate", 0.01),
    ("skill_load_dir", 0.01),
    ("skills_sync_round", 0.01),
    ("cmd_load", 0.01),
    ("prompt_snapshot_tool_replay", 0.01),
    ("mcp_list_tools", 0.01),
    ("plugin_load_manifest", 0.01),
    ("turn", 0.01),
    ("plugin_load_settings", 0.01),
    ("agent_proxy_request", 0.01),
    ("workflow_discover", 0.01),
    ("plugin_load_workflows", 0.01),
    ("hook_stop_handler", 0.01),
    ("skill_load_commands_dir", 0.01),
    ("tool_read", 0.01),
    ("artifact_live_subscribe", 0.01),
    ("tool_mcp_tool", 0.01),
    ("mcp_sdk_connect", 0.01),
    ("ccr_partial_messages", 0.01),
    ("ccr_task_status_publish", 0.01),
    ("mcp_sdk_manifest", 0.01),
    ("plugin_load_commands", 0.01),
    ("plugin_load_skills", 0.01),
    ("mcp_connect", 0.01),
    ("ccr_no_subscriber_hold", 0.01),
    ("queued_message_delivered", 0.01),
    ("shoji_engine", 0.01),
    ("plugin_function_hooks_load", 0.01),
    ("bash_edit_diff", 0.01),
    ("tool_edit", 0.01),
    ("context_git_detect", 0.01),
    ("plugin_load_agents", 0.01),
    ("context_claude_md_load", 0.01),
    ("remote_managed_settings_pull", 0.01),
    ("plugin_load_all", 0.01),
    ("hook_stop", 0.01),
    ("ccr_worker_state_publish", 0.01),
    ("output_style_load", 0.01),
    ("tool_write", 0.01),
    ("plugin_load_hooks", 0.01),
    ("policy_limits_poll", 0.01),
    ("startup_resolve_model", 0.01),
    ("mcp_list_resources", 0.01),
    ("swarm_session_cleanup", 0.01),
    ("plugin_load_output_styles", 0.01),
    ("lsp_init", 0.01),
    ("lsp_diagnostics_register", 0.01),
    ("agents_cross_session_inbox", 0.01),
    ("lsp_config_load", 0.01),
    ("update_check", 0.01),
    ("api_midconv_cache_proxy", 0.01),
    ("lsp_shutdown", 0.01),
    ("hook_user_prompt_submit", 0.01),
    ("api_bootstrap_fetch", 0.01),
    ("prompt_suggestion_generate", 0.01),
];

/// 一条 Datadog 条目按 [`DD_FEATURE_SAMPLING_2_1_293`] 抽：`false` 即不发；抽中的补上 `sample_rate`。只看
/// 条目自己带的 `message` / `feature_name` / `version`，别的事件、不在表里的、或版本早于 2.1.293 的原样放行。
/// 进待发队列的 Datadog 条目（[`Pending::extend_dd`]）与保活那份都过这一道。
pub fn sample_dd(entry: &mut Value) -> bool {
    sample_dd_at(entry, rand::random())
}

/// [`sample_dd`] 的本体：`roll` 是掷出的 `[0, 1)` 随机数，小于抽样率才留（官方 `Sjr`）。
pub(super) fn sample_dd_at(entry: &mut Value, roll: f64) -> bool {
    let field = |k: &str| entry.get(k).and_then(|v| v.as_str());
    if field("message") != Some("tengu_feature_ok")
        || !field("version").is_some_and(|v| version_at_least(v, "2.1.293"))
    {
        return true;
    }
    let Some(&(_, rate)) = field("feature_name")
        .and_then(|f| DD_FEATURE_SAMPLING_2_1_293.iter().find(|(n, _)| *n == f))
    else {
        return true;
    };
    if roll >= rate {
        return false;
    }
    insert_before(entry, "user_bucket", vec![("sample_rate", js_number(rate))]);
    true
}

/// billing-only 模拟（[`Subst::bare`]）不报的事件：都在宣告注入进请求里的东西（环境说明、日期、各类附件），而这条路
/// 除 billing header 外一样都没注入。
pub(super) const BARE_SKIPPED: &[&str] =
    &["tengu_attachments", "tengu_context_announcement", "tengu_context_rendering_recorded"];

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
    /// 文件历史的快照张数（`tengu_file_history_snapshot_success.snapshotCount`）：起初是输入序号，2.1.285 起
    /// 由拍快照那一刻按账本数的覆盖（[`Files`]，注入的那几轮不拍）。
    pub(super) prompt_index: u32,
    /// 会话在 git 仓库里（[`Identity::vcs`]）：启动时的文件建议走 `git ls-files`，见 [`emit_template`]。
    pub(super) git: bool,
    /// 这条调用的稳定散列：几项本机耗时按它在官方的范围里取值，不至于每个会话都一模一样。
    pub(super) seed: u32,
    /// 文件历史跟踪中的文件数（`tengu_file_history_snapshot_success.trackedFilesCount`），见 [`Files`]。
    pub(super) tracked_files: usize,
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
    /// 这个会话的模型属于哪一套工具声明，`tengu_context_size` 的工具计数随它变（[`context_tool_counts`]）。
    pub(super) tool_family: ToolFamily,
    /// 正文里关着的那几个用户可关的工具（[`RequestShape::tools_off`]）。`-p` 不看它：打印模式本来就
    /// 不带 Artifact / SendFeedback（不是用户关的），也没有关掉之后的样本。
    pub(super) tools_off: ToolsOff,
    /// 工具声明是来访客户端自带的（真实 CC 经网关透传），`tengu_context_size` 按网关基线报，见
    /// [`gateway_tool_tokens`]。
    pub(super) client_tools: bool,
    /// billing-only 模拟，见 [`super::ApiCall::bare`]：模板里宣告注入内容的那几条不发。
    pub(super) bare: bool,
    /// 这一轮那几条 `tengu_lucky_cerf` 一系实验曝光，空即不发——按会话钉住的分组与「模型走不走
    /// 精简提示词」见 [`crate::proxy::lucky_cerf_exposures`]，一个进程每个一次见
    /// [`super::session::TurnFacts::lucky_cerf`]。
    pub(super) lucky_cerf: Vec<crate::proxy::LuckyCerfExposure>,
    /// 这个会话的 `<total_tokens>` 那一族由 bootstrap 的 `client_data` 给出的几项（[`crate::proxy::lapis_anchor`]）：
    /// 交互式在首次输入那段的 `tengu_mcp_instructions_pool_change` 之后、`-p` 在启动段的
    /// `tengu_fork_subagent_enabled` 之前，按序各报一条 `tengu_lapis_anchor_from_client_data`（同一毫秒，
    /// `cap/auto-2.1.311-20261027-full` 的 opus / sonnet 会话 12/14 有 `{mode}` 那条，交互式 `00072`、`-p` `01448`；
    /// `{userTurn}` / `{budget}` 没有样本，跟在它后面）。
    pub(super) lapis: Vec<Value>,
    /// 这条出站带了 `x-claude-code-sandy-plover`（[`super::ApiCall::tools_used_header`]）：启动段的
    /// `feature_ok{provider_route}` 之后 1ms 紧跟一条 `feature_ok{api_tools_used_header}`——官方头一回在请求上补这个头
    /// 时报，一个进程一次（`cap/auto-2.1.312-20261028-full` 交互式与 `-p` 全部 22 个会话都是这个位置）。
    pub(super) tools_used_header: bool,
    /// 这条的头装不下、会话锁存头一回丢名字（重发过的看整串，[`super::ApiCall::chain_tools_used_truncation`]）：
    /// 同一处、首次使用那条之前先报 `feature_sad{sent, dropped, api_tools_used_header, incomplete_list}`（官方拼头时
    /// 先报它，再记首次使用）。
    pub(super) tools_used_truncation: Option<(usize, usize)>,
}

/// 模型用的是哪一套内建工具声明：haiku-4.5 那套长描述（2.1.291 起），haiku-5-5 那套（与 opus 相同，
/// 只是延迟池里没有 EndConversation，2.1.293 起），其余模型一套。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolFamily {
    Main,
    HaikuLong,
    Haiku55,
}

impl ToolFamily {
    pub(super) fn of(model: &str) -> Self {
        if crate::config::cc_haiku_is_5_5_family(model) {
            Self::Haiku55
        } else if model.to_ascii_lowercase().contains("haiku") {
            Self::HaikuLong
        } else {
            Self::Main
        }
    }
}

/// `tengu_context_size` 里的 `non_mcp_tools_count` / `non_mcp_tools_tokens`：客户端本地按全部内建
/// 工具（含延迟池里的）估的，随版本、模型族与用户关掉的工具变。`-p`（`sdk`）本来就不带 Artifact /
/// SendFeedback，没有关掉之后的样本，照不关的报。
///
/// 2.1.291（`cap/auto-2.1.291-20261006-full` 各会话、`cap/auto-2.1.291-20261006` 关前 / 关后各四个
/// 会话）：opus 等 34 / 13834，haiku-4.5 37 / 14305，`-p` 26 / 6399、haiku 30 / 6900。三个全关：数
/// 34 → 29（Artifact 连带 `ArtifactComments` / `ArtifactData` 少 3 个，另两个各 1 个），token 少 6851，
/// haiku 同样少 6851。只关其中几个没有样本，token 按三家工具声明的字符数分摊那 6851（Artifact 一家
/// 34399 + 7438 + 12241，ListAgents 1180，SendFeedback 5537），三个全关时正好等于抓包值。
///
/// 2.1.293（`cap/auto-2.1.293-20261008-full`）：`Agent` 多了 `effort` 参数、`Artifact` 改了措辞，
/// opus 等 34 / 13942、haiku-4.5 37 / 14413（都多 108），`-p` 26 / 6490（多 91——`-p` 没有 Artifact，
/// 这 91 全是 Agent，余下 17 记在 Artifact 头上）；haiku-5-5 是 33 / 13912，比 opus 少一个工具
/// （EndConversation）、30 token；`-p` 的 haiku 也是 haiku-5-5，同 `-p` 那份。`-p` 的 haiku-4.5 没有
/// 样本，按 2.1.291 那份加 Agent 的 91 估。
/// 2.1.296 起 `Read` 多了 `allow_large` 参数，`non_mcp_tools_tokens` 各族（含 `-p`）一律多 83，工具数
/// 不变（`cap/auto-2.1.296-20261013-full`：opus 等 14025、haiku-5-5 13995、haiku-4.5 14496、`-p` 6573，
/// 对 2.1.293 的 13942、13912、14413、6490）。`Agent` 按模型换的那段说明（`tengu_lucky_cerf`）不进这个
/// 估算：sonnet-5-5、haiku-5-5 与 opus 多出来的都只是这 83。
const READ_ALLOW_LARGE_TOKENS: u64 = 83;

/// 2.1.300 起 `Artifact` 的 schema 按版本变，交互式各族（`-p` 没有 Artifact）的 token 跟着变、工具数不变；
/// 关掉 Artifact 时连这部分一起减。逐版对抓包里 `Artifact.input_schema` 的 `round(长度 / 4)` 差：
/// 2.1.300 加 `edits` 参数 +274（`cap/auto-2.1.300-20261015-full`：opus 等 14299、haiku-5-5 14269、
/// haiku-4.5 14770），2.1.301 那句去掉 `root` −2（`cap/auto-2.1.301-20261016-full`：14297 / 14267 /
/// 14768），2.1.303 `intent` 加 dashboard 那句 +8。
fn artifact_schema_delta(version: &str) -> u64 {
    match () {
        // 2.1.309 改写 `root` 参数说明 +45：按订阅端（模拟路径注入的固定资产 [`crate::proxy::cc_tools_core`]）
        // 那份 `Artifact.input_schema` 15438→15620 字符（compact JSON，官方按 round(字符/4) 估），3860→3905，
        // 交互式各族的 `non_mcp_tools_tokens` 15457→15502。真实 CC 经网关那份 schema 短，同一处改动是 +46，
        // 那边另算，见 [`gateway_tool_tokens`]。Bash 加的那句 `@name` 提示不进这个估算（`-p` 带 Bash 却仍是 7742）。
        // 2.1.311 改写 `intent` 参数说明 +37：同一份资产 15620→15766 字符，3905→3942（`cap/auto-2.1.311-20261027-full`
        // 交互式各族 15502→15539，`-p` 仍是 7742）。Bash 那句 `cd` 说明同样不进这个估算。
        _ if version_at_least(version, "2.1.311") => 274 - 2 + 8 + 45 + 37,
        _ if version_at_least(version, "2.1.309") => 274 - 2 + 8 + 45,
        _ if version_at_least(version, "2.1.303") => 274 - 2 + 8,
        _ if version_at_least(version, "2.1.301") => 274 - 2,
        _ if version_at_least(version, "2.1.300") => 274,
        _ => 0,
    }
}

/// 真实 CC 经 `ANTHROPIC_BASE_URL` 走网关时（[`Subst::client_tools`]）`non_mcp_tools_tokens` 对订阅端标准值的
/// 修正。客户端是按**全部**内建工具（含延迟池里没发出来的）估的，单条请求里只有一部分（2.1.309 网关主请求只带 16
/// 个、客户端计 34），所以不能拿出站 `tools` 逐个求和，只能按抓包定基线。
///
/// `cap/auto-2.1.308-20261022-api` 与 `cap/auto-2.1.309-20261023-api`（同一台机器同一会话先后跑 309 / 308）：
/// opus 等 308 34 / 14415、309 34 / 14461，haiku-5-5 308 33 / 14385、309 33 / 14431，`-p` 两版都是 26 / 7452；
/// 2.1.307 的 opus 也是 14415。工具数与订阅端相同（订阅端多的那个 PublishPlugin 本来就不算），token 拆成两块：
/// - Artifact 以外的工具网关形态多 [`GATEWAY_OTHER_TOOLS_TOKENS`]：`-p` 没有 Artifact，7452 − 6590 = 862；
/// - Artifact 的 schema 网关形态短：308 12432 字符 = 3108 对订阅端 3860，少 752；309 12614 = 3154 对 3905，少 751。
///
/// 合起来交互式 308 +110、309 +111，各族都对得上。2.1.307 以前没有同条件的网关样本（2.1.293 那份是另一台
/// 配置、工具数都不同），haiku-4.5 主模型也没有网关样本，这两种照订阅端报。关掉 Artifact 时整组不在、只加
/// 862——ArtifactComments / ArtifactData 两种形态是否同长没有样本，按同长算。
const GATEWAY_OTHER_TOOLS_TOKENS: u64 = 862;

fn gateway_tool_tokens(
    version: &str,
    sdk: bool,
    family: ToolFamily,
    artifact_off: bool,
    tokens: u64,
) -> u64 {
    if !version_at_least(version, "2.1.307") || family == ToolFamily::HaikuLong {
        return tokens;
    }
    let artifact_shortfall = match () {
        _ if sdk || artifact_off => 0,
        // 2.1.311 没有网关样本：`intent` 说明那 146 字符在两种形态里是同一段，网关那份按 12614+146=12760 → 3190，
        // 订阅端 3942，少 752（推算）。
        _ if version_at_least(version, "2.1.311") => 752,
        _ if version_at_least(version, "2.1.309") => 751,
        _ => 752,
    };
    tokens + GATEWAY_OTHER_TOOLS_TOKENS - artifact_shortfall
}

/// 2.1.303 起 `-p` 的 `Agent` 默认开 fork，`effort` 等参数说明随之变长：`-p` 多 17（`cap/auto-2.1.303-20261019-full`
/// D1 是 27 / 7742，减去下面那个 PublishPlugin 的 1152 后比 2.1.301 的 6573 多 17）。交互式的 Agent
/// 本来就带 fork 说明，不变。
///
/// **不跟的**：2.1.303 抓包各族工具数都多 1、token 多 1152，是 `PublishPlugin`（schema 1152）——它在 2.1.301
/// 里就有，是否启用看 GrowthBook `tengu_copper_gazette`（301 / 303 判据相同），这次抓包那个号恰好开了，
/// 不随版本，所以计数照旧、不算它（交互式抓包 15457 − 1152 = 14305，正好是 2.1.301 的 14297 加 Artifact 的 8）。
const SDK_AGENT_FORK_TOKENS_2_1_303: u64 = 17;

fn context_tool_counts(v293: bool, sdk: bool, family: ToolFamily, off: ToolsOff) -> (u64, u64) {
    use ToolFamily::*;
    let (count, tokens) = match (v293, sdk, family) {
        (false, true, HaikuLong) => return (30, 6900),
        (false, true, _) => return (26, 6399),
        (false, false, HaikuLong) => (37, 14305),
        (false, false, _) => (34u64, 13834u64),
        (true, true, HaikuLong) => return (30, 6900 + 91),
        (true, true, _) => return (26, 6490),
        (true, false, HaikuLong) => (37, 14413),
        (true, false, Haiku55) => (33, 13912),
        (true, false, Main) => (34, 13942),
    };
    let artifact: u64 = if v293 { 6094 + 17 } else { 6094 };
    const LIST_AGENTS: u64 = 133;
    let send_feedback: u64 = 6851 - 6094 - LIST_AGENTS;
    let (mut c, mut t) = (count, tokens);
    if off.artifact {
        c -= 3;
        t -= artifact;
    }
    if off.list_agents {
        c -= 1;
        t -= LIST_AGENTS;
    }
    if off.send_feedback {
        c -= 1;
        t -= send_feedback;
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
    // 交互式模板（2.1.285 起各份）启动段那个 IDE 的 MCP 连接（`ws-ide`）：终端里跑的官方客户端没有它，
    // 而同一会话启动 30 秒后又报 `ide_detect{found:false}`，自相矛盾；键也停在 2.1.285（缺 2.1.291 起的
    // `processUptimeAtConnectStartMs` / `discoverSource`）。连同它的 `tools_listed` 与跟着的
    // `mcp_connect` / `mcp_list_tools` 整组不发——这几份模板里这两条 feature_ok 只属于它。
    let ide_mcp =
        version_at_least(subst.version, "2.1.285") && tpl.iter().any(|e| is_ide_transport(&e.meta));
    for e in tpl {
        if ide_mcp
            && (is_ide_transport(&e.meta)
                || (e.name == "tengu_feature_ok"
                    && matches!(
                        e.meta.get("feature_name").and_then(|f| f.as_str()),
                        Some("mcp_connect" | "mcp_list_tools")
                    )))
        {
            continue;
        }
        let t = anchor + chrono::Duration::milliseconds(e.off);
        for _ in 0..e.repeat.unwrap_or(1).max(1) {
            if e.kind == "growth" {
                // `tengu_lucky_cerf` 一系（`Agent` 按模型换说明）的曝光：客户端只在精简提示词的模型上读
                // 这两个开关（haiku-4.5、sonnet-4.x / 5、opus-4-7 及更老都不读，`cap/auto-2.1.296-20261013-full`
                // B3c 两个会话里 haiku-5-5 那个有、haiku-4.5 那个没有），读了且这个号在实验里才有，实验名、
                // 分组与发哪几条照这个号的 eval（[`Subst::lucky_cerf`]）。模板那条（2.1.296 是清单的 v3，
                // 2.1.299 是正文的 v4）只给位置：2.1.299 抓包里两者都紧挨在 `tool_search_mode_decision` 前。
                if e.feature
                    .as_deref()
                    .is_some_and(|f| f.starts_with(crate::proxy::LUCKY_CERF_FEATURE))
                {
                    for x in &subst.lucky_cerf {
                        events
                            .push((t, id.growth_event(t, &x.key, x.var, x.feature, subst.version)));
                    }
                    continue;
                }
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
            // IDE 里抓的模板底子：2.1.285 起的自动抓包（终端里跑）一次都没有。启动段那两条
            // `ide_detect{found:true}` 同理——终端里是启动 30 秒后一条 `found:false`，由代码补
            // （`Session::take_ide_detect`）。
            if (e.name == "tengu_mcp_ide_server_connection_succeeded"
                || (e.name == "tengu_feature_ok"
                    && e.meta.get("feature_name").and_then(|f| f.as_str()) == Some("ide_detect")))
                && version_at_least(subst.version, "2.1.285")
            {
                continue;
            }
            // 只在 2.1.293 那次抓包出现过：那台机器上某个技能的 frontmatter 带了未知键，是环境不是版本。
            if e.name == "tengu_frontmatter_shadow_unknown_key" {
                continue;
            }
            // 启动时的文件建议：git 仓库里官方一律走 `git ls-files`（`cap/auto-2.1.285-20260930` 起每份全量抓包
            // 26 ~ 30 条，没有一条 ripgrep），否则与同一会话的 `vcs: git` 自相矛盾。代理不知道仓库里有几个文件，
            // 个数沿用模板那条、全算已跟踪；耗时官方 48 ~ 89ms，按调用散列取。
            if e.name == "tengu_file_suggestions_ripgrep" && subst.git {
                let n = e.meta.get("file_count").cloned().unwrap_or(json!(15));
                let meta = json!({
                    "file_count": n,
                    "tracked_count": n,
                    "untracked_count": 0,
                    "duration_ms": 48 + subst.seed % 42
                });
                let name = "tengu_file_suggestions_git_ls_files";
                events.push((t, id.event_at(e.stage, name, t, &ctx(t), meta)));
                continue;
            }
            // billing-only 模拟体里没有环境说明与附件，宣告它们的不报（[`super::ApiCall::bare`]）。
            if subst.bare && BARE_SKIPPED.contains(&e.name.as_str()) {
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
                obj.insert("trackedFilesCount".into(), Value::from(subst.tracked_files));
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
                let v293 = version_at_least(subst.version, "2.1.293");
                let (count, mut tokens) =
                    context_tool_counts(v293, subst.sdk, subst.tool_family, off);
                if version_at_least(subst.version, "2.1.296") {
                    tokens += READ_ALLOW_LARGE_TOKENS;
                }
                if subst.sdk {
                    if version_at_least(subst.version, "2.1.303") {
                        tokens += SDK_AGENT_FORK_TOKENS_2_1_303;
                    }
                } else if !off.artifact {
                    tokens += artifact_schema_delta(subst.version);
                }
                if subst.client_tools {
                    tokens = gateway_tool_tokens(
                        subst.version,
                        subst.sdk,
                        subst.tool_family,
                        off.artifact,
                        tokens,
                    );
                }
                o.insert("non_mcp_tools_count".into(), json!(count));
                o.insert("non_mcp_tools_tokens".into(), json!(tokens));
            }
            // 2.1.293 起 haiku-4.5 当主模型时模型目录那条不带这两项（haiku 的默认档换成了 haiku-5-5，
            // `cap/auto-2.1.293-20261008-full` 的 haiku-4-5 会话只剩 `default_match`；2.1.291 时它还带着）。
            if e.name == "tengu_model_catalog_compare"
                && subst.tool_family == ToolFamily::HaikuLong
                && version_at_least(subst.version, "2.1.293")
                && let Some(o) = meta.as_object_mut()
            {
                o.shift_remove("default_output_match");
                o.shift_remove("default_effort_match");
            }
            // 2.1.301 起模型目录那条多三项比对（`cap/auto-2.1.301-20261016-full` 起每份 22/23，值恒为 true）；
            // haiku-4.5 主模型那个会话三项都不带，同上。2.1.300 ~ 2.1.305 的模板都没补上。
            if e.name == "tengu_model_catalog_compare"
                && subst.tool_family != ToolFamily::HaikuLong
                && version_at_least(subst.version, "2.1.301")
                && meta.get("capabilities_match").is_none()
            {
                insert_after(
                    &mut meta,
                    "served_not_offered_count",
                    vec![("capabilities_match", json!(true))],
                );
                insert_after(
                    &mut meta,
                    "haiku_match",
                    vec![("context_window_match", json!(true)), ("max_output_match", json!(true))],
                );
            }
            // 2.1.310 起组织配置闸的影子判定在 `settings_cache_age_ms` 后多五项（`cap/auto-2.1.310-20261024-full`
            // 全部 25 条都带）：两份签名副本恒 `trusted`；现拉的签名在那一侧还没答复（`unavailable_pending`）时报
            // `no_answer`、拉到了报 `valid`；`decision_if_strict` 与 `decision` 同值。
            if e.name == "tengu_org_config_gate_shadow"
                && version_at_least(subst.version, "2.1.310")
                && meta.get("settings_stored_copy").is_none()
            {
                let live = |side: &str| {
                    if meta.get(side).and_then(|x| x.as_str()) == Some("unavailable_pending") {
                        "no_answer"
                    } else {
                        "valid"
                    }
                };
                let pairs = vec![
                    ("settings_stored_copy", json!("trusted")),
                    ("policy_stored_copy", json!("trusted")),
                    ("settings_live_signed", json!(live("settings_reason"))),
                    ("policy_live_signed", json!(live("policy_reason"))),
                    ("decision_if_strict", meta.get("decision").cloned().unwrap_or(Value::Null)),
                ];
                insert_after(&mut meta, "settings_cache_age_ms", pairs);
            }
            // 2.1.311 起启动那条 `tengu_init` 在 `rendererEntryPath` 之后多一项 `config_dir_kind`：`CLAUDE_CONFIG_DIR`
            // 没设时恒为 `unset`（`cap/auto-2.1.311-20261027-full` 交互式 16 条、`-p` 7 条都是；设了且与设置目录不同时
            // 才再多一项 `config_dir_settings_kind`）。
            if e.name == "tengu_init"
                && version_at_least(subst.version, "2.1.311")
                && meta.get("config_dir_kind").is_none()
            {
                insert_after(
                    &mut meta,
                    "rendererEntryPath",
                    vec![("config_dir_kind", json!("unset"))],
                );
            }
            // 2.1.312 起上下文宣告在 `updates_copy` 之后多一项 `assumed_utc`：只有日期那条、且客户端按 UTC 推当天日期
            // （远程会话、进程时区是 UTC、服务端开关 `tengu_peppy_nest` 开着）时为真，本机的交互式与 `-p` 恒为 `false`
            // （`cap/auto-2.1.312-20261028-full` 全部 75 条，主线程、`-p` 与子代理都是）。
            if e.name == "tengu_context_announcement"
                && version_at_least(subst.version, "2.1.312")
                && meta.get("assumed_utc").is_none()
            {
                insert_after(&mut meta, "updates_copy", vec![("assumed_utc", json!(false))]);
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
            let lapis = |c: &EventCtx<'_>| {
                let name = "tengu_lapis_anchor_from_client_data";
                let at = |m: &Value| (t, id.event_at(e.stage, name, t, c, m.clone()));
                subst.lapis.iter().map(at).collect::<Vec<_>>()
            };
            if subst.sdk && e.name == "tengu_fork_subagent_enabled" {
                events.extend(lapis(&c));
            }
            events.push((t, id.event_at(e.stage, &e.name, t, &c, meta.clone())));
            if !subst.sdk && e.name == "tengu_mcp_instructions_pool_change" {
                events.extend(lapis(&c));
            }
            let tools_used_at_route = e.name == "tengu_feature_ok"
                && meta.get("feature_name").and_then(|f| f.as_str()) == Some("provider_route");
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
            if tools_used_at_route {
                let ta = t + chrono::Duration::milliseconds(1);
                let ca = ctx(ta);
                for (name, meta) in
                    tools_used_header_events(subst.tools_used_header, subst.tools_used_truncation)
                {
                    events.push((ta, id.event_at(e.stage, name, ta, &ca, meta.clone())));
                    dd.push(id.dd_entry_at(e.stage, name, &ca, resp_model, meta));
                }
            }
        }
    }
    (events, dd)
}

/// `x-claude-code-sandy-plover` 带出来的两条（官方拼头那一刻）：头装不下、锁存头一回丢名字时先报
/// `feature_sad{sent, dropped, feature_name, error_code}`（官方 `p(name, code, {sent, dropped})` 的键序，日志层再追加
/// `tengu_quizzical_giraffe`），进程头一回带上这个头再报 `feature_ok{api_tools_used_header}`。两条都进 Datadog。
pub(super) fn tools_used_header_events(
    first_use: bool,
    truncation: Option<(usize, usize)>,
) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    if let Some((sent, dropped)) = truncation {
        out.push((
            "tengu_feature_sad",
            json!({
                "sent": sent,
                "dropped": dropped,
                "feature_name": "api_tools_used_header",
                "error_code": "incomplete_list",
                "tengu_quizzical_giraffe": "unset"
            }),
        ));
    }
    if first_use {
        out.push(("tengu_feature_ok", json!({ "feature_name": "api_tools_used_header" })));
    }
    out
}

/// 模板事件是不是 IDE 那个 MCP 服务的（`transportType` 为 `ws-ide` / `sse-ide`）。
fn is_ide_transport(meta: &Value) -> bool {
    matches!(meta.get("transportType").and_then(|t| t.as_str()), Some("ws-ide" | "sse-ide"))
}

/// [`emit_template`] 的产物：带时间戳的事件与 Datadog 条目。
pub(super) type TplOutput = (Vec<(DateTime<Utc>, Value)>, Vec<Value>);
