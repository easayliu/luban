//! 官方 Claude Code 客户端的遥测模拟：**逐请求**那一半。
//!
//! 官方客户端每发一条 `/v1/messages`，都会在本地攒下一串 `tengu_*` 事件——发出前的
//! `tengu_api_query`、首字节到达时的 `tengu_feature_ok{api_request}`、结束时的
//! `tengu_api_success`（带上游 `request-id`、逐项 token 数、花费、TTFT）与 `tengu_turn_end`
//! ——然后分三路上报：一方事件 `POST /api/event_logging/v2/batch`（每 ~30s 一批）、Datadog
//! 日志（每 ~10s 一批）、OTel 指标 `POST /api/claude_code/metrics`（每 5 分钟）。
//!
//! 此前 luban 只有 [`crate::oauth`] 里的保活遥测：每张凭证每 30 分钟报一组「空闲版本检查」
//! 事件，`session.count` 恒为 1、`cost.usage` 恒为 0.042。于是上游看到的是一个账号有大量
//! `/v1/messages` 用量、遥测里却一条 API 调用都没有——这是比任何单个字段都显眼的破绽。
//! 本模块补上这一半：转发路径在响应流结束时把这条请求的形态与用量交给 [`Telemetry::record`]，
//! 由它按 `cap/2.1.258` 的事件链造出事件、攒批、按官方节奏发出。
//!
//! **身份取自实际发往上游的那份请求**：`metadata.user_id` 里的 `device_id`/`account_uuid`/
//! `session_id`（经过 [`crate::proxy`] 的身份改写之后的值）、出站 `anthropic-beta`、出站 UA
//! 的版本号，以及上游响应头里的 `anthropic-organization-id`。遥测那一侧与 `/v1/messages`
//! 那一侧必须是同一个人、同一台设备、同一个会话，否则两边一比对就是矛盾。
//!
//! 事件字段的取法逐项对照 `cap/2.1.258/00020`、`00032`（event_logging）与 `00019`、`00029`
//! （Datadog）。拿不到的量（客户端内部的消息条数、渲染路径等）按抓包里的规律估，见各处注释。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Bytes;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config;

// ---------- 静态事件模板（会话启动 / 每轮输入 / 每轮收尾） ----------

/// 模板里的一条事件，见 `assets/cc_telemetry_template.json` 顶部的说明。
#[derive(Debug, serde::Deserialize)]
struct TplEvent {
    /// 相对锚点的毫秒偏移。
    off: i64,
    /// `event`（`ClaudeCodeInternalEvent`）或 `growth`（`GrowthbookExperimentEvent`）。
    #[serde(rename = "type")]
    kind: String,
    /// 事件名；growth 那类是 `experiment_id`。
    name: String,
    #[serde(default)]
    meta: Value,
    /// 连续重复几条（`tengu_skill_loaded` 那串）。
    #[serde(default)]
    repeat: Option<u32>,
    /// growth：`experiment_metadata.feature_id`。
    #[serde(default)]
    feature: Option<String>,
    /// growth：`variation_id`。
    #[serde(default)]
    var: Option<i64>,
    /// 这条事件的 `additional_metadata` 阶段，见 [`MetaStage`]。缺省即
    /// [`MetaStage::Prompt`]（三项都写），模板里只给另外两阶段的事件标了这个字段。
    #[serde(default)]
    stage: MetaStage,
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
enum MetaStage {
    /// 只有 `subscription_type`：进程启动到界面起来之间。
    Startup,
    /// `renderer_mode` + `subscription_type`：界面起来了，用户还没提交。
    Renderer,
    /// 三项齐全：用户已经提交，这一轮有 `cc_prompt_id` 了。
    #[default]
    Prompt,
}

#[derive(Debug, serde::Deserialize)]
struct Template {
    /// 进程启动到第一次提交之间的那串（锚点：首条 `tengu_api_query`）。
    startup: Vec<TplEvent>,
    /// 会话第一次用户输入围绕 `tengu_api_query` 的那串。
    prompt: Vec<TplEvent>,
    /// 第二次起每次用户输入的那串（少了首轮才有的样式/记忆加载，多了几条附件计算）。
    prompt_next: Vec<TplEvent>,
    /// 只在会话第一次输入时多出来的那几条（首次拉 bootstrap / MCP 配置等）。
    first_prompt: Vec<TplEvent>,
    /// 每轮结束围绕 `tengu_api_success` 的那串。
    turn: Vec<TplEvent>,
    /// 会话第一轮结束后的版本检查那串。
    first_turn: Vec<TplEvent>,
    /// 会话跑久了才有的后台任务（2.1.285：插件自动更新检查、保留期清理），`off` 相对进程启动；
    /// 会话里后来的请求越过那个时刻才补发，见 `background_done`。2.1.260 那份没有。
    #[serde(default)]
    background: Vec<TplEvent>,
}

static TEMPLATE: std::sync::LazyLock<Template> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("assets/cc_telemetry_template.json"))
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
static TEMPLATE_2_1_285: std::sync::LazyLock<Template> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("assets/cc_telemetry_template_2_1_285.json"))
        .expect("assets/cc_telemetry_template_2_1_285.json must parse")
});

/// 2.1.285 `-p` 打印模式的模板（`cap/auto-2.1.285-20260930/00445`、`00448`，整理规则见文件顶部的
/// `_comment`）：进程只跑一轮，启动段连着输入与附件一直到首条 api_query，收尾段一直到进程退出。
static TEMPLATE_2_1_285_SDK: std::sync::LazyLock<Template> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("assets/cc_telemetry_template_2_1_285_sdk.json"))
        .expect("assets/cc_telemetry_template_2_1_285_sdk.json must parse")
});

/// 出站版本用哪一份模板：2.1.285 起用 [`TEMPLATE_2_1_285`]（`-p` 打印模式用
/// [`TEMPLATE_2_1_285_SDK`]），之前的仍是 2.1.260 那份。
fn template_for(version: &str, sdk: bool) -> &'static Template {
    match (version_at_least(version, "2.1.285"), sdk) {
        (true, true) => &TEMPLATE_2_1_285_SDK,
        (true, false) => &TEMPLATE_2_1_285,
        _ => &TEMPLATE,
    }
}

/// 官方 Datadog 那份日志只收这几类事件（`cap/2.1.258` 四批 285 条与 `cap/2.1.260-1` 六批
/// 对照：`tengu_feature_ok` 全部、`tengu_api_success`、启动那几条，其余只进 event_logging）。
///
/// `tengu_api_error` 与 `tengu_feature_bad` 抓包里没出现过——那几个会话没有失败请求；
/// 它们在 2.1.260 自己那份 Datadog 白名单里，故一并收（失败收尾那两条事件由
/// [`Telemetry::process`] 直接推 Datadog，不经这张表）。
const DD_EVENT_NAMES: &[&str] = &[
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
fn dd_listed(name: &str, version: &str) -> bool {
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

/// 模板占位符的取值。
struct Subst<'a> {
    version: &'a str,
    /// 展示模型名（`claude-opus-5[1m]`）。
    model: &'a str,
    /// 用户设置里的模型别名（`opus[1m]`），见 [`model_setting`]。
    model_setting: &'a str,
    permission_mode: &'a str,
    /// 这个会话是 `--resume` 回来的（`tengu_timer{startup}.resumed`）。
    resumed: bool,
    /// 第几次输入（`tengu_file_history_snapshot_success.snapshotCount`）。
    prompt_index: u32,
    /// 正文里有延迟加载的工具（`defer_loading: true`）。没有就不发
    /// `tengu_deferred_tools_pool_change`、`tengu_attachments` 里也没有 `deferred_tools_delta`
    /// ——官方 API-key 端（全量声明、无 ToolSearch）正是这个形态（`cap/2.1.258-api/00002`、
    /// `00022`），而订阅端延迟形态两者都有（`cap/2.1.258/00020`、`00032`）。
    deferred: bool,
    /// `tengu_tool_search_mode_decision` 的 meta，见 [`tool_search_decision`]。模板里那两条
    /// 的 meta 只是占位，一律以这份覆盖。
    tool_search: Value,
    /// `-p` 打印模式（SDK 模板）：附件的 `query_source` 与估算按 SDK 那套报，见
    /// [`fill_attachment_estimates`]。
    sdk: bool,
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
fn tool_search_decision(shape: &RequestShape, checked_model: &str, agent: bool) -> Value {
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
fn model_setting(display: &str) -> String {
    let bare = display.trim_end_matches("[1m]");
    let family = bare.strip_prefix("claude-").unwrap_or(bare);
    let family = family.split('-').next().unwrap_or(family);
    if display.ends_with("[1m]") { format!("{family}[1m]") } else { family.to_string() }
}

/// 把 meta 里的 `{{…}}` 占位符换成实际值（只动字符串）。
fn substitute(v: &Value, s: &Subst<'_>) -> Value {
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
fn emit_template<'a, F>(
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
            let c = ctx(t);
            events.push((t, id.event_at(e.stage, &e.name, t, &c, meta.clone())));
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

// ---------- 身份与事件构造（保活与逐请求两路共用） ----------

/// `org_type` → 遥测里的 `subscription_type`。
pub fn subscription_type(org_type: Option<&str>) -> &'static str {
    match org_type {
        Some(t) if t.contains("team") => "team",
        Some(t) if t.contains("enterprise") => "enterprise",
        _ => "individual",
    }
}

/// 一份遥测身份：发事件时所有 `env`/`auth`/`device_id` 之类的公共字段都从这里取。
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub session_id: String,
    /// sha256 hex，64 位。
    pub device_id: String,
    pub account_uuid: String,
    /// 组织 id：`/v1/messages` 响应头 `anthropic-organization-id` 学到的优先，没有就用凭证上
    /// 从 profile 存下来的 `org_uuid`（见 [`Telemetry::seed_org_uuid`]）；两处都没有才为
    /// `None`，此时 `auth` 块只带 `account_uuid`。
    pub organization_uuid: Option<String>,
    pub subscription_type: String,
    /// 客户端版本（`2.1.258`），与出站 UA 一致。
    pub version: String,
    /// 子代理支线号（出站头 `x-claude-code-agent-id`）。只有子代理与它的摘要请求那条链上的
    /// 事件才带：event_logging 在顶层 `device_id` 之后追 `agent_id` + `agent_type: "subagent"`，
    /// Datadog 在 `swe_bench_task_id` 之后（`cap/2.1.280` 子代理 a51764… 那一串）。
    pub agent_id: Option<String>,
    /// 工作目录的版本控制（`git`；不是仓库或不知道为 `None`）。有值时 `env` 在
    /// `is_local_agent_mode` 之后、Datadog 在 `deployment_environment` 之后多一项 `vcs`
    /// （`cap/auto-2.1.285-20260930` 全部事件都带，工作目录是 git 仓库）。
    pub vcs: Option<&'static str>,
    /// `/clear` 之后同一进程里开的新会话：每条事件顶层在 `device_id` 之后带上一个会话的 id
    /// （`cap/auto-2.1.285-20260930` 02c9… 那 142 条都带 `parent_session_id: fdea…`）。
    pub parent_session_id: Option<String>,
    /// `-p` 打印模式：`entrypoint` / `client_type` 报 `sdk-cli`，`is_interactive` 为 false
    /// （Datadog 那份是字串 `"false"`）。
    pub sdk: bool,
}

impl Identity {
    fn entrypoint(&self) -> &'static str {
        if self.sdk { "sdk-cli" } else { "cli" }
    }
}

/// 逐条事件变化的那几项。
pub struct EventCtx<'a> {
    /// 事件顶层 `model`：**展示名**（`claude-opus-5[1m]`），不是出站体里的规范名。
    pub model: &'a str,
    /// 事件顶层 `betas`：会话级 beta 集合，见 [`session_betas`]。
    pub betas: &'a str,
    /// `additional_metadata.cc_prompt_id`。
    pub prompt_id: &'a str,
    /// 进程运行秒数（`process.uptime`）。
    pub uptime_secs: f64,
}

impl Identity {
    /// `build_time`，按版本查表。
    pub fn build_time(&self) -> &'static str {
        config::cc_build_time(&self.version)
    }

    /// 所有事件共用的 `env` 块（键序照 `cap/2.1.258/00022`）。
    pub fn env_block(&self) -> Value {
        let mut env = json!({
            "platform": "darwin",
            "node_version": "v26.3.0",
            "terminal": "vscode",
            "package_managers": "npm,pnpm",
            "runtimes": "bun,node",
            "is_running_with_bun": true,
            "is_ci": false,
            "is_claubbit": false,
            "is_github_action": false,
            "is_claude_code_action": false,
            "is_claude_ai_auth": true,
            "version": &self.version,
            "arch": "arm64",
            "is_claude_code_remote": false,
            "deployment_environment": "unknown-darwin",
            "is_conductor": false,
            "version_base": &self.version,
            "build_time": self.build_time(),
            "is_local_agent_mode": false,
            "platform_raw": "darwin",
            "shell": "zsh"
        });
        if let Some(vcs) = self.vcs {
            insert_after(&mut env, "is_local_agent_mode", vec![("vcs", json!(vcs))]);
        }
        env
    }

    /// `auth` 块：官方带 `organization_uuid` + `account_uuid`（345/345 条），拿到组织 id 前
    /// 只能先带账号那一项。
    pub fn auth_block(&self) -> Value {
        match &self.organization_uuid {
            Some(org) => json!({ "organization_uuid": org, "account_uuid": &self.account_uuid }),
            None => json!({ "account_uuid": &self.account_uuid }),
        }
    }

    /// `additional_metadata`：标准 base64（**带填充**，抓包里以 `=` 收尾；此前保活用的
    /// url-safe 无填充是另一种编码，一眼可辨）。前几项固定，`extra` 追加在后。
    ///
    /// 前几项**按阶段给**（[`MetaStage`]）：启动早期只有 `subscription_type`，界面起来后
    /// 多 `renderer_mode`，用户提交后才多 `cc_prompt_id`。键序照抓包：`renderer_mode` →
    /// `subscription_type` → `cc_prompt_id`。
    ///
    /// `-p`（[`Self::sdk`]）不起终端界面，`renderer_mode` 一条都不写，`cc_prompt_id` 照常分阶段
    /// （`cap/auto-2.1.285-20260930` 九个 `-p` 会话 1751 条、Datadog 920 条都是这样）。
    fn metadata_b64_at(&self, stage: MetaStage, prompt_id: &str, extra: Value) -> String {
        let mut m = Map::new();
        if stage != MetaStage::Startup && !self.sdk {
            m.insert("renderer_mode".into(), "default".into());
        }
        m.insert("subscription_type".into(), self.subscription_type.clone().into());
        if stage == MetaStage::Prompt {
            m.insert("cc_prompt_id".into(), prompt_id.into());
        }
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                m.insert(k.clone(), v.clone());
            }
        }
        STANDARD.encode(Value::Object(m).to_string())
    }

    /// 一条 `ClaudeCodeInternalEvent`（`additional_metadata` 按用户输入之后那个阶段写）。
    pub fn event(&self, name: &str, ts: DateTime<Utc>, ctx: &EventCtx<'_>, extra: Value) -> Value {
        self.event_at(MetaStage::Prompt, name, ts, ctx, extra)
    }

    /// [`Self::event`] 的分阶段版本，见 [`MetaStage`]。
    fn event_at(
        &self,
        stage: MetaStage,
        name: &str,
        ts: DateTime<Utc>,
        ctx: &EventCtx<'_>,
        extra: Value,
    ) -> Value {
        // 顶层 `model` 跟事件自己的 meta.model 走（api_query 是这条请求的展示名、api_success
        // 是规范名，标题生成那条就是 haiku），没有 meta.model 的事件才用会话主模型
        // （`cap/2.1.260-2`：title_generated / tool_schema_sizes 顶层都是 `claude-opus-5[1m]`）。
        let model = extra.get("model").and_then(|m| m.as_str()).unwrap_or(ctx.model);
        // 顶层 `betas` 同理：**API 事件报这条请求的完整 beta 串**，界面事件报会话级那份。
        // `cap/2.1.260-2/00016` 里 `tengu_api_query`/`tengu_api_success` 的 `betas` 是出站头
        // 那一整串（含 `advanced-tool-use`/`effort`/`afk-mode`…），而同一批里的
        // `tengu_turn_end` 只有会话级那 9 项。一套 ctx 走天下就会把两者报成同一个值。
        let betas = extra.get("betas").and_then(|b| b.as_str()).unwrap_or(ctx.betas);
        let mut ev = json!({
            "event_type": "ClaudeCodeInternalEvent",
            "event_data": {
                "event_name": name,
                "client_timestamp": ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "model": model,
                "session_id": &self.session_id,
                "user_type": "external",
                "betas": betas,
                "env": self.env_block(),
                "entrypoint": self.entrypoint(),
                "is_interactive": !self.sdk,
                "client_type": self.entrypoint(),
                "process": process_b64(ctx.uptime_secs),
                "additional_metadata": self.metadata_b64_at(stage, ctx.prompt_id, extra),
                "auth": self.auth_block(),
                "event_id": uuid_v4(),
                "device_id": &self.device_id
            }
        });
        if let Some(parent) = &self.parent_session_id {
            ev["event_data"]["parent_session_id"] = json!(parent);
        }
        if let Some(agent) = &self.agent_id {
            ev["event_data"]["agent_id"] = json!(agent);
            ev["event_data"]["agent_type"] = json!("subagent");
        }
        ev
    }

    /// 一条 `GrowthbookExperimentEvent`（特性实验曝光，形态取自 `cap/2.1.260-1/00034`）。
    pub fn growth_event(
        &self,
        ts: DateTime<Utc>,
        experiment_id: &str,
        variation_id: i64,
        feature_id: &str,
        version: &str,
    ) -> Value {
        json!({
            "event_type": "GrowthbookExperimentEvent",
            "event_data": {
                "event_id": uuid_v4(),
                "timestamp": ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "experiment_id": experiment_id,
                "variation_id": variation_id,
                "environment": "production",
                "user_attributes": json!({ "appVersion": version }).to_string(),
                "experiment_metadata": json!({ "feature_id": feature_id }).to_string(),
                "device_id": &self.device_id,
                "auth": self.auth_block(),
                "session_id": &self.session_id
            }
        })
    }

    /// 一条 Datadog 日志（扁平形态，取自 `cap/2.1.258/00019`）。`extra` 是已经 snake_case 的
    /// 附加字段，直接平铺；带 `provider` 时 `ddtags` 里也多一项（api_success 的形态）。
    ///
    /// 手工建表而不是一个大 `json!`：字段太多会撞宏的 recursion_limit。
    /// 一条 Datadog 日志（`additional_metadata` 那三项在这里是平铺的顶层字段，按用户输入
    /// 之后那个阶段写）。
    pub fn dd_entry(&self, message: &str, ctx: &EventCtx<'_>, model: &str, extra: Value) -> Value {
        self.dd_entry_at(MetaStage::Prompt, message, ctx, model, extra)
    }

    /// [`Self::dd_entry`] 的分阶段版本。
    ///
    /// Datadog 那份与 event_logging 完全同一套阶段规则（`cap/2.1.260-2/00017` 一批 80 条：
    /// 59 条既无 `renderer_mode` 也无 `prompt_id`，7 条只有 `renderer_mode`，16 条两者都有）。
    /// 原先这两项是无条件写的，于是每个会话有近六十条启动日志带着「界面模式」和一个
    /// 当时还不存在的 prompt id。
    fn dd_entry_at(
        &self,
        stage: MetaStage,
        message: &str,
        ctx: &EventCtx<'_>,
        model: &str,
        extra: Value,
    ) -> Value {
        let s = |v: &str| Value::String(v.to_string());
        // 附加字段平铺在公共字段之后，同名会盖掉：`tengu_api_success` 的 meta 自带 `model`
        // （这条请求实际用的规范名），于是 DD 那份的 `model` 与 `ddtags` 都跟它走——
        // `cap/2.1.258/00019` 里会话模型是 `claude-opus-5[1m]`，api_success 那条却是
        // `model:claude-opus-5`，正是被 meta 盖掉的结果。其它事件没有 meta.model，用会话主模型。
        // Datadog 那份对 meta.model 还会去掉日期后缀：标题那条是 `claude-haiku-4-5`
        // （`cap/2.1.260-2/00062`），opus 没有后缀所以看不出来。
        let short = extra.get("model").and_then(|m| m.as_str()).map(dd_model_short);
        let model = short.as_deref().unwrap_or(model);
        // `event:` 之后的标签按键名字母序：`provider` 落在 platform 与 subscription_type 之间
        // （api_success），tether 判定的 `decision` / `reason` 分别落在 client_type 之后与
        // platform 之后（`cap/2.1.280` 的 Datadog 批次）。
        let extra_tag =
            |k: &str| extra.get(k).and_then(|p| p.as_str()).map(|v| format!("{k}:{v},"));
        let provider_tag = extra_tag("provider").unwrap_or_default();
        // 2.1.285 的 api_success 多一个 `uncovered_tail_reason`，落在 subscription_type 之后、
        // user_bucket 之前（`cap/2.1.285/00037`）。
        let tail_tag = extra_tag("uncovered_tail_reason").unwrap_or_default();
        // `tengu_tool_use_success` 的 ddtags 在 subscription_type 之后多一个 `tool_name:`
        // （`cap/2.1.280`、`cap/2.1.285` 的 Datadog 批次都有）。
        let tool_tag = if message == "tengu_tool_use_success" {
            extra_tag("tool_name").unwrap_or_default()
        } else {
            String::new()
        };
        let (decision_tag, reason_tag) = if message == "tengu_tether_decision" {
            (extra_tag("decision").unwrap_or_default(), extra_tag("reason").unwrap_or_default())
        } else {
            (String::new(), String::new())
        };
        let mut m = Map::new();
        m.insert("ddsource".into(), s("nodejs"));
        m.insert(
            "ddtags".into(),
            s(&format!(
                "event:{message},arch:arm64,client_type:{ep},{decision_tag}entrypoint:{ep},\
                 model:{model},platform:darwin,{provider_tag}{reason_tag}subscription_type:{},\
                 {tool_tag}{tail_tag}user_bucket:15,user_type:external,version:{v},version_base:{v}",
                self.subscription_type,
                v = self.version,
                ep = self.entrypoint(),
            )),
        );
        m.insert("message".into(), s(message));
        m.insert("service".into(), s("claude-code"));
        m.insert("hostname".into(), s("claude-code"));
        m.insert("env".into(), s("external"));
        m.insert("model".into(), s(model));
        m.insert("session_id".into(), s(&self.session_id));
        m.insert("user_type".into(), s("external"));
        // 同 `model`：meta 自带 `betas` 的（只有 `tengu_api_success`）就跟它走，报这条请求的
        // 完整 beta 串；其余事件用会话级那份。`cap/2.1.260-2/00017` 里整批 80 条日志只有
        // `tengu_api_success` 那条的 betas 带着 `afk-mode`/`extended-cache-ttl`。
        m.insert(
            "betas".into(),
            s(extra.get("betas").and_then(|b| b.as_str()).unwrap_or(ctx.betas)),
        );
        // 紧跟 `betas`，不在后面那组布尔值里（`cap/2.1.280`、`cap/2.1.285` 的 Datadog 批次
        // 989 条无一例外）。
        m.insert("is_claude_ai_auth".into(), Value::Bool(true));
        m.insert("entrypoint".into(), s(self.entrypoint()));
        m.insert("is_interactive".into(), s(if self.sdk { "false" } else { "true" }));
        m.insert("client_type".into(), s(self.entrypoint()));
        m.insert("process_metrics".into(), process_metrics(ctx.uptime_secs));
        for k in ["swe_bench_run_id", "swe_bench_instance_id", "swe_bench_task_id"] {
            m.insert(k.into(), s(""));
        }
        if let Some(agent) = &self.agent_id {
            m.insert("agent_id".into(), s(agent));
            m.insert("agent_type".into(), s("subagent"));
        }
        m.insert("subscription_type".into(), s(&self.subscription_type));
        // 分阶段，见 [`MetaStage`]：启动早期两项都没有，界面起来后只有 `renderer_mode`，
        // 用户提交后才多 `prompt_id`。`-p` 没有界面，不写 `renderer_mode`（同 [`Self::metadata_b64_at`]）。
        if stage != MetaStage::Startup && !self.sdk {
            m.insert("renderer_mode".into(), s("default"));
        }
        if stage == MetaStage::Prompt {
            m.insert("prompt_id".into(), s(ctx.prompt_id));
        }
        m.insert("platform".into(), s("darwin"));
        m.insert("platform_raw".into(), s("darwin"));
        m.insert("arch".into(), s("arm64"));
        m.insert("node_version".into(), s("v26.3.0"));
        m.insert("terminal".into(), s("vscode"));
        m.insert("shell".into(), s("zsh"));
        m.insert("package_managers".into(), s("npm,pnpm"));
        m.insert("runtimes".into(), s("bun,node"));
        for (k, v) in [
            ("is_running_with_bun", true),
            ("is_ci", false),
            ("is_claubbit", false),
            ("is_claude_code_remote", false),
            ("is_local_agent_mode", false),
            ("is_conductor", false),
            ("is_github_action", false),
            ("is_claude_code_action", false),
        ] {
            m.insert(k.into(), Value::Bool(v));
        }
        m.insert("version".into(), s(&self.version));
        m.insert("version_base".into(), s(&self.version));
        m.insert("build_time".into(), s(self.build_time()));
        m.insert("deployment_environment".into(), s("unknown-darwin"));
        if let Some(vcs) = self.vcs {
            m.insert("vcs".into(), s(vcs));
        }
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                m.insert(k.clone(), v.clone());
            }
        }
        // meta 里那份 `model` 是全名，DD 顶层要的是去掉日期后缀的那份，盖回去。
        m.insert("model".into(), s(model));
        m.insert("user_bucket".into(), Value::Number(15.into()));
        Value::Object(m)
    }
}

/// `process` 运行时指标：随运行时长缓慢增长的 rss/heap/cpu（真实值在 300MB 上下浮动）。
pub fn process_metrics(uptime_secs: f64) -> Value {
    let rss = 300_000_000.0 + uptime_secs * 6.0;
    let heap = 120_000_000.0 + uptime_secs * 5.0;
    json!({
        "uptime": uptime_secs,
        "rss": rss as u64,
        "heapTotal": (heap * 0.72) as u64,
        "heapUsed": heap as u64,
        "external": (50_000_000.0 + uptime_secs * 12.0) as u64,
        "arrayBuffers": 1_300_000_u64,
        "constrainedMemory": 34_359_738_368_u64,
        "cpuUsage": {
            "user": (1_200_000.0 + uptime_secs * 6300.0) as u64,
            "system": (190_000.0 + uptime_secs * 1200.0) as u64
        }
    })
}

/// base64 编码的 `process`（标准字典、带填充，同 `Identity::metadata_b64_at`）。
pub fn process_b64(uptime_secs: f64) -> String {
    STANDARD.encode(process_metrics(uptime_secs).to_string())
}

/// 随机 UUID v4。
pub fn uuid_v4() -> String {
    let mut buf = [0u8; 16];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut buf);
    buf[6] = (buf[6] & 0x0F) | 0x40;
    buf[8] = (buf[8] & 0x3F) | 0x80;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        u16::from_be_bytes([buf[4], buf[5]]),
        u16::from_be_bytes([buf[6], buf[7]]),
        u16::from_be_bytes([buf[8], buf[9]]),
        u64::from_be_bytes([0, 0, buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]),
    )
}

/// Datadog 的 `model` 字段去掉 `-YYYYMMDD` 日期后缀：`claude-haiku-4-5-20251001` → `claude-haiku-4-5`。
fn dd_model_short(model: &str) -> String {
    match model.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) => {
            head.to_string()
        }
        _ => model.to_string(),
    }
}

/// camelCase → snake_case，按 Datadog 那份扁平日志的口径：**每个大写字母前插一个下划线**，
/// 于是 `costUSD` → `cost_u_s_d`、`isTTY` → `is_t_t_y`（抓包原样如此），已经是 snake 的键不动。
pub fn camel_to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// 从出站 `anthropic-beta` 里筛出会话级那几项，顺序照原串。见
/// [`config::TELEMETRY_SESSION_BETA_PREFIXES`]。
pub fn session_betas(header: &str) -> String {
    // haiku 的出站头把 `claude-code` 排在中间（`…prompt-caching-scope,claude-code,advisor…`），
    // 它的会话级那份**没有**这一项（`cap/2.1.285/00060`、`cap/2.1.280/00041` 的 haiku 事件：
    // `oauth,interleaved,redact,ttc,cm,pcs`）；opus / fable / sonnet 以它开头的照留。
    let leads_with_cc = header.trim_start().starts_with(config::CC_BETA_CLAUDE_CODE);
    let mut out: Vec<&str> = header
        .split(',')
        .map(str::trim)
        .filter(|b| config::TELEMETRY_SESSION_BETA_PREFIXES.iter().any(|p| b.starts_with(p)))
        .filter(|b| leads_with_cc || !b.starts_with("claude-code-"))
        .collect();
    // `redact-thinking` **不跟请求头走**：会话级那份始终带着它。
    //
    // `cap/2.1.260-2/00016` 是硬证据——同一批事件里，`tengu_api_query` 的 betas（= 出站头）
    // **没有** `redact-thinking`（2.1.260 的 opus 主线程已经不发了），而 `tengu_turn_end`
    // 那份会话级的**有**。也就是说会话级集合是客户端自己的一张固定表，不是请求头的子集。
    // 只按头过滤，2.1.260 起每个会话的界面事件都会少这一项。
    //
    // 落位在 `interleaved-thinking` 之后（抓包序，也正是
    // [`config::TELEMETRY_SESSION_BETA_PREFIXES`] 里的位置）。
    if !out.iter().any(|b| b.starts_with("redact-thinking-")) {
        let at = out
            .iter()
            .position(|b| b.starts_with("interleaved-thinking-"))
            .map_or(out.len(), |i| i + 1);
        out.insert(at, config::CC_BETA_REDACT_THINKING);
    }
    out.join(",")
}

/// 出站 UA（`claude-cli/2.1.258 (external, cli)`）里的版本号；认不出时退回
/// [`config::CC_VERSION_BASE`]。
pub fn version_from_ua(ua: &str) -> String {
    ua.strip_prefix("claude-cli/")
        .or_else(|| ua.strip_prefix("claude-code/"))
        .and_then(|rest| rest.split([' ', '(']).next())
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map(str::to_string)
        .unwrap_or_else(|| config::CC_VERSION_BASE.to_string())
}

// ---------- 上报 ----------

/// 一次 HTTP 上报的结果：`Some(status)`；网络层失败为 `None`。
pub async fn post_event_logging(
    client: &wreq::Client,
    access_token: &str,
    version: &str,
    events: &[Value],
) -> Option<u16> {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_EVENT_LOGGING);
    crate::oauth::axios(
        client
            .post(&url)
            .header("Accept", config::AXIOS_ACCEPT)
            .header("Content-Type", "application/json")
            .header("User-Agent", format!("claude-code/{version}"))
            .header("x-service-name", "claude-code")
            .header("Authorization", format!("Bearer {access_token}"))
            .header("anthropic-beta", config::OAUTH_BETA_HEADER)
            .json(&json!({ "events": events })),
        "event_logging",
    )
    .send()
    .await
    .ok()
    .map(|r| r.status().as_u16())
}

/// Datadog 日志摄入。真实客户端用 axios 直发，不带 Authorization。
pub async fn post_datadog(client: &wreq::Client, entries: &[Value]) -> Option<u16> {
    crate::oauth::axios(
        client
            .post(config::DATADOG_INTAKE_URL)
            .header("Accept", config::AXIOS_ACCEPT)
            .header("Content-Type", "application/json")
            .header("DD-API-KEY", config::DATADOG_API_KEY)
            .header("User-Agent", config::DATADOG_USER_AGENT)
            .json(&entries),
        "datadog",
    )
    .send()
    .await
    .ok()
    .map(|r| r.status().as_u16())
}

/// OTel 指标。
pub async fn post_metrics(
    client: &wreq::Client,
    access_token: &str,
    version: &str,
    body: &Value,
) -> Option<u16> {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_METRICS);
    crate::oauth::axios(
        client
            .post(&url)
            .header("Accept", config::AXIOS_ACCEPT)
            .header("Content-Type", "application/json")
            .header("User-Agent", format!("claude-code/{version}"))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("anthropic-beta", config::OAUTH_BETA_HEADER)
            .json(body),
        "metrics",
    )
    .send()
    .await
    .ok()
    .map(|r| r.status().as_u16())
}

// ---------- 逐请求：转发路径交过来的一条 API 调用 ----------

/// 出站请求上 2.1.277 起的三个 `x-claude-code-*` 头（`cap/2.1.280/00165`）：子代理带
/// `agent-id`（支线号，17 位 hex）与 `agent-type`（内置的写类型名如 `Explore`，自定义的写
/// `custom`），每条都带 `request-class`（`main` / `subagent` / `auxiliary`）。
///
/// 遥测靠它们认出子代理与它的摘要请求（`request-class: auxiliary` 且带 `agent-id`），
/// **只给这两类**的事件顶层写 `agent_id` / `agent_type`——主线程与其余侧查询的事件没有
/// 这两项。模拟路径只写 `request-class`，另两个只有真 CC 来访才有。
#[derive(Debug, Clone, Default)]
pub struct AgentHeaders {
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub request_class: Option<String>,
}

/// 转发路径在响应流结束时交过来的一条已完成的 `/v1/messages`。
///
/// 请求侧的量都从 `body`（**实际发往上游的那份**）里解析，响应侧的量由
/// [`crate::proxy`] 的用量嗅探给出。
pub struct ApiCall {
    pub cred_id: i64,
    pub account_uuid: Option<String>,
    pub org_type: Option<String>,
    /// 实际发往上游的请求体。
    pub body: Bytes,
    /// 实际发出的 `anthropic-beta`。
    pub betas: Option<String>,
    /// 实际发出的 `X-Claude-Code-Session-Id`（body 里没有时的兜底）。
    pub session_header: Option<String>,
    /// 实际发出的 UA，取版本号用。
    pub ua_out: String,
    /// 上游响应头 `anthropic-organization-id`。
    pub organization_id: Option<String>,
    /// 请求发出的时刻。
    pub started_at: SystemTime,
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    /// 上游响应头 `request-id`。
    pub request_id: Option<String>,
    /// 出站的 `x-client-request-id`（官方客户端每请求一个 uuid v4）。失败事件要报它。
    pub client_request_id: Option<String>,
    /// 出站的 `x-claude-code-*` 头，见 [`AgentHeaders`]。
    pub agent: AgentHeaders,
    /// 响应里的 `message.id`（`msg_…`）。
    pub message_id: Option<String>,
    pub stop_reason: Option<String>,
    /// 响应回报的模型名（规范名）。
    pub resp_model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    /// 响应正文里 text / thinking 的字符数。
    pub text_chars: usize,
    /// 这条回复按 `inputTextCharLength` 口径的字数，见 [`ThreadBase::reply_chars`]。
    pub reply_input_chars: usize,
    pub thinking_chars: usize,
    /// 响应里出现过思考块（`redacted_thinking` 与空思考块都算），决定要不要报
    /// `thinkingContentLength`。
    pub saw_thinking: bool,
    /// 响应里每个工具的入参 JSON 字符数（工具名 → 之和，按首次出现排序），
    /// 即 `toolUseContentLengths`。
    pub tool_use_lens: Vec<(String, usize)>,
    /// 回复里每个要客户端执行的工具调用（id、名字、入参、auto 模式的服务端判决），见
    /// [`ToolCall`]。
    pub tool_calls: Vec<ToolCall>,
    pub cost_usd: Option<f64>,
    pub speed: Option<String>,
    /// 这条请求**失败**了：报 `tengu_api_error` 而不是 `tengu_api_success`。
    pub failure: Option<CallFailure>,
    /// 流式回复还没收尾，客户端就把连接掐了（按 Esc 打断、猜下一句被新输入顶掉）：官方那头
    /// 既不报 success 也不报 error，报的是取消那一串（`aborted_streaming`）。
    pub aborted: bool,
}

/// 回复里的一个工具调用。下一条请求把它的结果带回来时，那串工具事件（入参字节数、Bash 的
/// 命令分类、auto 模式的判决）照它报。
#[derive(Debug, Clone, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// 响应 `safeguard_results` 里给它的判决：`not_flagged` / `flagged` / `skipped`；没有判决
    /// （default 模式、只读工具）为 `None`。
    pub verdict: Option<String>,
}

/// 一条失败请求客户端那头看到的东西。
///
/// 官方客户端对失败请求发的是 `tengu_api_error`（外加一条
/// `tengu_feature_bad{api_request}` 与一条 `terminal_reason: "api_error"` 的
/// `tengu_turn_end`），字段与 `tengu_api_success` 大半重合但不含任何用量。
pub struct CallFailure {
    /// HTTP 状态码；中途 `event: error` 那种客户端拿到的是 200 + 错误负载，SDK 那边
    /// 状态码为空，故这里也留空（见 `in_band`）。
    pub status: Option<u16>,
    /// 上游 `error.type`（`rate_limit_error`、`overloaded_error`…）。
    pub error_type: Option<String>,
    /// 上游 `error.message`，或本地对断流的描述。
    pub message: String,
    /// 错误是裹在 200 里的流内事件（`event: error`）而不是 HTTP 状态码。官方对这类的
    /// `errorType` 报 `in_band_<上游 type>`。
    pub in_band: bool,
}

/// 转发路径在建 `ReqLog` 时先攒好的那部分（响应侧的量在流结束时才有）。
pub struct Capture {
    pub sink: Telemetry,
    pub account_uuid: Option<String>,
    pub org_type: Option<String>,
    pub body: Bytes,
    pub betas: Option<String>,
    pub session_header: Option<String>,
    /// 实际发出的 `x-client-request-id`。
    pub client_request_id: Option<String>,
    pub agent: AgentHeaders,
    pub organization_id: Option<String>,
    pub started_at: SystemTime,
}

impl Capture {
    /// 一条**没走到正常收尾**的请求：连接层就失败了（`ReqLog` 压根没建起来），或者在
    /// `ReqLog` 建起来之前就早退了（401 换号那条路）。响应侧的量一概没有，收尾走
    /// [`Telemetry::process`] 的失败分支，与正常路径上那条 `tengu_api_error` 同一套。
    ///
    /// 放在这里而不是在 [`crate::proxy`] 里现拼一个 [`ApiCall`]：那结构二十多个字段，
    /// 在调用点各写一份迟早会漂开——「哪些字段在失败时该留空」是这一侧的知识。
    pub fn record_failure(
        self,
        cred_id: i64,
        ua_out: String,
        total_ms: u64,
        request_id: Option<String>,
        failure: CallFailure,
    ) {
        let sink = self.sink.clone();
        sink.record(ApiCall {
            cred_id,
            account_uuid: self.account_uuid,
            org_type: self.org_type,
            body: self.body,
            betas: self.betas,
            session_header: self.session_header,
            ua_out,
            organization_id: self.organization_id,
            started_at: self.started_at,
            // 首字节从未到达（连接层失败），或响应体没读成（401 那条）。
            ttft_ms: None,
            total_ms,
            request_id,
            client_request_id: self.client_request_id,
            agent: self.agent,
            message_id: None,
            stop_reason: None,
            resp_model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            text_chars: 0,
            reply_input_chars: 0,
            thinking_chars: 0,
            saw_thinking: false,
            tool_use_lens: Vec::new(),
            tool_calls: Vec::new(),
            cost_usd: None,
            speed: None,
            failure: Some(failure),
            aborted: false,
        });
    }
}

/// 这条请求在会话里扮演的角色，决定 `querySource` 与要不要发 turn 级事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
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
    fn query_source(self) -> &'static str {
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
    fn category(self) -> &'static str {
        match self {
            Kind::Main => "main",
            Kind::Subagent => "subagent",
            _ => "auxiliary",
        }
    }
    /// 带 `queryChainId` / `queryDepth` 的那几类；标题那类查询没有链。
    fn has_chain(self) -> bool {
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
    fn has_boundary(self) -> bool {
        self.has_chain()
    }
    /// 系统提示词里有静态/动态分界（`tengu_sysprompt_boundary_found`）。子代理那份没有：
    /// 报两条 `tengu_sysprompt_missing_boundary_marker{promptBlockCount: 6}`（`cap/2.1.277`
    /// 自定义子代理与 `cap/2.1.280` Explore 子代理共 58 次请求，全是 6）。
    fn has_boundary_marker(self) -> bool {
        matches!(
            self,
            Kind::Main | Kind::Suggestion | Kind::AwaySummary | Kind::SideQuestion | Kind::Compact
        )
    }
    /// 从主线程分叉出来、不写缓存的那几类（`skipCacheWrite: true`）。
    fn is_fork(self) -> bool {
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
    fn is_side_query(self) -> bool {
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
    fn is_agent(self) -> bool {
        matches!(self, Kind::Subagent | Kind::AgentSummary)
    }
}

/// 子代理请求的 `querySource`：内置的是 `agent:builtin:<类型>`（`cap/2.1.280` Explore），
/// 自定义的是 `agent:custom`（`cap/2.1.277`，请求头 `agent-type: custom`）。没有请求头
/// （2.1.277 之前的客户端）按内置的 general-purpose 报。
fn agent_query_source(agent_type: Option<&str>) -> String {
    match agent_type.filter(|t| !t.is_empty()) {
        Some("custom") => "agent:custom".to_string(),
        Some(t) => format!("agent:builtin:{t}"),
        None => "agent:builtin:general-purpose".to_string(),
    }
}

/// `2.1.260` 这种三段版本号是否不低于 `min`。
fn version_at_least(v: &str, min: &str) -> bool {
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
fn fill_attachment_estimates(meta: &mut Value, query_source: &str, sdk: bool, model: &str) {
    let Some(obj) = meta.as_object_mut() else { return };
    let types: Vec<String> = obj
        .get("attachment_types")
        .and_then(|t| t.as_array())
        .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let haiku = model.contains("haiku");
    let model_estimate = if haiku {
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

/// 模型的默认 effort（`tengu_api_success.default_effort_level`）：opus-5-5 与 sonnet-5-5 是
/// `medium`，opus-4-7 是 `xhigh`，其余带 effort 的都是 `high`（`cap/2.1.285` 11 个模型、
/// `cap/2.1.280` 的 opus-5-5[1m]）。
fn default_effort_of(model: &str) -> &'static str {
    if model.starts_with("claude-opus-5-5") || model.starts_with("claude-sonnet-5-5") {
        "medium"
    } else if model.starts_with("claude-opus-4-7") {
        "xhigh"
    } else {
        "high"
    }
}

/// 工具跑完那条 `tengu_feature_ok` 的名字：`tool_` + 工具名的 snake_case（`WebFetch` →
/// `tool_web_fetch`、`SubagentHandback` → `tool_subagent_handback`）。
fn tool_feature_name(tool: &str) -> String {
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
fn insert_after(v: &mut Value, after: &str, pairs: Vec<(&str, Value)>) {
    let Some(obj) = v.as_object_mut() else { return };
    let at = obj.keys().position(|k| k == after).map_or(obj.len(), |i| i + 1);
    for (i, (k, val)) in pairs.into_iter().enumerate() {
        obj.shift_insert(at + i, k.to_string(), val);
    }
}

/// billing header 的 `cc_turn_origin` 换成事件里的写法：下划线换连字符
/// （`task_notification` → `task-notification`，`cap/2.1.280/00180` 与同批事件），
/// `human` / `peer` 原样。
fn turn_origin_of(header: &str) -> String {
    header.replace('_', "-")
}

/// `tengu_api_success.snapshotHash`：12 位 hex，整个会话恒定、换会话就变（`cap/2.1.277`
/// 与 `cap/2.1.280` 两个会话 `system[1]`、`system[2]` 逐字相同，hash 却不同，只有末块
/// 动态段不同）。官方算的是客户端内部那份快照，代理看不到；取动态段的 hash 再加盐派生，
/// 满足「会话内恒定、会话间不同」，又不与任何一块的原始 sha256 撞上。
fn snapshot_hash_of(shape: &RequestShape) -> String {
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
fn model_held_stateless(model: &str) -> bool {
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
fn tether_decide(
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

/// 上一条回复里的一次工具调用（续轮请求的倒数第二条 assistant 消息里的 `tool_use` 块）。
#[derive(Debug, Clone, Default)]
struct ToolUse {
    id: String,
    name: String,
    /// 工具入参：请求历史里 assistant 那条的 `tool_use.input`，或（thread 续轮看不到历史）回复
    /// 流里记下的那份（[`ToolCall`]）。都没有是 `Null`。
    input: Value,
    /// auto 模式的服务端判决，见 [`ToolCall::verdict`]。
    verdict: Option<String>,
    /// `tool_result.is_error`：工具执行失败（Bash 非零退出、被用户拒绝……）。
    is_error: bool,
    /// `tool_result` 正文的开头（至多 400 字符），认「被用户拒绝」「命令失败」用。
    result_head: String,
    /// 见 [`ToolResultInfo::stripped_bytes`] / [`ToolResultInfo::image_b64`] /
    /// [`ToolResultInfo::media_blocks`]。
    stripped_bytes: usize,
    image_b64: usize,
    media_blocks: usize,
    /// `input` 的 JSON 字符数。
    input_len: usize,
    /// Bash 的 `command` 长度（其它工具为 0）。
    command_len: usize,
    /// 对应 `tool_result` 的内容字符数。
    result_len: usize,
    /// 结果太大被客户端落了盘、请求里只带预览（`<persisted-output>` 开头）时，预览里写的原始
    /// 大小（字节，按 `Output too large (58.9KB)` 还原）。见 [`persisted_original_size`]。
    persisted_from: Option<usize>,
}

/// 被落盘的工具结果在请求里是一段预览：`<persisted-output>\nOutput too large (58.9KB). Full
/// output saved to: …`（`cap/2.1.285/00131`）。返回预览里写的原始大小（KB / MB 换成字节）；
/// 不是这个形态返回 `None`。官方那条 `tool_use_success.toolResultSizeBytes` 报的是原始大小
/// （60278，预览写 58.9KB），落盘本身另有一条 `tengu_tool_result_persisted`。
fn persisted_original_size(content: &Value) -> Option<usize> {
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
struct RequestShape {
    model: String,
    messages_len: usize,
    /// 客户端启动时探额度的那条：haiku、`max_tokens: 1`、唯一一条消息就是 `quota`、无 system
    /// 无 tools（`cap/2.1.260-1/00004`、`00021`）。官方对它**不发任何 api 事件**（那个会话的
    /// 首批里只有真实对话那一条 `tengu_api_success`），所以遥测这边要跳过。
    quota_probe: bool,
    /// 末条消息是用户新输入（而不是 tool_result 续轮）。
    new_prompt: bool,
    /// 用户新输入的字符数。
    prompt_len: usize,
    /// billing header 里的 `cc_prompt_id`。
    cc_prompt_id: Option<String>,
    /// billing header 里的 `cc_prev_req`：**这条请求自己声明的上一条 request-id**。
    ///
    /// 与 `previousRequestId` 是同一件事——`cap/2.1.260-2` 那个会话三条续轮逐字相同
    /// （体里 `cc_prev_req=req_011CeiBW8Yx9…` ↔ 事件里 `previousRequestId` 同值），官方
    /// 两处出自同一个 `requestJournal`。故它是权威源：出站体已经这么发给上游了，遥测
    /// 再从会话状态另算一份，两份不一致时上游把请求体和事件批一 join 就能看出来。
    cc_prev_req: Option<String>,
    /// billing header 里的 `cc_turn_origin`：这一轮是谁发起的（`human` / `peer` /
    /// `task_notification`…），见 [`turn_origin_of`]。
    turn_origin: Option<String>,
    /// 顶层 `diagnostics.previous_message_id`：同理，是这条请求自己声明的上一条 message.id。
    /// 空值（`null`）与字段缺失都记 `None`。
    diag_prev_message_id: Option<String>,
    is_subagent: bool,
    system_blocks: usize,
    system_chars: usize,
    /// `system[0]`（billing header）的长度与 sha256。
    sys0_len: usize,
    sys0_hash: String,
    /// 倒数第二块 / 最后一块的长度（`tengu_sysprompt_boundary_found`）。
    static_len: usize,
    dynamic_len: usize,
    tools_count: usize,
    tools_chars: usize,
    tools_hash: String,
    /// `{"Agent":3078,…}` 那串 JSON。
    tool_lens: String,
    deferred_tools: usize,
    /// 正文里**非延迟**的 `mcp__*` 工具数（`tengu_tool_search_mode_decision.mcpToolCount`
    /// 在非延迟形态下的取值：`cap/2.1.258-api` 报 2，正文里正好是两个 `mcp__ide__*`）。
    mcp_tools: usize,
    /// 正文里声明了 `ToolSearch` 这个工具（非延迟）。它是延迟加载真正能用的前提：模型要靠它
    /// 把 `defer_loading` 的工具搜出来。只有占位声明、没有它，搜索无处发起。
    has_tool_search: bool,
    input_text_chars: usize,
    /// `estimatedInputTokens`，见 [`parse_shape`] 里的口径说明。
    estimated_tokens: usize,
    image_blocks: usize,
    image_bytes: usize,
    doc_blocks: usize,
    doc_bytes: usize,
    temperature: f64,
    thinking_type: String,
    /// `output_config.effort`；侧查询（标题生成）不带。
    effort: Option<String>,
    fast_mode: bool,
    permission_mode: &'static str,
    cache_ttl_1h: bool,
    /// 体里有没有任何 `cache_control`（标题那类没有 → `cachingEnabled: false`）。
    has_cache_control: bool,
    api_system_messages: usize,
    /// 末条用户消息以 `[SUGGESTION MODE:` 开头。
    suggestion: bool,
    /// 末条用户消息是离开回顾那句固定提示（见 [`Kind::AwaySummary`]）。
    away_summary: bool,
    /// system 里有会话标题生成的指令。
    title: bool,
    /// system 里有会话起名（kebab-case）的指令，见 [`Kind::RenameName`]。
    rename: bool,
    /// 工具只有 `web_search` 这一个 server tool：WebSearch 工具另发的那条，见 [`Kind::WebSearchTool`]。
    web_search_tool: bool,
    /// 无工具、唯一一条用户消息以 `Web page content:` 开头：WebFetch 取回网页后的页面处理。
    web_fetch_page: bool,
    /// 有 `system`、无工具、`max_tokens: 1`：`/model` 选完模型后的「Hi」探测，见
    /// [`Kind::ModelValidation`]。额度探测没有 `system`，另见 `quota_probe`。
    model_validation: bool,
    /// 末条用户消息里有 `/btw` 插问那段提醒，见 [`Kind::SideQuestion`]。
    side_question: bool,
    /// 末条用户消息以 `/compact` 的「CRITICAL: Respond with TEXT ONLY」开头，见 [`Kind::Compact`]。
    compact: bool,
    /// billing header 的 `cc_entrypoint=sdk-cli`：`-p` 打印模式，主线程报 `querySource: sdk`。
    sdk: bool,
    /// 这一轮是 `!` 跑的 shell 命令（末条用户消息以 `<bash-input>` 开头）：报 `tengu_input_bash`
    /// 而不是 `tengu_input_prompt`，`turn_origin` 是 `unstamped`（`cap/auto-2.1.285-20260930/00191`）。
    bash_input: bool,
    /// `!` 跑的那条命令与它的输出字数（`<bash-input>` / `<bash-stdout>`）。
    bash_typed: Option<(String, usize)>,
    /// 请求里环境那段写的工作目录（`Primary working directory: …`）。
    cwd: Option<String>,
    /// 环境那段的 `Is a git repository: true/false`。
    git_repo: Option<bool>,
    /// 开场附上的 `gitStatus` 里 `Status:` 下面有没有改动（`M calc.py`、`?? x`），没有这段为 `None`。
    git_dirty: Option<bool>,
    /// 这次新输入里夹着的后台任务通知（`[SYSTEM NOTIFICATION - NOT USER INPUT]` 那段提醒里的
    /// `<task-notification>…</task-notification>`，UTF-16 长度）：通知是在上一轮收尾时送到的，
    /// 没有单独发请求，官方当时就报了一条 `turn_origin: task-notification` 的输入
    /// （`cap/auto-2.1.285-20260930` 07:51:33.115，`prompt_length` 954 正是那一段）。
    merged_notifications: Vec<usize>,
    /// 这次新输入之前，上一轮在权限弹框上被用户拒掉的工具（结果是「The user doesn't want to
    /// proceed…」那段），见 [`last_is_new_prompt`]。
    rejected: Vec<ToolResultInfo>,
    /// 这一轮是提示词型斜杠命令起的头（`<command-message>init</command-message>`）：这一轮每条
    /// 主线程请求的 `tengu_api_success` 都带 `attributionSkill`（`00266`–`00268`）。
    command_skill: Option<String>,
    /// `permission_mode` 取自请求体 `safeguards[].classifier_context.permission_mode`——客户端
    /// 自己写的，比从消息里的提示推断可靠（plan 模式也只在这里）。
    permission_declared: bool,
    /// 续轮请求：上一条 assistant 消息里的工具调用，配上末条消息里的 tool_result 大小。
    tool_uses: Vec<ToolUse>,
    /// 末条 user 消息里的 `tool_result`（`tool_use_id`、内容字符数）——前面**没有** assistant
    /// 的时候才记：message-threads 续轮的增量体就是这个样子，工具调用在上一条回复里，要由
    /// [`ThreadBase::fill`] 配回去。
    orphan_results: Vec<ToolResultInfo>,
    assistant_messages: usize,
    /// 最后一条 assistant 消息之后还有几条消息（没有 assistant 消息记 0）：
    /// `tengu_tether_live_outcome.deltaMessageCount`（`cap/2.1.277` 主线程续轮报 2、
    /// 子代理续轮报 1，与这个数逐条相等）。
    after_last_assistant: usize,
    /// 缓存没盖住的尾段：最后一条带断点的消息之后还有几条、按块估的 token 数（[`tail_tokens_est`]）、
    /// 是否全是 `system` 角色。`messages` 里一个断点都没有时全为零。`tengu_api_success` 的
    /// `uncoveredTail*` 那组按它报，见那里。
    tail_messages: usize,
    tail_tokens_est: u64,
    tail_all_system: bool,
    /// `system[1..]` 与整个 `tools` 数组各自的紧凑 JSON 长度（UTF-16 计）之和：续用线程时
    /// 省掉不发的那部分，`tengu_tether_live_outcome.omittedBytes`（`cap/2.1.277/00031`：
    /// 13184 + 72661 = 85845，与事件逐字节相等）。
    omitted_bytes: usize,
    /// system 最后一块（会话级的动态段）的 sha256 前缀，`tengu_api_success.snapshotHash`
    /// 的来源，见 [`snapshot_hash_of`]。
    dynamic_hash: String,
    /// 顶层 `thread.type`（`create` / `continue`），见 [`ThreadBase`]。
    thread_type: Option<String>,
    /// 消息里出现过 auto 模式的进入/退出提示（`permission_mode` 是从消息里判的）。
    auto_marker: bool,
    device_id: Option<String>,
    session_id: Option<String>,
    account_uuid: Option<String>,
}

/// 内容块数组里 text 的字符数（tool_result 的 content 既可能是字符串也可能是块数组）。
fn content_chars(c: &Value) -> usize {
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
fn last_two_non_system(messages: &[Value]) -> (Option<&Value>, Option<&Value>) {
    let mut it =
        messages.iter().rev().filter(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"));
    let last = it.next();
    let prev = it.next();
    (prev, last)
}

/// 末条 user 消息里的 `tool_result`，前一条不是 assistant 时（续轮增量体）才有意义，见
/// [`RequestShape::orphan_results`]。
fn orphan_results_of(messages: &[Value]) -> Vec<ToolResultInfo> {
    let (prev, Some(last)) = last_two_non_system(messages) else { return vec![] };
    if prev.is_some_and(|p| p.get("role").and_then(|r| r.as_str()) == Some("assistant")) {
        return vec![];
    }
    tool_results_of(last)
}

/// 去掉结果末尾客户端追加的 `\n<system-reminder>…</system-reminder>`。只去它自己那个换行：
/// 命令输出本身的末行换行官方照算（`00412`：`…deletions(-)\n` 报 119）。
fn strip_trailing_reminder(text: &str) -> &str {
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
struct ToolResultInfo {
    id: String,
    /// 内容字符数。
    len: usize,
    /// 见 [`ToolUse::persisted_from`]。
    persisted: Option<usize>,
    is_error: bool,
    /// 正文开头，见 [`ToolUse::result_head`]。
    head: String,
    /// Read 结果去掉行号前缀后的字节数，见 [`read_content_bytes`]。
    stripped_bytes: usize,
    /// 结果里图片块 base64 的总长（Read 读图片时 `sidecarFileBase64Bytes`）。
    image_b64: usize,
    /// 结果里的图片块数（`toolResultMediaBlocks`）。
    media_blocks: usize,
}

/// 一条消息里的全部 `tool_result`。
fn tool_results_of(msg: &Value) -> Vec<ToolResultInfo> {
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
fn tool_input_len(name: &str, input: &Value) -> usize {
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
    fn apply_call(&mut self, name: &str, input: &Value, verdict: Option<&String>) {
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

fn tool_uses_of(messages: &[Value]) -> Vec<ToolUse> {
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

fn sha256_hex(data: &[u8]) -> String {
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
fn error_kind(f: &CallFailure) -> String {
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
fn api_request_error_code(f: &CallFailure) -> &'static str {
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
fn tail_tokens_est(m: &Value) -> u64 {
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
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// 内容块里 base64 数据的字节数估算（4 个字符 3 字节）。
fn source_bytes(block: &Value) -> usize {
    block
        .get("source")
        .and_then(|s| s.get("data"))
        .and_then(|d| d.as_str())
        .map(|d| d.len() * 3 / 4)
        .unwrap_or(0)
}

/// 一条消息的 content：字符串或块数组。
fn walk_content(content: &Value, shape: &mut RequestShape) {
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
const INTERRUPTED_FOR_TOOL_USE: &str = "[Request interrupted by user for tool use]";
/// 用户在权限弹框上拒绝后，那个 `tool_result` 的开头。
const USER_REJECTED_TOOL_USE: &str = "The user doesn't want to proceed with this tool use.";

fn last_is_new_prompt(messages: &[Value]) -> (bool, usize) {
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
fn parse_user_id(v: &Value) -> (Option<String>, Option<String>, Option<String>) {
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

fn parse_shape(body: &[u8]) -> Option<RequestShape> {
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

/// 一条 shell 命令在官方遥测里的画像（`tengu_bash_tool_command_executed` 与 Bash 的
/// `tengu_tool_use_success` 那组字段）。规则照 `cap/auto-2.1.285-20260930` 的 16 条逐条对：
///
/// - 按引号外的 `&&` / `||` / `;` / `|` / 换行切成简单命令；打头的 `cd …` 不算一条（`cd X &&
///   find … | xargs wc -l` 是 2 条、argv0 `find`）；`$(…)` 与反引号里的不另算；
/// - argv0 取文件名，`python3` 记 `python`，`xargs` / `sudo` / `env` / `time` / `nohup` 往后取；
///   `bash_last_argv0` 是最后一条的；
/// - 类别看第一条：解释器 `lang_runtime`、`git` 一类 `vcs`、`grep` / `find` / `ls` 一类
///   `file_search`、`cat` / `head` / `tail` 一类 `file_read`、`echo` / `sleep` 一类 `shell_builtin`；
/// - `command_type` 是 `git` / `python3` 这类认得的命令原名，其余 `other`；`git` 另报子命令；
/// - 重定向到 `/dev/null`、`2>&1` 这类 fd 复制不算 `has_redirect`（`ls; head -5 README* 2>/dev/null`
///   为 false，`echo >> README.md` 为 true）。
#[derive(Debug, Default, PartialEq)]
struct BashProfile {
    command_type: String,
    class: &'static str,
    argv0: String,
    last_argv0: String,
    subcommand: Option<String>,
    has_pipe: bool,
    has_redirect: bool,
    has_chain: bool,
    has_subshell: bool,
    has_heredoc: bool,
    simple_commands: usize,
    /// 命令里出现的绝对路径与 `~` 路径（判「只读」时要看它们是不是在工作目录里）。
    paths: Vec<String>,
    /// 每条简单命令的 argv0（与 `argv0` 同一规则）。
    argv0s: Vec<String>,
}

fn bash_profile(cmd: &str) -> BashProfile {
    // 按引号外的操作符切段。
    let mut segs: Vec<String> = Vec::new();
    let (mut cur, mut quote, mut depth) = (String::new(), None::<char>, 0i32);
    let (mut has_pipe, mut has_chain, mut has_redirect) = (false, false, false);
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                cur.push(c);
            }
            None => match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' if cur.ends_with('$') => {
                    depth += 1;
                    cur.push(c);
                }
                ')' if depth > 0 => {
                    depth -= 1;
                    cur.push(c);
                }
                _ if depth > 0 => cur.push(c),
                '&' if next == Some('&') => {
                    has_chain = true;
                    segs.push(std::mem::take(&mut cur));
                    i += 1;
                }
                '|' if next == Some('|') => {
                    has_chain = true;
                    segs.push(std::mem::take(&mut cur));
                    i += 1;
                }
                '|' => {
                    has_pipe = true;
                    segs.push(std::mem::take(&mut cur));
                }
                ';' | '\n' => {
                    has_chain = true;
                    segs.push(std::mem::take(&mut cur));
                }
                '>' => {
                    // `2>&1` / `>&2`：fd 复制；`> /dev/null`：丢弃。都不算重定向。
                    let rest: String = chars[i + 1..].iter().collect();
                    let rest = rest.trim_start_matches('>');
                    let target = rest.trim_start();
                    if !rest.starts_with('&') && !target.starts_with("/dev/null") {
                        has_redirect = true;
                    }
                    cur.push(c);
                }
                _ => cur.push(c),
            },
        }
        i += 1;
    }
    segs.push(cur);
    let words = |seg: &str| -> Vec<String> {
        seg.split_whitespace()
            .map(|w| w.trim_matches(|c| c == '"' || c == '\'').to_string())
            .collect()
    };
    let argv0_of = |ws: &[String]| -> (String, Option<String>) {
        let mut it = ws.iter().skip_while(|w| w.contains('=') && !w.starts_with('-'));
        let mut first = it.next().cloned().unwrap_or_default();
        while matches!(first.as_str(), "xargs" | "sudo" | "env" | "time" | "nohup" | "exec") {
            first = it.find(|w| !w.starts_with('-')).cloned().unwrap_or_default();
        }
        let base = first.rsplit('/').next().unwrap_or("").to_string();
        let sub = it.find(|w| !w.starts_with('-')).cloned();
        (base, sub)
    };
    let simple: Vec<Vec<String>> = segs
        .iter()
        .map(|s| words(s))
        .filter(|ws| !ws.is_empty())
        .filter(|ws| ws[0] != "cd")
        .collect();
    let norm = |a: &str| -> String {
        if a.starts_with("python") { "python".to_string() } else { a.to_string() }
    };
    let (first_raw, sub) = simple.first().map(|ws| argv0_of(ws)).unwrap_or_default();
    let (last_raw, _) = simple.last().map(|ws| argv0_of(ws)).unwrap_or_default();
    let argv0 = norm(&first_raw);
    let class = match argv0.as_str() {
        "python" | "node" | "ruby" | "perl" | "php" | "java" | "deno" | "bun" => "lang_runtime",
        "git" | "hg" | "svn" | "gh" => "vcs",
        "grep" | "rg" | "find" | "fd" | "ls" | "tree" | "locate" | "du" => "file_search",
        "cat" | "head" | "tail" | "less" | "more" | "wc" | "xxd" | "stat" | "file" => "file_read",
        "echo" | "sleep" | "printf" | "true" | "false" | "export" | "test" | "[" | "pwd" => {
            "shell_builtin"
        }
        "npm" | "npx" | "yarn" | "pnpm" | "pip" | "pip3" | "cargo" | "go" | "make" | "uv" => {
            "build"
        }
        _ => "other",
    };
    let command_type = match first_raw.as_str() {
        "git" | "python3" | "python" | "node" | "npm" | "npx" | "yarn" | "pnpm" | "cargo"
        | "go" | "make" | "docker" | "pip" | "pip3" => first_raw.clone(),
        _ => "other".to_string(),
    };
    BashProfile {
        command_type,
        class,
        argv0,
        last_argv0: norm(&last_raw),
        subcommand: (class == "vcs").then_some(sub).flatten(),
        has_pipe,
        has_redirect,
        has_chain,
        has_subshell: cmd.contains("$(") || cmd.contains('`'),
        has_heredoc: cmd.contains("<<"),
        simple_commands: simple.len(),
        argv0s: simple.iter().map(|ws| norm(&argv0_of(ws).0)).collect(),
        paths: simple
            .iter()
            .flatten()
            .filter(|w| w.starts_with('/') || w.starts_with('~'))
            .cloned()
            .collect(),
    }
}

/// `tengu_bash_tool_command_executed` 的正文（键序照 `cap/auto-2.1.285-20260930`）。用户在输入框里
/// 用 `!` 跑的那种没有 `tool_use_id`、`user_typed_shell_dispatch` 与 `dangerously_disable_sandbox`
/// 为真（`00191`）。
fn bash_executed_meta(
    p: &BashProfile,
    stdout_len: usize,
    tool_use_id: Option<&str>,
    backgrounded: bool,
    user_typed: bool,
    permission_mode: &str,
) -> Value {
    let mut m = Map::new();
    m.insert("command_type".into(), json!(&p.command_type));
    m.insert("bash_command_class".into(), json!(p.class));
    m.insert("bash_argv0".into(), json!(&p.argv0));
    m.insert("bash_last_argv0".into(), json!(&p.last_argv0));
    if let Some(sub) = &p.subcommand {
        m.insert("bash_subcommand".into(), json!(sub));
    }
    for (k, v) in [
        ("has_pipe", p.has_pipe),
        ("has_redirect", p.has_redirect),
        ("has_chain", p.has_chain),
        ("has_subshell", p.has_subshell),
        ("has_heredoc", p.has_heredoc),
    ] {
        m.insert(k.into(), json!(v));
    }
    m.insert("simple_command_count".into(), json!(p.simple_commands));
    m.insert("stdout_length".into(), json!(stdout_len));
    m.insert("stderr_length".into(), json!(0));
    m.insert("exit_code".into(), json!(0));
    m.insert("interrupted".into(), json!(false));
    m.insert("executor_shell".into(), json!("zsh"));
    m.insert("executor_shell_overridden".into(), json!(false));
    m.insert("sandboxed".into(), json!(false));
    m.insert("sandbox_enabled".into(), json!(false));
    m.insert("dangerously_disable_sandbox".into(), json!(user_typed));
    m.insert("user_typed_shell_dispatch".into(), json!(user_typed));
    m.insert("filesystem_policy".into(), json!("strict"));
    m.insert("call_origin".into(), json!("local"));
    m.insert("had_sandbox_violation".into(), json!(false));
    m.insert("was_backgrounded".into(), json!(backgrounded));
    if let Some(id) = tool_use_id {
        m.insert("tool_use_id".into(), json!(id));
    }
    m.insert("destructive_category".into(), json!("none"));
    m.insert("destructive_target_scope".into(), json!("none"));
    m.insert("git_destructive_target".into(), json!("none"));
    m.insert("permission_mode".into(), json!(permission_mode));
    Value::Object(m)
}

/// [`tool_success_extras`] 的文件表里记「上一份计划的 Write 入参长度」用的键（不会是真路径）。
const PLAN_INPUT_KEY: &str = "\0plan";

/// `tengu_tool_use_success` 里各工具自己那组字段（`cap/auto-2.1.285-20260930` 的键序）：结果之后、
/// `toolInputSizeBytes` 之前是 sidecar 那几项，之后是路径 / 命令那几项。`success` 进来时最后一个
/// 键是 `toolInputSizeBytes`。
fn tool_success_extras(success: &mut Value, tu: &ToolUse, sizes: &mut HashMap<String, usize>) {
    let path = tu.input.get("file_path").and_then(|p| p.as_str()).unwrap_or("");
    let path_fields = |v: &mut Value| {
        v["fileExtension"] = json!(file_ext(path));
        v["filePathLen"] = json!(js_len(path));
    };
    let mut sidecar: Vec<(&str, Value)> = Vec::new();
    match tu.name.as_str() {
        "Read" if !path.is_empty() => {
            if tu.image_b64 > 0 {
                sidecar.push(("sidecarFileBase64Bytes", json!(tu.image_b64)));
            } else {
                sidecar.push(("sidecarFileContentBytes", json!(tu.stripped_bytes)));
                if tu.input.get("offset").is_none() && tu.input.get("limit").is_none() {
                    sizes.insert(path.to_string(), tu.stripped_bytes);
                }
            }
        }
        "Edit" if !path.is_empty() => {
            let old = tu.input.get("old_string").and_then(|s| s.as_str()).unwrap_or("");
            let new = tu.input.get("new_string").and_then(|s| s.as_str()).unwrap_or("");
            let original = sizes.get(path).copied().unwrap_or(old.len() * 3);
            sidecar.push(("sidecarOriginalFileBytes", json!(original)));
            sidecar.push(("sidecarStructuredPatchBytes", json!(old.len() + new.len() + 40)));
            sizes.insert(path.to_string(), (original + new.len()).saturating_sub(old.len()));
        }
        "Write" if !path.is_empty() => {
            let content = tu.input.get("content").and_then(|s| s.as_str()).unwrap_or("");
            let original = sizes.get(path).copied();
            if let Some(o) = original {
                sidecar.push(("sidecarOriginalFileBytes", json!(o)));
            }
            sidecar.push(("sidecarContentBytes", json!(js_len(content))));
            let patch = if original.is_some() { js_len(content).saturating_sub(11) } else { 0 };
            sidecar.push(("sidecarStructuredPatchBytes", json!(patch)));
            sizes.insert(path.to_string(), js_len(content));
            // 规划模式写的是计划文件：随后的 `ExitPlanMode` 客户端把这份计划塞进入参，
            // `toolInputSizeBytes` 与这条 Write 的相等（`00162` / `00164` 都是 1589）。
            if path.contains("/plans/") {
                sizes.insert(PLAN_INPUT_KEY.to_string(), tu.input_len);
            }
        }
        _ => {}
    }
    if !sidecar.is_empty() {
        insert_after(success, "toolResultWillPersist", sidecar);
    }
    match tu.name.as_str() {
        "Read" if !path.is_empty() => {
            path_fields(success);
            success["readHasLimit"] = json!(tu.input.get("limit").is_some());
            success["readHasOffset"] = json!(tu.input.get("offset").is_some());
        }
        "Edit" | "Write" if !path.is_empty() => path_fields(success),
        "Bash" => {
            let command = tu.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
            success["bashCommandLen"] = json!(tu.command_len);
            if !command.is_empty() {
                let p = bash_profile(command);
                success["bash_command_class"] = json!(p.class);
                success["bash_argv0"] = json!(&p.argv0);
                if let Some(sub) = &p.subcommand {
                    success["bash_subcommand"] = json!(sub);
                }
                success["has_pipe"] = json!(p.has_pipe);
            }
        }
        _ => {}
    }
}

/// 只读的 shell 命令：default 模式下不弹框、auto 模式下服务端判决直接放行
/// （`serverHeldShellAllowFrom: readOnly`）。查找、读文件、`git` 的只读子命令，且不写文件。
///
/// 碰到工作目录外面的路径就不算（`~/.codex/config.toml`：`/init` 那条读配置的命令 default 模式下
/// 照样弹了框，`cap/auto-2.1.285-20260930` 08:02:04.556）；工作目录里的绝对路径照算只读。
fn bash_read_only(p: &BashProfile, cwd: Option<&str>) -> bool {
    let inside = p.paths.iter().all(|w| {
        w == "/dev/null" || (!w.starts_with('~') && cwd.is_some_and(|c| w.starts_with(c)))
    });
    if !inside {
        return false;
    }
    let git_ro = p.class == "vcs"
        && p.subcommand
            .as_deref()
            .is_some_and(|s| matches!(s, "status" | "log" | "diff" | "show" | "branch" | "blame"));
    // 每一条都得是只读的：`ls …; which …` 那条（`07:58:12.088` 前）没弹框。
    (matches!(p.class, "file_search" | "file_read") || git_ro)
        && !p.has_redirect
        && p.argv0s.iter().all(|a| {
            matches!(
                a.as_str(),
                "ls" | "cat"
                    | "head"
                    | "tail"
                    | "wc"
                    | "grep"
                    | "rg"
                    | "find"
                    | "tree"
                    | "stat"
                    | "file"
                    | "du"
                    | "xxd"
                    | "echo"
                    | "which"
                    | "pwd"
                    | "sort"
                    | "uniq"
                    | "cut"
                    | "git"
            )
        })
}

/// Bash 失败结果开头的 `Exit code N`；认不出按 1。
fn exit_code_of(head: &str) -> i64 {
    head.trim_start()
        .strip_prefix("Exit code ")
        .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(1)
}

/// `tengu_auto_mode_decision` 的正文（键序照 `cap/auto-2.1.285-20260930`）。两种形态：
///
/// - 服务端判了（`not_flagged` / `flagged`）：长的那份，Bash 多破坏性判定三项；只读命令服务端
///   直接放行，报 `serverVerdictOverrodeFastPath: true` + `serverHeldShellAllowFrom: readOnly`；
///   复合命令的判定来源是 `subcommandResults`，SubagentHandback 是 `safetyCheck`，其余工具没有这一项；
///   同一条回复里第二个起的工具 `sameTurnSiblings` 递增、不再等分类器；
/// - 服务端没判（`skipped`，acceptEdits 快路径下的 Edit / Write）：短的那份。
///
/// `session*Tokens` 是会话到上一条回复为止的用量合计。
#[allow(clippy::too_many_arguments)]
fn auto_mode_decision_meta(
    tu: &ToolUse,
    verdict: &str,
    profile: Option<&BashProfile>,
    agent_msg_id: &str,
    sibling: usize,
    usage: [i64; 4],
    seed: u32,
    cwd: Option<&str>,
) -> Value {
    let wait = if sibling == 0 { 850 + i64::from(seed % 1400) } else { 0 };
    let decision = if verdict == "flagged" { "denied" } else { "allowed" };
    let mut m = Map::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_string(), v);
    };
    put("decision", json!(decision));
    put("toolName", json!(&tu.name));
    put("isMcp", json!(false));
    put("inProtectedNamespace", json!(false));
    put("chromeAutomode", json!(false));
    if verdict == "skipped" {
        put("mcpAlwaysAllowOverride", json!(false));
        put("mcpServerAskOverride", json!(false));
        put("agentMsgId", json!(agent_msg_id));
        put("confidence", json!("high"));
        put("fastPath", json!("acceptEdits"));
        put("chromePointerPress", json!(false));
        put("hookAllowVouch", json!(false));
        put("classifierSource", json!("server"));
        put("serverClassifierNoVerdict", json!("server_call_skipped"));
        put("classifierTotalWaitMs", json!(if sibling == 0 { wait } else { 0 }));
        return Value::Object(m);
    }
    for k in ["chromeNavigationForced", "chromePointerPress", "chromeClickRouted", "hookAllowVouch"]
    {
        put(k, json!(false));
    }
    put("mcpAlwaysAllowOverride", json!(false));
    put("mcpServerAskOverride", json!(false));
    let read_only = profile.is_some_and(|p| bash_read_only(p, cwd));
    if let Some(p) = profile {
        put("destructive_category", json!("none"));
        put("destructive_target_scope", json!("none"));
        put("git_destructive_target", json!("none"));
        put("stripAllBashFlag", json!(false));
        let compound = p.simple_commands > 1 && !read_only;
        put(
            "originalDecisionReasonType",
            json!(if compound { "subcommandResults" } else { "other" }),
        );
    } else {
        put("stripAllBashFlag", json!(false));
        if tu.name == "SubagentHandback" {
            put("originalDecisionReasonType", json!("safetyCheck"));
        }
    }
    put("editClassificationGated", json!(false));
    put("serverVerdictOverrodeFastPath", json!(read_only));
    if read_only {
        put("serverHeldShellAllowFrom", json!("readOnly"));
    }
    put("agentMsgId", json!(agent_msg_id));
    put("sameTurnSiblings", json!(sibling));
    put("classifierQueueDepth", json!(0));
    put("classifierQueueWaitMs", json!(0));
    put("classifierSource", json!("server"));
    put("classifierTotalWaitMs", json!(wait));
    put("classifierModel", json!("nonconforming"));
    put("consecutiveDenials", json!(0));
    put("totalDenials", json!(0));
    put("classifierDurationMs", json!(0));
    put("sessionInputTokens", json!(usage[0]));
    put("sessionOutputTokens", json!(usage[1]));
    put("sessionCacheReadInputTokens", json!(usage[2]));
    put("sessionCacheCreationInputTokens", json!(usage[3]));
    Value::Object(m)
}

/// 路径的扩展名（不带点）；没有的为空。
fn file_ext(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_string(),
        _ => String::new(),
    }
}

/// Read 结果里文件本身的字节数：去掉每行开头的行号前缀（`     1\t` / `1→`）再按 UTF-8 数，
/// 行与行之间一个 `\n`（`sidecarFileContentBytes`：`cap/auto-2.1.285-20260930` 167 字的结果对
/// 140 字节的 calc.py）。
fn read_content_bytes(result: &str) -> usize {
    let lines: Vec<&str> = result
        .lines()
        .take_while(|l| !l.trim_start().starts_with("<system-reminder>"))
        .map(|l| {
            let t = l.trim_start();
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 {
                let rest = &t[digits..];
                rest.strip_prefix('\t').or_else(|| rest.strip_prefix('→')).unwrap_or(rest)
            } else {
                l
            }
        })
        .collect();
    lines.iter().map(|l| l.len()).sum::<usize>() + lines.len().saturating_sub(1)
}

/// [`emit_template`] 的产物：带时间戳的事件与 Datadog 条目。
type TplOutput = (Vec<(DateTime<Utc>, Value)>, Vec<Value>);

/// 一条调用结束的时刻。
fn this_end_wall(call: &ApiCall) -> SystemTime {
    call.started_at + Duration::from_millis(call.total_ms)
}

/// 挂着的建议被用户的下一次输入顶掉：`tengu_prompt_suggestion{outcome: ignored}`，报在提交那一刻
/// （`cap/auto-2.1.285-20260930` 20 条）。`timeToIgnoreMs` 是建议出来到提交的间隔；
/// `timeToFirstKeystrokeMs` 再扣掉敲这段输入的时间（按 [`Telemetry::process`] 里 `user_secs` 的
/// 口径估，间隔不够就按间隔）；`similarity` 是**敲的字数 ÷ 建议的字数**（20 条里认得出原文的
/// 12 条逐位相等：79 / 28 = 2.82、8 / 11 = 0.73……）。
fn ignored_suggestion_meta(
    request_id: &str,
    shown_end: SystemTime,
    suggestion_chars: usize,
    submit: DateTime<Utc>,
    typed_chars: usize,
) -> Value {
    let shown: DateTime<Utc> = shown_end.into();
    let to_ignore = (submit - shown).num_milliseconds().max(0);
    let typing = ((800.0 + 100.0 * typed_chars as f64) as i64).min(to_ignore);
    json!({
        "source": "cli",
        "outcome": "ignored",
        "prompt_id": "user_intent",
        "generationRequestId": request_id,
        "timeToIgnoreMs": to_ignore,
        "timeToFirstKeystrokeMs": (to_ignore - typing).max(0),
        "wasFocusedWhenShown": true,
        "similarity": typed_chars as f64 / suggestion_chars.max(1) as f64
    })
}

/// 客户端的权限模式名 → 事件里报的那个（同名）。认不出的不报，退回推断。
fn permission_mode_name(mode: &str) -> Option<&'static str> {
    ["default", "auto", "plan", "acceptEdits", "bypassPermissions", "dontAsk"]
        .into_iter()
        .find(|m| *m == mode)
}

/// `estimatedInputTokens`，口径见 [`parse_shape`]。
fn estimate_tokens(model: &str, chars: usize) -> usize {
    if model.contains("haiku") { (chars as f64 / 4.0).round() as usize } else { chars.div_ceil(3) }
}

/// 消息里最后一条 auto 模式提示是进入还是退出；一条都没有为 `None`。从末尾往前找，
/// 通常几条之内就停。
///
/// **只认客户端自己注入的那两种位置**（`cap/2.1.277`、`cap/2.1.280` 全部 36 处）：
/// - `role: system` 的消息（环境说明、附件）——用户正文不会落在这种消息里；
/// - user 消息里以 `<system-reminder>` 开头、紧跟着就是提示本身的文本块。
///
/// 用户正文里引用这段话、工具输出（`tool_result`）里恰好有这段文字、assistant 回复里复述
/// 它，都不算：否则一条 `cat` 了 Claude Code 文档的 Bash 结果就能把 permissionMode 翻掉，
/// 连带子代理沿用的模式与 auto 模式那几条事件一起错。
fn auto_mode_of_messages(msgs: &[Value]) -> Option<bool> {
    const ENTER: &str = "While auto mode is active:";
    const EXIT: &str = "## Exited Auto Mode";
    const REMINDER: &str = "<system-reminder>";
    // 一段文本里最后出现的是进入还是退出。
    let last_marker = |t: &str| match (t.rfind(ENTER), t.rfind(EXIT)) {
        (None, None) => None,
        (Some(a), Some(e)) => Some(a > e),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
    };
    // user 消息里的提示块：标签之后（跳过空白）直接就是提示。
    let reminder_marker = |t: &str| {
        let rest = t.trim_start().strip_prefix(REMINDER)?.trim_start();
        if rest.starts_with(ENTER) {
            Some(true)
        } else if rest.starts_with(EXIT) {
            Some(false)
        } else {
            None
        }
    };
    for m in msgs.iter().rev() {
        let role = m.get("role").and_then(|r| r.as_str());
        let texts: Vec<&str> = match m.get("content") {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect(),
            _ => Vec::new(),
        };
        for t in texts.iter().rev() {
            let hit = match role {
                Some("system") => last_marker(t),
                Some("user") => reminder_marker(t),
                _ => None,
            };
            if hit.is_some() {
                return hit;
            }
        }
    }
    None
}

/// 一个遥测会话（同一凭证 + 同一 `session_id`）跨请求要记住的东西。
struct Session {
    /// 会话「进程」的起点：首条请求前几秒（客户端启动到第一次提交之间的那段）。
    started_wall: SystemTime,
    last_seen: Instant,
    /// 用户的第几次输入（事件里的 `prompt_index`）：同伴会话发来的（peer）不占号。
    prompt_index: u32,
    /// 会话里一共有过几次新输入，**含** peer：判「会话首条」、选首轮模板、推客户端内部的
    /// 消息条数都用它——peer 那一轮也是客户端里实打实的一次提交与一条消息，只是不编号。
    prompts_seen: u32,
    prompt_id: String,
    chain_id: String,
    /// 最近一条**主线程**请求的上游 request-id：`previousRequestId` 只串主线程那条链
    /// （标题生成那类侧查询不算，猜下一句那条也接在主线程后面），退出时的
    /// `cache_eviction_hint.last_request_id` 同样取它。
    last_main_request_id: Option<String>,
    /// 最近一条主线程回复的 `message.id`（工具权限事件的 `messageID`）。
    last_main_message_id: Option<String>,
    /// 最近一条主线程请求的 queryDepth（猜下一句的 depth = 它 + 2）。
    last_main_depth: u32,
    /// 本轮下一条主线程请求的 queryDepth：新输入归零，每次 `tool_use` 续轮 +1。
    turn_depth: u32,
    /// 本轮开始（用户提交）的时刻，`tengu_turn_end.duration_ms` 与首字上屏时长的起点。
    turn_started: Option<SystemTime>,
    /// **上一轮**用户提交的时刻：`active_time.total{type:user}` 的窗口下界，见 `user_secs`。
    prev_prompt_submit: Option<SystemTime>,
    /// 本轮已经出现过正文（`tengu_turn_first_text` 只发一次）。
    turn_text_seen: bool,
    /// 本轮已执行的工具调用数。
    turn_tool_calls: u32,
    /// 本轮主线程发过的 API 请求数与耗时之和（`-p` 收尾那条 `tengu_sdk_result` 的 `num_turns` 与
    /// `duration_api_ms`：`cap/auto-2.1.285-20260930/00790` → `00795` 一次输入两次请求，官方报
    /// 2 与 5734 + 3909 + 3 = 9646）。
    turn_api_calls: u32,
    turn_api_ms: i64,
    /// 会话里已经发过 `shell_snapshot_create`（首次 Bash 才有）。
    shell_snapshot_done: bool,
    /// 会话里的首条主线程请求已经报过它那几条一次性事件（2.1.285：首字节时的
    /// `api_per_turn_effort` / `mcp_late_tool_additions` / `api_kept_reminder_clear_at`，收尾时的
    /// `tengu_wire_shape_recorded`）。
    first_main_done: bool,
    /// 主线程那份会话级 beta（[`session_betas`] 过滤出站头）。子代理那条支线上除 API 调用本身
    /// 外的事件报的都是它，见 `betas_session` 的取法。
    main_betas: String,
    /// 会话里已经发出去的主线程请求条数（上下文回放那对事件只跟前两条）。
    main_requests: u32,
    /// 首次输入那段模板（`prompt` + `first_prompt`）已经发过：客户端注入的一轮（peer /
    /// task-notification）不套模板，第一次真正的用户输入才是「首次」。
    first_prompt_tpl_done: bool,
    /// 模板 `background` 段已经补发到第几条。
    background_done: usize,
    /// 最近几条请求的完成时刻（不分类别）。`timeSinceLastApiCallMs` 取「在这条之前完成的最近
    /// 一条」：并发时 `last_call_end` 可能是一条比这条晚结束的，差出负数就报不出来。
    recent_ends: VecDeque<SystemTime>,
    /// 任一类请求最近一次结束的时刻（`timeSinceLastApiCallMs`）。
    last_call_end: Option<SystemTime>,
    /// 上一条**主线程**请求的 token 总量（input + cache_read + cache_creation + output）：
    /// `messageTokens` 报的是「对话此刻的 token 数」，抓包里三条续轮逐一对得上。
    prev_total_input: i64,
    /// 这一轮起头的提示词型斜杠命令，见 [`RequestShape::command_skill`]。
    turn_skill: Option<String>,
    /// 会话第一条请求（被处理的那条）的发出时刻。
    first_call_at: SystemTime,
    /// `/clear` 之前那个会话的 id，见 [`Identity::parent_session_id`]。
    parent_session_id: Option<String>,
    /// `-p` 打印模式的会话。
    sdk: bool,
    /// 在同一进程里被 `/clear` 换掉了：它不会有退出那一串（进程还在），[`Telemetry::gc`] 静默收掉。
    cleared: bool,
    /// 每条线程（主线程 / 各子代理）上一条回复里的工具调用（[`ToolCall`]）：下一条请求带回它们的
    /// 结果时，工具事件按 id 取入参与判决。
    reply_calls: HashMap<String, Vec<ToolCall>>,
    /// 这个会话里读过、写过的文件大小（路径 → 字节）：Edit / Write 的 `sidecarOriginalFileBytes`
    /// 要的是改之前那份有多大，代理只能从先前的 Read / Write / Edit 推。
    file_sizes: HashMap<String, usize>,
    /// 最近一次在请求里看到的工作目录，见 [`RequestShape::cwd`]。thread 续轮的增量体里没有它。
    cwd: Option<String>,
    /// 会话里到目前为止的用量合计：输入、输出、缓存读、缓存写（`tengu_auto_mode_decision` 的
    /// `session*Tokens`）。
    usage_totals: [i64; 4],
    /// 见 [`RequestShape::git_repo`] / [`RequestShape::git_dirty`]：会话里最近一次看到的。
    git_repo: Option<bool>,
    git_dirty: Option<bool>,
    /// 上一条「猜下一句」出了建议（有正文）：`(request-id, 结束时刻, 建议的字数)`。用户接着敲了
    /// 别的，下一次输入时报 `tengu_prompt_suggestion{outcome: ignored}`（`cap/auto-2.1.285-20260930`
    /// 20 条）。
    shown_suggestion: Option<(String, SystemTime, usize)>,
    /// 上一轮主线程被按 Esc 打断时那条回复的 `message.id`：下一次输入的 `tengu_input_prompt`
    /// 带 `interrupted_message_id`（`00264`）。
    interrupted_message_id: Option<String>,
    /// 上一条主线程请求结束的时刻（夹带的后台任务通知就是那时送到的）。
    last_main_end: Option<SystemTime>,
    /// 刚做完一次 `/compact`，下一条主线程请求是压缩之后的第一条。
    post_compact: bool,
    /// `default_model`：用户设置里的默认模型，取会话启动时那台设备的
    /// （[`State::device_default_model`]）。设备上还没记过的，取会话第一条**主线程**请求的展示
    /// 模型名——首条是侧查询（标题生成用的 haiku）时先记它占位，等主线程那条来了再换
    /// （`main_model_seen`）。`cap/auto-2.1.285-20260930`：`--model fable` 的会话报 opus，`/model
    /// haiku` 之后拉起的 `-p` 会话全报 haiku。
    default_model: String,
    main_model_seen: bool,
    last_message_id: Option<String>,
    last_model: Option<String>,
    /// 上次报过的工具长度表 hash，主线程一组、侧查询一组各记各的：官方整个会话只有两条
    /// `tengu_tool_schema_sizes`（主线程 16 个工具一条、标题生成空表一条），第二轮主线程
    /// 不重发——两类查询的工具集互不覆盖。
    tools_hash_main: Option<String>,
    tools_hash_side: Option<String>,
    /// `claude_code.session.count` 已经报过。
    counted: bool,
    /// 第一轮结束后的版本检查那串已经发过。
    first_turn_done: bool,
    /// 这个会话的设备与账号身份、客户端版本、会话级 beta 串——保活要把空闲事件挂到真实会话
    /// 上时从这里取（见 [`Telemetry::latest_session`]）。版本与 beta 随每条请求刷新。
    device_id: String,
    account_uuid: String,
    version: String,
    betas: String,
    subscription_type: String,
    /// 扣住等新一轮 prompt id 的侧查询：`(调用, 扣住时的上一条结束时刻, 扣住的时刻)`。
    /// 见 [`config::TELEMETRY_SIDE_QUERY_HOLD_SECS`]。
    deferred: Vec<(ApiCall, Option<SystemTime>, Instant)>,
    /// 会话的系统提示词快照（`tengu_api_success.snapshotHash`）：首条带边界的请求记下
    /// （那条报 `systemPromptSource: live_recorded`），之后整个会话都报同一个值、
    /// `from_snapshot`。
    snapshot_hash: Option<String>,
    /// 本会话已经报过 `tengu_sleepy_snowflake_applied` 的模型：官方每个模型只在头一次
    /// 被用于新输入时报一次（`cap/2.1.280` opus/fable/sonnet/haiku 各一条，再切回来不报）。
    sleepy_models: Vec<String>,
    /// 主线程的 tether 线程状态，见 [`TetherThread`]；子代理的各记在 [`AgentState`] 里。
    tether_main: Option<TetherThread>,
    /// 每条线程上一条完整请求的形态，键是 `main` 或 `agent:<支线号>`，见 [`ThreadBase`]。
    thread_bases: HashMap<String, ThreadBase>,
    /// 会话里的子代理，按支线号（`x-claude-code-agent-id`）分开记。
    agents: HashMap<String, AgentState>,
    /// 最近一条回复里调了 `Agent` 工具的主线程请求：随后拉起的子代理首条报它为
    /// `invokingRequestId`。不能拿「最近一条主线程请求」代替——子代理是异步跑的，它的首条
    /// 回来之前主线程往往已经又完成了一条（`cap/2.1.280`：主线程 27.020 完成、子代理首条
    /// 28.759 才完成，而 invokingRequestId 指的是 22.953 那条）。
    last_spawn_request_id: Option<String>,
    /// 主线程最近一条的权限模式。子代理的请求体里没有 auto 模式的提示（`cap/2.1.280`
    /// Explore 七条一条都没有），官方报的却是 `auto`——子代理沿用主线程的模式。
    main_permission: &'static str,
    /// 本轮的发起方（事件写法，如 `task-notification`）：新输入时从 billing header 取，
    /// 同一轮的续轮请求没带就沿用。
    turn_origin: String,
}

/// 处理一条子代理请求时从 [`AgentState`] 抄出来的那几项（会话状态随后还要改，不能一直借着）。
struct AgentView {
    chain_id: String,
    steps: u32,
    last_request_id: Option<String>,
    last_message_id: Option<String>,
    prev_total: i64,
    tools_hash: Option<String>,
    /// 只有支线首条才有。
    invoking_request_id: Option<String>,
}

/// 一个子代理（会话里的一条支线）跨请求要记住的东西。字段取值见 `cap/2.1.280` Explore
/// 子代理 a51764… 的 6 条请求与它的 1 条摘要请求。
#[derive(Default)]
struct AgentState {
    /// 支线自己的 `queryChainId`：六条请求同一个，摘要请求另起。
    chain_id: String,
    /// 已经完成的请求数：`queryDepth` = 2 + 它（2、3、4…），收尾的 `assistant_message_count`。
    steps: u32,
    /// 首条请求发出的时刻：收尾 `turn_end.duration_ms` / `agent_tool_completed.duration_ms` 的起点。
    started: Option<SystemTime>,
    /// 首条的用户提示字数（`agent_tool_completed.prompt_char_count`，抓包 481）。
    prompt_chars: usize,
    /// 各续轮带回来的工具结果数之和（`total_tool_uses`）。
    tool_uses: u32,
    /// 上一条的 request-id / message.id / 总 token（`previousRequestId`、工具事件的
    /// `messageID`、`messageTokens`）。
    last_request_id: Option<String>,
    last_message_id: Option<String>,
    prev_total: i64,
    /// 支线自己的提示词快照（与主线程的不同，`cap/2.1.280` 为 `7d8050a24c2c`）。
    snapshot_hash: Option<String>,
    tether: Option<TetherThread>,
    /// 首条的 `invokingRequestId`（拉起它的那条主线程请求）。
    invoking_request_id: Option<String>,
    /// 工具长度表报过的那份 hash（`tengu_tool_schema_sizes` 每条支线首次报一次）。
    tools_hash: Option<String>,
    /// 子代理类型（请求头 `x-claude-code-agent-type`）。它的摘要请求头上只有 `agent-id`，
    /// 按类型定的取值（claude-code-guide 的 `dontAsk`）要从这里取。
    agent_type: Option<String>,
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
struct ThreadBase {
    tools_count: usize,
    tools_chars: usize,
    tools_hash: String,
    tool_lens: String,
    deferred_tools: usize,
    mcp_tools: usize,
    has_tool_search: bool,
    system_blocks: usize,
    system_chars: usize,
    static_len: usize,
    dynamic_len: usize,
    dynamic_hash: String,
    omitted_bytes: usize,
    permission_mode: &'static str,
    messages: usize,
    assistant_messages: usize,
    input_text_chars: usize,
    /// 这条请求的**回复**按 `inputTextCharLength` 口径的字数：下一条增量请求里省掉的正是它，
    /// 客户端报的输入长度却含它。
    reply_chars: usize,
    /// 这条回复里的工具调用（工具名 → 入参字符数之和，按首次出现排序，即
    /// [`ApiCall::tool_use_lens`]）。下一条增量请求只带 `tool_result`，工具名要从这里配。
    reply_tools: Vec<(String, usize)>,
}

impl ThreadBase {
    fn of(s: &RequestShape, reply_chars: usize, reply_tools: Vec<(String, usize)>) -> Self {
        ThreadBase {
            tools_count: s.tools_count,
            tools_chars: s.tools_chars,
            tools_hash: s.tools_hash.clone(),
            tool_lens: s.tool_lens.clone(),
            deferred_tools: s.deferred_tools,
            mcp_tools: s.mcp_tools,
            has_tool_search: s.has_tool_search,
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
    fn fill(&self, s: &mut RequestShape, auto_in_delta: bool) {
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
struct TetherThread {
    model: String,
    betas: String,
    effort: Option<String>,
    tools_hash: String,
    thinking_type: String,
    messages: usize,
    turns: u32,
}

/// 一条请求的 tether 判定结果，事件链里 `tether_decision` / `echo_audit` / `live_outcome`
/// 三条共用。
struct Tether {
    decision: &'static str,
    reason: &'static str,
    changed_model: bool,
    changed_tools: bool,
    changed_betas: bool,
    changed_latched: bool,
    changed_thinking: bool,
    changed_effort: bool,
    turns: u32,
    prev_messages: usize,
    delta: usize,
    /// 线程里此前已有请求：只有这种才有 `tengu_tether_echo_audit`（主线程另起线程也算，
    /// 那是同一会话的回声；子代理只在续用时有）。
    echo: bool,
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
struct CallMetric {
    session_id: String,
    device_id: String,
    account_uuid: String,
    model: String,
    category: &'static str,
    /// 没有 `output_config.effort` 的侧查询不带 `effort` 属性（抓包里标题生成那组没有）。
    effort: Option<String>,
    /// 子代理的类型：子代理那组数据点多一个 `agent.name`（`cap/2.1.285/00157`：
    /// `query_source: subagent, agent.name: claude-code-guide`），落在 `effort` 之后。
    agent_name: Option<String>,
    cost: f64,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
    /// `active_time.total{type:cli}` 的贡献：只有以 `end_turn` 收尾的主线程请求算它的时长
    /// （`cap/2.1.260-2`：10.053s ≈ 3.779 + 6.338，中间那条 tool_use 收尾的 3.585 与两条侧查询
    /// 都不算；单请求会话 2.827 / 2.347 与 API 时长几乎相等）。
    cli_secs: f64,
    /// `active_time.total{type:user}` 的贡献：新输入前用户敲字/思考的那段（上一轮结束到这次
    /// 提交，封顶 5s；首次输入取 0.9s——两份单轮会话是 0.878 / 1.118）。
    user_secs: f64,
    /// 这条是该会话的第一条：带一条 `session.count`。
    new_session: bool,
    /// 这个会话 id 此前已按退出收尾过，这次是 resume（`start_type: resume`）。
    resumed: bool,
    /// 新进程 `--continue` 接上的（`start_type: continue`），见 [`State::process_starts`]。
    continued: bool,
    /// 这条请求真的拿到了用量。失败的请求照样占 `session.count` 与 `active_time`（会话确实
    /// 起了、CLI 确实忙过），但**不进** `cost.usage` 与 `token.usage`——官方那两个计数器
    /// 是在成功回包时才加的，替它记一串 0 只会凭空多出一堆零值数据点。
    usage: bool,
}

/// 一张凭证攒着还没发出去的东西。
#[derive(Default)]
struct Pending {
    version: String,
    subscription_type: String,
    events: Vec<(DateTime<Utc>, Value)>,
    events_since: Option<Instant>,
    dd: Vec<Value>,
    dd_since: Option<Instant>,
    metrics: Vec<CallMetric>,
    metrics_since: Option<Instant>,
    /// 这个会话最近一条请求的身份与上下文：指标导出时要就地造一条
    /// `tengu_feature_ok{internal_metrics_export}`，用的就是这份。
    identity: Option<Identity>,
    model: String,
    betas: String,
    prompt_id: String,
    started_wall: Option<SystemTime>,
    /// 退出收尾时指定的导出事件时间戳（排在 `lsp_shutdown` 之后、`cache_eviction_hint`
    /// 之前）；平时为 `None`，导出时取当下。
    export_at: Option<DateTime<Utc>>,
}

/// `process` 前半段推出来、后半段拼事件与入队要用的那些量，与原先的局部变量一一对应。
struct TurnFacts {
    device_id: String,
    session_id: String,
    account_uuid: String,
    version: String,
    now: Instant,
    continued_from: Option<(String, SystemTime)>,
    cleared_from: Option<String>,
    cleared_prev: Option<(Option<String>, Option<SystemTime>, SystemTime, usize)>,
    identity: Identity,
    base_identity: Identity,
    kind: Kind,
    has_1m: bool,
    display_model: String,
    resp_model: String,
    key: (i64, String),
    is_new_session: bool,
    resumed: bool,
    continued: bool,
    device_key: (i64, String),
    git_outcome: &'static str,
    turn_over: bool,
    failed: bool,
    aborted: bool,
    emit_first_turn: bool,
    is_main: bool,
    new_prompt: bool,
    turn_skill: Option<String>,
    post_compaction: bool,
    turn_origin: String,
    prev_turn: (String, u32, Option<SystemTime>),
    rejected_calls: Vec<(ToolCall, ToolResultInfo)>,
    user_turn: bool,
    notification_base: u32,
    notifications: Vec<usize>,
    prev_main_end: Option<SystemTime>,
    prompt_id: String,
    agent: Option<AgentView>,
    previous_request_id: Option<String>,
    prev_main_message_id: Option<String>,
    prev_main_request_id: Option<String>,
    prev_main_depth: u32,
    prev_end: Option<SystemTime>,
    time_since_last: Option<u64>,
    message_tokens: i64,
    default_model: String,
    device_default_update: Option<String>,
    model_changed: bool,
    previous_message_id: Option<String>,
    tools_changed: bool,
    counted: bool,
    started_wall: SystemTime,
    main_chain: String,
    chain_id: String,
    query_depth: u32,
    turn_started: DateTime<Utc>,
    first_text_in_turn: bool,
    first_text_interrupted: bool,
    tool_calls_before: u32,
    user_secs: f64,
    shell_snapshot_first: bool,
    prompt_index: u32,
    prompt_seq: u32,
    v270: bool,
    v277: bool,
    v280: bool,
    v285: bool,
    injected: bool,
    first_prompt_tpl: bool,
    background: std::ops::Range<usize>,
    first_main: bool,
    snapshot: Option<(&'static str, String)>,
    sleepy: bool,
    tether: Option<Tether>,
    usage_before: [i64; 4],
    ignored_suggestion: Option<(String, SystemTime, usize)>,
    interrupted_message_id: Option<String>,
    handback: Option<ToolCall>,
    agent_done: Option<AgentState>,
    ctx_model: String,
    dd_model: String,
    sess_turn_tools: u32,
    sess_turn_api_calls: u32,
    sess_turn_api_ms: i64,
    session_cwd: Option<String>,
    betas_full: String,
    betas_own: String,
    thread_step: Option<u32>,
    betas_session: String,
}

/// [`Telemetry::build_events`] 的产出。
struct BuiltEvents {
    events: Vec<(DateTime<Utc>, Value)>,
    dd: Vec<Value>,
    /// `--continue` 接上旧会话时，挂在启动探测那个临时会话上的那一串。
    probe_side: Option<(Identity, TplOutput)>,
}

impl Pending {
    /// 追加一串事件与 Datadog 条目，两路各记下最早攒进来的时刻。
    fn push_batch(&mut self, (events, dd): TplOutput, now: Instant) {
        self.events_since.get_or_insert(now);
        self.events.extend(events);
        self.dd_since.get_or_insert(now);
        self.dd.extend(dd);
    }

    /// 会话的头一条常是标题生成那种不带环境段的侧查询，那时还不知道工作目录是不是 git 仓库；
    /// 等主线程那条说了，把还没发出去的那些补上 `vcs`（官方同一会话每条都带）。
    fn backfill_vcs(&mut self, vcs: &'static str) {
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
struct State {
    sessions: HashMap<(i64, String), Session>,
    /// 待发批次按 `(凭证, session_id)` 分开攒：真实客户端一个进程一个会话，各自往上报，
    /// **一个批次里只会有一个 `session_id` / `device_id`**。同一张凭证被几台设备同时用时，
    /// 合在一个 POST 里就是官方不会产生的混合批次。
    pending: HashMap<(i64, String), Pending>,
    org_uuid: HashMap<i64, String>,
    /// 已按「客户端退出」收尾的会话及收尾时刻（见 [`Telemetry::gc`]）。同一个 id 再来就是
    /// `claude --resume`：新进程、从头计数，但 `session.count` 报 `start_type: resume`。
    /// 保留 [`config::TELEMETRY_ENDED_SESSION_MEMORY_SECS`]。
    ended: HashMap<(i64, String), Instant>,
    /// 每台设备（凭证 + device_id）用户设置里的默认模型，见 [`Session::default_model`]。
    /// `/model` 选完模型就写进设置（那条 `model_validation` 探测就是它），**之后**拉起的进程
    /// 才按新的报；进行中的会话仍报它启动时那个。没见过 `/model` 的设备以它第一条主线程请求的
    /// 模型为准。
    device_default_model: HashMap<(i64, String), (String, Instant)>,
    /// 每台设备最近一次进程启动：启动时那条额度探测（`quota`）的会话 id 与时刻。官方客户端每拉起
    /// 一次进程都先发它，`/clear` 不发——据此分出三种会话形态（`cap/auto-2.1.285-20260930`）：
    /// 探测的会话 id 就是新会话的 → 新进程；新会话前没有新的探测、同一台设备上还有交互会话在跑 →
    /// 同一进程里 `/clear`；探测之后冒出来的是之前见过的会话 id → 新进程 `--continue` 接上了它
    /// （探测带的是进程启动时那个临时 id）。
    process_starts: HashMap<(i64, String), (String, SystemTime)>,
}

/// 一个新会话要做的启动握手：真实客户端每次拉起进程都会用当前账号打这一串端点
/// （`cap/2.1.260-1` 17:14:56–17:15:05），luban 替它补上，身份取该会话的。
///
/// 由转发路径在该会话**首条请求发出之前**构造并 spawn，见
/// [`crate::proxy::spawn_session_handshake`]；凭证本身作为单独的参数传给
/// [`crate::oauth::HandshakeRunner`]，不放进这个结构。
pub struct Handshake {
    pub snapshot: SessionSnapshot,
    /// bootstrap 的 `model=` 参数：规范名。
    pub model: String,
}

/// 逐请求遥测的汇聚点：转发路径往里 [`Telemetry::record`]，[`run_flusher`] 定时取走发出。
#[derive(Clone, Default)]
pub struct Telemetry(Arc<Shared>);

#[derive(Default)]
struct Shared {
    state: parking_lot::Mutex<State>,
    ingest: IngestQueue,
}

/// 待处理的调用队列：**入队是同步的，出队只有一个消费者**，故处理顺序恒等于
/// [`crate::proxy::ReqLog`] 的析构顺序，也就是响应完成的先后。
///
/// 为什么不能一条一个 `spawn_blocking`：那是往一个最多 512 线程的池子里扔任务，前后脚
/// 提交的两条谁先跑没有任何保证。而 [`Telemetry::process`] 里一多半状态是**按顺序**累积的
/// （`last_main_request_id` 那条链、`turn_depth`、`prev_total_input`、扣住侧查询要看的
/// 「这个会话在不在」……），顺序一乱，报出去的就是一份自相矛盾的会话历史。
///
/// 同一会话上真会并发的是「主线程 + 标题/安全分类/子代理」这几对，两条响应在同一毫秒内
/// 结束并不稀奇；最难看的一种是标题那条抢在会话首条主请求前面被处理——扣留分支要求会话
/// **已经存在**，抢先了就会以一条 haiku 标题请求为起点铺开整串启动事件。
#[derive(Default)]
struct IngestQueue {
    calls: parking_lot::Mutex<VecDeque<ApiCall>>,
    /// 已经有一个消费者在跑。只用来保证「同时最多一个」，队列本身的顺序由 `calls` 保证。
    draining: AtomicBool,
}

/// 一次要发出去的东西（一张凭证下的一个会话）。
pub struct Flush {
    pub cred_id: i64,
    /// 这一批属于哪个会话（日志里只展示前 8 位）。
    pub session_id: String,
    pub version: String,
    pub events: Vec<Value>,
    pub dd: Vec<Value>,
    pub metrics: Option<Value>,
}

impl Telemetry {
    /// 记一条已完成的 API 调用。解析请求体要几毫秒（100KB+ 的 JSON），且调用方在 `Drop`
    /// 里——扔到运行时上做，拿不到运行时（测试）就就地做。
    ///
    /// **入队这一步是同步的**：队列顺序 = `Drop` 顺序 = 响应完成顺序，随后由唯一的消费者
    /// 按序处理，见 [`IngestQueue`]。
    pub fn record(&self, call: ApiCall) {
        {
            let mut q = self.0.ingest.calls.lock();
            // 消费慢过生产时封顶：每条 `ApiCall` 拎着一份出站体（100KB+ 是常态），
            // 无上限的队列在上游长时间挂起时能把内存吃穿。丢**新**的而不是旧的——
            // 旧的丢掉会把已经排好的那条链从中间截断。
            if q.len() >= config::TELEMETRY_INGEST_QUEUE_MAX {
                tracing::warn!(
                    queued = q.len(),
                    "the telemetry ingest queue is full; dropping this call's events"
                );
                return;
            }
            q.push_back(call);
        }
        // 已经有消费者在跑就交给它——顺序正是靠「同时只有一个」保住的。
        if self.0.ingest.draining.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        let work = move || me.drain();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn_blocking(work);
            }
            Err(_) => work(),
        }
    }

    /// 唯一的消费者：把队列按 FIFO 排空。
    ///
    /// 队列锁**不跨** `process`（那是几毫秒的 JSON 解析加事件构造）：先 `pop_front` 拿到
    /// 手里再去锁状态，两把锁不嵌套，入队方也就从不被处理阻塞。
    fn drain(&self) {
        loop {
            let next = self.0.ingest.calls.lock().pop_front();
            let Some(call) = next else {
                // 空了：先放掉标志再复查一次。这中间入队的那条看到的是「有人在跑」，
                // 不会自己起一个消费者，复查就是接住它的那一手。
                self.0.ingest.draining.store(false, Ordering::Release);
                if self.0.ingest.calls.lock().is_empty() {
                    return;
                }
                // 又有了：抢回消费者身份接着跑；抢不到说明刚入队那条已经起了一个，让给它。
                if self.0.ingest.draining.swap(true, Ordering::AcqRel) {
                    return;
                }
                continue;
            };
            let mut st = self.0.state.lock();
            Self::process(&mut st, call, true, None);
        }
    }

    /// 某凭证最近一次响应头里的 `anthropic-organization-id`（保活事件的 `auth` 块用）。
    pub fn org_uuid(&self, cred_id: i64) -> Option<String> {
        self.0.state.lock().org_uuid.get(&cred_id).cloned()
    }

    /// 用凭证上存的组织 id（profile 的 `organization.uuid`）**垫底**：还没从响应头学到时先用它，
    /// 学到了以响应头为准（那是同一个值，只是更新鲜）。转发路径在建遥测材料时调一次。
    ///
    /// 没有这一步，一张刚登录、或久没转发过请求的号发出去的头几条事件 `auth` 块里就没有
    /// `organization_uuid`——官方 345/345 条都带。
    pub fn seed_org_uuid(&self, cred_id: i64, org_uuid: Option<&str>) {
        let Some(org) = org_uuid.map(str::trim).filter(|o| !o.is_empty()) else { return };
        self.0.state.lock().org_uuid.entry(cred_id).or_insert_with(|| org.to_string());
    }

    /// 某凭证最近活跃的真实会话（`max_idle` 内有过请求的那些里最新的一个）；没有则 `None`。
    /// 保活拿它把空闲事件挂到真实会话上，见 [`SessionSnapshot`]。
    pub fn latest_session(&self, cred_id: i64, max_idle: Duration) -> Option<SessionSnapshot> {
        let st = self.0.state.lock();
        let now = Instant::now();
        st.sessions
            .iter()
            .filter(|((c, _), s)| *c == cred_id && now.duration_since(s.last_seen) < max_idle)
            .max_by_key(|(_, s)| s.last_seen)
            .map(|((_, sid), s)| SessionSnapshot {
                session_id: sid.clone(),
                device_id: s.device_id.clone(),
                account_uuid: s.account_uuid.clone(),
                version: s.version.clone(),
                model: s.last_model.clone().unwrap_or_else(|| s.default_model.clone()),
                betas: s.betas.clone(),
                prompt_id: s.prompt_id.clone(),
                started_wall: s.started_wall,
            })
    }

    /// 就地处理一条调用（测试用）。转发路径走 [`Self::record`]——那条要经队列才能保住
    /// 顺序，而测试本来就是单线程按序调用，直接进 `process` 少一层异步。
    #[cfg(test)]
    fn ingest(&self, call: ApiCall) {
        let mut st = self.0.state.lock();
        Self::process(&mut st, call, true, None);
        tests::dump_pending(&st);
    }

    /// 把一条调用变成事件入队。`allow_defer` 为真时，侧查询会先扣住等同会话的下一条主线程
    /// 请求（拿新一轮的 prompt id）；`prev_end_override` 是扣住时记下的「上一条结束时刻」，
    /// 补发时算 `timeSinceLastApiCallMs` 用，否则会被后来的主线程请求顶掉。
    fn process(
        st: &mut State,
        call: ApiCall,
        allow_defer: bool,
        prev_end_override: Option<SystemTime>,
    ) {
        let Some(mut shape) = parse_shape(&call.body) else { return };
        if shape.quota_probe {
            // 官方不为它报任何事件，但它标着一次进程启动，见 [`State::process_starts`]。
            if let (Some(dev), Some(sid)) = (
                shape.device_id.clone(),
                shape.session_id.clone().or_else(|| call.session_header.clone()),
            ) {
                st.process_starts.insert((call.cred_id, dev), (sid, call.started_at));
            }
            return;
        }
        // 速度档以上游回报为准（fast 被限流时会回落到标准档）。
        if let Some(speed) = call.speed.as_deref() {
            shape.fast_mode = speed == "fast";
        }
        // 身份三件缺一不发：没有 device_id/session_id/account_uuid 的请求在官方那边根本
        // 不是订阅客户端的形态，替它报遥测只会造出一份自相矛盾的记录。
        let Some(device_id) = shape.device_id.clone() else { return };
        let Some(session_id) = shape.session_id.clone().or_else(|| call.session_header.clone())
        else {
            return;
        };
        let Some(account_uuid) = shape
            .account_uuid
            .clone()
            .or_else(|| call.account_uuid.clone())
            .filter(|a| !a.trim().is_empty())
        else {
            return;
        };
        let version = version_from_ua(&call.ua_out);
        let now = Instant::now();

        if let Some(org) = call.organization_id.as_deref().filter(|o| !o.is_empty()) {
            st.org_uuid.insert(call.cred_id, org.to_string());
        }
        // 子代理：billing header 里 `cc_is_subagent=true`，2.1.277 起还有 `x-claude-code-agent-id`。
        // 每个子代理是会话里的一条支线，状态按支线号分开记（见 [`AgentState`]）。
        let is_agent = shape.is_subagent || call.agent.agent_id.is_some();
        let agent_key = call.agent.agent_id.clone().unwrap_or_default();
        let thread_key = if is_agent { format!("agent:{agent_key}") } else { "main".to_string() };
        // 线程增量请求先补成客户端视角的全量，再往下判类别（见 [`ThreadBase`]）。
        if shape.thread_type.as_deref() == Some("continue")
            && let Some(base) = st
                .sessions
                .get(&(call.cred_id, session_id.clone()))
                .and_then(|s| s.thread_bases.get(&thread_key))
        {
            let auto_in_delta = shape.auto_marker || shape.permission_declared;
            base.fill(&mut shape, auto_in_delta);
        }
        // ---- 会话形态：`/clear` 与 `--continue`，见 [`State::process_starts`] ----
        let skey = (call.cred_id, session_id.clone());
        let dkey = (call.cred_id, device_id.clone());
        let marker = st.process_starts.get(&dkey).cloned();
        // 同一台设备上可能同时跑着几个进程（另开一个终端、脚本里跑 `claude -p`），别的进程的启动
        // 探测不能把正在跑的会话当成 `--continue`。三样都满足才算：这个会话最后一次活动在探测之前、
        // 这一条不是 thread 续轮（新进程接不上旧进程的线程，`--continue` 之后首条是 create）、探测
        // 那个临时会话 id 没有自己发过请求（另一个进程会接着用它自己的 id）。
        let continued_from = marker.as_ref().and_then(|(probe_sid, at)| {
            let seen_before = st
                .sessions
                .get(&skey)
                .is_some_and(|s| s.last_call_end.unwrap_or(s.first_call_at) <= *at)
                || st.ended.contains_key(&skey);
            let probe_has_own_session =
                st.sessions.contains_key(&(call.cred_id, probe_sid.clone()));
            (probe_sid != &session_id
                && !shape.sdk
                && seen_before
                && shape.thread_type.as_deref() != Some("continue")
                && !probe_has_own_session)
                .then(|| (probe_sid.clone(), *at))
        });
        if continued_from.is_some() {
            // 新进程接上旧会话：会话里的一切从头来（与退出后 `--resume` 一样），指标报 `continue`。
            st.sessions.remove(&skey);
            st.ended.remove(&skey);
            st.process_starts.remove(&dkey);
        }
        let cleared_from: Option<String> = if continued_from.is_none()
            && !st.sessions.contains_key(&skey)
            && !shape.sdk
            && let Some((probe_sid, _)) = marker.as_ref()
            && probe_sid != &session_id
        {
            // 被换掉的是这台设备上**最近在用**的那个交互会话。几个进程同时开着时，按探测时刻去挑，
            // 挑中的是最后启动的那个进程，而不是用户刚在里面敲 `/clear` 的那个。
            st.sessions
                .iter()
                .filter(|((c, sid), s)| {
                    *c == call.cred_id
                        && sid != &session_id
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
            let p = st.sessions.get_mut(&(call.cred_id, prev.clone()))?;
            p.cleared = true;
            Some((
                p.last_main_request_id.clone(),
                p.last_call_end,
                p.started_wall,
                p.background_done,
            ))
        });
        let parent_session_id = cleared_from
            .clone()
            .or_else(|| st.sessions.get(&skey).and_then(|s| s.parent_session_id.clone()));
        let identity = Identity {
            parent_session_id: parent_session_id.clone(),
            sdk: shape.sdk,
            session_id: session_id.clone(),
            device_id: device_id.clone(),
            account_uuid: account_uuid.clone(),
            organization_uuid: st.org_uuid.get(&call.cred_id).cloned(),
            subscription_type: subscription_type(call.org_type.as_deref()).to_string(),
            version: version.clone(),
            agent_id: if is_agent { call.agent.agent_id.clone() } else { None },
            vcs: shape
                .git_repo
                .or_else(|| {
                    st.sessions.get(&(call.cred_id, session_id.clone())).and_then(|s| s.git_repo)
                })
                .filter(|r| *r)
                .map(|_| "git"),
        };
        // 会话级的那份（待发批次、退出收尾、启动模板用）不带支线号。
        let base_identity = Identity { agent_id: None, ..identity.clone() };

        let kind = if is_agent {
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
        };
        // 侧查询（标题生成、页面处理、起名）与分叉出来的插问、压缩按 `default` 权限模式跑，
        // 不管主线程是不是 auto（`cap/auto-2.1.285-20260930`：auto 会话里的 `/compact` 也报
        // default）；WebSearch 那条跟主线程（同一会话报 auto）。
        if matches!(
            kind,
            Kind::Title
                | Kind::Helper
                | Kind::WebFetchApply
                | Kind::RenameName
                | Kind::ModelValidation
                | Kind::SideQuestion
                | Kind::Compact
        ) {
            shape.permission_mode = "default";
        }
        // 展示名：出站体里 `[1m]` 已经被客户端剥成 `context-1m` beta，这里还原回去。
        let has_1m = call.betas.as_deref().is_some_and(|b| b.contains("context-1m-"));
        let display_model =
            if has_1m { format!("{}[1m]", shape.model) } else { shape.model.clone() };
        let resp_model = call.resp_model.clone().unwrap_or_else(|| shape.model.clone());

        let key = (call.cred_id, session_id.clone());
        let is_new_session = !st.sessions.contains_key(&key);
        // 侧查询（标题生成等）先扣住：它没有 cc_prompt_id，真实客户端给它打的是紧随其后那条
        // 主线程请求的新一轮 id。等到那条再补发，或者超时按现有 id 发（见 [`Self::gc`]）。
        if allow_defer
            && matches!(kind, Kind::Title | Kind::Helper)
            && let Some(sess) = st.sessions.get_mut(&key)
        {
            let prev_end = sess.last_call_end;
            // 它确实已经完成了：后到的主线程请求算 `timeSinceLastApiCallMs` 时要以它为准
            // （抓包里主线程那条的 2529ms 量的正是到标题完成的距离）。
            let this_end = call.started_at + Duration::from_millis(call.total_ms);
            sess.last_call_end = Some(prev_end.map_or(this_end, |e| e.max(this_end)));
            sess.recent_ends.push_back(this_end);
            sess.deferred.push((call, prev_end, now));
            return;
        }
        // 同一个 id 在按退出收尾之后再出现 = `--resume`：新进程从头计数，只在指标上标 resume。
        let resumed = is_new_session && st.ended.remove(&key).is_some();
        let continued = continued_from.is_some();
        let device_key = (call.cred_id, device_id.clone());
        let device_default = st.device_default_model.get(&device_key).map(|(m, _)| m.clone());
        let sess = st.sessions.entry(key.clone()).or_insert_with(|| Session {
            // 进程启动到首条请求：2.1.260 模板的启动段从 -1.3s 起、按 3.1s 算；2.1.285 那份启动段
            // 从 -6.2s 起（`cap/2.1.285/00040`：进程 06:34:08.6 起，扣掉信任对话框后约 6.2s 到首条
            // api_query）。
            started_wall: cleared_prev.as_ref().map_or_else(
                || {
                    call.started_at
                        - Duration::from_millis(if shape.sdk {
                            // `-p` 的启动段从 -4.4s 起（`00448`）。
                            4_450
                        } else if version_at_least(&version, "2.1.285") {
                            6_300
                        } else {
                            3_100
                        })
                },
                |p| p.2,
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
            background_done: cleared_prev.as_ref().map_or(0, |p| p.3),
            recent_ends: VecDeque::new(),
            last_call_end: None,
            prev_total_input: 0,
            default_model: device_default.clone().unwrap_or_else(|| display_model.clone()),
            main_model_seen: device_default.is_some(),
            last_message_id: None,
            last_model: None,
            tools_hash_main: None,
            tools_hash_side: None,
            counted: false,
            first_turn_done: false,
            device_id: device_id.clone(),
            account_uuid: account_uuid.clone(),
            version: version.clone(),
            betas: String::new(),
            subscription_type: identity.subscription_type.clone(),
            deferred: Vec::new(),
            snapshot_hash: None,
            sleepy_models: Vec::new(),
            tether_main: None,
            thread_bases: HashMap::new(),
            agents: HashMap::new(),
            last_spawn_request_id: None,
            main_permission: "default",
            turn_origin: "human".to_string(),
            turn_skill: None,
            first_call_at: call.started_at,
            parent_session_id: cleared_from.clone(),
            sdk: shape.sdk,
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
        });
        sess.last_seen = now;
        // 续轮带回的工具结果配上一条回复里记下的调用：thread 续轮全靠它，全量请求也拿它补上判决。
        if let Some(calls) = sess.reply_calls.get(&thread_key) {
            for tu in shape.tool_uses.iter_mut() {
                if let Some(c) = calls.iter().find(|c| c.id == tu.id) {
                    tu.apply_call(&c.name, &c.input, c.verdict.as_ref());
                }
            }
        }
        // 上一条回复的工具调用：这条新输入若带着被拒的工具结果，要用它认出是哪个工具。
        let prev_reply_calls = sess.reply_calls.get(&thread_key).cloned().unwrap_or_default();
        if matches!(kind, Kind::Main | Kind::Subagent) && !call.aborted {
            sess.reply_calls.insert(thread_key.clone(), call.tool_calls.clone());
        }
        if shape.cwd.is_some() {
            sess.cwd = shape.cwd.clone();
        }
        if shape.git_repo.is_some() {
            sess.git_repo = shape.git_repo;
        }
        if shape.git_dirty.is_some() {
            sess.git_dirty = shape.git_dirty;
        }
        // auto 模式那条 git 状态探测报的结果：仓库里有改动 `dirty`、没有 `clean`，不是仓库才
        // `not_a_repo`（`cap/auto-2.1.285-20260930` 75 条全是 dirty）。
        let git_outcome = match (sess.git_repo, sess.git_dirty) {
            (Some(true), Some(false)) => "clean",
            (Some(true), _) => "dirty",
            _ => "not_a_repo",
        };
        // 客户端跑 Bash 之前把多余的 `cd <工作目录> && ` 去掉（`cap/auto-2.1.285-20260930`：
        // 官方的 `bashCommandLen` / `toolInputSizeBytes` 恰好短这 117 字，`has_chain` 也因此为 false）。
        if let Some(cwd) = sess.cwd.as_deref() {
            let prefix = format!("cd {cwd} && ");
            for tu in shape.tool_uses.iter_mut().filter(|t| t.name == "Bash") {
                let stripped = tu
                    .input
                    .get("command")
                    .and_then(|c| c.as_str())
                    .and_then(|c| c.strip_prefix(prefix.as_str()))
                    .map(str::to_string);
                if let Some(cmd) = stripped {
                    tu.input["command"] = json!(cmd);
                    tu.input_len = tool_input_len("Bash", &tu.input);
                    tu.command_len = js_len(&cmd);
                }
            }
        }
        if kind == Kind::Main {
            sess.main_permission = shape.permission_mode;
        } else if kind == Kind::WebSearchTool
            || (kind.is_agent() && !shape.auto_marker && !shape.permission_declared)
        {
            // WebSearch 那条与没自带模式的子代理跟主线程走。
            shape.permission_mode = sess.main_permission;
        }
        // 内置的 claude-code-guide 子代理自带 `dontAsk` 权限模式，它与它的摘要请求都这么报
        // （`cap/2.1.285` 八条，同一会话主线程是 default）；Explore 这类跟着主线程走
        // （`cap/2.1.280` 七条 auto）。
        let agent_type = call
            .agent
            .agent_type
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| sess.agents.get(&agent_key).and_then(|a| a.agent_type.clone()));
        if kind.is_agent() && agent_type.as_deref() == Some("claude-code-guide") {
            shape.permission_mode = "dontAsk";
        }
        // 一轮结束（`end_turn`）才有 stop hook 与 turn_end；`tool_use` 是同一轮的中间步。
        let turn_over = call.stop_reason.as_deref().is_none_or(|s| s != "tool_use");
        // 这条请求客户端那头是失败的：收尾走 `tengu_api_error` 那一串，且没有任何用量。
        let failed = call.failure.is_some();
        // 客户端取消（见 [`ApiCall::aborted`]）：没有 success 也没有 error，收尾是取消那一串。
        let aborted = call.aborted && !failed;
        // 首轮那串版本检查是「一轮跑完了」才发的，失败的那轮不算。
        let emit_first_turn = kind == Kind::Main && turn_over && !failed && !sess.first_turn_done;
        if emit_first_turn {
            sess.first_turn_done = true;
        }
        let is_main = kind == Kind::Main;
        // 版本跟着每条走没问题（同一会话所有请求同一个 UA），但 **`betas` 只跟主线程**。
        //
        // 会话级 beta 是「这个会话」的属性，而侧查询（标题生成用 haiku + structured-outputs、
        // 安全分类用 auto-mode-classifier）各有一套完全不同的 beta。无条件覆盖之后，从侧
        // 查询结束到下一条主请求之间，[`Telemetry::latest_session`]（保活挂身份用）与指标
        // 导出那条 `tengu_feature_ok{internal_metrics_export}` 报的就是标题生成的 beta ——
        // 一个「会话主模型是 opus、会话 beta 却是标题生成那套」的组合，官方不产生。
        //
        // 会话第一条就是侧查询时还是要写一次，否则整个会话的 beta 都是空的。
        sess.version = version.clone();
        if is_main || sess.betas.is_empty() {
            sess.betas = session_betas(call.betas.as_deref().unwrap_or(""));
        }
        // 新一轮用户输入（只有主线程算）：prompt 计数 +1、换 prompt_id（优先用 billing header
        // 里客户端自己的）与 queryChainId、depth 归零。tool_result 续轮沿用上一轮的，depth +1。
        // 侧查询（标题、猜下一句）不动这些计数。
        let new_prompt = is_main && (shape.new_prompt || sess.prompts_seen == 0);
        // 本轮发起方：请求自己声明的优先（新输入与续轮都带），没带就沿用本轮的。
        if let Some(o) = shape.turn_origin.as_deref() {
            if is_main {
                sess.turn_origin = turn_origin_of(o);
            }
        } else if new_prompt {
            sess.turn_origin = "human".to_string();
        }
        // `!` 跑的命令那一轮与 `-p` 的输入官方都不打标：billing header 写 `human` / `sdk`，事件报
        // `unstamped`（`cap/auto-2.1.285-20260930/00191`、`00441` 等十条）。
        if is_main && (shape.sdk || (new_prompt && shape.bash_input)) {
            sess.turn_origin = "unstamped".to_string();
        }
        if new_prompt {
            sess.turn_skill = shape.command_skill.clone();
        }
        let turn_skill = if is_main { sess.turn_skill.clone() } else { None };
        // 压缩之后的第一条主线程请求：快照重录、对话 token 从零算、报 `isPostCompaction`
        // （`00178`）。
        let post_compaction = is_main && std::mem::take(&mut sess.post_compact);
        if post_compaction {
            sess.snapshot_hash = None;
            sess.prev_total_input = 0;
        }
        if kind == Kind::Compact && !failed {
            sess.post_compact = true;
        }
        let turn_origin = sess.turn_origin.clone();
        // 上一轮的链、深度与起点：这次新输入前若有工具被用户拒了，拒绝那串与 `aborted_tools`
        // 的收尾挂在上一轮上。
        let prev_turn = (sess.chain_id.clone(), sess.turn_depth, sess.turn_started);
        let rejected_calls: Vec<(ToolCall, ToolResultInfo)> = if new_prompt && is_main {
            shape
                .rejected
                .iter()
                .filter_map(|r| {
                    let c = prev_reply_calls.iter().find(|c| c.id == r.id)?;
                    Some((c.clone(), r.clone()))
                })
                .collect()
        } else {
            Vec::new()
        };
        // 用户自己起的一轮：敲的（`human`），或官方不打标的 `!` 命令与 `-p` 输入（`unstamped`）。
        // 同伴会话、后台任务通知这类客户端注入的一轮不算。
        let user_turn = matches!(turn_origin.as_str(), "human" | "unstamped");
        // 夹在这次输入里的后台任务通知各占一个输入号，排在这次输入前面（见
        // [`RequestShape::merged_notifications`]）。
        let notification_base = sess.prompt_index;
        let notifications = if new_prompt && user_turn && is_main {
            shape.merged_notifications.clone()
        } else {
            Vec::new()
        };
        sess.prompt_index += notifications.len() as u32;
        let prev_main_end = sess.last_main_end;
        if is_main && !aborted {
            sess.last_main_end = Some(this_end_wall(&call));
        }
        if new_prompt {
            // 同伴会话发来的一轮（`peer`）不算用户的第几次输入：官方那条 input_prompt 不带
            // `prompt_index`，下一次输入接着原来的数（`cap/2.1.280`：…2、peer、3）。
            // `!` 跑的命令那一轮报 `input_bash`，同样不占输入号（`cap/auto-2.1.285-20260930` 07:58:47
            // 那次输入是 22，跳过了前面那条 `!git log`）。
            if turn_origin != "peer" && !shape.bash_input {
                sess.prompt_index += 1;
            }
            sess.prompts_seen += 1;
            sess.chain_id = uuid_v4();
            sess.turn_depth = 0;
            // 换 `turn_started` 之前先把上一轮的提交时刻挪走：`user_secs` 的窗口下界要它。
            sess.prev_prompt_submit = sess.turn_started;
            sess.turn_started = Some(call.started_at - Duration::from_millis(15));
            sess.turn_text_seen = false;
            sess.turn_tool_calls = 0;
            sess.turn_api_calls = 0;
            sess.turn_api_ms = 0;
        }
        if let Some(pid) = shape.cc_prompt_id.clone() {
            sess.prompt_id = pid;
        } else if sess.prompt_id.is_empty() {
            sess.prompt_id = uuid_v4();
        }
        let prompt_id = sess.prompt_id.clone();
        // 主线程与猜下一句走 `previousRequestId` 链；标题那类没有。
        //
        // **以出站体里那份 `cc_prev_req` 为准**（同 `cc_prompt_id` 的取法）：那是这条请求
        // 已经发给上游的声明，而会话状态是回程时另算的一份。两者本该恒等（`cap/2.1.260-2`
        // 三条续轮逐字相同），但它们的更新路径不同——`cc_prev_req` 由
        // [`crate::proxy::CcSessionLink::record`] 在 `ReqLog::drop` 里**同步**写，
        // 这里的 `last_main_request_id` 走 [`Telemetry::record`] 那条队列。让遥测复述请求
        // 自己说过的话，两份就不可能对不上；体里没有（会话首轮、没有 billing header 的
        // 来访）才回落到会话状态。
        // 子代理支线：首条请求建档（链、起点、拉起它的那条主线程请求），之后每条读它。
        // 摘要请求只读不推进。
        let agent = kind.is_agent().then(|| {
            let spawn = sess.last_spawn_request_id.clone();
            let a = sess.agents.entry(agent_key.clone()).or_default();
            if a.agent_type.is_none() {
                a.agent_type = call.agent.agent_type.clone().filter(|t| !t.is_empty());
            }
            if a.chain_id.is_empty() {
                a.chain_id = uuid_v4();
                a.started = Some(call.started_at);
                a.prompt_chars = shape.prompt_len;
                a.invoking_request_id = spawn;
            }
            AgentView {
                chain_id: a.chain_id.clone(),
                steps: a.steps,
                last_request_id: a.last_request_id.clone(),
                last_message_id: a.last_message_id.clone(),
                prev_total: a.prev_total,
                tools_hash: a.tools_hash.clone(),
                invoking_request_id: (a.steps == 0)
                    .then(|| a.invoking_request_id.clone())
                    .flatten(),
            }
        });
        let previous_request_id = kind
            .has_chain()
            .then(|| {
                shape.cc_prev_req.clone().or_else(|| match &agent {
                    Some(a) => a.last_request_id.clone(),
                    None => sess.last_main_request_id.clone(),
                })
            })
            .flatten();
        let prev_main_message_id = sess.last_main_message_id.clone();
        let prev_main_request_id = sess.last_main_request_id.clone();
        let prev_main_depth = sess.last_main_depth;
        // `timeSinceLastApiCallMs` = **这条完成时刻 − 上一条完成时刻**（不分主线程/侧查询，
        // 按完成先后）。`cap/2.1.260-2`：标题 09.490−06.166=3323、主线程 12.019−09.490=2529、
        // 续轮 19.458−12.019=7439、猜下一句 21.385−19.458=1927，全部对上；用「这条开始」算
        // 的话并发的标题与主线程会出负数、续轮只剩工具执行那一秒。
        let this_end = call.started_at + Duration::from_millis(call.total_ms);
        let prev_end: Option<SystemTime> = prev_end_override.or(sess.last_call_end);
        // 并发的请求（子代理摘要与子代理本身、主线程与后台的回顾）谁先结束说不准：取在这条之前
        // 结束的最近一条（`cap/2.1.285` 的 agent_summary 两条、主线程续轮都报了这一项）。
        let prev_done = prev_end_override
            .or_else(|| sess.recent_ends.iter().filter(|t| **t < this_end).max().copied())
            .or(prev_end);
        let time_since_last =
            prev_done.and_then(|t| this_end.duration_since(t).ok()).map(|d| d.as_millis() as u64);
        sess.recent_ends.push_back(this_end);
        while sess.recent_ends.len() > 16 {
            sess.recent_ends.pop_front();
        }
        let message_tokens = match &agent {
            Some(a) => a.prev_total,
            None if kind.has_chain() => sess.prev_total_input,
            None => 0,
        };
        if kind == Kind::Main && !sess.main_model_seen {
            sess.main_model_seen = true;
            sess.default_model = display_model.clone();
        }
        let default_model = sess.default_model.clone();
        let device_default_update = if kind == Kind::ModelValidation && !failed {
            Some(resp_model.clone())
        } else if kind == Kind::Main && device_default.is_none() {
            Some(default_model.clone())
        } else {
            None
        };
        // 事件顶层 `model` 与 Datadog 的 `model` 是**会话主模型**（用户设置的那个），侧查询
        // 自己用的 haiku 只出现在 api 事件的 meta 里。
        let session_model = sess.last_model.clone().unwrap_or_else(|| sess.default_model.clone());
        let model_changed =
            is_main && sess.last_model.as_deref().is_some_and(|m| m != display_model);
        // 同理：`diagnostics.previous_message_id` 是这条请求自己声明的那个，优先于会话状态。
        let previous_message_id =
            shape.diag_prev_message_id.clone().or_else(|| sess.last_message_id.clone());
        let tools_slot = match &agent {
            Some(a) => &a.tools_hash,
            None if kind.has_boundary() => &sess.tools_hash_main,
            None => &sess.tools_hash_side,
        };
        let tools_changed = tools_slot.as_deref() != Some(shape.tools_hash.as_str());
        let counted = sess.counted;
        let started_wall = sess.started_wall;
        // queryDepth：主线程本轮第几次请求；猜下一句 = 主线程最后一次 + 2（抓包：0→2、1→3）；
        // 子代理从 2 起每条 +1、整条支线一个链（`cap/2.1.277` 2…27、`cap/2.1.280` 2…7），
        // 它的摘要请求恒为 3、链另起（两份抓包 7 条都是）。
        // 主线程当前的链：猜下一句的 fork 统计里引用的是这条父链。
        let main_chain = sess.chain_id.clone();
        let (chain_id, query_depth) = match kind {
            Kind::Main => (sess.chain_id.clone(), sess.turn_depth),
            Kind::Suggestion | Kind::AwaySummary => (uuid_v4(), sess.last_main_depth + 2),
            Kind::Subagent => {
                let a = agent.as_ref().expect("subagent calls carry agent state");
                (a.chain_id.clone(), 2 + a.steps)
            }
            Kind::AgentSummary => (uuid_v4(), 3),
            Kind::SideQuestion | Kind::Compact => (uuid_v4(), 1),
            Kind::Title
            | Kind::Helper
            | Kind::WebFetchApply
            | Kind::WebSearchTool
            | Kind::RenameName
            | Kind::ModelValidation => (String::new(), 0),
        };
        let turn_started: DateTime<Utc> =
            sess.turn_started.unwrap_or(call.started_at - Duration::from_millis(15)).into();
        let first_text_in_turn = is_main && call.text_chars > 0 && !sess.turn_text_seen;
        // 一个字都还没出就被打断：首字事件报 `interrupted`（`cap/auto-2.1.285-20260930` 08:01:42.635）。
        let first_text_interrupted =
            is_main && aborted && call.text_chars == 0 && !sess.turn_text_seen;
        // 首字之前跑过的工具数 = 本轮此前的 + 这条续轮带回来的。
        let tool_calls_before = sess.turn_tool_calls + shape.tool_uses.len() as u32;
        // `active_time.total{type:user}`：用户敲这条输入花掉的时间。
        //
        // 官方的口径不是「上一条请求结束到这次提交」，而是**输入框里每次改动之间的间隔之和**
        // （`ActivityTracker.recordUserActivity`：每个按键/粘贴/提交各记一次，只累加
        // 间隔小于 `USER_ACTIVITY_TIMEOUT_MS` = 5s 的那些，且 CLI 忙着的时候不算）。
        // 也就是说它量的是「打字时长」，与那一轮 API 花了多久无关——旧口径取 CLI 时长的
        // 一个比例是量错了对象。
        //
        // 代理这一侧看不见按键，但看得见输入的**字数**，而打字时长就是它的线性函数。
        // 三份抓包（一次输入 2 字 → 0.878s / 2 字 → 1.118s / 3 字 + 20 字 → 3.988s 合计）
        // 拟合出 `0.8 + 0.1 × 字数`：2 字 → 1.0（实测均值 0.998）、3 字 + 20 字 → 1.1 + 2.8
        // = 3.9（实测 3.988）。0.8s 是「上一次活动到第一个按键」加「最后一个按键到提交」
        // 那两段，0.1s/字 ≈ 10 字/秒。
        //
        // 再按可用窗口截断：窗口是**上一次提交到这次提交**（会话第一条则从进程起点算），
        // 不能是「上一条请求结束到这次提交」——用户会边看回复边打字，抓包里第二次输入
        // 贡献的 2.9s 就大于上一轮结束之后剩下的那 2.2s。粘贴一大段时估算值会顶到窗口上限，
        // 方向也是对的（官方那边粘贴只记一次改动，只有两段间隔）。
        let user_secs = if !new_prompt {
            0.0
        } else {
            let submit = call.started_at - Duration::from_millis(15);
            let typed = 0.8 + 0.1 * shape.prompt_len as f64;
            let base = sess.prev_prompt_submit.unwrap_or(sess.started_wall);
            let window = submit.duration_since(base).map_or(typed, |d| d.as_secs_f64());
            typed.min(window).max(0.0)
        };
        let shell_snapshot_first = is_main
            && !sess.shell_snapshot_done
            && shape.tool_uses.iter().any(|t| t.name == "Bash");
        let prompt_index = sess.prompt_index.max(1);
        let prompt_seq = sess.prompts_seen.max(1);

        // 版本分档。2.1.270 起 `tengu_api_success` 多了 `firstContentMs` / `clientRequestId` /
        // `snapshotHash`，`systemPromptSource` 分成 `live_recorded`（会话首条）与
        // `from_snapshot`（之后）；2.1.277 起多 `turn_origin`，没有 effort 的输入（haiku）
        // 不再报 `effort_level`。tether 那三条、`declared_tool_set_held`、
        // `sleepy_snowflake_applied` 在 2.1.270–2.1.277 之间键集合还在变（`rh`、
        // `claimedCollapse`、`drop*` 几项进进出出），只按 `cap/2.1.280` 的布局给 2.1.280 起。
        let v270 = version_at_least(&version, "2.1.270");
        let v277 = version_at_least(&version, "2.1.277");
        let v280 = version_at_least(&version, "2.1.280");
        // 2.1.285：`tengu_api_success` 多十项（`dispatch`、`echoWireToolInputs`、未覆盖尾段那六项、
        // `queryOverheadMs`、`requestPrepareMs`），tether 两条多 `creditRetryStateless` /
        // `toolResultClearingHeldStateless`，auto 模式不再把请求钉成无状态（`cap/2.1.285`）。
        let v285 = version_at_least(&version, "2.1.285");
        // 客户端自己注入的一轮（后台任务通知 / 同伴会话）：没有提交、粘贴、渲染那几条，只有
        // 一条排队消息送达（`cap/2.1.280` 两条、`cap/2.1.285` 06:57:29.588）。
        let injected = new_prompt && version_at_least(&version, "2.1.277") && !user_turn;
        let first_prompt_tpl = new_prompt && !injected && !sess.first_prompt_tpl_done;
        if first_prompt_tpl {
            sess.first_prompt_tpl_done = true;
        }
        // 后台任务那段：进程启动后多久该出现的，这条请求发出时已经过了就补上（每条一次）。
        let background = {
            let bg = &template_for(&version, shape.sdk).background;
            let from = sess.background_done;
            let mut to = from;
            while to < bg.len()
                && sess.started_wall + Duration::from_millis(bg[to].off.max(0) as u64)
                    <= call.started_at
            {
                to += 1;
            }
            sess.background_done = to;
            from..to
        };
        let first_main = v280 && is_main && !failed && !sess.first_main_done;
        if first_main {
            sess.first_main_done = true;
        }
        // 快照一条线一份：主线程（含猜下一句）一份，每个子代理（含它的摘要请求）各一份
        // （`cap/2.1.280` 主线程 `b06e…`/`d6bc…`、Explore 子代理 `7d8050a24c2c`，子代理首条
        // 同样报 `live_recorded`）。
        let snapshot = (v270 && kind.has_boundary()).then(|| {
            let slot = if kind.is_agent() {
                &mut sess.agents.get_mut(&agent_key).expect("agent state exists").snapshot_hash
            } else {
                &mut sess.snapshot_hash
            };
            let recorded = slot.is_none();
            let hash = slot.get_or_insert_with(|| snapshot_hash_of(&shape)).clone();
            (if recorded { "live_recorded" } else { "from_snapshot" }, hash)
        });
        let sleepy = v280 && new_prompt && !sess.sleepy_models.contains(&display_model);
        if sleepy {
            sess.sleepy_models.push(display_model.clone());
        }
        // tether 只管主线程与子代理（摘要、猜下一句、标题这类辅助调用一条都没有），子代理
        // 每条支线一个线程（`cap/2.1.280` Explore 首条 `create/first_request`、之后 `continue/append`）。
        let tether = (v280 && matches!(kind, Kind::Main | Kind::Subagent)).then(|| {
            let slot = if is_main {
                &mut sess.tether_main
            } else {
                &mut sess.agents.get_mut(&agent_key).expect("agent state exists").tether
            };
            let betas = call.betas.clone().unwrap_or_default();
            let t = tether_decide(slot.as_ref(), &shape, &display_model, &betas, is_main);
            *slot = Some(TetherThread {
                model: display_model.clone(),
                betas,
                effort: shape.effort.clone(),
                tools_hash: shape.tools_hash.clone(),
                thinking_type: shape.thinking_type.clone(),
                messages: shape.messages_len,
                turns: t.turns,
            });
            t
        });
        // 这条线程的全量形态记下来，给下一条增量请求补（见 [`ThreadBase`]）。
        if matches!(kind, Kind::Main | Kind::Subagent) && shape.tools_count > 0 {
            sess.thread_bases.insert(
                thread_key.clone(),
                ThreadBase::of(&shape, call.reply_input_chars, call.tool_use_lens.clone()),
            );
        }

        // 更新会话状态给下一条用。
        // 补发的侧查询比后来的主线程请求结束得早，别把「最近一次结束」往回拨。
        sess.last_call_end = Some(sess.last_call_end.map_or(this_end, |e| e.max(this_end)));
        // 被取消的那条不进链：官方下一条的 `previousRequestId` / `previousMessageId` 仍指它之前
        // 那条（`cap/auto-2.1.285-20260930/00264`）。
        if !aborted {
            sess.last_message_id = call.message_id.clone().or(sess.last_message_id.take());
        }
        // 「猜下一句」出的建议：有正文就挂着，等用户下一次输入（或 `/compact`、`/btw` 这类斜杠
        // 命令）时报 ignored；这条输入把它消费掉。
        let usage_before = sess.usage_totals;
        if !failed && !aborted {
            for (t, v) in sess.usage_totals.iter_mut().zip([
                call.input_tokens,
                call.output_tokens,
                call.cache_read_tokens,
                call.cache_creation_tokens,
            ]) {
                *t += v;
            }
        }
        let ignored_suggestion =
            if (new_prompt && user_turn) || matches!(kind, Kind::Compact | Kind::SideQuestion) {
                sess.shown_suggestion.take()
            } else {
                None
            };
        if kind == Kind::Suggestion && !failed && !aborted && call.text_chars > 0 {
            sess.shown_suggestion =
                Some((call.request_id.clone().unwrap_or_default(), this_end, call.text_chars));
        }
        let interrupted_message_id =
            if new_prompt { sess.interrupted_message_id.take() } else { None };
        if is_main && aborted {
            sess.interrupted_message_id = call.message_id.clone();
        }
        if is_main && !aborted {
            sess.last_main_request_id =
                call.request_id.clone().or(sess.last_main_request_id.take());
            sess.last_main_message_id =
                call.message_id.clone().or(sess.last_main_message_id.take());
        }
        if is_main {
            sess.last_main_depth = sess.turn_depth;
            // 失败那条没有用量，`messageTokens`（「对话此刻的 token 数」）不该被它清零。
            if !failed && !aborted {
                sess.prev_total_input = call.input_tokens
                    + call.cache_read_tokens
                    + call.cache_creation_tokens
                    + call.output_tokens;
            }
            sess.last_model = Some(display_model.clone());
            sess.turn_tool_calls += shape.tool_uses.len() as u32;
            sess.turn_api_calls += 1;
            sess.turn_api_ms += call.total_ms as i64;
            if first_text_in_turn {
                sess.turn_text_seen = true;
            }
            if shell_snapshot_first {
                sess.shell_snapshot_done = true;
            }
            if !turn_over {
                sess.turn_depth += 1;
            }
        }
        // 拉起子代理的那条主线程请求（回复里调了 `Agent`），见 [`Session::last_spawn_request_id`]。
        if is_main && call.tool_use_lens.iter().any(|(name, _)| name == "Agent" || name == "Task") {
            sess.last_spawn_request_id = call.request_id.clone();
        }
        // 子代理以 SubagentHandback 收尾（`cap/auto-2.1.285-20260930` 07:51:29.419–.455）：那个工具
        // 客户端当场执行，子代理不再发请求，这一条就是它的最后一条。
        let handback = (kind == Kind::Subagent && !turn_over && !failed && !aborted)
            .then(|| call.tool_calls.iter().find(|c| c.name == "SubagentHandback").cloned())
            .flatten();
        // 子代理支线推进一步（摘要请求不算）。
        let mut agent_done: Option<AgentState> = None;
        if kind == Kind::Subagent
            && let Some(a) = sess.agents.get_mut(&agent_key)
        {
            a.steps += 1;
            a.tool_uses += shape.tool_uses.len() as u32;
            a.last_request_id = call.request_id.clone().or(a.last_request_id.take());
            a.last_message_id = call.message_id.clone().or(a.last_message_id.take());
            if !failed {
                a.prev_total = call.input_tokens
                    + call.cache_read_tokens
                    + call.cache_creation_tokens
                    + call.output_tokens;
                a.tools_hash = Some(shape.tools_hash.clone());
            }
            // `end_turn` 收尾就是子代理跑完了：收尾事件要它的全程统计，档案随之删掉。以
            // SubagentHandback 工具收尾的也是（客户端就地执行、不再发请求）。
            if (turn_over || handback.is_some()) && !failed && !aborted {
                agent_done = sess.agents.remove(&agent_key);
            }
        }
        // 长度表报过一次就不再重发——但失败那条压根没报（`tengu_tool_schema_sizes` 在官方
        // 那边就长在 `tengu_api_success` 里），别让它把「已报过」的标记占掉。
        if !failed && !kind.is_agent() && kind != Kind::ModelValidation {
            if kind.has_boundary() {
                sess.tools_hash_main = Some(shape.tools_hash.clone());
            } else {
                sess.tools_hash_side = Some(shape.tools_hash.clone());
            }
        }
        sess.counted = true;
        let ctx_model = if is_main { display_model.clone() } else { session_model };
        let dd_model = ctx_model.trim_end_matches("[1m]").to_string();

        let mut file_sizes = std::mem::take(&mut sess.file_sizes);
        // 这一轮到这条为止跑过的工具数（`-p` 收尾那条 `tengu_sdk_result.tool_use_count`）。
        let sess_turn_tools = sess.turn_tool_calls;
        let (sess_turn_api_calls, sess_turn_api_ms) = (sess.turn_api_calls, sess.turn_api_ms);
        let session_cwd = sess.cwd.clone();
        let betas_full = call.betas.clone().unwrap_or_default();
        let betas_own = session_betas(&betas_full);
        // 子代理支线上的事件顶层 `betas` 报**主线程**那份，只有它自己的 `api_query` /
        // `api_success` / `agent_tool_completed` 报它这条请求的（`cap/2.1.285`：haiku 子代理 251
        // 条事件里 226 条是主线程那份 `claude-code…mid-conversation-system`，那三类 25 条是
        // haiku 的 `oauth,interleaved…`；`cap/2.1.280` 的 Explore 与主线程同模型，两份本来相同）。
        if is_main {
            sess.main_betas = betas_own.clone();
        }
        // 这条线程上第几条请求（0 起）：主线程数会话里的主线程请求，子代理数它那条支线的步数。
        let thread_step = if is_main {
            let n = sess.main_requests;
            sess.main_requests += 1;
            Some(n)
        } else if kind == Kind::Subagent {
            agent.as_ref().map(|a| a.steps)
        } else {
            None
        };
        let betas_session = if kind.is_agent() && !sess.main_betas.is_empty() {
            sess.main_betas.clone()
        } else {
            betas_own.clone()
        };

        let facts = TurnFacts {
            device_id,
            session_id,
            account_uuid,
            version,
            now,
            continued_from,
            cleared_from,
            cleared_prev,
            identity,
            base_identity,
            kind,
            has_1m,
            display_model,
            resp_model,
            key,
            is_new_session,
            resumed,
            continued,
            device_key,
            git_outcome,
            turn_over,
            failed,
            aborted,
            emit_first_turn,
            is_main,
            new_prompt,
            turn_skill,
            post_compaction,
            turn_origin,
            prev_turn,
            rejected_calls,
            user_turn,
            notification_base,
            notifications,
            prev_main_end,
            prompt_id,
            agent,
            previous_request_id,
            prev_main_message_id,
            prev_main_request_id,
            prev_main_depth,
            prev_end,
            time_since_last,
            message_tokens,
            default_model,
            device_default_update,
            model_changed,
            previous_message_id,
            tools_changed,
            counted,
            started_wall,
            main_chain,
            chain_id,
            query_depth,
            turn_started,
            first_text_in_turn,
            first_text_interrupted,
            tool_calls_before,
            user_secs,
            shell_snapshot_first,
            prompt_index,
            prompt_seq,
            v270,
            v277,
            v280,
            v285,
            injected,
            first_prompt_tpl,
            background,
            first_main,
            snapshot,
            sleepy,
            tether,
            usage_before,
            ignored_suggestion,
            interrupted_message_id,
            handback,
            agent_done,
            ctx_model,
            dd_model,
            sess_turn_tools,
            sess_turn_api_calls,
            sess_turn_api_ms,
            session_cwd,
            betas_full,
            betas_own,
            thread_step,
            betas_session,
        };
        let BuiltEvents { events, dd, probe_side } =
            Self::build_events(&call, &shape, &facts, &mut file_sizes);
        let TurnFacts {
            device_id,
            session_id,
            account_uuid,
            version,
            now,
            identity,
            base_identity,
            kind,
            display_model,
            key,
            is_new_session,
            resumed,
            continued,
            device_key,
            turn_over,
            failed,
            aborted,
            is_main,
            prompt_id,
            device_default_update,
            counted,
            started_wall,
            user_secs,
            v285,
            betas_session,
            ..
        } = facts;

        // 启动握手**不在这里排队**。这里是回程（`ReqLog` 收尾之后），排在这儿等于让上游
        // 先看到一条 messages、几秒后才看到这个「会话」的启动流量——顺序整个反了。
        // 现在由转发路径在**首条请求发出之前**直接开跑，见
        // [`crate::proxy::spawn_session_handshake`]；那里还能分辨模拟与真实 CC，后者自己
        // 会打这一串，luban 不该重复。
        let _ = is_new_session;

        if let Some(m) = device_default_update {
            st.device_default_model.insert(device_key, (m, now));
        }
        if let Some((probe_identity, out)) = probe_side {
            let p =
                st.pending.entry((call.cred_id, probe_identity.session_id.clone())).or_default();
            p.version = version.clone();
            p.subscription_type = probe_identity.subscription_type.clone();
            p.model = display_model.clone();
            p.betas = betas_session.clone();
            p.identity = Some(probe_identity);
            p.push_batch(out, now);
        }
        if let Some(sess) = st.sessions.get_mut(&key) {
            sess.file_sizes = file_sizes;
        }

        // ---- 入队 ----
        let pending = st.pending.entry((call.cred_id, session_id.clone())).or_default();
        pending.version = version;
        pending.subscription_type = identity.subscription_type.clone();
        if let Some(vcs) = identity.vcs {
            pending.backfill_vcs(vcs);
        }
        pending.identity = Some(base_identity.clone());
        // **模型 / beta / prompt_id 只跟主线程走**（第一条就是侧查询时先占个位）。
        //
        // 这三项是导出指标时那条 `tengu_feature_ok{internal_metrics_export}` 的上下文，
        // 代表的是「这个会话」。被一条标题生成（haiku + structured-outputs、且没有
        // `cc_prompt_id`）覆盖之后，导出事件报的就成了 haiku 与标题那套 beta——而同一批
        // 指标里的 `model` 属性仍是会话主模型，自相矛盾。
        if is_main || pending.model.is_empty() {
            pending.model = display_model.clone();
            pending.betas = betas_session.clone();
            pending.prompt_id = prompt_id.clone();
        }
        pending.started_wall = Some(started_wall);
        pending.push_batch((events, dd), now);
        pending.metrics_since.get_or_insert(now);
        pending.metrics.push(CallMetric {
            session_id,
            device_id,
            account_uuid,
            model: display_model,
            category: kind.category(),
            effort: shape.effort.clone(),
            agent_name: (v285 && kind == Kind::Subagent)
                .then(|| call.agent.agent_type.clone())
                .flatten()
                .filter(|t| !t.is_empty()),
            cost: call.cost_usd.unwrap_or(0.0),
            input: call.input_tokens,
            output: call.output_tokens,
            cache_read: call.cache_read_tokens,
            cache_creation: call.cache_creation_tokens,
            cli_secs: if is_main && turn_over { call.total_ms as f64 / 1000.0 } else { 0.0 },
            user_secs,
            new_session: is_new_session && !counted,
            resumed: resumed || continued,
            continued,
            usage: !failed && !aborted,
        });

        // 主线程请求到了：新一轮的 prompt id 已经写进会话，把扣住的侧查询补发出去。
        if is_main {
            Self::replay_deferred(st, &key);
        }
    }

    /// 拼这条调用的事件链：只读 [`TurnFacts`]，不碰会话状态。
    fn build_events(
        call: &ApiCall,
        shape: &RequestShape,
        f: &TurnFacts,
        file_sizes: &mut HashMap<String, usize>,
    ) -> BuiltEvents {
        let version = &f.version;
        let continued_from = &f.continued_from;
        let cleared_from = &f.cleared_from;
        let cleared_prev = &f.cleared_prev;
        let identity = &f.identity;
        let base_identity = &f.base_identity;
        let kind = f.kind;
        let has_1m = f.has_1m;
        let display_model = &f.display_model;
        let resp_model = &f.resp_model;
        let is_new_session = f.is_new_session;
        let resumed = f.resumed;
        let git_outcome = f.git_outcome;
        let turn_over = f.turn_over;
        let failed = f.failed;
        let aborted = f.aborted;
        let emit_first_turn = f.emit_first_turn;
        let is_main = f.is_main;
        let new_prompt = f.new_prompt;
        let turn_skill = &f.turn_skill;
        let post_compaction = f.post_compaction;
        let turn_origin = &f.turn_origin;
        let prev_turn = &f.prev_turn;
        let rejected_calls = &f.rejected_calls;
        let user_turn = f.user_turn;
        let notification_base = f.notification_base;
        let notifications = &f.notifications;
        let prev_main_end = f.prev_main_end;
        let prompt_id = &f.prompt_id;
        let agent = &f.agent;
        let previous_request_id = &f.previous_request_id;
        let prev_main_message_id = &f.prev_main_message_id;
        let prev_main_request_id = &f.prev_main_request_id;
        let prev_main_depth = f.prev_main_depth;
        let prev_end = f.prev_end;
        let time_since_last = f.time_since_last;
        let message_tokens = f.message_tokens;
        let default_model = &f.default_model;
        let model_changed = f.model_changed;
        let previous_message_id = &f.previous_message_id;
        let tools_changed = f.tools_changed;
        let started_wall = f.started_wall;
        let main_chain = &f.main_chain;
        let chain_id = &f.chain_id;
        let query_depth = f.query_depth;
        let turn_started = f.turn_started;
        let first_text_in_turn = f.first_text_in_turn;
        let first_text_interrupted = f.first_text_interrupted;
        let tool_calls_before = f.tool_calls_before;
        let shell_snapshot_first = f.shell_snapshot_first;
        let prompt_index = f.prompt_index;
        let prompt_seq = f.prompt_seq;
        let v270 = f.v270;
        let v277 = f.v277;
        let v280 = f.v280;
        let v285 = f.v285;
        let injected = f.injected;
        let first_prompt_tpl = f.first_prompt_tpl;
        let background = &f.background;
        let first_main = f.first_main;
        let snapshot = &f.snapshot;
        let sleepy = f.sleepy;
        let tether = &f.tether;
        let usage_before = f.usage_before;
        let ignored_suggestion = &f.ignored_suggestion;
        let interrupted_message_id = &f.interrupted_message_id;
        let handback = &f.handback;
        let agent_done = &f.agent_done;
        let ctx_model = &f.ctx_model;
        let dd_model = &f.dd_model;
        let sess_turn_tools = f.sess_turn_tools;
        let sess_turn_api_calls = f.sess_turn_api_calls;
        let sess_turn_api_ms = f.sess_turn_api_ms;
        let session_cwd = &f.session_cwd;
        let betas_full = &f.betas_full;
        let betas_own = &f.betas_own;
        let thread_step = f.thread_step;
        let betas_session = &f.betas_session;

        // ---- 事件链 ----
        let t0: DateTime<Utc> = call.started_at.into();
        let ms = |dt: DateTime<Utc>, d: i64| dt + chrono::Duration::milliseconds(d);
        let ttft = call.ttft_ms.unwrap_or(call.total_ms.min(1_500)) as i64;
        let total = call.total_ms as i64;
        let t_first = ms(t0, ttft);
        let t_end = ms(t0, total);
        let uptime = |dt: DateTime<Utc>| -> f64 {
            let start: DateTime<Utc> = started_wall.into();
            ((dt - start).num_milliseconds().max(0) as f64) / 1000.0
        };
        let ctx = |dt: DateTime<Utc>| EventCtx {
            model: ctx_model,
            betas: betas_session,
            prompt_id,
            uptime_secs: uptime(dt),
        };
        let build_age_mins = {
            let bt = DateTime::parse_from_rfc3339(identity.build_time())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or(t0);
            (t0 - bt).num_minutes().max(0)
        };
        let query_source_owned = match kind {
            Kind::Subagent => agent_query_source(call.agent.agent_type.as_deref()),
            // `-p` 打印模式的主线程（billing header `cc_entrypoint=sdk-cli`）报 `sdk`
            // （`cap/auto-2.1.285-20260930/00441` 等十条）。
            Kind::Main if shape.sdk => "sdk".to_string(),
            _ => kind.query_source().to_string(),
        };
        let query_source: &str = &query_source_owned;
        let builtin_agent = call.agent.agent_type.as_deref().filter(|t| *t != "custom");
        let cache_ttl = if shape.cache_ttl_1h { "1h" } else { "5m" };
        let effort = shape.effort.clone();
        let effort_value = effort.clone().unwrap_or_else(|| "high".to_string());
        // 2.1.260 起 `tengu_api_success` 多了 `systemPromptSource`。
        let modern = version_at_least(version, "2.1.260");
        // 链路字段的写法：有链的带 chain/depth，没链的一律不带。
        let chain_fields = |obj: &mut Map<String, Value>| {
            if kind.has_chain() {
                obj.insert("queryChainId".into(), json!(&chain_id));
                obj.insert("queryDepth".into(), json!(query_depth));
            }
        };

        let mut events: Vec<(DateTime<Utc>, Value)> = Vec::with_capacity(16);
        let mut dd: Vec<Value> = Vec::with_capacity(6);
        // 这条支线请求里挂在**主线程**身份上的事件（子代理首步前的 `agent_tool_selected`），
        // 最后与模板事件一起并进 `events`。
        let mut main_side: Vec<(DateTime<Utc>, Value)> = Vec::new();
        let mut push = |dt: DateTime<Utc>, name: &str, extra: Value| {
            events.push((dt, identity.event(name, dt, &ctx(dt), extra)));
        };
        let feature = |name: &str| json!({ "feature_name": name });
        // 一轮结束（`end_turn`）才有 stop hook 与 turn_end；`tool_use` 是同一轮的中间步。
        //
        // `error_kind` 只有 `terminal_reason: "api_error"` 那种才带（官方是
        // `error_kind: Te(reason==="api_error" ? errorKind ?? "unknown" : undefined)`，
        // 其余情形整个键不出现）。
        let turn_end = |terminal: &str, duration: i64, error_kind: Option<&str>| {
            let mut o = Map::new();
            o.insert("terminal_reason".into(), json!(terminal));
            if let Some(k) = error_kind {
                o.insert("error_kind".into(), json!(k));
            }
            o.insert("is_error".into(), json!(error_kind.is_some()));
            o.insert("is_subagent".into(), json!(kind.is_agent()));
            o.insert("goal_active".into(), json!(false));
            o.insert("duration_ms".into(), json!(duration));
            o.insert("query_source".into(), json!(query_source));
            o.insert("query_source_category".into(), json!(kind.category()));
            Value::Object(o)
        };

        // 静态模板那几串攒在这里，最后再并进 `events`/`dd`（`push` 闭包还借着它们）。
        let mut tpl_events: Vec<(DateTime<Utc>, Value)> = Vec::new();
        let mut tpl_dd: Vec<Value> = Vec::new();
        let setting = model_setting(display_model);
        let subst = Subst {
            version,
            model: display_model,
            model_setting: &setting,
            permission_mode: shape.permission_mode,
            resumed,
            prompt_index: prompt_seq,
            deferred: shape.deferred_tools > 0,
            tool_search: tool_search_decision(shape, display_model, kind.is_agent()),
            sdk: shape.sdk,
        };
        let mut take_tpl = |tpl: &[TplEvent], anchor: DateTime<Utc>| {
            let (ev, d) = emit_template(tpl, anchor, base_identity, ctx, dd_model, &subst);
            tpl_events.extend(ev);
            tpl_dd.extend(d);
        };
        // 新会话：进程启动那串（120 多条），锚在首条 api_query 前 1.3s 起。
        let tpl = template_for(version, shape.sdk);
        // 新进程 `--continue` 接上旧会话：启动那串报在进程启动时那个临时会话 id 上，随后在这个
        // 会话上报接续那三条（`cap/auto-2.1.285-20260930` 08:03:54.947，比启动探测早 250ms 上下）。
        let mut probe_side: Option<(Identity, TplOutput)> = None;
        if let Some((probe_sid, at)) = &continued_from {
            let at: DateTime<Utc> = (*at).into();
            let t_res = ms(at, -257).min(ms(t0, -1_000));
            let probe_identity = Identity {
                session_id: probe_sid.clone(),
                parent_session_id: None,
                ..base_identity.clone()
            };
            let (ev, d) =
                emit_template(&tpl.startup, t_res, &probe_identity, ctx, dd_model, &subst);
            probe_side = Some((probe_identity, (ev, d)));
            push(
                t_res,
                "tengu_session_start",
                json!({
                    "previous_session_id": probe_sid,
                    "source": "resume",
                    "permissionMode": shape.permission_mode,
                    "dangerouslySkipPermissionsPassed": false,
                    "modeIsBypass": false,
                    "print": false
                }),
            );
            push(
                t_res,
                "tengu_resume_model_restore",
                json!({ "outcome": "restored", "is_eap": false }),
            );
            push(t_res, "tengu_continue", json!({ "success": true, "resume_duration_ms": 49 }));
        } else if let Some(prev) = &cleared_from {
            // 同一进程里 `/clear`：没有启动那一串，只有清空那几条（08:02:27.765–.771，第一句输入前
            // 5 秒上下），之后这个会话的每条事件都带 `parent_session_id`。
            let (prev_req, prev_end) =
                cleared_prev.clone().map(|(r, e, _, _)| (r, e)).unwrap_or_default();
            let floor: DateTime<Utc> = prev_end.map_or(ms(t0, -5_400), |e| {
                DateTime::<Utc>::from(e) + chrono::Duration::milliseconds(500)
            });
            let tc = ms(t0, -5_400).max(floor).min(ms(t0, -50));
            push(
                tc,
                "tengu_cache_eviction_hint",
                json!({ "scope": "conversation_clear", "last_request_id": prev_req.as_deref().unwrap_or("") }),
            );
            push(tc, "tengu_shell_set_cwd", json!({ "success": true }));
            push(
                ms(tc, 1),
                "tengu_session_start",
                json!({
                    "previous_session_id": prev,
                    "source": "clear",
                    "permissionMode": shape.permission_mode,
                    "dangerouslySkipPermissionsPassed": false,
                    "modeIsBypass": false,
                    "print": false
                }),
            );
            for (off, name) in [(3, "cmd_clear"), (6, "cmd_dispatch")] {
                push(ms(tc, off), "tengu_feature_ok", feature(name));
                dd.push(identity.dd_entry(
                    "tengu_feature_ok",
                    &ctx(ms(tc, off)),
                    dd_model,
                    feature(name),
                ));
            }
            push(
                ms(tc, 6),
                "tengu_input_command",
                json!({ "input": "clear", "invocation_trigger": "user-slash" }),
            );
        } else if is_new_session {
            take_tpl(&tpl.startup, t0);
        }
        if !background.is_empty() {
            take_tpl(&tpl.background[background.clone()], started_wall.into());
        }
        // 上一轮最后一条回复里的工具在权限弹框上被拒（`cap/auto-2.1.285-20260930` 08:00:57.305 弹框、
        // 08:01:01.376 按 Esc）：弹框、拒绝那几条、turn、`turn_end{aborted_tools}`，没有 stop hook；
        // 客户端不再发请求，直到用户敲下一句——代理只能在这条新输入里看出来，补在它前面。
        if !rejected_calls.is_empty() {
            let prev_end_dt: DateTime<Utc> =
                prev_end.unwrap_or(call.started_at - Duration::from_secs(6)).into();
            let shown = ms(prev_end_dt, 8);
            let wait = ((t0 - shown).num_milliseconds() / 2).clamp(300, 4_000);
            let t_rej = ms(shown, wait);
            let prev_req = prev_main_request_id.clone().unwrap_or_default();
            let prev_msg = prev_main_message_id.clone().unwrap_or_default();
            for (c, _) in rejected_calls {
                let bash = c.name == "Bash";
                push(
                    shown,
                    "tengu_tool_use_show_permission_request",
                    json!({
                        "messageID": &prev_msg,
                        "toolName": &c.name,
                        "isMcp": false,
                        "sandboxEnabled": false,
                        "permissionMode": shape.permission_mode,
                        "originAgentType": "main"
                    }),
                );
                push(t_rej, "tengu_feature_ok", feature("permission_user_deny"));
                dd.push(identity.dd_entry(
                    "tengu_feature_ok",
                    &ctx(t_rej),
                    dd_model,
                    feature("permission_user_deny"),
                ));
                push(t_rej, "tengu_permission_request_escape", json!({}));
                push(
                    t_rej,
                    "tengu_tool_use_can_use_tool_rejected",
                    json!({
                        "messageID": &prev_msg,
                        "toolName": &c.name,
                        "deniedBy": "user_reject",
                        "decisionReasonType": "unknown",
                        "queryChainId": &prev_turn.0,
                        "queryDepth": prev_main_depth,
                        "requestId": &prev_req
                    }),
                );
                let mut rejected = json!({
                    "messageID": &prev_msg,
                    "isMcp": false,
                    "toolName": &c.name,
                    "sandboxEnabled": false,
                    "waiting_for_user_permission_ms": wait
                });
                if bash {
                    rejected["destructive_category"] = json!("none");
                    rejected["destructive_target_scope"] = json!("none");
                    rejected["git_destructive_target"] = json!("none");
                    rejected["permission_mode"] = json!(shape.permission_mode);
                }
                rejected["hasFeedback"] = json!(false);
                push(t_rej, "tengu_tool_use_rejected_in_prompt", rejected.clone());
                dd.push(identity.dd_entry(
                    "tengu_tool_use_rejected_in_prompt",
                    &ctx(t_rej),
                    dd_model,
                    snake_flat(&rejected),
                ));
            }
            let t2 = ms(t_rej, 2);
            push(t2, "tengu_feature_ok", feature("turn"));
            dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t2), dd_model, feature("turn")));
            let started: DateTime<Utc> = prev_turn.2.unwrap_or(call.started_at).into();
            push(
                t2,
                "tengu_turn_end",
                turn_end("aborted_tools", (t2 - started).num_milliseconds().max(0), None),
            );
        }
        // 上一轮收尾时送到、夹进这次输入的后台任务通知：送达一条、输入一条（`turn_origin:
        // task-notification`），各占一个输入号。
        if !notifications.is_empty() {
            let base: DateTime<Utc> = prev_main_end.map_or(ms(t0, -2_000), DateTime::<Utc>::from);
            for (i, len) in notifications.iter().enumerate() {
                let tn = ms(base, 17 + i as i64).min(ms(t0, -60));
                let queued = json!({
                    "feature_name": "queued_message_delivered",
                    "delivery": "turn_end",
                    "command_count": 1,
                    "prompt_count": 0,
                    "relay_count": 0,
                    "artifact_comment_count": 0,
                    "wait_ms": 3_655
                });
                push(tn, "tengu_feature_ok", queued.clone());
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(tn), dd_model, queued));
                let mut input = json!({
                    "is_negative": false,
                    "is_keep_going": false,
                    "is_wakeup": false,
                    "prompt_index": notification_base + 1 + i as u32,
                    "prompt_length": len,
                    "prompt_source": "system"
                });
                if let Some(e) = &effort {
                    input["effort_level"] = json!(e);
                }
                input["turn_origin"] = json!("task-notification");
                push(tn, "tengu_input_prompt", input);
            }
        }
        if new_prompt {
            if injected {
                let mut queued = json!({
                    "feature_name": "queued_message_delivered",
                    "delivery": "turn_end",
                    "command_count": 1,
                    "prompt_count": u32::from(turn_origin == "peer")
                });
                if v285 {
                    queued["relay_count"] = json!(0);
                    queued["artifact_comment_count"] = json!(0);
                }
                queued["wait_ms"] = json!(6);
                push(ms(t0, -12), "tengu_feature_ok", queued.clone());
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(ms(t0, -12)), dd_model, queued));
                // 模板那段不套，工具搜索判定照有（`cap/2.1.285` 06:57:29.600）。
                push(
                    ms(t0, -1),
                    "tengu_tool_search_mode_decision",
                    tool_search_decision(shape, display_model, kind.is_agent()),
                );
            } else if first_prompt_tpl {
                take_tpl(&tpl.prompt, t0);
                take_tpl(&tpl.first_prompt, t0);
            } else {
                take_tpl(&tpl.prompt_next, t0);
            }
            // 用户敲的报 `typed`，同伴会话发来的（peer）、后台任务完成的通知
            // （task-notification）这类由客户端自己注入的报 `system`（`cap/2.1.277` 1 条、
            // `cap/2.1.280` 2 条）。
            let typed = user_turn;
            let mut input = json!({
                "is_negative": false,
                "is_keep_going": false,
                // 2.1.260 那两份抓包首次输入是 true（把进程从等待里叫醒的那一次）；
                // 2.1.277 / 2.1.280 的 17 次输入全是 false，含每个会话的第一次。
                "is_wakeup": prompt_index == 1 && !v277
            });
            if !(v277 && turn_origin == "peer") {
                input["prompt_index"] = json!(prompt_index);
            }
            input["prompt_length"] = json!(shape.prompt_len);
            // `-p` 的输入报 `sdk`（`cap/auto-2.1.285-20260930` 十条）。
            input["prompt_source"] = json!(if shape.sdk {
                "sdk"
            } else if typed {
                "typed"
            } else {
                "system"
            });
            // 2.1.277 起没有 effort 的模型（haiku）整个键不出现（`cap/2.1.277`、`cap/2.1.280`
            // 各两条），之前的版本照旧报默认的 high。
            if !v277 || effort.is_some() {
                input["effort_level"] = json!(&effort_value);
            }
            if v277 {
                input["turn_origin"] = json!(&turn_origin);
            }
            if let Some(id) = &interrupted_message_id {
                input["interrupted_message_id"] = json!(id);
            }
            // 2.1.285 首次输入那条落在首条 api_query 前 35ms，排在模板里的附件、上下文宣告与
            // `artifact_*` 之前（`cap/2.1.285/00040` 06:34:16.504 / .539）；之后的输入是 -13ms 上下。
            //
            // `-p` 的首次输入跟着 SDK 模板走：启动那串一路排到 API 前 3.8s 上下（附件、记忆、上下文
            // 宣告都在模板里），输入落在上下文宣告前 8ms、`sleepy_snowflake_applied` 在它后 4ms
            // （`cap/auto-2.1.285-20260930/00448`：-3807 / -3799 / -3795，与模板同一批）。按 -35
            // 报会排到宣告之后 3.7s，顺序整个反了。
            let sdk_announce = (v285 && shape.sdk && first_prompt_tpl)
                .then(|| tpl.startup.iter().find(|e| e.name == "tengu_context_announcement"))
                .flatten()
                .map(|e| e.off);
            let input_at = match sdk_announce {
                Some(off) => off - 8,
                None if v285 && first_prompt_tpl => -35,
                None => -15,
            };
            if let Some((rid, shown_end, chars)) = &ignored_suggestion {
                let submit = ms(t0, if shape.bash_input { -57 } else { input_at - 3 });
                push(
                    submit,
                    "tengu_prompt_suggestion",
                    ignored_suggestion_meta(rid, *shown_end, *chars, submit, shape.prompt_len),
                );
            }
            if shape.bash_input {
                push(
                    ms(t0, -56),
                    "tengu_input_bash",
                    json!({ "powershell": false, "respond": true }),
                );
                // 那条命令当场在本机跑完（`07:58:39.375`，比输入晚 50ms）。
                if let Some((cmd, out)) = &shape.bash_typed {
                    let meta = bash_executed_meta(
                        &bash_profile(cmd),
                        *out + 1,
                        None,
                        false,
                        true,
                        shape.permission_mode,
                    );
                    push(ms(t0, -6), "tengu_bash_tool_command_executed", meta.clone());
                    dd.push(identity.dd_entry(
                        "tengu_bash_tool_command_executed",
                        &ctx(ms(t0, -6)),
                        dd_model,
                        snake_flat(&meta),
                    ));
                }
            } else {
                push(ms(t0, input_at), "tengu_input_prompt", input);
            }
            // 每个模型头一次用于新输入时报一次（值恒为 growthbook / all）。
            if sleepy {
                push(
                    ms(t0, sdk_announce.map_or(-10, |off| off + 4)),
                    "tengu_sleepy_snowflake_applied",
                    json!({ "model": &display_model, "source": "growthbook", "value": "all" }),
                );
            }
            // auto 模式下每次输入先探一次工作区的 git 状态（`cap/2.1.280` 前三次输入是 auto，
            // 各一条；之后切回 default 就没有了），续轮等其余请求见下面。代理看不见客户端的
            // 工作目录，结果照抓包报。
            if v280 && shape.permission_mode == "auto" {
                push(
                    ms(t0, -10),
                    "tengu_auto_mode_git_state_probe",
                    json!({
                        "duration_ms": u32::from(!prompt_index.is_multiple_of(3)),
                        "wait_ms": 0,
                        "outcome": git_outcome,
                        "truncated": false
                    }),
                );
            }
        }

        // 续轮：上一条回复里的工具调用在两次请求之间执行，把权限判定、执行、附件计算那串
        // 补在这条请求之前（`cap/2.1.260-2` 09:43:12–09:43:13）。权限判定发生在上一条回复
        // 流到工具块时，时间戳落在上一条结束之前。
        // 子代理每一步之间也是这一串（`cap/2.1.280` Explore：Bash 的权限判定、执行、成功，
        // 再攒附件），深度与消息 id 取它自己那条支线的上一条。
        let is_sub = kind == Kind::Subagent;
        if (is_main || is_sub) && !new_prompt && !shape.tool_uses.is_empty() {
            let prev_end_dt: DateTime<Utc> =
                prev_end.unwrap_or(call.started_at - Duration::from_secs(1)).into();
            // 工具是上一条主线程回复产生的：事件里的 requestId / messageID 都指上一条。
            let prev_req = previous_request_id.clone().unwrap_or_default();
            let prev_msg = if is_sub {
                agent.as_ref().and_then(|a| a.last_message_id.clone()).unwrap_or_default()
            } else {
                prev_main_message_id.clone().unwrap_or_default()
            };
            let prev_depth = if is_sub { query_depth.saturating_sub(1) } else { prev_main_depth };
            let n = shape.tool_uses.len() as i64;
            for (i, tu) in shape.tool_uses.iter().enumerate() {
                let i = i as i64;
                // 权限判定。配置里放行的工具（default 模式下也是，`cap/2.1.280` 的 Agent、
                // `cap/2.1.285` 的 Agent / WebFetch / Read）报 `granted_in_config` →
                // `permission_auto_approve_config` → `can_use_tool_allowed`；键序是
                // `messageID, isMcp, toolName, sandboxEnabled`，只有 auto 模式的 Bash 多
                // `destructive_*` 与 `permission_mode`。非 auto 模式的 Skill 要用户点头：弹框
                // （`show_permission_request`，回复一结束就弹）→ 本次放行
                // （`granted_in_prompt_temporary` + `permission_user_grant`）→ `can_use_tool_allowed`
                // （`cap/2.1.285` 06:55:27.663 / 30.944 / 30.946；2.1.280 那次用户拒了）。
                // 此前只在 auto 模式下报，default 模式一条权限事件都没有。
                let tg = ms(prev_end_dt, -6 - 3 * (n - i));
                let command = tu.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
                let profile = (tu.name == "Bash").then(|| bash_profile(command));
                let read_only =
                    profile.as_ref().is_some_and(|p| bash_read_only(p, session_cwd.as_deref()));
                // auto 模式：服务端分类器给每个工具调用的判决（响应里的 `safeguard_results`），客户端在
                // 放行前报一条 `tengu_auto_mode_decision`（`cap/auto-2.1.285-20260930` 23 条）。要用户
                // 亲自回答的 AskUserQuestion 不报。
                if shape.permission_mode == "auto"
                    && tu.name != "AskUserQuestion"
                    && let Some(verdict) = tu.verdict.as_deref()
                {
                    let td = ms(tg, -1);
                    let decision = auto_mode_decision_meta(
                        tu,
                        verdict,
                        profile.as_ref(),
                        &prev_msg,
                        i as usize,
                        usage_before,
                        tu.id
                            .bytes()
                            .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b))),
                        session_cwd.as_deref(),
                    );
                    push(td, "tengu_auto_mode_decision", decision.clone());
                    dd.push(identity.dd_entry(
                        "tengu_auto_mode_decision",
                        &ctx(td),
                        dd_model,
                        snake_flat(&decision),
                    ));
                }
                // default 模式下要用户点头的：Edit / Write / NotebookEdit、非只读的 Bash（`cap/auto-2.1.285-20260930`
                // 08:00:45.839 的 Edit、08:02:04.556 的 Bash），以及非 auto 模式的 Skill。只读的 Bash
                // （`ls`、`git status`……）照配置放行。
                let prompted = (tu.name == "Skill" && shape.permission_mode != "auto")
                    || (shape.permission_mode == "default"
                        && (matches!(tu.name.as_str(), "Edit" | "Write" | "NotebookEdit")
                            || (tu.name == "Bash" && !read_only)))
                    // 要用户亲自作答的两种，任何模式都弹（`07:55:36.016` AskUserQuestion、规划模式下
                    // `07:56:18.339` 的 ExitPlanMode）。
                    || tu.name == "AskUserQuestion"
                    || tu.name == "ExitPlanMode";
                if prompted {
                    let shown = ms(prev_end_dt, -5);
                    let mut show = json!({
                        "messageID": &prev_msg,
                        "toolName": &tu.name,
                        "isMcp": false
                    });
                    // 复合命令逐条判过（`subcommandResults`），子代理里的弹框另标来源
                    // （`07:58:16.980`、`08:02:04.556`）。
                    if profile.as_ref().is_some_and(|p| p.simple_commands > 1) {
                        show["decisionReasonType"] = json!("subcommandResults");
                    }
                    show["sandboxEnabled"] = json!(false);
                    // ExitPlanMode 的工具事件跟着下一条请求补，那时已经退出规划模式了；弹框那一刻
                    // 仍是 `plan`。
                    let shown_mode =
                        if tu.name == "ExitPlanMode" { "plan" } else { shape.permission_mode };
                    show["permissionMode"] = json!(shown_mode);
                    if is_sub {
                        show["requestSource"] = json!("subagent");
                    }
                    show["originAgentType"] = json!(if is_sub { "subagent" } else { "main" });
                    push(shown, "tengu_tool_use_show_permission_request", show);
                    // 用户点头落在这条请求发出前（两次请求之间就是在等他）。
                    let tp = ms(t0, -40).max(ms(shown, 1));
                    // 点「Yes」：确认提交，Bash 另报选了第几项。Skill、AskUserQuestion、ExitPlanMode
                    // 没有这一步。弹框挂在子代理上时，这两条是主线程界面上的操作，不带子代理身份
                    // （`07:58:17.850`）。
                    if matches!(tu.name.as_str(), "Edit" | "Write" | "NotebookEdit" | "Bash") {
                        let ta = ms(tp, -3);
                        let accept = json!({
                            "toolName": &tu.name,
                            "isMcp": false,
                            "has_instructions": false,
                            "instructions_length": 0,
                            "entered_feedback_mode": false
                        });
                        let mut ui = vec![("tengu_accept_submitted", accept)];
                        if tu.name == "Bash" {
                            ui.push((
                                "tengu_permission_request_option_selected",
                                json!({ "option_index": 1 }),
                            ));
                        }
                        for (name, meta) in ui {
                            if is_sub {
                                main_side.push((ta, base_identity.event(name, ta, &ctx(ta), meta)));
                            } else {
                                push(ta, name, meta);
                            }
                        }
                    }
                    let mut grant = json!({
                        "messageID": &prev_msg,
                        "isMcp": false,
                        "toolName": &tu.name,
                        "sandboxEnabled": false,
                        "waiting_for_user_permission_ms": (tp - shown).num_milliseconds()
                    });
                    if tu.name == "Bash" {
                        grant["destructive_category"] = json!("none");
                        grant["destructive_target_scope"] = json!("none");
                        grant["git_destructive_target"] = json!("none");
                        grant["permission_mode"] = json!(shape.permission_mode);
                    }
                    push(tp, "tengu_tool_use_granted_in_prompt_temporary", grant.clone());
                    if v285 {
                        dd.push(identity.dd_entry(
                            "tengu_tool_use_granted_in_prompt_temporary",
                            &ctx(tp),
                            dd_model,
                            snake_flat(&grant),
                        ));
                    }
                    push(tp, "tengu_feature_ok", feature("permission_user_grant"));
                    dd.push(identity.dd_entry(
                        "tengu_feature_ok",
                        &ctx(tp),
                        dd_model,
                        feature("permission_user_grant"),
                    ));
                    push(
                        ms(tp, 2),
                        "tengu_tool_use_can_use_tool_allowed",
                        json!({
                            "messageID": &prev_msg,
                            "toolName": &tu.name,
                            "queryChainId": &chain_id,
                            "queryDepth": prev_depth,
                            "requestId": &prev_req
                        }),
                    );
                } else {
                    let mut granted = json!({
                        "messageID": &prev_msg,
                        "isMcp": false,
                        "toolName": &tu.name,
                        "sandboxEnabled": false
                    });
                    // Bash 放行时带破坏性判定与权限模式，default 模式下也是（08:00:42.542）。
                    if tu.name == "Bash" {
                        granted["destructive_category"] = json!("none");
                        granted["destructive_target_scope"] = json!("none");
                        granted["git_destructive_target"] = json!("none");
                        granted["permission_mode"] = json!(shape.permission_mode);
                    }
                    push(tg, "tengu_tool_use_granted_in_config", granted);
                    push(ms(tg, 1), "tengu_feature_ok", feature("permission_auto_approve_config"));
                    dd.push(identity.dd_entry(
                        "tengu_feature_ok",
                        &ctx(ms(tg, 1)),
                        dd_model,
                        feature("permission_auto_approve_config"),
                    ));
                    push(
                        ms(tg, 2),
                        "tengu_tool_use_can_use_tool_allowed",
                        json!({
                            "messageID": &prev_msg,
                            "toolName": &tu.name,
                            "queryChainId": &chain_id,
                            "queryDepth": prev_depth,
                            "requestId": &prev_req
                        }),
                    );
                }
                // 工具执行：按调用数把上一条结束到这条发出之间的时间均分。
                let gap = (t0 - prev_end_dt).num_milliseconds().max(50);
                // 2.1.285：工具跑完就发下一条，完成事件贴着这条请求之前（WebFetch 要先等它那条
                // 页面处理回来，`cap/2.1.285` 06:56:20.385 完成、.389 起下一条）；之前的版本按
                // 调用数均分。
                let t_done = if v285 {
                    ms(t0, -8 - 4 * (n - 1 - i)).max(ms(prev_end_dt, i + 1))
                } else {
                    ms(prev_end_dt, gap * (i + 1) / (n + 1))
                };
                let duration = (gap / (n + 1) - 12).max(1);
                let bash_failed = tu.name == "Bash" && tu.is_error;
                if tu.name == "Bash" {
                    if shell_snapshot_first && is_main && i == 0 {
                        push(ms(t_done, -40), "tengu_feature_ok", feature("shell_snapshot_create"));
                        dd.push(identity.dd_entry(
                            "tengu_feature_ok",
                            &ctx(ms(t_done, -40)),
                            dd_model,
                            feature("shell_snapshot_create"),
                        ));
                    }
                    let command = tu.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    let backgrounded =
                        tu.input.get("run_in_background").and_then(|b| b.as_bool()) == Some(true);
                    // 官方量的是命令的原始输出，末尾那个换行在 tool_result 里被去掉了（10 对 9、258 对
                    // 257……）；转到后台的那种当场只有一个换行（`00146`）。
                    let stdout = if backgrounded { 1 } else { tu.result_len + 1 };
                    let mut bash = bash_executed_meta(
                        &bash_profile(command),
                        stdout,
                        Some(&tu.id),
                        backgrounded,
                        false,
                        shape.permission_mode,
                    );
                    if bash_failed {
                        // 非零退出（`cap/auto-2.1.285-20260930` 07:58:12.123）：同一份画像报
                        // `command_failed`，退出码取结果开头的 `Exit code N`，没有 `was_backgrounded`；
                        // 工具那头报 `feature_sad{tool_shell_error}` 与 `tool_use_error`，不报成功。
                        if let Some(o) = bash.as_object_mut() {
                            o.shift_remove("was_backgrounded");
                            o.insert("exit_code".into(), json!(exit_code_of(&tu.result_head)));
                        }
                        push(t_done, "tengu_bash_tool_command_failed", bash.clone());
                        dd.push(identity.dd_entry(
                            "tengu_bash_tool_command_failed",
                            &ctx(t_done),
                            dd_model,
                            snake_flat(&bash),
                        ));
                        let sad = json!({ "feature_name": "tool_bash", "error_code": "tool_shell_error" });
                        push(t_done, "tengu_feature_sad", sad.clone());
                        dd.push(identity.dd_entry(
                            "tengu_feature_sad",
                            &ctx(t_done),
                            dd_model,
                            sad,
                        ));
                    } else {
                        push(t_done, "tengu_bash_tool_command_executed", bash.clone());
                        dd.push(identity.dd_entry(
                            "tengu_bash_tool_command_executed",
                            &ctx(t_done),
                            dd_model,
                            snake_flat(&bash),
                        ));
                        push(t_done, "tengu_feature_ok", feature("tool_bash"));
                        dd.push(identity.dd_entry(
                            "tengu_feature_ok",
                            &ctx(t_done),
                            dd_model,
                            feature("tool_bash"),
                        ));
                    }
                }
                // 每个工具跑完都有一条 `tool_<工具名>`，紧挨在 `tool_use_success` 前（`cap/2.1.277`
                // `tool_read` / `tool_skill`、`cap/2.1.280` `tool_agent` / `tool_subagent_handback`、
                // `cap/2.1.285` `tool_web_fetch`，条数与 `tool_use_success` 逐个相等）；Bash 那条
                // 在上面跟着命令执行报了。Agent 前另有 `subagent_launch`、Skill 前另有
                // `skill_invoke`（`cap/2.1.285` 06:56:08.489、06:55:30.993）。
                // 结果大到被落盘：先报一条落盘（`cap/2.1.285` 06:56:24.411：原始 60278 字节、
                // 预览 2267 字节、阈值 50000），`tool_use_success` 报的仍是原始大小。
                if v285 && let Some(orig) = tu.persisted_from {
                    push(
                        ms(t_done, -1),
                        "tengu_tool_result_persisted",
                        json!({
                            "toolName": &tu.name,
                            "originalSizeBytes": orig,
                            "persistedSizeBytes": tu.result_len,
                            "estimatedOriginalTokens": (orig + 2) / 4,
                            "estimatedPersistedTokens": (tu.result_len + 2) / 4,
                            "thresholdUsed": 50_000,
                            "truncatedAtCap": false
                        }),
                    );
                }
                if tu.name != "Bash" {
                    let before = match tu.name.as_str() {
                        "Agent" | "Task" => Some("subagent_launch"),
                        "Skill" => Some("skill_invoke"),
                        _ => None,
                    };
                    for name in before.into_iter().chain([tool_feature_name(&tu.name).as_str()]) {
                        push(t_done, "tengu_feature_ok", feature(name));
                        dd.push(identity.dd_entry(
                            "tengu_feature_ok",
                            &ctx(t_done),
                            dd_model,
                            feature(name),
                        ));
                    }
                }
                let mut success = json!({
                    "messageID": &prev_msg,
                    "toolName": &tu.name,
                    "isMcp": false
                });
                // 子代理里跑的工具多报是哪类子代理（`cap/2.1.280`：`subagent_type: Explore`、
                // `is_built_in_agent: true`，紧跟 `isMcp`）。
                if is_sub {
                    success["subagent_type"] =
                        json!(call.agent.agent_type.as_deref().unwrap_or("general-purpose"));
                    success["is_built_in_agent"] = json!(builtin_agent.is_some());
                }
                let mut rest = json!({
                    "effort_level": &effort_value,
                    "durationMs": duration,
                    "rssDeltaBytes": 1_081_344,
                    "heapUsedDeltaBytes": 3_887_104,
                    "externalDeltaBytes": 2_870_395,
                    "preToolHookDurationMs": 0,
                    "permissionDurationMs": 11,
                    "toolResultSizeBytes": tu.persisted_from.unwrap_or(tu.result_len),
                    "toolInputSizeBytes": if tu.name == "ExitPlanMode" && tu.input_len <= 2 {
                        file_sizes.get(PLAN_INPUT_KEY).copied().unwrap_or(tu.input_len)
                    } else {
                        tu.input_len
                    }
                });
                // 没有 effort 的模型（haiku 子代理，`cap/2.1.285` 九条）不报这一项。
                if v285
                    && effort.is_none()
                    && let Some(o) = rest.as_object_mut()
                {
                    o.shift_remove("effort_level");
                }
                // 2.1.285：结果的 token 估算（字节数 / 4 四舍五入，11 条里 10 条相等）、媒体块数、
                // 会不会被落盘（超过 50000 字节的那条 60278 为 true，41469 为 false）。
                if v285 {
                    let size = tu.persisted_from.unwrap_or(tu.result_len) as u64;
                    insert_after(
                        &mut rest,
                        "toolResultSizeBytes",
                        vec![
                            ("toolResultTokensEst", json!((size + 2) / 4)),
                            ("toolResultMediaBlocks", json!(tu.media_blocks)),
                            ("toolResultWillPersist", json!(size > 50_000)),
                        ],
                    );
                }
                if let (Some(obj), Some(rest)) = (success.as_object_mut(), rest.as_object()) {
                    obj.extend(rest.clone());
                }
                tool_success_extras(&mut success, tu, file_sizes);
                success["queryChainId"] = json!(&chain_id);
                success["queryDepth"] = json!(prev_depth);
                success["requestId"] = json!(&prev_req);
                if bash_failed {
                    let mut err = json!({
                        "messageID": &prev_msg,
                        "toolName": &tu.name,
                        "toolUseID": &tu.id,
                        "isMcp": false,
                        "toolInputSizeBytes": tu.input_len,
                        "durationMs": duration,
                        "preToolHookDurationMs": 0,
                        "permissionDurationMs": 5
                    });
                    if is_sub {
                        err["subagent_type"] =
                            json!(call.agent.agent_type.as_deref().unwrap_or("general-purpose"));
                        err["is_built_in_agent"] = json!(builtin_agent.is_some());
                    }
                    if let Some(e) = &effort {
                        err["effort_level"] = json!(e);
                    }
                    err["queryChainId"] = json!(&chain_id);
                    err["queryDepth"] = json!(prev_depth);
                    err["requestId"] = json!(&prev_req);
                    err["error"] = json!("ShellError");
                    err["error_message_hash"] = json!(&sha256_hex(tu.result_head.as_bytes())[..12]);
                    err["error_stack_hash"] = json!("739abbe6a843");
                    err["error_top_frame"] = json!("chunk-59zy4j10.js:3389:5792");
                    err["errorCode"] = json!("ShellError");
                    err["rssDeltaBytes"] = json!(1_081_344);
                    err["heapUsedDeltaBytes"] = json!(0);
                    err["externalDeltaBytes"] = json!(2_856);
                    push(t_done, "tengu_tool_use_error", err.clone());
                    dd.push(identity.dd_entry(
                        "tengu_tool_use_error",
                        &ctx(t_done),
                        dd_model,
                        snake_flat(&err),
                    ));
                } else {
                    push(t_done, "tengu_tool_use_success", success.clone());
                    dd.push(identity.dd_entry(
                        "tengu_tool_use_success",
                        &ctx(t_done),
                        dd_model,
                        snake_flat(&success),
                    ));
                }
            }
            // 工具都跑完，攒附件再发下一条。这两条的深度仍是上一条的（三个版本的抓包都是
            // 「下一条 api_query 的深度 − 1」：主线程 0→1、子代理 2→3…）。
            let ta = ms(t0, -4);
            // 2.1.285 的子代理续轮不再报附件计算耗时（五条续轮前一条都没有）。
            let labels: &[&str] =
                if v285 && is_sub { &[] } else { &["agent_pending_messages", "memory_update"] };
            for label in labels {
                push(
                    ta,
                    "tengu_attachment_compute_duration",
                    json!({ "label": label, "duration_ms": 0, "attachment_size_bytes": 0, "attachment_count": 0 }),
                );
            }
            let mut attachments = json!({ "attachment_types": ["total_tokens_reminder"] });
            if v285 {
                fill_attachment_estimates(&mut attachments, query_source, shape.sdk, display_model);
            }
            let results = shape.tool_uses.len();
            // 先 `query_before_attachments`、再攒附件、再 `query_after_attachments`（`cap/2.1.277`
            // 30 次、`cap/2.1.280` 5 次、`cap/2.1.285` 6 次都是这个顺序）。
            push(
                ms(ta, -1),
                "tengu_query_before_attachments",
                json!({
                    "messagesForQueryCount": shape.messages_len + 3,
                    "assistantMessagesCount": shape.assistant_messages,
                    "toolResultsCount": results,
                    "queryChainId": &chain_id,
                    "queryDepth": prev_depth
                }),
            );
            push(ta, "tengu_attachments", attachments);
            push(
                ms(ta, 1),
                "tengu_query_after_attachments",
                json!({
                    "totalToolResultsCount": results + 1,
                    "fileChangeAttachmentCount": 0,
                    "queryChainId": &chain_id,
                    "queryDepth": prev_depth
                }),
            );
        }

        // auto 模式下**每条**带工具的请求发出前都探一次 git 状态，不只是新输入那条
        // （`cap/2.1.280` 第二个会话：主线程续轮、猜下一句、子代理每一步与它的摘要请求前都有，
        // 紧跟在攒附件之后）。新输入那条已经在上面报过。
        if v280
            && !new_prompt
            && shape.permission_mode == "auto"
            && shape.tools_count > 0
            && kind != Kind::WebSearchTool
        {
            push(
                ms(t0, -2),
                "tengu_auto_mode_git_state_probe",
                json!({ "duration_ms": query_depth % 2, "wait_ms": 0, "outcome": git_outcome, "truncated": false }),
            );
        }
        // 续轮与侧查询在发请求前也判一次工具搜索模式（首次输入的那条在模板里）；它排在
        // 规范化那串之前（`cap/2.1.260-2` 与 `cap/2.1.280/00048` 都是）。
        if !new_prompt {
            push(
                ms(t0, -1),
                "tengu_tool_search_mode_decision",
                tool_search_decision(shape, display_model, kind.is_agent()),
            );
        }
        // 2.1.285：每条线程的头两条请求前，客户端把上下文宣告、提醒折叠、工具入参回显这几样
        // 记一次、回放一次（`cap/2.1.285`：主线程首条那串在模板里，第二条只有两条 `*_replayed`；
        // 子代理首步 06:56:08.494–.497 是整串，第二步 06:56:20.389 / .392 又是那两条回放）。
        // 回放那两条夹着工具搜索判定：折叠在前、回显在后。
        // 子代理首步之前，主线程那边先选定子代理类型、解析它的模型（`cap/2.1.285` 06:56:08.488：
        // `agent_tool_selected` 挂在主线程上，`subagent_model_resolve` 已经挂在子代理支线上）。
        if v285 && kind == Kind::Subagent && thread_step == Some(0) {
            let ts = ms(t0, -11);
            let family = shape.model.trim_start_matches("claude-").split('-').next().unwrap_or("");
            let selected = json!({
                "agent_type": call.agent.agent_type.as_deref().unwrap_or("general-purpose"),
                "model": &shape.model,
                "source": if builtin_agent.is_some() { "built-in" } else { "custom" },
                "is_built_in_agent": builtin_agent.is_some(),
                "is_resume": false,
                "is_async": true,
                "is_fork": false,
                "agent_depth": 1,
                "agent_system_prompt_chars": shape.system_chars
            });
            main_side.push((
                ts,
                base_identity.event("tengu_agent_tool_selected", ts, &ctx(ts), selected),
            ));
            let resolve = json!({
                "feature_name": "subagent_model_resolve",
                "source": "spawn",
                "precedence": "frontmatter",
                "requested_family": family,
                "resolved_family": family,
                "requested_model": family,
                "resolved_model": &shape.model
            });
            push(ts, "tengu_feature_ok", resolve.clone());
            dd.push(identity.dd_entry("tengu_feature_ok", &ctx(ts), dd_model, resolve));
        }
        // `/compact`、`/btw` 也是一次输入：挂着的建议在提交那一刻算 ignored（`07:57:01.971`）。
        if matches!(kind, Kind::Compact | Kind::SideQuestion)
            && let Some((rid, shown_end, chars)) = &ignored_suggestion
        {
            let submit = ms(t0, -11);
            // 敲的是斜杠命令本身：`/compact` 8 个字，`/btw ` 加问句。
            let typed = if kind == Kind::Compact { 8 } else { shape.prompt_len + 5 };
            push(
                submit,
                "tengu_prompt_suggestion",
                ignored_suggestion_meta(rid, *shown_end, *chars, submit, typed),
            );
        }
        // `/btw` 插问发出前同样回放一次提醒折叠与工具入参回显（`cap/auto-2.1.285-20260930`
        // 08:00:15.238 / .242）。
        if v285 && kind == Kind::SideQuestion {
            push(
                ms(t0, -5),
                "tengu_reminder_fold_replayed",
                json!({ "fold": false, "cached": false, "matched": true, "cachedSource": "payload" }),
            );
            push(
                ms(t0, -1),
                "tengu_wire_tool_input_echo_replayed",
                json!({ "echo": true, "cached": true, "matched": true, "cachedSource": "payload" }),
            );
        }
        if v285 && let Some(step) = thread_step {
            let recorded = step == 0 && kind == Kind::Subagent;
            let replayed = step <= 1 && (kind == Kind::Subagent || step == 1);
            if recorded {
                let tr = ms(t0, -5);
                push(tr, "tengu_feature_ok", feature("context_git_detect"));
                dd.push(identity.dd_entry(
                    "tengu_feature_ok",
                    &ctx(tr),
                    dd_model,
                    feature("context_git_detect"),
                ));
                for (kind_name, tokens) in [("session_context", 82), ("date", 9)] {
                    push(
                        tr,
                        "tengu_context_announcement",
                        json!({
                            "announcement_type": kind_name,
                            "token_estimate": tokens,
                            "updates_copy": false,
                            "query_source": query_source
                        }),
                    );
                }
                let t4 = ms(t0, -4);
                push(
                    t4,
                    "tengu_reminder_fold_recorded",
                    json!({ "recorded": true, "fold": false, "carriedForward": false, "inherited": false, "cached": false, "cachedSource": "payload" }),
                );
                push(
                    t4,
                    "tengu_wire_tool_input_echo_recorded",
                    json!({ "recorded": true, "echo": true, "carriedForward": false, "inherited": false, "cached": true, "cachedSource": "payload" }),
                );
                push(
                    t4,
                    "tengu_context_rendering_recorded",
                    json!({ "rendering": "announced", "carriedForward": false }),
                );
            }
            if replayed {
                push(
                    ms(t0, -3),
                    "tengu_reminder_fold_replayed",
                    json!({ "fold": false, "cached": false, "matched": true, "cachedSource": "payload" }),
                );
                push(
                    ms(t0, if new_prompt { 0 } else { -1 }),
                    "tengu_wire_tool_input_echo_replayed",
                    json!({ "echo": true, "cached": true, "matched": true, "cachedSource": "payload" }),
                );
            }
        }
        // 每条带工具的请求发出前核一次声明的工具集（主线程、猜下一句、离开摘要、子代理都有，
        // 标题那类无工具的没有）。`deferredLate` 两份抓包 55 条恒为 3、其余计数恒为 0；
        // 没有 `ToolSearch` 的形态抓包里没见过，按字段名报 0 与 `toolSearchAbsent: true`。
        // 子代理只在工具表里有 `ToolSearch` 时才核（`cap/2.1.280` Explore 七条都有；
        // `cap/2.1.285` 的 claude-code-guide 没有 `ToolSearch`，十二条请求一条都没有）。
        // `-p` 的第一条请求不核（`cap/auto-2.1.285-20260930` 十个 `-p` 进程的首条都没有）。
        if v280
            && shape.tools_count > 0
            && kind != Kind::WebSearchTool
            && !(shape.sdk && new_prompt)
            && (!kind.is_agent() || shape.has_tool_search)
        {
            push(ms(t0, -1), "tengu_declared_tool_set_held", {
                let mut held = json!({
                    "deferredLate": if shape.has_tool_search { 3 } else { 0 },
                    "redeclared": 0,
                    "fromRecord": 0,
                    "queryDepth": query_depth,
                    "toolSearchAbsent": !shape.has_tool_search
                });
                // 2.1.285 末尾多两项，11 条恒为 false / 0（`cap/2.1.285`）。
                if v285 {
                    insert_after(
                        &mut held,
                        "toolSearchAbsent",
                        vec![
                            ("noDeferredChannel", json!(false)),
                            ("unclassifiedDepartures", json!(0)),
                        ],
                    );
                }
                held
            });
        }

        // 客户端内部的消息条数比 API 那份多（harness 注入的 system-reminder 等）：
        // `cap/2.1.260-2`：8→2、12→5、16→8、18→10，即 post + 5 + 第几次输入 + 本轮已有的
        // 续轮次数；`apiSystemMessageCount` = 第几次输入 + 本轮已有的续轮次数（1、2、3、3）。
        // 「本轮已有的续轮次数」主线程就是这条自己的 depth，猜下一句则是主线程最后一条的
        // depth（它自己的 depth 是 +2 过的，不能拿来算）。侧查询没有这些，pre == post、0。
        let turn_extra = if is_main { query_depth } else { prev_main_depth } as usize;
        let (pre_count, api_system) = if kind.is_agent() {
            // 子代理：`apiSystemMessageCount` 就是体里 `role: system` 的条数，规范化前比规范化后
            // 多出 这些 + 8（首条 + 6、摘要请求 + 10）——`cap/2.1.280` Explore 七条里六条逐条相等。
            let extra = match (kind, agent.as_ref().map_or(0, |a| a.steps)) {
                (Kind::AgentSummary, _) => 10,
                (_, 0) => 6,
                _ => 8,
            };
            (shape.messages_len + shape.api_system_messages + extra, shape.api_system_messages)
        } else if kind.has_boundary() {
            (
                shape.messages_len + 5 + prompt_seq as usize + turn_extra,
                prompt_seq as usize + turn_extra,
            )
        } else {
            (shape.messages_len, 0)
        };
        push(
            ms(t0, -1),
            "tengu_api_before_normalize",
            json!({ "preNormalizedMessageCount": pre_count }),
        );
        push(
            t0,
            "tengu_api_after_normalize",
            json!({
                "postNormalizedMessageCount": shape.messages_len,
                "apiSystemMessageCount": api_system
            }),
        );
        // 2.1.258 那版 5 条以上消息会钉住分叉点（markerCount 2），2.1.260 起恒为 1；
        // 猜下一句那条不写缓存。
        let pinned = !modern && shape.messages_len > 2;
        // 2.1.285 的离开回顾钉住分叉点，断点仍是一个（`cap/2.1.285/00099`、`00160` 都是
        // `forkPointPinned: true, markerCount: 1`；2.1.280 的 `00048`、`00088` 是 false）。
        let fork_pinned = v285 && kind == Kind::AwaySummary;
        let breakpoints = json!({
            "totalMessageCount": shape.messages_len,
            "cachingEnabled": shape.has_cache_control,
            // fork 出来的查询不写缓存：猜下一句、子代理摘要与离开回顾都是 true（`cap/2.1.280`、
            // `cap/2.1.285`）。
            "skipCacheWrite": kind.is_fork(),
            "forkPointPinned": pinned || fork_pinned,
            "markerCount": if pinned { 2 } else { 1 }
        });
        // 发请求前的顺序照 `cap/2.1.280/00036`：边界、首块、边界，然后 tether 判定与回声审计，
        // 再是缓存断点和 api_query。
        if kind.has_boundary_marker() && shape.system_blocks >= 2 {
            let boundary = json!({
                "blockCount": shape.system_blocks,
                "staticBlockLength": shape.static_len,
                "dynamicBlockLength": shape.dynamic_len
            });
            push(t0, "tengu_sysprompt_boundary_found", boundary.clone());
            if shape.sys0_len > 0 {
                push(
                    t0,
                    "tengu_sysprompt_block",
                    json!({ "length": shape.sys0_len, "hash": &shape.sys0_hash }),
                );
            }
            push(t0, "tengu_sysprompt_boundary_found", boundary);
        } else if kind.is_agent() {
            // 子代理：标记缺失、首块、标记缺失，块数恒报 6（见 [`Kind::has_boundary_marker`]）。
            let missing = json!({ "promptBlockCount": 6 });
            push(t0, "tengu_sysprompt_missing_boundary_marker", missing.clone());
            if shape.sys0_len > 0 {
                push(
                    t0,
                    "tengu_sysprompt_block",
                    json!({ "length": shape.sys0_len, "hash": &shape.sys0_hash }),
                );
            }
            push(t0, "tengu_sysprompt_missing_boundary_marker", missing);
        } else if v285 {
            // 2.1.285 的无工具侧查询（标题、WebFetch 页面处理）是缺标记、首块、缺标记，与子代理
            // 同序（`cap/2.1.285/00040` 06:34:37.218、`00137` 06:56:14.139）。
            let missing = json!({ "promptBlockCount": shape.system_blocks });
            if shape.system_blocks > 0 {
                push(t0, "tengu_sysprompt_missing_boundary_marker", missing.clone());
            }
            if shape.sys0_len > 0 {
                push(
                    t0,
                    "tengu_sysprompt_block",
                    json!({ "length": shape.sys0_len, "hash": &shape.sys0_hash }),
                );
            }
            if shape.system_blocks > 0 {
                push(t0, "tengu_sysprompt_missing_boundary_marker", missing);
            }
        } else {
            if shape.sys0_len > 0 {
                push(
                    t0,
                    "tengu_sysprompt_block",
                    json!({ "length": shape.sys0_len, "hash": &shape.sys0_hash }),
                );
            }
            if shape.system_blocks > 0 {
                let missing = json!({ "promptBlockCount": shape.system_blocks });
                push(t0, "tengu_sysprompt_missing_boundary_marker", missing.clone());
                push(t0, "tengu_sysprompt_missing_boundary_marker", missing);
            }
        }
        // 三条 tether 事件共用的「无状态」判定：模型被固定成无状态发送，或 auto 模式的分类器
        // 在跑（`cap/2.1.280`：auto 的三条 true，切回 default 的四条 false）。两者任一为真，
        // 这条实际就不走线程（`sentThreadType: "none"`）。
        let model_held = model_held_stateless(display_model);
        // 2.1.285 起 auto 模式不再钉成无状态：前三轮 auto 的 `classifierHeldStateless` 全是
        // false，请求照带 `message-threads` 并建线程（`cap/2.1.285/00030`、`00040` 批次）。
        let classifier_held = !v285 && shape.permission_mode == "auto";
        if let Some(t) = &tether {
            let decision = json!({
                "decision": t.decision,
                "reason": t.reason,
                "sourceCategory": kind.category(),
                "threadUnsupported": false,
                "modelHeldStateless": model_held,
                "relayHeldStateless": false,
                "classifierHeldStateless": classifier_held,
                "dropHeldStateless": false,
                "evictedOther": false,
                "changedModel": t.changed_model,
                "changedSystem": false,
                "changedTools": t.changed_tools,
                "changedBetas": t.changed_betas,
                "changedLatchedHeaders": t.changed_latched,
                "changedThinking": t.changed_thinking,
                "changedToolChoice": false,
                "changedEffort": t.changed_effort,
                "changedExtraBody": false,
                "turnsInThread": t.turns,
                "messageCount": shape.messages_len,
                "prevMessageCount": t.prev_messages,
                "firstChangedIndex": -1,
                "deltaMessageCount": t.delta,
                "claimedCompact": false,
                "claimedSnip": false,
                "claimedToolResultClear": false,
                "claimedRewind": false,
                "claimedClear": false,
                "claimedAbortStrip": false,
                "unclaimedHistoryChange": t.reason == "history_changed"
            });
            let mut decision = decision;
            if v285 {
                insert_after(
                    &mut decision,
                    "classifierHeldStateless",
                    vec![("creditRetryStateless", json!(false))],
                );
            }
            push(t0, "tengu_tether_decision", decision.clone());
            dd.push(identity.dd_entry(
                "tengu_tether_decision",
                &ctx(t0),
                dd_model,
                snake_flat(&decision),
            ));
            // 回声审计：客户端把上游回过来的 assistant 轮次原样带回去了没有。经代理转发的
            // 历史就是客户端自己拼的那份，按「全部原样」报。
            if t.echo {
                let turns = shape.assistant_messages;
                let echo = json!({
                    "sourceCategory": kind.category(),
                    "messageCount": shape.messages_len,
                    "anchorDiverged": false,
                    "turnsReceived": turns,
                    "turnsIdentical": turns,
                    "turnsDiverged": 0,
                    "reordered": 0,
                    "turnSplit": 0,
                    "turnDropped": 0,
                    "addedToolUseGettask": 0,
                    "addedToolUsePoll": 0,
                    "addedToolUseOther": 0,
                    "addedOther": 0,
                    "droppedToolUse": 0,
                    "droppedThinking": 0,
                    "droppedText": 0,
                    "droppedOther": 0,
                    "toolNameChanged": 0,
                    "callerDropped": 0
                });
                push(t0, "tengu_tether_echo_audit", echo.clone());
                dd.push(identity.dd_entry(
                    "tengu_tether_echo_audit",
                    &ctx(t0),
                    dd_model,
                    snake_flat(&echo),
                ));
            }
        }
        push(t0, "tengu_api_cache_breakpoints", breakpoints.clone());
        let mut query = Map::new();
        query.insert("model".into(), json!(&display_model));
        query.insert("messagesLength".into(), json!(shape.messages_len));
        query.insert("temperature".into(), json!(shape.temperature));
        query.insert("provider".into(), json!("firstParty"));
        query.insert("buildAgeMins".into(), json!(build_age_mins));
        query.insert("betas".into(), json!(&betas_full));
        query.insert("permissionMode".into(), json!(shape.permission_mode));
        query.insert("querySource".into(), json!(query_source));
        chain_fields(&mut query);
        query.insert("thinkingType".into(), json!(&shape.thinking_type));
        if let Some(e) = &effort {
            query.insert("effortValue".into(), json!(e));
        }
        query.insert("fastMode".into(), json!(shape.fast_mode));
        if let Some(prev) = &previous_request_id {
            query.insert("previousRequestId".into(), json!(prev));
        }
        push(t0, "tengu_api_query", Value::Object(query));
        push(ms(t0, 1), "tengu_api_cache_breakpoints", breakpoints);

        // 首字节到达。失败那条没有这一条：官方的 `tengu_feature_ok{api_request}` 是请求
        // **成功返回**之后才打的，失败走的是下面的 `tengu_feature_bad{api_request}`。
        if !failed && (!aborted || call.ttft_ms.is_some()) {
            push(t_first, "tengu_feature_ok", feature("api_request"));
            dd.push(identity.dd_entry(
                "tengu_feature_ok",
                &ctx(t_first),
                dd_model,
                feature("api_request"),
            ));
            // 会话首条主线程请求的首字节处，客户端头一回用上这几项能力各报一次，与 beta 一一
            // 对应（`cap/2.1.285/00040` 06:34:19.500，紧跟 `api_request`；`cap/2.1.280` 两个会话
            // 各一组）。
            if first_main {
                for (beta, name) in [
                    (config::CC_BETA_PER_TURN_CONTROL, "api_per_turn_effort"),
                    ("mid-conversation-tool-changes-", "mcp_late_tool_additions"),
                    ("mid-conversation-system-clear-at-", "api_kept_reminder_clear_at"),
                ] {
                    let prefix = beta.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-');
                    if betas_full.split(',').any(|b| b.trim().starts_with(prefix)) {
                        push(t_first, "tengu_feature_ok", feature(name));
                        dd.push(identity.dd_entry(
                            "tengu_feature_ok",
                            &ctx(t_first),
                            dd_model,
                            feature(name),
                        ));
                    }
                }
            }
        }
        if first_text_interrupted {
            let mut first_text = json!({
                "first_text_wait_end": "interrupted",
                "user_wait_before_first_text_ms": 0,
                "user_waits_before_first_text": 0,
                "queryChainId": &chain_id,
                "terminal_reason": "aborted_streaming",
                "prompt_submit_to_send_ms": 22,
                "prompt_queued_ms": 0
            });
            if v277
                && turn_origin != "human"
                && let Some(o) = first_text.as_object_mut()
            {
                o.shift_remove("prompt_submit_to_send_ms");
                o.shift_remove("prompt_queued_ms");
            }
            push(ms(t_end, 5), "tengu_turn_first_text", first_text);
        }
        if first_text_in_turn {
            let t_paint = ms(t_first, 30);
            let mut first_text = json!({
                    "first_text_wait_end": "painted",
                    "ttfvt_first_text_paint_ms": (t_paint - turn_started).num_milliseconds().max(0),
                    "first_text_path": if query_depth == 0 { "direct" } else { "after_tool_use" },
                    "requests_before_first_text": query_depth + 1,
                    "tool_calls_before_first_text": tool_calls_before,
                    "first_text_assistant_message_id": call.message_id.as_deref().unwrap_or(""),
                    "first_text_request_id": call.request_id.as_deref().unwrap_or(""),
                    "first_text_render_path": "block_complete",
                    "user_wait_before_first_text_ms": 0,
                    "user_waits_before_first_text": 0,
                    "queryChainId": &chain_id,
                    "prompt_submit_to_send_ms": 24,
                    "prompt_queued_ms": 0
            });
            // 不是用户敲的那轮（后台任务通知）没有「提交到发出」这两项（`cap/2.1.280`、
            // `cap/2.1.285` 的 task-notification 轮各一条）。
            if v277
                && turn_origin != "human"
                && let Some(o) = first_text.as_object_mut()
            {
                o.shift_remove("prompt_submit_to_send_ms");
                o.shift_remove("prompt_queued_ms");
            }
            push(t_paint, "tengu_turn_first_text", first_text);
        }

        // 换模型后缓存全 miss，客户端会收到上游的缓存诊断并上报。
        if model_changed && let Some(rid) = &call.request_id {
            push(
                t_end,
                "tengu_prompt_cache_diagnosis_received",
                json!({
                    "diagnosisType": "model_changed",
                    "tokensMissed": call.cache_creation_tokens,
                    "requestId": rid,
                    "previousMessageId": previous_message_id.as_deref().unwrap_or(""),
                    "model": &display_model,
                    "isCowork": false,
                    "is1hCacheTTL": shape.cache_ttl_1h,
                    "querySource": query_source,
                    "queryDepth": query_depth
                }),
            );
        }

        // 两条收尾事件都要的量。
        //
        // `durationMsIncludingRetries` 比 `durationMs` 多出的那 1–5ms 是客户端重试包装层的
        // 开销，官方五条是 +3/+5/+1/+1/+4；按 request-id 取个稳定的零头，别恒等。
        let req_hash = call
            .request_id
            .as_deref()
            .map(|r| r.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b))))
            .unwrap_or(0);
        let retry_pad = req_hash % 5;
        let body_chars = std::str::from_utf8(&call.body).map(js_len).unwrap_or(call.body.len());
        // 请求体不到 4 KiB 不压缩，报 `below_min_size`；够大的才报「走代理不压缩」的 `proxy`
        // （`cap/auto-2.1.285-20260930` 128 条：22 条 573–3821 字是前者，106 条 4107 起是后者）。
        let gzip_skip = if body_chars < 4096 { "below_min_size" } else { "proxy" };

        // tether 收尾那条的写法，成功与被取消的两条路共用（取消报 `finalOutcome: aborted`，
        // `cap/auto-2.1.285-20260930` 08:01:42.632）。
        let live_outcome = |final_outcome: &str| -> Option<Value> {
            let t = tether.as_ref()?;
            let sent = match shape.thread_type.as_deref() {
                Some("create") => "create",
                Some("continue") => "continue",
                _ => "none",
            };
            let stateless = sent == "none";
            let omitted = sent == "continue";
            let live = json!({
                "requestId": call.request_id.as_deref().unwrap_or(""),
                "sentThreadType": sent,
                "sourceCategory": kind.category(),
                "planReason": if stateless { "none" } else { t.reason },
                "engineDecision": t.decision,
                "engineReason": t.reason,
                "firstThreadError": "none",
                "finalOutcome": final_outcome,
                "replayed": false,
                "droppedFrom": "none",
                "threadUnsupported": false,
                "modelHeldStateless": model_held,
                "relayHeldStateless": false,
                "classifierHeldStateless": classifier_held,
                "serverToolHistory": false,
                // 就是「这条请求的 beta 里有 `mid-conversation-tool-changes`」：`cap/2.1.280`、
                // `cap/2.1.285` 逐条相等（opus-5 / fable-5 / opus-4-8 这几个
                // `modelHeldStateless` 为 false 的也是 true）。此前按 `modelHeldStateless`
                // 报，2.1.277 那条 opus-5 的「例外」正是这个缘故。
                "toolAdditionHistory": betas_full.contains("mid-conversation-tool-changes-"),
                "toolRemovalHistory": false,
                "toolChangeHistory": false,
                "requestScopedStateless": false,
                "keptReminderClearAt": false,
                "keptReminderScope": "all",
                "deltaMessageCount": shape.after_last_assistant,
                "messageCount": shape.messages_len,
                "turnsInThread": t.turns,
                "omittedSystem": omitted,
                "omittedTools": omitted,
                "omittedBytes": if omitted { shape.omitted_bytes } else { 0 },
                "inheritBreakerTripped": false,
                "dropArmed": false,
                "dropHeldStateless": false,
                "continuesSinceDropCreate": -1,
                "dropReportUnmatched": false
            });
            let mut live = live;
            if v285 {
                insert_after(
                    &mut live,
                    "classifierHeldStateless",
                    vec![
                        ("creditRetryStateless", json!(false)),
                        ("toolResultClearingHeldStateless", json!(false)),
                    ],
                );
            }
            Some(live)
        };
        // 收尾：成功报 `tengu_api_success`、失败报 `tengu_api_error`——两条并列，且互斥。
        //
        // 官方对失败请求发的是 `tengu_api_error`（`ate()` 分类 + 上游文案，没有任何用量）
        // 外加一条 `tengu_feature_bad{api_request}`；成功那条独有的
        // `tengu_tool_schema_sizes` 与标题生成收尾也只在成功时发（前者就在官方
        // `tengu_api_success` 那个函数里）。被客户端取消的两样都不报。
        if !failed && !aborted {
            // 收尾：tengu_api_success（键序照抓包）。
            let mut success = Map::new();
            let mut put = |k: &str, v: Value| {
                success.insert(k.to_string(), v);
            };
            put("model", json!(&resp_model));
            // 2.1.285 起报这条请求头上的 `anthropic-dispatch-id`（12 条恒为 `v2d`，见
            // [`config::CC_DISPATCH_ID`]）。
            if v285 {
                put("dispatch", json!(config::CC_DISPATCH_ID));
            }
            if has_1m {
                put("preNormalizedModel", json!(&display_model));
            }
            put("betas", json!(&betas_full));
            if v285 {
                put("echoWireToolInputs", json!(true));
            }
            put("messageCount", json!(shape.messages_len));
            put("messageTokens", json!(message_tokens));
            put("inputTokens", json!(call.input_tokens));
            put("outputTokens", json!(call.output_tokens));
            put("cachedInputTokens", json!(call.cache_read_tokens));
            put("uncachedInputTokens", json!(call.cache_creation_tokens));
            // 2.1.285：缓存没盖住的尾段，即最后一条带断点的消息之后那几条（[`tail_tokens_est`] 估
            // token）。`cap/2.1.285` 与 `cap/auto-2.1.285-20260930` 里五种取值：
            //
            // - 不带断点（标题那类）：`caching_off`，整段消息都算没盖住，估算全 0；
            // - thread 续轮（主线程与子代理）：`threaded_continue`；
            // - fork 出来的查询（猜下一句、插问、压缩、离开回顾、子代理摘要）：
            //   `fork_tail_skip_cache_write`，尾段条数与估算记在 `resentTailTokensEst`（猜下一句恒
            //   341、插问 336、压缩 1775）；
            // - 主线程尾段全是 system 消息（fable-5-1 不走 thread，工具续轮末尾那条只发一次的
            //   system 提示，`00385`、`00391`）：`sent_once_text`，条数报 0、估算记在
            //   `sentOnceTailTokensEst`，`plainInputExcessTokens` 扣掉它（32 − 32 = 0）；
            // - 其余带断点的：`none`，全 0，`plainInputExcessTokens` 等于 `inputTokens`。
            //
            // 前三种不报 `plainInputExcessTokens`。
            if v285 {
                let est = shape.tail_tokens_est;
                let fork = matches!(
                    kind,
                    Kind::AgentSummary
                        | Kind::AwaySummary
                        | Kind::Suggestion
                        | Kind::SideQuestion
                        | Kind::Compact
                );
                let (reason, tail, resent, sent_once) = if !shape.has_cache_control {
                    ("caching_off", shape.messages_len, 0, 0)
                } else if shape.thread_type.as_deref() == Some("continue") {
                    ("threaded_continue", 0, 0, 0)
                } else if fork {
                    ("fork_tail_skip_cache_write", shape.tail_messages.max(1), est, 0)
                } else if shape.tail_all_system {
                    ("sent_once_text", 0, 0, est)
                } else {
                    ("none", 0, 0, 0)
                };
                put("uncoveredTailReason", json!(reason));
                put("uncoveredTailMessages", json!(tail));
                put("resentTailTokensEst", json!(resent));
                put("sentOnceTailTokensEst", json!(sent_once));
                put("unexcusedTailTokensEst", json!(0));
                if matches!(reason, "none" | "sent_once_text") {
                    put(
                        "plainInputExcessTokens",
                        json!((call.input_tokens.max(0) as u64).saturating_sub(sent_once)),
                    );
                }
            }
            put("durationMs", json!(total));
            put("durationMsIncludingRetries", json!(total + 1 + i64::from(retry_pad)));
            put("attempt", json!(1));
            put("ttftMs", json!(ttft));
            // 首个内容块到达：比首字节晚 0–1ms（`cap/2.1.280` 九条里七条 +1、两条 +0）。
            if v270 {
                put("firstContentMs", json!(ttft + i64::from(retry_pad % 2)));
            }
            // 2.1.285：发请求前客户端这一侧的开销，主线程 6–46ms（首条 25），标题那类侧查询
            // 不报。按 request-id 取个稳定值，同 `retry_pad`。
            if v285 && !kind.is_side_query() {
                put("queryOverheadMs", json!(6 + i64::from(req_hash % 41)));
            }
            put("buildAgeMins", json!(build_age_mins));
            put("provider", json!("firstParty"));
            put("requestId", json!(call.request_id.as_deref().unwrap_or("")));
            if v270 && let Some(crid) = call.client_request_id.as_deref().filter(|c| !c.is_empty())
            {
                put("clientRequestId", json!(crid));
            }
            // 子代理首条：是哪条主线程请求拉起的它（`cap/2.1.280`：`invokingRequestId` 指回
            // 调了 Agent 的那条，`invocationKind: spawn`）；之后的各条不带。
            if let Some(inv) = agent.as_ref().and_then(|a| a.invoking_request_id.as_deref())
                && kind == Kind::Subagent
            {
                put("invokingRequestId", json!(inv));
                put("invocationKind", json!("spawn"));
            }
            put("stop_reason", json!(call.stop_reason.as_deref().unwrap_or("end_turn")));
            if let Some(e) = &effort {
                put("effort_level", json!(e));
            }
            // 只有主线程带（子代理、猜下一句、标题都没有），取值同本轮的 input_prompt。
            if v277 && is_main {
                put("turn_origin", json!(&turn_origin));
            }
            // 只有主线程报「是不是默认模型/默认 effort」；子代理（`cap/2.1.280` Explore 六条）、
            // 猜下一句和标题那类都不报。
            if is_main {
                // `[1m]` 不算换了模型：`/model opus[1m]` 之后官方照报 true（`cap/auto-2.1.285-20260930/00235`）。
                let bare = |m: &str| m.trim_end_matches("[1m]").to_string();
                put("is_default_model", json!(bare(display_model) == bare(default_model)));
                put("default_model", json!(&default_model));
                if let Some(e) = &effort {
                    // 2.1.280 起按模型的默认 effort 比（`cap/2.1.285/00040`、`00079`，
                    // `cap/2.1.280/00167`）；更老的版本没有样本，照旧报「就是默认」。
                    let default = if v280 { default_effort_of(display_model) } else { e.as_str() };
                    put("is_default_effort", json!(e == default));
                    put("default_effort_level", json!(default));
                }
            }
            put("costUSD", json!(call.cost_usd.unwrap_or(0.0)));
            put("didFallBackToNonStreaming", json!(false));
            // `-p` 打印模式（`00441` 等十条）：非交互、print、没有 TTY。
            put("isNonInteractiveSession", json!(shape.sdk));
            put("print", json!(shape.sdk));
            put("isTTY", json!(!shape.sdk));
            put("querySource", json!(query_source));
            if kind.has_chain() {
                put("queryChainId", json!(&chain_id));
                put("queryDepth", json!(query_depth));
            }
            put("permissionMode", json!(shape.permission_mode));
            put("globalCacheStrategy", json!("system_prompt"));
            if shape.has_cache_control {
                put("prompt_cache_ttl", json!(cache_ttl));
                // 订阅用户的 1h 缓存报 `subscriber`；子代理那份是 5m，报 `default`（`cap/2.1.280`）。
                put(
                    "prompt_cache_ttl_reason",
                    json!(if shape.cache_ttl_1h { "subscriber" } else { "default" }),
                );
            }
            put("textContentLength", json!(call.text_chars));
            // 官方的判据是「这条回复里有没有思考块」（`redacted_thinking` 与空思考块都算），嗅探器
            // 按块类型记（`saw_thinking`）。此前「整条没有正文」也算，可 `tool_use` 收尾的回复并不
            // 必带思考块：`cap/auto-2.1.285-20260930` 31 条没有思考块的官方都不报这一项。
            if call.saw_thinking || call.thinking_chars > 0 {
                put("thinkingContentLength", json!(call.thinking_chars));
            }
            put("narrationBlockCount", json!(0));
            // `toolUseContentLengths`：这条回复里每个工具的入参 JSON 字符数之和，键按首次
            // 出现排序，整张表**序列化成一个字符串**塞进事件（与 `toolSchemaCharLengths`
            // 同一种写法）；一个 `tool_use` 块都没有时整个字段不出现。
            if !call.tool_use_lens.is_empty() {
                let mut table = Map::new();
                for (name, len) in &call.tool_use_lens {
                    table.insert(name.clone(), json!(len));
                }
                put("toolUseContentLengths", json!(Value::Object(table).to_string()));
            }
            put("imageBlockCount", json!(shape.image_blocks));
            put("imageTotalPixels", json!(0));
            put("imageTotalBytes", json!(shape.image_bytes));
            put("documentBlockCount", json!(shape.doc_blocks));
            put("documentTotalBytes", json!(shape.doc_bytes));
            put("inputTextCharLength", json!(shape.input_text_chars));
            put("estimatedInputTokens", json!(shape.estimated_tokens));
            put("systemCharLength", json!(shape.system_chars));
            if let Some((source, hash)) = &snapshot {
                put("systemPromptSource", json!(source));
                put("snapshotHash", json!(hash));
            } else if modern && kind.has_boundary() {
                put("systemPromptSource", json!("live_unrecorded"));
            }
            put("toolsCharLength", json!(shape.tools_chars));
            put("toolsCount", json!(shape.tools_count));
            put("deferredToolsCount", json!(shape.deferred_tools));
            put("toolSchemasHash", json!(&shape.tools_hash));
            put("requestBodyEncoding", json!("identity"));
            put("requestBodyChars", json!(body_chars));
            // 2.1.285：组请求体的耗时，每条都报，0 或 1ms。
            if v285 {
                put("requestPrepareMs", json!((req_hash / 41) % 2));
            }
            put("gzipSkipReason", json!(gzip_skip));
            put("fastMode", json!(shape.fast_mode));
            if let Some(prev) = &previous_request_id {
                put("previousRequestId", json!(prev));
            }
            if post_compaction {
                put("isPostCompaction", json!(true));
            }
            if let Some(skill) = &turn_skill {
                put("attributionSkill", json!(skill));
            }
            // 内置子代理多报一项是哪类子代理，排在链字段之后（`cap/2.1.280` Explore 六条都有，
            // 它的摘要请求没有）。自定义子代理那边报的是触发它的 skill（`attributionSkill`），
            // 代理这一侧不知道是哪个 skill，不报。
            if kind == Kind::Subagent
                && v270
                && let Some(t) = builtin_agent
            {
                put("attributionAgent", json!(t));
            }
            if let Some(ms_since) = time_since_last {
                put("timeSinceLastApiCallMs", json!(ms_since));
            }
            let success = Value::Object(success);
            // tether 收尾排在 api_success 之前、同一毫秒（`cap/2.1.277`、`cap/2.1.280` 全部如此）。
            // 失败那条抓包里只见过流被中断的 `aborted`，代理这边的失败形态没有样本，不报。
            if let Some(live) = live_outcome("ok") {
                push(t_end, "tengu_tether_live_outcome", live.clone());
                dd.push(identity.dd_entry(
                    "tengu_tether_live_outcome",
                    &ctx(t_end),
                    dd_model,
                    snake_flat(&live),
                ));
            }
            push(t_end, "tengu_api_success", success.clone());
            // Datadog 那份比 event_logging 少两项：工具长度表的 hash 与各工具入参长度
            // （`cap/2.1.260-2`、`2.1.277`、`2.1.280` 三份的 Datadog 批次里一条都没有）。
            let mut dd_success = snake_flat(&success);
            if let Some(o) = dd_success.as_object_mut() {
                o.shift_remove("tool_schemas_hash");
                o.shift_remove("tool_use_content_lengths");
            }
            dd.push(identity.dd_entry("tengu_api_success", &ctx(t_end), dd_model, dd_success));

            // 工具集变了才报一次（无工具的侧查询也算一种：`{}` 那份）。
            if tools_changed {
                push(
                    t_end,
                    "tengu_tool_schema_sizes",
                    json!({
                        "toolSchemasHash": &shape.tools_hash,
                        "toolSchemaCharLengths": &shape.tool_lens,
                        "toolsCharLength": shape.tools_chars,
                        "toolsCount": shape.tools_count,
                        "deferredToolsCount": shape.deferred_tools
                    }),
                );
            }
            // 首条主线程请求收尾时记一次这个会话发出去的请求形态（`cap/2.1.285/00040`
            // 06:34:21.881，2.1.280 没有）：system 角色消息、工具变更头、保留提醒这三项跟着对应的
            // beta 走。
            // `-p` 的只在带 `mid-conversation-system` 的模型上记（fable / opus 那两条有、haiku 那条没有）。
            if first_main
                && v285
                && (!shape.sdk || betas_full.contains("mid-conversation-system-2"))
            {
                let has = |p: &str| betas_full.split(',').any(|b| b.trim().starts_with(p));
                push(
                    ms(t_end, 1),
                    "tengu_wire_shape_recorded",
                    json!({
                        "systemTurns": has("mid-conversation-system-2"),
                        "toolChangeHeader": has("mid-conversation-tool-changes-"),
                        "inlineTools": false,
                        "keptReminders": has("mid-conversation-system-clear-at-"),
                        "overwrote": false
                    }),
                );
            }
            if kind == Kind::Title {
                // 2.1.285 多两项：输入有没有被截断（`cap/2.1.285/00040`，都是 false）。
                let title = if v285 {
                    json!({ "success": true, "input_capped": false, "input_over_cap": false })
                } else {
                    json!({ "success": true })
                };
                push(t_end, "tengu_session_title_generated", title);
            }
            if kind == Kind::RenameName {
                push(ms(t_end, 4), "tengu_agent_name_set", json!({ "source": "auto" }));
            }
        }

        // 失败收尾：`tengu_feature_bad{api_request}` + `tengu_api_error`，两条都进 Datadog
        // （官方那份 Datadog 白名单里 `tengu_api_error` 与 `tengu_feature_bad` 都在）。
        let mut error_kind_name = String::new();
        if let Some(fail) = &call.failure {
            let kind_name = error_kind(fail);
            let code = api_request_error_code(fail);
            let bad = json!({ "feature_name": "api_request", "error_code": code });
            push(t_end, "tengu_feature_bad", bad.clone());
            dd.push(identity.dd_entry("tengu_feature_bad", &ctx(t_end), dd_model, bad));

            let mut err = Map::new();
            let mut put = |k: &str, v: Value| {
                err.insert(k.to_string(), v);
            };
            put("model", json!(&display_model));
            // 上游文案原样报（官方截断在 4000 字），除非连 request-id 都没拿到——那种情形
            // 官方换成 `API error: type=… status=…` 这句合成文案。
            let has_request_id = call.request_id.as_deref().is_some_and(|r| !r.is_empty());
            let message = if has_request_id && !fail.message.trim().is_empty() {
                let m = fail.message.trim();
                match m.char_indices().nth(4_000) {
                    Some((cut, _)) => format!("{}\u{2026}<truncated>", &m[..cut]),
                    None => m.to_string(),
                }
            } else {
                format!(
                    "API error: type={kind_name} status={}",
                    fail.status.map_or_else(|| "none".to_string(), |s| s.to_string())
                )
            };
            put("error", json!(message));
            // `status` 是十进制串而不是数字（官方 `FP()` 就是 `String(status)`）；流内错误
            // 那种 SDK 侧没有状态码，整个字段不出现。
            if let Some(st) = fail.status {
                put("status", json!(st.to_string()));
            }
            put("errorType", json!(&kind_name));
            if let Some(e) = &effort {
                put("effort_level", json!(e));
            }
            put("messageCount", json!(shape.messages_len));
            put("messageTokens", json!(message_tokens));
            put("durationMs", json!(total));
            put("durationMsIncludingRetries", json!(total + 1 + i64::from(retry_pad)));
            put("attempt", json!(1));
            put("provider", json!("firstParty"));
            if has_request_id {
                put("requestId", json!(call.request_id.as_deref().unwrap_or("")));
            }
            if let Some(crid) = call.client_request_id.as_deref().filter(|c| !c.is_empty()) {
                put("clientRequestId", json!(crid));
            }
            put("didFallBackToNonStreaming", json!(false));
            put("requestBodyEncoding", json!("identity"));
            put("requestBodyChars", json!(body_chars));
            put("gzipSkipReason", json!(gzip_skip));
            if kind.has_chain() {
                put("queryChainId", json!(&chain_id));
                put("queryDepth", json!(query_depth));
            }
            put("querySource", json!(query_source));
            put("fastMode", json!(shape.fast_mode));
            if let Some(prev) = &previous_request_id {
                put("previousRequestId", json!(prev));
            }
            let err = Value::Object(err);
            push(t_end, "tengu_api_error", err.clone());
            dd.push(identity.dd_entry("tengu_api_error", &ctx(t_end), dd_model, snake_flat(&err)));
            error_kind_name = kind_name;
        }

        // fork 统计里的 `messageCount` 是分叉查询回来的内容块数：思考块 + 正文，只有正文就是 1
        // （`cap/2.1.285`：haiku 摘要带思考两条都是 2，离开回顾一条纯文本 1、一条带思考 2；
        // `cap/2.1.280` 的猜下一句同样 1 与 2 都有）。
        let fork_messages = 1 + u32::from(call.saw_thinking);

        if is_main && turn_over && failed {
            // 请求失败也是一轮的终点，但没有 stop hook、没有 `tengu_feature_ok{turn}`、
            // 也没有首轮那串——那些都挂在「跑完了」这条路上。
            push(
                ms(t_end, 1),
                "tengu_turn_end",
                turn_end(
                    "api_error",
                    (ms(t_end, 1) - turn_started).num_milliseconds().max(total),
                    Some(if error_kind_name.is_empty() { "unknown" } else { &error_kind_name }),
                ),
            );
        } else if is_main && aborted {
            // 按 Esc 打断（`cap/auto-2.1.285-20260930` 08:01:42.630–.635）：取消、tether 收尾报
            // `aborted`、turn、`turn_end{aborted_streaming}`，没有 stop hook；一个字都还没出的话
            // 客户端把这一轮撤回（`conversation_rewind{auto_restore_cancel}`），输入框里恢复原文。
            let stream_mode = if call.text_chars > 0 {
                "responding"
            } else if call.saw_thinking {
                "thinking"
            } else {
                "requesting"
            };
            let mut cancel = json!({ "source": "escape", "streamMode": stream_mode });
            if let Some(e) = &effort {
                cancel["effort_level"] = json!(e);
            }
            cancel["message_id"] = json!(call.message_id.as_deref().unwrap_or(""));
            push(t_end, "tengu_cancel", cancel);
            if let Some(live) = live_outcome("aborted") {
                push(ms(t_end, 2), "tengu_tether_live_outcome", live.clone());
                dd.push(identity.dd_entry(
                    "tengu_tether_live_outcome",
                    &ctx(ms(t_end, 2)),
                    dd_model,
                    snake_flat(&live),
                ));
            }
            let t3 = ms(t_end, 3);
            for name in ["shoji_engine", "turn"] {
                push(t3, "tengu_feature_ok", feature(name));
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t3), dd_model, feature(name)));
            }
            push(
                t3,
                "tengu_turn_end",
                turn_end(
                    "aborted_streaming",
                    (t3 - turn_started).num_milliseconds().max(total),
                    None,
                ),
            );
            if call.text_chars == 0 {
                let t5 = ms(t_end, 5);
                let before = pre_count + 1;
                push(
                    t5,
                    "tengu_conversation_rewind",
                    json!({
                        "preRewindMessageCount": before,
                        "postRewindMessageCount": before - 4,
                        "messagesRemoved": 4,
                        "rewindToMessageIndex": before - 4,
                        "source": "auto_restore_cancel"
                    }),
                );
                push(t5, "tengu_feature_ok", feature("repl_rewind_conversation"));
                dd.push(identity.dd_entry(
                    "tengu_feature_ok",
                    &ctx(t5),
                    dd_model,
                    feature("repl_rewind_conversation"),
                ));
            }
        } else if is_main && turn_over {
            take_tpl(&tpl.turn, t_end);
            // `-p` 跑完这一轮进程就退出（`00448` 08:06:45.632–46.376）：首字耗时与结果两条（从进程
            // 启动算），最后一条会话结束的缓存逐出提示；中间那几条静态的退出收尾在模板里。
            if shape.sdk {
                let t4 = ms(t_end, 4);
                let start: DateTime<Utc> = started_wall.into();
                push(
                    t4,
                    "tengu_sdk_ttft",
                    json!({
                        "ttft_ms": (t_first - start).num_milliseconds().max(0),
                        "model": &resp_model,
                        "tool_pool_reused": false
                    }),
                );
                push(
                    t4,
                    "tengu_sdk_result",
                    json!({
                        "subtype": "success",
                        "is_error": false,
                        "num_turns": sess_turn_api_calls,
                        "duration_ms": (t4 - start).num_milliseconds().max(0),
                        "duration_api_ms": sess_turn_api_ms + 3,
                        "saw_retry": false,
                        "saw_compact": false,
                        "tool_use_count": sess_turn_tools,
                        "mcp_tool_calls": 0,
                        "toolsearch_calls": 0,
                        "builtin_tool_calls": sess_turn_tools,
                        "turn_index": 0,
                        "mcp_pending_at_start": 0,
                        "mcp_pending_at_end": 0
                    }),
                );
                push(
                    ms(t_end, 748),
                    "tengu_cache_eviction_hint",
                    json!({
                        "scope": "session_end",
                        "last_request_id": call.request_id.as_deref().unwrap_or("")
                    }),
                );
            }
            if emit_first_turn {
                take_tpl(&tpl.first_turn, t_end);
            }
            let t1 = ms(t_end, 1);
            let t2 = ms(t_end, 2);
            // 2.1.280 起每轮收尾都报一次「猜下一句」为什么没出（`cap/2.1.285` 14 轮 14 条，
            // `cap/2.1.280` 同样逐轮有）：这一轮
            // 大半在写缓存（首轮、换模型之后）是 `cache_cold`，其余是 `unfocused`——终端不在
            // 前台，客户端也就没发猜下一句那条请求，与代理这边看到的一致。分界取「缓存写入超过
            // 读取的一成」，14 条全对得上（写 884 / 读 87942 那两条是 unfocused）。
            //
            // 2.1.285（`cap/auto-2.1.285-20260930` 35 条）改成按「猜下一句」那条请求的结果报，一条
            // `unfocused` 都没有：这一轮大半在写缓存就不发那条请求，当场报 `cache_cold`；会话第一轮
            // 报 `early_conversation`（auto 模式那个会话的首轮仍是 `cache_cold`）；其余的轮次客户端
            // 会发那条请求，这里不报，等它的结果——正文为空报 `empty`、被新输入顶掉报 `aborted`、
            // 出了建议则在下一次输入时报 `ignored`（见 [`ignored_suggestion_meta`]）。`-p` 模式不猜。
            if v280 && !shape.sdk {
                let cold = call.cache_creation_tokens * 10 > call.cache_read_tokens;
                let early = v285 && prompt_seq == 1 && shape.permission_mode != "auto";
                let suggestion = if early {
                    Some(
                        json!({ "source": "cli", "outcome": "suppressed", "reason": "early_conversation", "prompt_id": "user_intent" }),
                    )
                } else if cold {
                    Some(
                        json!({ "source": "cli", "outcome": "suppressed", "reason": "cache_cold", "cacheColdBy": "cache_write", "prompt_id": "user_intent" }),
                    )
                } else if v285 {
                    None
                } else {
                    Some(
                        json!({ "source": "cli", "outcome": "suppressed", "reason": "unfocused", "prompt_id": "user_intent" }),
                    )
                };
                if let Some(suggestion) = suggestion {
                    push(t1, "tengu_prompt_suggestion", suggestion);
                }
            }
            push(t1, "tengu_feature_ok", feature("hook_stop_handler"));
            push(t2, "tengu_feature_ok", feature("turn"));
            push(
                t2,
                "tengu_turn_end",
                turn_end("completed", (t2 - turn_started).num_milliseconds().max(total), None),
            );
            dd.push(identity.dd_entry(
                "tengu_feature_ok",
                &ctx(t1),
                dd_model,
                feature("hook_stop_handler"),
            ));
            dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t2), dd_model, feature("turn")));
        }
        // 猜下一句：自己就是一轮（`cap/2.1.260-2` 09:43:21），收尾多一条 fork 统计，没有 tips。
        // 失败的那条只有上面的错误串（fork 统计报的是这一发的用量，没有用量就没有它）。
        // 猜下一句被新输入顶掉（`07:52:58.552`）：fork 统计（用量全 0）、`suppressed/aborted`、
        // `turn_end{aborted_streaming}`，没有 stop hook。
        if kind == Kind::Suggestion && aborted {
            let fork = json!({
                "forkLabel": "prompt_suggestion",
                "querySource": "prompt_suggestion",
                "durationMs": total,
                "messageCount": 1,
                "relayEligible": true,
                "inputTokens": 0,
                "outputTokens": 0,
                "cacheReadInputTokens": 0,
                "cacheCreationInputTokens": 0,
                "serviceTier": "standard",
                "cacheCreationEphemeral1hTokens": 0,
                "cacheCreationEphemeral5mTokens": 0,
                "cacheHitRate": 0,
                "queryChainId": &main_chain,
                "queryDepth": prev_main_depth
            });
            push(t_end, "tengu_fork_agent_query", fork);
            push(
                t_end,
                "tengu_prompt_suggestion",
                json!({ "source": "cli", "outcome": "suppressed", "reason": "aborted", "prompt_id": "user_intent" }),
            );
            push(t_end, "tengu_turn_end", turn_end("aborted_streaming", total, None));
        }
        if kind == Kind::Suggestion && !failed && !aborted {
            let t1 = ms(t_end, 1);
            let total_in = call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
            let hit_rate =
                if total_in > 0 { call.cache_read_tokens as f64 / total_in as f64 } else { 0.0 };
            let mut fork = json!({
                "forkLabel": "prompt_suggestion",
                "querySource": "prompt_suggestion",
                "durationMs": total + 7,
                "messageCount": fork_messages
            });
            // 2.1.277 起多一项 `relayEligible`（猜下一句恒 true，子代理摘要恒 false）。
            if v277 {
                fork["relayEligible"] = json!(true);
            }
            let rest = json!({
                "inputTokens": call.input_tokens,
                "outputTokens": call.output_tokens,
                "cacheReadInputTokens": call.cache_read_tokens,
                "cacheCreationInputTokens": call.cache_creation_tokens,
                "serviceTier": "standard",
                "cacheCreationEphemeral1hTokens": 0,
                "cacheCreationEphemeral5mTokens": 0,
                "cacheHitRate": hit_rate,
                "queryChainId": &main_chain,
                "queryDepth": prev_main_depth
            });
            if let (Some(o), Some(r)) = (fork.as_object_mut(), rest.as_object()) {
                o.extend(r.clone());
            }
            // 2.1.280 的顺序是 stop hook、turn、turn_end、fork 统计，最后才是
            // `prompt_suggestion_generate`（`cap/2.1.280` 三条逐条如此）；之前的版本三个
            // feature 连着报在前头。
            let names: &[&str] = if v280 {
                &["hook_stop_handler", "turn"]
            } else {
                &["hook_stop_handler", "prompt_suggestion_generate", "turn"]
            };
            for name in names {
                push(t1, "tengu_feature_ok", feature(name));
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t1), dd_model, feature(name)));
            }
            if v280 {
                push(t1, "tengu_turn_end", turn_end("completed", total + 7, None));
                push(t1, "tengu_fork_agent_query", fork);
                let name = "prompt_suggestion_generate";
                push(t1, "tengu_feature_ok", feature(name));
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t1), dd_model, feature(name)));
                // 回来的正文是空的：没有建议可出（`07:50:09.353`，紧跟 `prompt_suggestion_generate`）。
                if v285 && call.text_chars == 0 {
                    push(
                        t1,
                        "tengu_prompt_suggestion",
                        json!({ "source": "cli", "outcome": "suppressed", "reason": "empty", "prompt_id": "user_intent" }),
                    );
                }
            } else {
                push(t1, "tengu_fork_agent_query", fork);
                push(t1, "tengu_turn_end", turn_end("completed", total + 7, None));
            }
        }

        // `/btw` 插问与 `/compact`：自己算一轮辅助调用。插问是 stop hook、缓存诊断
        // （`unavailable`，分叉请求上游不给诊断）、turn、fork 统计（没有链字段）、turn_end
        // （`cap/auto-2.1.285-20260930` 08:00:19.400–.401）；压缩是 stop hook、turn、turn_end、
        // fork 统计（`reactive-compact`，链另起、深度 -1），再是缓存逐出提示与压缩命令收尾
        // （07:57:42.737–.756）。
        if matches!(kind, Kind::SideQuestion | Kind::Compact) && !failed && !aborted {
            let t1 = ms(t_end, 1);
            let total_in = call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
            let hit_rate =
                if total_in > 0 { call.cache_read_tokens as f64 / total_in as f64 } else { 0.0 };
            let compact = kind == Kind::Compact;
            let mut fork = json!({
                "forkLabel": if compact { "reactive-compact" } else { "side_question" },
                "querySource": query_source,
                "durationMs": total + if compact { 10 } else { 37 },
                "messageCount": fork_messages,
                "relayEligible": true,
                "inputTokens": call.input_tokens,
                "outputTokens": call.output_tokens,
                "cacheReadInputTokens": call.cache_read_tokens,
                "cacheCreationInputTokens": call.cache_creation_tokens,
                "serviceTier": "standard",
                "cacheCreationEphemeral1hTokens": 0,
                "cacheCreationEphemeral5mTokens": 0,
                "cacheHitRate": hit_rate
            });
            if compact {
                fork["queryChainId"] = json!(uuid_v4());
                fork["queryDepth"] = json!(-1);
            }
            push(t1, "tengu_feature_ok", feature("hook_stop_handler"));
            dd.push(identity.dd_entry(
                "tengu_feature_ok",
                &ctx(t1),
                dd_model,
                feature("hook_stop_handler"),
            ));
            if !compact && let Some(rid) = &call.request_id {
                push(
                    t_end,
                    "tengu_prompt_cache_diagnosis_received",
                    json!({
                        "diagnosisType": "unavailable",
                        "tokensMissed": -1,
                        "requestId": rid,
                        "previousMessageId": previous_message_id.as_deref().unwrap_or(""),
                        "model": &display_model,
                        "isCowork": false,
                        "is1hCacheTTL": shape.cache_ttl_1h,
                        "querySource": query_source,
                        "queryDepth": query_depth
                    }),
                );
            }
            let t2 = ms(t1, 1);
            push(t2, "tengu_feature_ok", feature("turn"));
            dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t2), dd_model, feature("turn")));
            let elapsed = total + if compact { 9 } else { 36 };
            if compact {
                push(t2, "tengu_turn_end", turn_end("completed", elapsed, None));
                push(ms(t2, 1), "tengu_fork_agent_query", fork);
                let t3 = ms(t2, 17);
                push(
                    t3,
                    "tengu_cache_eviction_hint",
                    json!({
                        "scope": "compaction",
                        "last_request_id": prev_main_request_id.as_deref().unwrap_or("")
                    }),
                );
                for name in ["compact_reactive", "cmd_compact", "cmd_dispatch"] {
                    push(t3, "tengu_feature_ok", feature(name));
                    dd.push(identity.dd_entry(
                        "tengu_feature_ok",
                        &ctx(t3),
                        dd_model,
                        feature(name),
                    ));
                }
                push(
                    t3,
                    "tengu_input_command",
                    json!({ "input": "compact", "invocation_trigger": "user-slash" }),
                );
            } else {
                push(t2, "tengu_fork_agent_query", fork);
                push(t2, "tengu_turn_end", turn_end("completed", elapsed, None));
            }
        }

        // 离开回顾：同猜下一句，自己算一轮辅助调用；fork 统计挂回主线程那条链、深度 0，
        // `relayEligible: true`，最后是 `away_summary_generate`（`cap/2.1.285/00099`、`00160`，
        // `cap/2.1.280/00048`、`00088` 同序）。
        if kind == Kind::AwaySummary && !failed && !aborted {
            let t1 = ms(t_end, 1);
            for name in ["hook_stop_handler", "turn"] {
                push(t1, "tengu_feature_ok", feature(name));
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t1), dd_model, feature(name)));
            }
            push(t1, "tengu_turn_end", turn_end("completed", total + 6, None));
            let total_in = call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
            let hit_rate =
                if total_in > 0 { call.cache_read_tokens as f64 / total_in as f64 } else { 0.0 };
            push(
                t1,
                "tengu_fork_agent_query",
                json!({
                    "forkLabel": "away_summary",
                    "querySource": "away_summary",
                    "durationMs": total + 6,
                    "messageCount": fork_messages,
                    "relayEligible": true,
                    "inputTokens": call.input_tokens,
                    "outputTokens": call.output_tokens,
                    "cacheReadInputTokens": call.cache_read_tokens,
                    "cacheCreationInputTokens": call.cache_creation_tokens,
                    "serviceTier": "standard",
                    "cacheCreationEphemeral1hTokens": 0,
                    "cacheCreationEphemeral5mTokens": 0,
                    "cacheHitRate": hit_rate,
                    "queryChainId": &main_chain,
                    "queryDepth": 0
                }),
            );
            let name = "away_summary_generate";
            push(t1, "tengu_feature_ok", feature(name));
            dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t1), dd_model, feature(name)));
        }

        // 子代理的摘要请求：同猜下一句，自己算一轮辅助调用，fork 统计挂回子代理那条链、
        // 深度恒为 1（`cap/2.1.280` 1 条、`cap/2.1.277` 5 条）。
        if kind == Kind::AgentSummary && !failed && !aborted {
            let t1 = ms(t_end, 1);
            for name in ["hook_stop_handler", "turn"] {
                push(t1, "tengu_feature_ok", feature(name));
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t1), dd_model, feature(name)));
            }
            push(t1, "tengu_turn_end", turn_end("completed", total + 5, None));
            let total_in = call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
            let hit_rate =
                if total_in > 0 { call.cache_read_tokens as f64 / total_in as f64 } else { 0.0 };
            let agent_chain = agent.as_ref().map(|a| a.chain_id.clone()).unwrap_or_default();
            push(
                t1,
                "tengu_fork_agent_query",
                json!({
                    "forkLabel": "agent_summary",
                    "querySource": "agent_summary",
                    "durationMs": total + 5,
                    "messageCount": fork_messages,
                    "relayEligible": false,
                    "inputTokens": call.input_tokens,
                    "outputTokens": call.output_tokens,
                    "cacheReadInputTokens": call.cache_read_tokens,
                    "cacheCreationInputTokens": call.cache_creation_tokens,
                    "serviceTier": "standard",
                    "cacheCreationEphemeral1hTokens": 0,
                    "cacheCreationEphemeral5mTokens": 0,
                    "cacheHitRate": hit_rate,
                    "queryChainId": agent_chain,
                    "queryDepth": 1
                }),
            );
        }

        // 子代理跑完（它自己的 `end_turn`）：stop hook、turn、turn_end，随后是
        // `agent_tool_completed` 与支线收尾那几条（`cap/2.1.280` 10:16:10.901–.906）。
        // SubagentHandback 当场执行的那串工具事件：判决、放行、执行、成功，都挂在这条请求上
        // （它的 messageID / requestId / 深度），排在 api_success 之前。
        if let Some(hb) = &handback {
            let msg = call.message_id.as_deref().unwrap_or("");
            let rid = call.request_id.as_deref().unwrap_or("");
            let mut tu = ToolUse { id: hb.id.clone(), ..Default::default() };
            tu.apply_call(&hb.name, &hb.input, hb.verdict.as_ref());
            let th = ms(t_end, -32);
            if shape.permission_mode == "auto"
                && let Some(verdict) = tu.verdict.as_deref()
            {
                let decision = auto_mode_decision_meta(
                    &tu,
                    verdict,
                    None,
                    msg,
                    0,
                    usage_before,
                    tu.id.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b))),
                    session_cwd.as_deref(),
                );
                push(th, "tengu_auto_mode_decision", decision.clone());
                dd.push(identity.dd_entry(
                    "tengu_auto_mode_decision",
                    &ctx(th),
                    dd_model,
                    snake_flat(&decision),
                ));
            }
            let tg = ms(t_end, -3);
            push(tg, "tengu_feature_ok", feature("permission_auto_approve_config"));
            dd.push(identity.dd_entry(
                "tengu_feature_ok",
                &ctx(tg),
                dd_model,
                feature("permission_auto_approve_config"),
            ));
            push(
                tg,
                "tengu_tool_use_can_use_tool_allowed",
                json!({
                    "messageID": msg,
                    "toolName": &tu.name,
                    "queryChainId": &chain_id,
                    "queryDepth": query_depth,
                    "requestId": rid
                }),
            );
            push(
                tg,
                "tengu_tool_use_granted_in_config",
                json!({ "messageID": msg, "isMcp": false, "toolName": &tu.name, "sandboxEnabled": false }),
            );
            let td = ms(t_end, -2);
            push(td, "tengu_feature_ok", feature("tool_subagent_handback"));
            dd.push(identity.dd_entry(
                "tengu_feature_ok",
                &ctx(td),
                dd_model,
                feature("tool_subagent_handback"),
            ));
            let mut ok = json!({
                "messageID": msg,
                "toolName": &tu.name,
                "isMcp": false,
                "subagent_type": call.agent.agent_type.as_deref().unwrap_or("general-purpose"),
                "is_built_in_agent": builtin_agent.is_some()
            });
            if let Some(e) = &effort {
                ok["effort_level"] = json!(e);
            }
            for (k, v) in [
                ("durationMs", json!(28)),
                ("rssDeltaBytes", json!(5_308_416)),
                ("heapUsedDeltaBytes", json!(0)),
                ("externalDeltaBytes", json!(0)),
                ("preToolHookDurationMs", json!(0)),
                ("permissionDurationMs", json!(30)),
                // 回给子代理的那句固定提示（94 字，三条抓包相同）。
                ("toolResultSizeBytes", json!(94)),
                ("toolResultTokensEst", json!(15)),
                ("toolResultMediaBlocks", json!(0)),
                ("toolResultWillPersist", json!(false)),
                ("toolInputSizeBytes", json!(tu.input_len)),
                ("queryChainId", json!(&chain_id)),
                ("queryDepth", json!(query_depth)),
                ("requestId", json!(rid)),
            ] {
                ok[k] = v;
            }
            push(td, "tengu_tool_use_success", ok.clone());
            dd.push(identity.dd_entry(
                "tengu_tool_use_success",
                &ctx(td),
                dd_model,
                snake_flat(&ok),
            ));
        }
        if let Some(done) = &agent_done {
            let t1 = ms(t_end, 1);
            // 以 SubagentHandback 收尾的没有 stop hook，turn 之后先报一条「工具结果结束了这一轮」。
            let names: &[&str] =
                if handback.is_some() { &["turn"] } else { &["hook_stop_handler", "turn"] };
            for name in names {
                push(t1, "tengu_feature_ok", feature(name));
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t1), dd_model, feature(name)));
            }
            if handback.is_some() {
                push(
                    t1,
                    "tengu_mcp_tool_result_ended_turn",
                    json!({ "queryChainId": &chain_id, "queryDepth": query_depth, "source": "tool" }),
                );
            }
            let started: DateTime<Utc> = done.started.unwrap_or(call.started_at).into();
            let elapsed = (t1 - started).num_milliseconds().max(total);
            push(t1, "tengu_turn_end", turn_end("completed", elapsed, None));
            // 2.1.285 的收尾比 turn_end 晚 60 来毫秒、没有 `lively_waffle`（`cap/2.1.285`
            // 06:57:29.511 → .574）；2.1.280 的 `lively_waffle` 带 `flagged: false`。以 SubagentHandback
            // 收尾的紧接着报，照旧有 `lively_waffle`，另多一条交还提示（07:51:29.455）。
            let t2 = ms(t1, if v285 && handback.is_none() { 63 } else { 3 });
            if handback.is_some() {
                push(t2, "tengu_feature_ok", feature("agent_handback_pointer_notice"));
                dd.push(identity.dd_entry(
                    "tengu_feature_ok",
                    &ctx(t2),
                    dd_model,
                    feature("agent_handback_pointer_notice"),
                ));
            }
            if !v285 || handback.is_some() {
                let lively = json!({ "feature_name": "lively_waffle", "flagged": false });
                push(t2, "tengu_feature_ok", lively.clone());
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t2), dd_model, lively));
            }
            push(
                t2,
                "tengu_agent_tool_completed",
                json!({
                    "agent_type": call.agent.agent_type.as_deref().unwrap_or("general-purpose"),
                    "model": &display_model,
                    "prompt_char_count": done.prompt_chars,
                    // 子代理最后那条回复的正文字数（`cap/2.1.285`：2272，正是那条 textContentLength；
                    // `cap/2.1.280` 那条以 SubagentHandback 工具收尾，正文 0）。
                    "response_char_count": call.text_chars,
                    "assistant_message_count": done.steps,
                    "total_tool_uses": done.tool_uses,
                    "duration_ms": elapsed + 16,
                    "total_tokens": done.prev_total,
                    "is_built_in_agent": builtin_agent.is_some(),
                    "is_async": true,
                    "agent_depth": 1,
                    "final_model": &display_model,
                    "model_swapped": false
                }),
            );
            push(
                t2,
                "tengu_cache_eviction_hint",
                json!({
                    "scope": "subagent_end",
                    "last_request_id": call.request_id.as_deref().unwrap_or("")
                }),
            );
            let t3 = ms(t2, 1);
            for name in ["melodic_wolf", "task_local_agent", "subagent_complete"] {
                push(t3, "tengu_feature_ok", feature(name));
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t3), dd_model, feature(name)));
            }
        }

        // `take_tpl` 借着 `tpl_events`/`tpl_dd`，到这里已经不再用它，可以并进主队列。
        if kind.is_agent() && betas_session != betas_own {
            for (_, e) in events.iter_mut() {
                let own = matches!(
                    e["event_data"]["event_name"].as_str(),
                    Some("tengu_api_query" | "tengu_api_success" | "tengu_agent_tool_completed")
                );
                if own && let Some(d) = e.get_mut("event_data").and_then(|d| d.as_object_mut()) {
                    d.insert("betas".into(), json!(&betas_own));
                }
            }
        }
        // `/model` 的「Hi」探测：官方只有这一条字段极少的 `tengu_api_success`（`cap/auto-2.1.285-20260930`
        // 四条逐字同序），前后的规范化、断点、api_query 与收尾一概没有。
        if kind == Kind::ModelValidation {
            events.clear();
            dd.clear();
            if !failed {
                let ok = json!({
                    "requestId": call.request_id.as_deref().unwrap_or(""),
                    "clientRequestId": call.client_request_id.as_deref().unwrap_or(""),
                    "querySource": query_source,
                    "model": &resp_model,
                    "inputTokens": call.input_tokens,
                    "outputTokens": call.output_tokens,
                    "cachedInputTokens": call.cache_read_tokens,
                    "uncachedInputTokens": call.cache_creation_tokens,
                    "durationMs": total,
                    "durationMsIncludingRetries": total,
                    "attempt": 1,
                    "dispatch": config::CC_DISPATCH_ID,
                    "stop_reason": call.stop_reason.as_deref().unwrap_or("max_tokens"),
                    "requestBodyEncoding": "identity",
                    "requestBodyChars": body_chars,
                    "gzipSkipReason": gzip_skip
                });
                events.push((
                    t_end,
                    identity.event("tengu_api_success", t_end, &ctx(t_end), ok.clone()),
                ));
                dd.push(identity.dd_entry(
                    "tengu_api_success",
                    &ctx(t_end),
                    dd_model,
                    snake_flat(&ok),
                ));
            }
        }
        events.extend(tpl_events);
        events.extend(main_side);
        dd.extend(tpl_dd);
        BuiltEvents { events, dd, probe_side }
    }

    /// 把某会话扣住的侧查询按顺序补发（主线程请求到了，或扣得太久）。
    fn replay_deferred(st: &mut State, key: &(i64, String)) {
        let deferred =
            st.sessions.get_mut(key).map(|s| std::mem::take(&mut s.deferred)).unwrap_or_default();
        for (c, prev_end, _) in deferred {
            Self::process(st, c, false, prev_end);
        }
    }

    /// 取走到期该发的：事件攒满 [`config::TELEMETRY_EVENT_FLUSH_SECS`]、Datadog 攒满
    /// [`config::TELEMETRY_DATADOG_FLUSH_SECS`]、指标攒满 [`config::TELEMETRY_METRICS_FLUSH_SECS`]，
    /// 或任一路条数到了 [`config::TELEMETRY_BATCH_MAX`]。每个会话一份 [`Flush`]，同一张凭证
    /// 的多个会话各发各的。
    pub fn take_due(&self, now: Instant) -> Vec<Flush> {
        let mut st = self.0.state.lock();
        let org_uuids = st.org_uuid.clone();
        let mut out = Vec::new();
        for ((cred_id, session), p) in st.pending.iter_mut() {
            let due = |since: Option<Instant>, secs: u64, n: usize| {
                n >= config::TELEMETRY_BATCH_MAX
                    || since.is_some_and(|s| now.duration_since(s) >= Duration::from_secs(secs))
            };
            let mut f = Flush {
                cred_id: *cred_id,
                session_id: session.clone(),
                version: p.version.clone(),
                events: Vec::new(),
                dd: Vec::new(),
                metrics: None,
            };
            // 指标先算：导出会顺手往事件/日志队列里各塞一条 `internal_metrics_export`，退出
            // 收尾时三路同时到期，那两条得赶上同一批（真实退出批次里它就在队尾那串中间）。
            // 指标只按时间到期（`0` 条永远不触发按条数那一路），攒多少条都是一发聚合。
            if !p.metrics.is_empty()
                && due(p.metrics_since, config::TELEMETRY_METRICS_FLUSH_SECS, 0)
            {
                let calls = std::mem::take(&mut p.metrics);
                p.metrics_since = None;
                f.metrics = Some(metrics_body(
                    &calls,
                    &p.version,
                    &p.subscription_type,
                    org_uuids.get(cred_id).map(String::as_str),
                ));
                if let Some(id) = p.identity.clone() {
                    let t = p.export_at.take().unwrap_or_else(Utc::now);
                    let start: DateTime<Utc> = p.started_wall.map(Into::into).unwrap_or(t);
                    let (model, betas, prompt_id) =
                        (p.model.clone(), p.betas.clone(), p.prompt_id.clone());
                    let ctx = EventCtx {
                        model: &model,
                        betas: &betas,
                        prompt_id: &prompt_id,
                        uptime_secs: ((t - start).num_milliseconds().max(0) as f64) / 1000.0,
                    };
                    let extra = json!({ "feature_name": "internal_metrics_export" });
                    p.events.push((t, id.event("tengu_feature_ok", t, &ctx, extra.clone())));
                    p.events_since.get_or_insert(now);
                    p.dd.push(id.dd_entry(
                        "tengu_feature_ok",
                        &ctx,
                        model.trim_end_matches("[1m]"),
                        extra,
                    ));
                    p.dd_since.get_or_insert(now);
                }
            }
            if !p.events.is_empty()
                && due(p.events_since, config::TELEMETRY_EVENT_FLUSH_SECS, p.events.len())
            {
                let mut evs = std::mem::take(&mut p.events);
                evs.sort_by_key(|(t, _)| *t);
                f.events = evs.into_iter().map(|(_, v)| v).collect();
                p.events_since = None;
            }
            if !p.dd.is_empty() && due(p.dd_since, config::TELEMETRY_DATADOG_FLUSH_SECS, p.dd.len())
            {
                // Datadog 那份官方也是按发生顺序排的；补发的侧查询会晚于后来的主线程入队，
                // 按各条自带的 `process_metrics.uptime` 排回去。
                let mut dd = std::mem::take(&mut p.dd);
                let uptime = |v: &Value| {
                    v.get("process_metrics")
                        .and_then(|m| m.get("uptime"))
                        .and_then(|u| u.as_f64())
                        .unwrap_or(0.0)
                };
                dd.sort_by(|a, b| uptime(a).total_cmp(&uptime(b)));
                f.dd = dd;
                p.dd_since = None;
            }
            if !f.events.is_empty() || !f.dd.is_empty() || f.metrics.is_some() {
                out.push(f);
            }
        }
        st.pending.retain(|_, p| !p.events.is_empty() || !p.dd.is_empty() || !p.metrics.is_empty());
        out
    }

    /// 久无请求的会话按「客户端退出」收尾：补上退出那一串事件，并把这个会话攒着的三路
    /// 全部标成立刻到期。
    ///
    /// 真实客户端退出时（`cap/2.1.260-1`，17:10:24）会在一秒内连发三样：metrics、event_logging
    /// 批次（队尾是 `tengu_config_cache_stats` → `lsp_shutdown` → `swarm_session_cleanup` →
    /// `internal_metrics_export` → `tengu_cache_eviction_hint{scope:session_end,last_request_id}`）、
    /// Datadog（那三条 feature_ok）。luban 看不见客户端退出，只能以
    /// [`config::TELEMETRY_SESSION_IDLE_SECS`] 没有请求为准——真实用户也常把会话开着几小时
    /// 再关，这期间保活挂在这个会话上的空闲事件正好把这段空白填成「开着没说话」。
    pub fn gc(&self, now: Instant) {
        let idle = Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS);
        let mut st = self.0.state.lock();
        // 扣住太久的侧查询：没等到主线程请求，按会话现有的 prompt id 补发。
        let hold = Duration::from_secs(config::TELEMETRY_SIDE_QUERY_HOLD_SECS);
        let stale: Vec<(i64, String)> = st
            .sessions
            .iter()
            .filter(|(_, s)| s.deferred.iter().any(|(_, _, at)| now.duration_since(*at) >= hold))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            Self::replay_deferred(&mut st, &k);
        }
        let expired: Vec<((i64, String), Session)> = {
            let keys: Vec<(i64, String)> = st
                .sessions
                .iter()
                .filter(|(_, s)| now.duration_since(s.last_seen) >= idle)
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter().filter_map(|k| st.sessions.remove(&k).map(|s| (k, s))).collect()
        };
        // 记住这些 id 已经「退出」过，再来按 resume；太久的忘掉，免得这张表只增不减。
        let memory = Duration::from_secs(config::TELEMETRY_ENDED_SESSION_MEMORY_SECS);
        st.ended.retain(|_, t| now.duration_since(*t) < memory);
        // 设备级的两张表按设备算，客户端一多（或设备 id 在轮换）会一直长：启动探测的记号过了
        // 「已退出会话」的记忆期就没用了；默认模型那张也按同样的期限，最近没再更新、设备上也没有
        // 会话在跑的就忘掉（下次再以那台设备第一条主线程请求的模型为准）。
        let horizon = std::time::SystemTime::now()
            .checked_sub(memory)
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        st.process_starts.retain(|_, (_, at)| *at >= horizon);
        let live: std::collections::HashSet<(i64, String)> =
            st.sessions.iter().map(|((c, _), s)| (*c, s.device_id.clone())).collect();
        st.device_default_model
            .retain(|k, (_, at)| live.contains(k) || now.duration_since(*at) < memory);
        for (k, _) in &expired {
            st.ended.insert(k.clone(), now);
        }
        for ((cred_id, session_id), s) in expired {
            // 被 `/clear` 换掉的会话：进程还在，没有退出那一串。`-p` 的退出收尾在它那份模板的
            // turn 段里，跑完那一轮就报过了。
            if s.cleared || s.sdk {
                continue;
            }
            let identity = Identity {
                session_id: session_id.clone(),
                device_id: s.device_id,
                account_uuid: s.account_uuid,
                organization_uuid: st.org_uuid.get(&cred_id).cloned(),
                subscription_type: s.subscription_type,
                version: s.version.clone(),
                agent_id: None,
                vcs: s.git_repo.filter(|r| *r).map(|_| "git"),
                parent_session_id: s.parent_session_id.clone(),
                sdk: s.sdk,
            };
            let model = s.last_model.unwrap_or(s.default_model);
            let dd_model = model.trim_end_matches("[1m]").to_string();
            let t0 = Utc::now();
            let ms = |d: i64| t0 + chrono::Duration::milliseconds(d);
            let start: DateTime<Utc> = s.started_wall.into();
            let uptime =
                |dt: DateTime<Utc>| ((dt - start).num_milliseconds().max(0) as f64) / 1000.0;
            let ctx = |dt: DateTime<Utc>| EventCtx {
                model: &model,
                betas: &s.betas,
                prompt_id: &s.prompt_id,
                uptime_secs: uptime(dt),
            };
            let feature = |name: &str| json!({ "feature_name": name });
            let mut events: Vec<(DateTime<Utc>, Value)> = Vec::with_capacity(5);
            let mut dd: Vec<Value> = Vec::with_capacity(3);
            // 配置缓存命中数随会话长短走：抓包里 7 秒的会话 3779、一小时的 11054。
            let cache_hits = 3_000 + u64::from(s.prompt_index) * 2_500;
            events.push((
                t0,
                identity.event(
                    "tengu_config_cache_stats",
                    t0,
                    &ctx(t0),
                    json!({ "cache_hits": cache_hits, "cache_misses": 0, "hit_rate": 1 }),
                ),
            ));
            // `internal_metrics_export` 不在这里：它只在真有指标要导出时才出现（由
            // [`Self::take_due`] 导出时按 `export_at` 的时间戳补进来）。
            for (offset, name) in [(1, "lsp_shutdown"), (6, "swarm_session_cleanup")] {
                let t = ms(offset);
                events.push((t, identity.event("tengu_feature_ok", t, &ctx(t), feature(name))));
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t), &dd_model, feature(name)));
            }
            let t = ms(544);
            events.push((
                t,
                identity.event(
                    "tengu_cache_eviction_hint",
                    t,
                    &ctx(t),
                    json!({
                        "scope": "session_end",
                        "last_request_id": s.last_main_request_id.as_deref().unwrap_or("")
                    }),
                ),
            ));

            // 入队并强制到期：把三路的起算点拨到很久以前，下一次 `take_due` 就全发出去。
            let long_ago = now.checked_sub(Duration::from_secs(86_400)).unwrap_or(now);
            let p = st.pending.entry((cred_id, session_id)).or_default();
            if p.version.is_empty() {
                p.version = identity.version.clone();
                p.subscription_type = identity.subscription_type.clone();
            }
            // 导出事件要用的上下文以这个会话为准（pending 里那份可能是空的——比如指标早已
            // 导出、事件也早已发完，这里是重新建的条目）。
            p.identity = Some(identity);
            p.model = model;
            p.betas = s.betas;
            p.prompt_id = s.prompt_id;
            p.started_wall = Some(s.started_wall);
            p.events.extend(events);
            p.events_since = Some(long_ago);
            p.dd.extend(dd);
            p.dd_since = Some(long_ago);
            if !p.metrics.is_empty() {
                p.metrics_since = Some(long_ago);
                p.export_at = Some(ms(542));
            }
        }
    }
}

/// `tengu_api_success` 的 meta 转成 Datadog 扁平字段：键 camel → snake，去掉 base 已有的三项。
fn snake_flat(meta: &Value) -> Value {
    let mut out = Map::new();
    if let Some(obj) = meta.as_object() {
        for (k, v) in obj {
            if matches!(k.as_str(), "renderer_mode" | "subscription_type" | "cc_prompt_id") {
                continue;
            }
            out.insert(camel_to_snake(k), v.clone());
        }
    }
    Value::Object(out)
}

/// 把一批调用聚合成 OTel 指标请求体（形态取自 `cap/2.1.258/00030`）。
///
/// 官方还带 `user.email` 与 `user.account_id`——凭证里没有这两项，缺省。
fn metrics_body(
    calls: &[CallMetric],
    version: &str,
    subscription_type: &str,
    org: Option<&str>,
) -> Value {
    let ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let attrs = |c: &CallMetric| {
        let mut a = Map::new();
        a.insert("user.id".into(), json!(c.device_id));
        a.insert("session.id".into(), json!(c.session_id));
        if let Some(org) = org {
            a.insert("organization.id".into(), json!(org));
        }
        a.insert("user.account_uuid".into(), json!(c.account_uuid));
        a.insert("terminal.type".into(), json!("vscode"));
        a
    };
    let point =
        |a: Map<String, Value>, v: Value| json!({ "attributes": a, "value": v, "timestamp": &ts });

    // session.count：每个新会话一条。
    let mut sessions: Vec<Value> = Vec::new();
    for c in calls.iter().filter(|c| c.new_session) {
        let mut a = attrs(c);
        a.insert(
            "start_type".into(),
            json!(if c.continued {
                "continue"
            } else if c.resumed {
                "resume"
            } else {
                "fresh"
            }),
        );
        sessions.push(point(a, json!(1)));
    }
    // cost / token：按 (session, model, category, effort) 聚合。
    let mut cost: Vec<(&CallMetric, f64)> = Vec::new();
    let mut tokens: Vec<(&CallMetric, [i64; 4])> = Vec::new();
    let mut active: Vec<(&CallMetric, (f64, f64))> = Vec::new();
    let same = |a: &CallMetric, b: &CallMetric| {
        a.session_id == b.session_id
            && a.model == b.model
            && a.category == b.category
            && a.effort == b.effort
            && a.agent_name == b.agent_name
    };
    for c in calls {
        if c.usage {
            match cost.iter_mut().find(|(k, _)| same(k, c)) {
                Some((_, v)) => *v += c.cost,
                None => cost.push((c, c.cost)),
            }
            match tokens.iter_mut().find(|(k, _)| same(k, c)) {
                Some((_, v)) => {
                    v[0] += c.input;
                    v[1] += c.output;
                    v[2] += c.cache_read;
                    v[3] += c.cache_creation;
                }
                None => tokens.push((c, [c.input, c.output, c.cache_read, c.cache_creation])),
            }
        }
        // 只有辅助调用（离开回顾、标题这类）的那一批不报活跃时长（`cap/2.1.285/00104`、
        // `00161`，`cap/2.1.280/00089`）：用户没在用、客户端也没在跑一轮。
        if c.category == "auxiliary" {
            continue;
        }
        match active.iter_mut().find(|(k, _)| k.session_id == c.session_id) {
            Some((_, v)) => {
                v.0 += c.user_secs;
                v.1 += c.cli_secs;
            }
            None => active.push((c, (c.user_secs, c.cli_secs))),
        }
    }
    let with_model = |c: &CallMetric| {
        let mut a = attrs(c);
        a.insert("model".into(), json!(c.model));
        a.insert("query_source".into(), json!(c.category));
        if let Some(e) = &c.effort {
            a.insert("effort".into(), json!(e));
        }
        if let Some(n) = &c.agent_name {
            a.insert("agent.name".into(), json!(n));
        }
        a
    };
    let cost_points: Vec<Value> =
        cost.iter().map(|(c, v)| point(with_model(c), json!(v))).collect();
    let mut token_points: Vec<Value> = Vec::new();
    for (c, v) in &tokens {
        for (i, ty) in ["input", "output", "cacheRead", "cacheCreation"].iter().enumerate() {
            let mut a = with_model(c);
            a.insert("type".into(), json!(ty));
            token_points.push(point(a, json!(v[i])));
        }
    }
    let mut active_points: Vec<Value> = Vec::new();
    for (c, (user, cli)) in &active {
        for (ty, v) in [("user", *user), ("cli", *cli)] {
            let mut a = attrs(c);
            a.insert("type".into(), json!(ty));
            active_points.push(point(a, json!((v * 100.0).round() / 100.0)));
        }
    }
    let mut metrics = Vec::new();
    if !sessions.is_empty() {
        metrics.push(json!({
            "name": "claude_code.session.count",
            "description": "Count of CLI sessions started",
            "unit": "",
            "data_points": sessions
        }));
    }
    // 一批里全是失败请求时这两项没有任何数据点——官方那种批次里它们压根不出现，
    // 别发一个空数组。
    if !cost_points.is_empty() {
        metrics.push(json!({
            "name": "claude_code.cost.usage",
            "description": "Cost of the Claude Code session",
            "unit": "USD",
            "data_points": cost_points
        }));
    }
    if !token_points.is_empty() {
        metrics.push(json!({
            "name": "claude_code.token.usage",
            "description": "Number of tokens used",
            "unit": "tokens",
            "data_points": token_points
        }));
    }
    if !active_points.is_empty() {
        metrics.push(json!({
            "name": "claude_code.active_time.total",
            "description": "Total active time in seconds",
            "unit": "s",
            "data_points": active_points
        }));
    }
    json!({
        "resource_attributes": {
            "service.name": "claude-code",
            "service.version": version,
            "os.type": "darwin",
            "os.version": "27.0.0",
            "host.arch": "arm64",
            "aggregation.temporality": "delta",
            "user.customer_type": "claude_ai",
            "user.subscription_type": subscription_type
        },
        "metrics": metrics
    })
}

/// 定时把攒下的遥测发出去。每 5 秒看一眼到期的；发送用该凭证自己的出站客户端（配了代理
/// 走代理）与新鲜的 access_token。发失败只记日志——遥测丢一批不影响任何转发。
pub async fn run_flusher(
    t: Telemetry,
    store: Arc<crate::store::CredentialStore>,
    clients: Arc<crate::clients::ClientPool>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let now = Instant::now();
        t.gc(now);
        for f in t.take_due(now) {
            let Ok(Some(cred)) = store.get(f.cred_id) else { continue };
            // 订阅未生效暂停中的号同样不发：上游对它每一发都是 403。
            if cred.is_banned() || cred.is_subscription_paused() {
                continue;
            }
            let Ok(client) = clients.for_credential(&cred) else { continue };
            let token = match crate::store::ensure_fresh_token(&store, &clients, &cred).await {
                Ok(crate::store::TokenAttempt::Ready(t)) => t,
                _ => {
                    tracing::debug!(
                        cred_id = cred.id,
                        "telemetry: no fresh token, dropping this batch"
                    );
                    continue;
                }
            };
            send_flush(&client, &token, &cred, f).await;
        }
    }
}

/// 发一张凭证的这一批。
pub async fn send_flush(
    client: &wreq::Client,
    token: &str,
    cred: &crate::credentials::Credential,
    f: Flush,
) {
    // 会话 id 只展示前 8 位，与转发日志里 `device=` 的脱敏口径一致。
    let session: String = f.session_id.chars().take(8).collect();
    for chunk in f.events.chunks(config::TELEMETRY_BATCH_MAX) {
        let st = post_event_logging(client, token, &f.version, chunk).await;
        report(cred, &session, "event_logging", chunk.len(), st);
    }
    for chunk in f.dd.chunks(config::TELEMETRY_BATCH_MAX) {
        let st = post_datadog(client, chunk).await;
        report(cred, &session, "datadog", chunk.len(), st);
    }
    if let Some(body) = &f.metrics {
        let st = post_metrics(client, token, &f.version, body).await;
        report(cred, &session, "metrics", 1, st);
    }
}

fn report(
    cred: &crate::credentials::Credential,
    session: &str,
    what: &str,
    n: usize,
    status: Option<u16>,
) {
    match status {
        Some(s) if s < 400 => {
            tracing::debug!(cred_id = cred.id, cred = %cred.label, session, what, n, status = s, "telemetry sent")
        }
        Some(s) => {
            tracing::warn!(cred_id = cred.id, cred = %cred.label, session, what, n, status = s, "telemetry rejected upstream")
        }
        None => {
            tracing::warn!(cred_id = cred.id, cred = %cred.label, session, what, n, "telemetry request failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 重构对照 ----
    //
    // 设了 `LUBAN_TELEMETRY_DUMP=<目录>` 时，每次 `ingest` 之后把全部待发批次规范化后追加到
    // `<目录>/<测试名>.txt`：随机 uuid 按首次出现编号，时间戳换成相对测试起点的毫秒。改动
    // `process` 这类大段代码前后各跑一遍、diff 两个目录，就能确认产出的事件逐字没变。
    // 测试里的「当前时刻」统一走 [`frozen_now`]，否则两次运行之间的微秒抖动会让毫秒级字段跳 1。

    thread_local! {
        /// 每个测试线程首次取时定下、取整到秒，之后不再走。
        static FROZEN: SystemTime = {
            let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
            SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
        };
        static DUMP_SEQ: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static DUMP_UUIDS: std::cell::RefCell<HashMap<String, usize>> =
            std::cell::RefCell::new(HashMap::new());
    }

    fn frozen_now() -> SystemTime {
        FROZEN.with(|t| *t)
    }

    pub(super) fn dump_pending(st: &State) {
        use std::fmt::Write as _;
        let Some(dir) = std::env::var_os("LUBAN_TELEMETRY_DUMP") else { return };
        let name = std::thread::current().name().unwrap_or("main").replace("::", "-");
        let seq = DUMP_SEQ.with(|c| c.replace(c.get() + 1));
        let ts = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut out = format!("== ingest {seq}\n");
        let mut keys: Vec<_> = st.pending.keys().collect();
        keys.sort();
        for k in keys {
            let p = &st.pending[k];
            let _ = writeln!(
                out,
                "-- {k:?} version={} sub={} model={} betas={} prompt={} started={:?} export_at={:?}",
                p.version,
                p.subscription_type,
                p.model,
                p.betas,
                p.prompt_id,
                p.started_wall.map(|t| ts(t.into())),
                p.export_at.map(ts),
            );
            let _ = writeln!(out, "identity {:?}", p.identity);
            for (t, e) in &p.events {
                let _ = writeln!(out, "ev {} {e}", ts(*t));
            }
            for d in &p.dd {
                let _ = writeln!(out, "dd {d}");
            }
            for m in &p.metrics {
                let _ = writeln!(out, "metric {m:?}");
            }
        }
        let text = DUMP_UUIDS.with(|u| normalize_dump(&out, &mut u.borrow_mut()));
        let path = std::path::Path::new(&dir).join(format!("{name}.txt"));
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        std::io::Write::write_all(&mut f, text.as_bytes()).unwrap();
    }

    /// uuid → `<uN>`（按首次出现编号），一天以内的 RFC3339 时间戳 → `T+毫秒`（相对
    /// [`frozen_now`]；`build_time` 那种固定时刻原样留着），base64 的 JSON 串先解开再处理，
    /// 随真实日期走的 `build_age_mins` 抹掉。
    fn normalize_dump(s: &str, uuids: &mut HashMap<String, usize>) -> String {
        let s = &decode_b64_strings(s);
        let base: DateTime<Utc> = frozen_now().into();
        let b = s.as_bytes();
        let is_uuid = |w: &[u8]| {
            w.len() == 36
                && w.iter().enumerate().all(|(i, c)| match i {
                    8 | 13 | 18 | 23 => *c == b'-',
                    _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
                })
        };
        let is_ts_head = |w: &[u8]| {
            w.len() >= 11
                && w[..4].iter().all(u8::is_ascii_digit)
                && w[4] == b'-'
                && w[7] == b'-'
                && w[10] == b'T'
        };
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
            if boundary && i + 36 <= b.len() && is_uuid(&b[i..i + 36]) {
                let next = uuids.len() + 1;
                let n = *uuids.entry(s[i..i + 36].to_string()).or_insert(next);
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("<u{n}>"));
                i += 36;
                continue;
            }
            if boundary && is_ts_head(&b[i..b.len().min(i + 11)]) {
                let end = b[i..]
                    .iter()
                    .position(|c| !(c.is_ascii_digit() || b"-:T.Z+".contains(c)))
                    .map_or(b.len(), |p| i + p);
                if let Ok(t) = DateTime::parse_from_rfc3339(&s[i..end])
                    && (t.with_timezone(&Utc) - base).num_hours().abs() < 24
                {
                    let ms = (t.with_timezone(&Utc) - base).num_milliseconds();
                    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("T{ms:+}"));
                    i = end;
                    continue;
                }
            }
            for key in ["\"build_age_mins\":", "\"buildAgeMins\":"] {
                if s[i..].starts_with(key) {
                    let digits =
                        b[i + key.len()..].iter().take_while(|c| c.is_ascii_digit()).count();
                    out.push_str(key);
                    out.push('N');
                    i += key.len() + digits;
                }
            }
            let Some(ch) = s[i..].chars().next() else { break };
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }

    /// `"eyJ…"` 这种 base64 编码的 JSON 串换成 `b64:{…}`，好让里面的 uuid 与时间戳也被规范化。
    fn decode_b64_strings(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(p) = rest.find("\"eyJ") {
            out.push_str(&rest[..p + 1]);
            rest = &rest[p + 1..];
            let len = rest
                .bytes()
                .take_while(|c| c.is_ascii_alphanumeric() || b"+/=".contains(c))
                .count();
            match STANDARD.decode(&rest[..len]).ok().and_then(|v| String::from_utf8(v).ok()) {
                Some(json) if rest[len..].starts_with('"') => {
                    out.push_str("b64:");
                    out.push_str(&json);
                    rest = &rest[len..];
                }
                _ => {}
            }
        }
        out.push_str(rest);
        out
    }

    fn identity() -> Identity {
        Identity {
            session_id: "111e3644-948f-43fb-9bc2-cac60e65fd32".into(),
            device_id: "b9".repeat(32),
            account_uuid: "9922ef8e-7945-4f5a-ab4f-cf5f521531df".into(),
            organization_uuid: Some("09520b85-f6b6-432f-97e2-6ecb804a083f".into()),
            subscription_type: "team".into(),
            version: "2.1.258".into(),
            agent_id: None,
            ..Default::default()
        }
    }

    /// 与抓包一致的 CC 请求体（截取要紧的字段）。
    fn cc_body(last_user_text: bool) -> Vec<u8> {
        let last = if last_user_text {
            json!({"role":"user","content":[{"type":"text","text":"<system-reminder>x</system-reminder>"},{"type":"text","text":"hello there"}]})
        } else {
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]})
        };
        json!({
            "model": "claude-opus-5",
            "messages": [
                {"role":"user","content":"first"},
                {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]},
                last
            ],
            "system": [
                {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=0f7f8; cc_prompt_id=6c079143-0c53-4c48-817d-105460b3f622;"},
                {"type":"text","text":"You are Claude Code"},
                {"type":"text","text":"base prompt","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}},
                {"type":"text","text":"While auto mode is active: rules","cache_control":{"type":"ephemeral","ttl":"1h"}}
            ],
            "tools": [
                {"name":"Bash","description":"run","input_schema":{"type":"object"}},
                {"name":"DeferredToolPlaceholder","description":"d","input_schema":{"type":"object"},"defer_loading":true}
            ],
            "thinking": {"type":"adaptive"},
            "output_config": {"effort":"high"},
            "metadata": {"user_id": "{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"},
            "max_tokens": 64000,
            "stream": true
        })
        .to_string()
        .into_bytes()
    }

    fn call(body: Vec<u8>, request_id: &str, stop: &str) -> ApiCall {
        ApiCall {
            cred_id: 7,
            account_uuid: Some("9922ef8e-7945-4f5a-ab4f-cf5f521531df".into()),
            org_type: Some("claude_team".into()),
            body: Bytes::from(body),
            betas: Some(
                "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,effort-2025-11-24"
                    .into(),
            ),
            session_header: None,
            ua_out: "claude-cli/2.1.258 (external, cli)".into(),
            organization_id: Some("09520b85-f6b6-432f-97e2-6ecb804a083f".into()),
            started_at: frozen_now() - Duration::from_secs(8),
            ttft_ms: Some(1800),
            total_ms: 6118,
            request_id: Some(request_id.into()),
            client_request_id: Some("3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e".into()),
            agent: AgentHeaders::default(),
            message_id: Some("msg_011Cedjuoa4oBPzoB2CSUNEB".into()),
            stop_reason: Some(stop.into()),
            resp_model: Some("claude-opus-5".into()),
            input_tokens: 2,
            output_tokens: 31,
            cache_read_tokens: 26736,
            cache_creation_tokens: 8729,
            text_chars: 87,
            reply_input_chars: 87,
            thinking_chars: 0,
            saw_thinking: false,
            tool_use_lens: Vec::new(),
            tool_calls: Vec::new(),
            cost_usd: Some(0.18),
            speed: None,
            failure: None,
            aborted: false,
        }
    }

    /// 把一条调用改成「客户端那头失败了」。
    fn failed(mut c: ApiCall, status: Option<u16>, etype: &str, message: &str) -> ApiCall {
        c.stop_reason = None;
        c.message_id = None;
        c.resp_model = None;
        c.input_tokens = 0;
        c.output_tokens = 0;
        c.cache_read_tokens = 0;
        c.cache_creation_tokens = 0;
        c.text_chars = 0;
        c.cost_usd = None;
        c.failure = Some(CallFailure {
            status,
            error_type: (!etype.is_empty()).then(|| etype.to_string()),
            message: message.to_string(),
            in_band: status.is_none() && !etype.is_empty(),
        });
        c
    }

    /// `cc_body` 里 metadata 的 session_id；待发批次按 (凭证, 会话) 取。
    const SESSION: &str = "4dc73702-d904-4887-809d-17b93cc5357c";
    fn key() -> (i64, String) {
        (7, SESSION.to_string())
    }

    /// 事件名；GrowthBook 曝光事件没有 `event_name`，用 `experiment_id`。
    fn ev_name(e: &Value) -> &str {
        e["event_data"]["event_name"]
            .as_str()
            .or_else(|| e["event_data"]["experiment_id"].as_str())
            .unwrap_or("")
    }

    /// 事件时间戳；GrowthBook 那类叫 `timestamp`。
    fn ev_ts(e: &Value) -> &str {
        e["event_data"]["client_timestamp"]
            .as_str()
            .or_else(|| e["event_data"]["timestamp"].as_str())
            .unwrap_or("")
    }

    /// 新会话的首批带完整的启动那串 + 每轮输入那串 + 首轮版本检查，身份与占位符按会话替换；
    /// 同一会话第二条请求不再有启动与首轮那两串；握手任务每个会话排一次。
    #[test]
    fn new_session_gets_the_startup_burst() {
        let t = Telemetry::default();
        t.ingest(call(cc_body(true), "req_1", "end_turn"));
        {
            let st = t.0.state.lock();
            let p = &st.pending[&key()];
            let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
            assert!(
                names.len() > 150,
                "启动串 121 + 输入串 + api 链 + 收尾串，实际 {}",
                names.len()
            );
            for expected in [
                "tengu_cli_flags",
                "tengu_started",
                "tengu_init",
                "tengu_startup_telemetry",
                "tengu_policy_limits_fetch",
                "tengu_carved_slate",
                "tengu_input_prompt",
                "tengu_api_query",
                "tengu_policy_limits_cache_state_at_first_prompt",
                "tengu_api_success",
                "tengu_prompt_suggestion",
                "tengu_tip_shown",
                "tengu_native_auto_updater_start",
                "tengu_native_version_cleanup",
            ] {
                assert!(names.contains(&expected), "缺 {expected}");
            }
            assert_eq!(names.iter().filter(|n| **n == "tengu_skill_loaded").count(), 26);
            let init = p
                .events
                .iter()
                .find(|(_, e)| ev_name(e) == "tengu_init")
                .map(|(_, e)| e["event_data"].clone())
                .unwrap();
            let meta: Value = serde_json::from_slice(
                &STANDARD.decode(init["additional_metadata"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(meta["permissionMode"], "auto", "占位符按请求体替换");
            // 启动早期的事件**只有** `subscription_type`：那会儿界面还没画、用户一个字都
            // 没输，`renderer_mode` 与 `cc_prompt_id` 都还不存在，见 [`super::MetaStage`]。
            assert_eq!(meta["cc_prompt_id"], Value::Null, "启动事件不写 cc_prompt_id");
            assert_eq!(meta["renderer_mode"], Value::Null, "也不写 renderer_mode");
            assert_eq!(meta["subscription_type"], "team", "只有这一项");
            // 界面起来之后、用户提交之前的那几条：有 renderer_mode，仍没有 cc_prompt_id。
            let probe = p
                .events
                .iter()
                .find(|(_, e)| ev_name(e) == "tengu_terminal_probe")
                .map(|(_, e)| e["event_data"].clone())
                .expect("startup 模板里有这一条");
            let probe_meta: Value = serde_json::from_slice(
                &STANDARD.decode(probe["additional_metadata"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(probe_meta["renderer_mode"], "default");
            assert_eq!(probe_meta["cc_prompt_id"], Value::Null, "还没提交就没有这一项");
            // 用户提交之后那一批三项齐全。
            let query = p
                .events
                .iter()
                .find(|(_, e)| ev_name(e) == "tengu_api_query")
                .map(|(_, e)| e["event_data"].clone())
                .unwrap();
            let query_meta: Value = serde_json::from_slice(
                &STANDARD.decode(query["additional_metadata"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(query_meta["renderer_mode"], "default");
            assert_eq!(query_meta["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622");
            assert_eq!(init["session_id"], SESSION);
            assert_eq!(init["model"], "claude-opus-5[1m]");
            let timer = p
                .events
                .iter()
                .filter(|(_, e)| ev_name(e) == "tengu_timer")
                .map(|(_, e)| {
                    serde_json::from_slice::<Value>(
                        &STANDARD
                            .decode(e["event_data"]["additional_metadata"].as_str().unwrap())
                            .unwrap(),
                    )
                    .unwrap()
                })
                .find(|m| m["event"] == "startup")
                .unwrap();
            assert_eq!(timer["resumed"], false);
            assert_eq!(timer["durationMs"], 373);
            let setting = p
                .events
                .iter()
                .find(|(_, e)| ev_name(e) == "tengu_startup_manual_model_config")
                .map(|(_, e)| {
                    serde_json::from_slice::<Value>(
                        &STANDARD
                            .decode(e["event_data"]["additional_metadata"].as_str().unwrap())
                            .unwrap(),
                    )
                    .unwrap()
                })
                .unwrap();
            assert_eq!(setting["settings_file"], "opus[1m]");
            let growth = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_time_shell").unwrap();
            let g = &growth.1["event_data"];
            assert_eq!(growth.1["event_type"], "GrowthbookExperimentEvent");
            assert_eq!(g["environment"], "production");
            assert_eq!(g["user_attributes"], "{\"appVersion\":\"2.1.258\"}");
            assert_eq!(g["experiment_metadata"], "{\"feature_id\":\"tengu_stone_shell\"}");
            assert_eq!(g["auth"]["organization_uuid"], "09520b85-f6b6-432f-97e2-6ecb804a083f");
            assert_eq!(g["session_id"], SESSION);
            // Datadog 只收那几类。
            assert!(p.dd.iter().any(|d| d["message"] == "tengu_started"));
            assert!(
                p.dd.iter().any(|d| d["message"] == "tengu_init" && d["permission_mode"] == "auto")
            );
            assert!(
                p.dd.iter().any(|d| d["feature_name"] == "ca_certs_load" && d["cert_count"] == 144)
            );
            assert!(!p.dd.iter().any(|d| d["message"] == "tengu_skill_loaded"));
        }

        // 同一会话第二轮：有输入串、没有启动串与首轮那串。
        let mut second = call(cc_body(true), "req_2", "end_turn");
        second.started_at = frozen_now();
        t.ingest(second);
        let st = t.0.state.lock();
        let names: Vec<&str> = st.pending[&key()].events.iter().map(|(_, e)| ev_name(e)).collect();
        assert_eq!(names.iter().filter(|n| **n == "tengu_started").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "tengu_native_auto_updater_start").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "tengu_input_prompt").count(), 2);
        assert_eq!(names.iter().filter(|n| **n == "tengu_paste_text").count(), 2, "输入串每轮都有");
        assert_eq!(
            names
                .iter()
                .filter(|n| **n == "tengu_policy_limits_cache_state_at_first_prompt")
                .count(),
            1,
            "首次输入才有"
        );
    }

    #[test]
    fn model_setting_follows_the_settings_alias() {
        assert_eq!(model_setting("claude-opus-5[1m]"), "opus[1m]");
        assert_eq!(model_setting("claude-fable-5-1"), "fable");
        assert_eq!(model_setting("claude-haiku-4-5-20251001"), "haiku");
        assert_eq!(model_setting("claude-sonnet-5"), "sonnet");
    }

    #[test]
    fn sessions_on_one_credential_are_batched_separately() {
        let t = Telemetry::default();
        t.ingest(call(cc_body(true), "req_a", "end_turn"));
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["metadata"]["user_id"] = json!(
            "{\"device_id\":\"aa\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"other-session\"}"
        );
        t.ingest(call(body.to_string().into_bytes(), "req_b", "end_turn"));
        {
            let st = t.0.state.lock();
            assert_eq!(st.pending.len(), 2, "两个会话两个批次");
            assert!(st.pending.contains_key(&key()));
            assert!(st.pending.contains_key(&(7, "other-session".to_string())));
        }
        let due = t
            .take_due(Instant::now() + Duration::from_secs(config::TELEMETRY_EVENT_FLUSH_SECS + 1));
        assert_eq!(due.len(), 2, "同一张凭证两个会话各发各的");
        for f in &due {
            assert_eq!(f.cred_id, 7);
            let sids: std::collections::HashSet<&str> =
                f.events.iter().map(|e| e["event_data"]["session_id"].as_str().unwrap()).collect();
            assert_eq!(sids.len(), 1, "一个批次里只有一个 session_id");
            let dids: std::collections::HashSet<&str> =
                f.dd.iter().map(|e| e["session_id"].as_str().unwrap()).collect();
            assert_eq!(dids.len(), 1);
        }
    }

    #[test]
    fn latest_session_hands_the_real_identity_to_the_keepalive() {
        let t = Telemetry::default();
        assert!(t.latest_session(7, Duration::from_secs(3600)).is_none(), "还没有会话");
        t.ingest(call(cc_body(true), "req_1", "end_turn"));
        let s = t.latest_session(7, Duration::from_secs(3600)).expect("刚有过请求");
        assert_eq!(s.session_id, SESSION);
        assert_eq!(s.device_id.len(), 64);
        assert_eq!(s.account_uuid, "9922ef8e-7945-4f5a-ab4f-cf5f521531df");
        assert_eq!(s.version, "2.1.258");
        assert_eq!(s.model, "claude-opus-5[1m]");
        assert_eq!(
            s.betas,
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
             interleaved-thinking-2025-05-14,redact-thinking-2026-02-12",
            "会话级那份始终带 redact-thinking，哪怕请求头里没有"
        );
        assert_eq!(s.prompt_id, "6c079143-0c53-4c48-817d-105460b3f622");
        assert!(t.latest_session(8, Duration::from_secs(3600)).is_none(), "别的凭证没有");
        assert!(t.latest_session(7, Duration::ZERO).is_none(), "超过闲置上限就不算近期");

        // **侧查询不许把会话级上下文改掉**：标题生成用的是 haiku + 一套完全不同的 beta，
        // 覆盖之后，从它结束到下一条主请求之间，保活挂的身份与指标导出报的就都是标题生成
        // 那套——一个「会话主模型 opus、会话 beta 却是标题那套」的组合，官方不产生。
        let mut title = call(
            json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 32000,
                "stream": true,
                "thinking": {"type": "disabled"},
                "system": [
                    {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.ced; cc_entrypoint=cli; cch=b1b2c;"},
                    {"type":"text","text":"You are naming a coding session so the user can pick it out of a long list of sessions."}
                ],
                "messages": [{"role":"user","content":"<session>\nhi\n</session>\n\nWrite the title"}],
                "metadata": {"user_id": "{\"device_id\":\"b9\",\"account_uuid\":\"a\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"}
            })
            .to_string()
            .into_bytes(),
            "req_title",
            "end_turn",
        );
        title.betas = Some(
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             structured-outputs-2025-12-15"
                .into(),
        );
        t.ingest(title);
        // 侧查询先被扣住等新一轮 prompt id；覆盖发生在**补发**那一刻，所以要让它到期。
        assert_eq!(t.0.state.lock().sessions[&key()].deferred.len(), 1, "先扣住");
        t.gc(Instant::now() + Duration::from_secs(config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1));
        assert!(t.0.state.lock().sessions[&key()].deferred.is_empty(), "补发了");

        let after = t.latest_session(7, Duration::from_secs(3600)).expect("会话还在");
        assert_eq!(after.model, s.model, "会话主模型不该被标题生成改成 haiku");
        assert_eq!(after.betas, s.betas, "会话级 beta 不该被标题生成那套覆盖");
        assert_eq!(after.prompt_id, s.prompt_id, "prompt_id 同样不该被侧查询改掉");
    }

    /// 会话闲置到期 = 客户端退出：补退出事件链，三路立刻到期一起发（`cap/2.1.260-1`）。
    #[test]
    fn idle_session_ends_like_a_client_exit() {
        let t = Telemetry::default();
        // 一分钟前发的：首轮那串版本检查（api_success 后 6.9s）得落在「退出」之前，真实
        // 情形下退出离最后一条请求至少 3 小时。
        let mut last = call(cc_body(true), "req_last", "end_turn");
        last.started_at = frozen_now() - Duration::from_secs(60);
        t.ingest(last);
        let now = Instant::now();
        t.gc(now);
        assert!(t.latest_session(7, Duration::from_secs(3600)).is_some(), "还没闲置到期");
        assert!(t.take_due(now).is_empty());

        let later = now + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1);
        t.gc(later);
        assert!(t.latest_session(7, Duration::from_secs(u64::MAX / 4)).is_none(), "会话已忘掉");
        let due = t.take_due(later);
        assert_eq!(due.len(), 1);
        let f = &due[0];
        assert_eq!(f.session_id, SESSION);
        let names: Vec<&str> = f.events.iter().map(ev_name).collect();
        let tail = &names[names.len() - 5..];
        assert_eq!(
            tail,
            [
                "tengu_config_cache_stats",
                "tengu_feature_ok",
                "tengu_feature_ok",
                "tengu_feature_ok",
                "tengu_cache_eviction_hint"
            ],
            "队尾是退出那一串，排在这次请求的事件之后"
        );
        let last = f.events.last().unwrap()["event_data"].clone();
        let meta: Value = serde_json::from_slice(
            &STANDARD.decode(last["additional_metadata"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["scope"], "session_end");
        assert_eq!(meta["last_request_id"], "req_last");
        assert_eq!(meta["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622");
        assert_eq!(last["session_id"], SESSION);
        assert_eq!(last["model"], "claude-opus-5[1m]");
        assert_eq!(last["auth"]["organization_uuid"], "09520b85-f6b6-432f-97e2-6ecb804a083f");
        let dd_features: Vec<&str> =
            f.dd.iter()
                .filter_map(|d| d["feature_name"].as_str())
                .filter(|n| {
                    ["lsp_shutdown", "swarm_session_cleanup", "internal_metrics_export"].contains(n)
                })
                .collect();
        assert_eq!(
            dd_features,
            ["lsp_shutdown", "swarm_session_cleanup", "internal_metrics_export"]
        );
        assert!(f.metrics.is_some(), "退出时指标也一起发");
        assert!(t.take_due(later + Duration::from_secs(1)).is_empty());
    }

    /// 已按退出收尾的 session_id 再来 = `--resume`：新进程从头计数（新 chain、无
    /// previousRequestId），指标 `start_type` 报 `resume`。
    #[test]
    fn a_session_id_returning_after_exit_is_a_resume() {
        let t = Telemetry::default();
        t.ingest(call(cc_body(true), "req_1", "end_turn"));
        let now = Instant::now();
        let after_idle = now + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1);
        t.gc(after_idle);
        assert_eq!(t.take_due(after_idle).len(), 1, "退出那批发掉");

        let mut back = call(cc_body(false), "req_2", "end_turn");
        back.started_at = frozen_now();
        t.ingest(back);
        let flush_at = after_idle + Duration::from_secs(config::TELEMETRY_METRICS_FLUSH_SECS + 1);
        let due = t.take_due(flush_at);
        assert_eq!(due.len(), 1);
        let f = &due[0];
        let m = f.metrics.as_ref().expect("metrics");
        let sc = &m["metrics"][0];
        assert_eq!(sc["name"], "claude_code.session.count");
        assert_eq!(sc["data_points"][0]["attributes"]["start_type"], "resume");
        let success =
            f.events.iter().find(|e| e["event_data"]["event_name"] == "tengu_api_success").unwrap();
        let meta: Value = serde_json::from_slice(
            &STANDARD
                .decode(success["event_data"]["additional_metadata"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(meta.get("previousRequestId").is_none(), "新进程不接上一段的 request-id");
        assert_eq!(meta["messageTokens"], 0);
        assert!(
            f.events.iter().any(|e| e["event_data"]["event_name"] == "tengu_input_prompt"),
            "首条请求即便是 tool_result 续轮也按新进程的第一次输入计"
        );

        // 再次闲置退出后又回来，依旧是 resume（表里重新记了一次）。
        let again = flush_at + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1);
        t.gc(again);
        t.take_due(again);
        let mut third = call(cc_body(true), "req_3", "end_turn");
        third.started_at = frozen_now();
        t.ingest(third);
        let due = t.take_due(again + Duration::from_secs(config::TELEMETRY_METRICS_FLUSH_SECS + 1));
        assert_eq!(
            due[0].metrics.as_ref().unwrap()["metrics"][0]["data_points"][0]["attributes"]["start_type"],
            "resume"
        );

        // 另一个从没见过的会话仍是 fresh。
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["metadata"]["user_id"] = json!(
            "{\"device_id\":\"aa\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"brand-new\"}"
        );
        t.ingest(call(body.to_string().into_bytes(), "req_4", "end_turn"));
        let far = again + Duration::from_secs(2 * config::TELEMETRY_METRICS_FLUSH_SECS + 5);
        let due = t.take_due(far);
        let fresh = due.iter().find(|f| f.session_id == "brand-new").unwrap();
        assert_eq!(
            fresh.metrics.as_ref().unwrap()["metrics"][0]["data_points"][0]["attributes"]["start_type"],
            "fresh"
        );
    }

    /// 启动时的额度探测请求不产生任何 api 事件（`cap/2.1.260-1`）。
    #[test]
    fn quota_probe_is_not_reported() {
        let body = json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 1,
            "messages": [{"role":"user","content":"quota"}],
            "metadata": {"user_id": "{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"}
        });
        assert!(parse_shape(body.to_string().as_bytes()).unwrap().quota_probe);
        let t = Telemetry::default();
        t.ingest(call(body.to_string().into_bytes(), "req_q", "max_tokens"));
        assert!(t.0.state.lock().pending.is_empty());
        assert!(t.latest_session(7, Duration::from_secs(60)).is_none(), "也不算开了会话");
    }

    /// 每次指标导出都伴随一条 `internal_metrics_export`，进事件与 Datadog 两路的下一批。
    #[test]
    fn metrics_export_queues_its_feature_event() {
        let t = Telemetry::default();
        t.ingest(call(cc_body(true), "req_1", "end_turn"));
        let now = Instant::now();
        // 先把 30s / 15s 那两路清掉，只剩指标在攒。
        t.take_due(now + Duration::from_secs(config::TELEMETRY_EVENT_FLUSH_SECS + 1));
        let at = now + Duration::from_secs(config::TELEMETRY_METRICS_FLUSH_SECS + 1);
        let due = t.take_due(at);
        assert_eq!(due.len(), 1);
        assert!(due[0].metrics.is_some());
        assert!(due[0].events.is_empty() && due[0].dd.is_empty(), "导出事件刚入队，还没到期");
        let dd = t.take_due(at + Duration::from_secs(config::TELEMETRY_DATADOG_FLUSH_SECS + 1));
        assert_eq!(dd.len(), 1);
        assert_eq!(dd[0].dd.len(), 1);
        assert_eq!(dd[0].dd[0]["feature_name"], "internal_metrics_export");
        assert_eq!(dd[0].dd[0]["model"], "claude-opus-5");
        assert!(dd[0].events.is_empty(), "事件那路 30s 才到");
        let ev = t.take_due(at + Duration::from_secs(config::TELEMETRY_EVENT_FLUSH_SECS + 1));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].events.len(), 1);
        let d = &ev[0].events[0]["event_data"];
        assert_eq!(d["event_name"], "tengu_feature_ok");
        assert_eq!(d["session_id"], SESSION);
        assert_eq!(d["model"], "claude-opus-5[1m]");
        let meta: Value = serde_json::from_slice(
            &STANDARD.decode(d["additional_metadata"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["feature_name"], "internal_metrics_export");
    }

    fn meta_of(e: &Value) -> Value {
        serde_json::from_slice(
            &STANDARD.decode(e["event_data"]["additional_metadata"].as_str().unwrap()).unwrap(),
        )
        .unwrap()
    }

    /// 正文改成订阅端延迟形态：`ToolSearch` + `DeferredToolPlaceholder` 那一对都在
    /// （`cap/2.1.258/00012`）。
    fn tool_search_body() -> Vec<u8> {
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["tools"] = json!([
            {"name":"Bash","description":"run","input_schema":{"type":"object"}},
            {"name":"ToolSearch","description":"search","input_schema":{"type":"object"}},
            {"name":"DeferredToolPlaceholder","description":"d","input_schema":{"type":"object"},"defer_loading":true}
        ]);
        body.to_string().into_bytes()
    }

    /// 正文改成 API-key 端那种**全量声明、无延迟**的工具形态（`cap/2.1.258-api/00006`：内建 +
    /// 两个 `mcp__ide__*`，没有 `defer_loading`）。
    fn undeferred_body() -> Vec<u8> {
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["tools"] = json!([
            {"name":"Bash","description":"run","input_schema":{"type":"object"}},
            {"name":"Read","description":"read","input_schema":{"type":"object"}},
            {"name":"mcp__ide__getDiagnostics","description":"d","input_schema":{"type":"object"}},
            {"name":"mcp__ide__executeCode","description":"e","input_schema":{"type":"object"}}
        ]);
        body.to_string().into_bytes()
    }

    /// `tengu_tool_search_mode_decision` 按正文里**实际启用**的能力取值，三档对齐抓包：延迟形态
    /// `tst_enabled`；全量声明无延迟 `not_registered` 且 `mcpToolCount` 等于正文里 `mcp__*` 的
    /// 个数（`cap/2.1.258-api/00022` 报 2，正文正好两个 `mcp__ide__*`）；无工具
    /// `no_tools_in_request`。此前只要有工具就报 `tst_enabled`——模拟路径与 API-key 端的正文都
    /// 没有 ToolSearch，遥测却说延迟加载开着。
    ///
    /// **反例**：只有 `defer_loading` 占位、没有 `ToolSearch`（`cc_body(true)` 正是 Bash +
    /// DeferredToolPlaceholder）——延迟声明在、搜索能力不在，报 `not_registered`。官方样本里两者
    /// 总是同时出现，判据落在 `ToolSearch` 上才不会给半抄的正文宣称一个没有的能力。
    #[test]
    fn tool_search_decision_follows_the_body() {
        let official = parse_shape(&tool_search_body()).unwrap();
        assert!(official.has_tool_search && official.deferred_tools == 1);
        let m = tool_search_decision(&official, "claude-opus-5[1m]", false);
        assert_eq!(m["enabled"], true);
        assert_eq!(m["reason"], "tst_enabled");
        assert_eq!(m["checkedModel"], "claude-opus-5[1m]");

        let placeholder_only = parse_shape(&cc_body(true)).unwrap();
        assert!(!placeholder_only.has_tool_search && placeholder_only.deferred_tools == 1);
        let m = tool_search_decision(&placeholder_only, "claude-opus-5[1m]", false);
        assert_eq!(m["enabled"], false, "只有占位、没有 ToolSearch 不算启用");
        assert_eq!(m["reason"], "not_registered");

        let full = parse_shape(&undeferred_body()).unwrap();
        assert_eq!(full.deferred_tools, 0);
        assert_eq!(full.mcp_tools, 2);
        let m = tool_search_decision(&full, "claude-opus-5[1m]", false);
        assert_eq!(m["enabled"], false);
        assert_eq!(m["reason"], "not_registered");
        assert_eq!(m["mcpToolCount"], 2);

        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["tools"] = json!([]);
        let none = parse_shape(&body.to_string().into_bytes()).unwrap();
        let m = tool_search_decision(&none, "claude-haiku-4-5-20251001", false);
        assert_eq!(m["enabled"], false);
        assert_eq!(m["reason"], "no_tools_in_request");
        assert_eq!(m["mcpToolCount"], 0);
    }

    /// 首轮模板里的三条延迟工具相关事件跟着正文走：工具池两样看 `defer_loading` 声明、搜索
    /// 模式看 `ToolSearch`。正文没有 `defer_loading` 工具时，不发
    /// `tengu_deferred_tools_pool_change`、`tengu_attachments` 里没有 `deferred_tools_delta`、
    /// `tengu_tool_search_mode_decision` 报 `not_registered`——与 `cap/2.1.258-api/00002`
    /// 一致；有延迟工具时三样照旧（`cap/2.1.258/00020`）。
    #[test]
    fn template_deferred_tool_events_follow_the_body() {
        let run = |body: Vec<u8>| -> (usize, Vec<Value>, Vec<Value>) {
            let t = Telemetry::default();
            t.ingest(call(body, "req_1", "end_turn"));
            let st = t.0.state.lock();
            let p = &st.pending[&key()];
            let by_name = |n: &str| -> Vec<Value> {
                p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
            };
            (
                by_name("tengu_deferred_tools_pool_change").len(),
                by_name("tengu_attachments"),
                by_name("tengu_tool_search_mode_decision"),
            )
        };
        let has_delta = |atts: &[Value]| {
            atts.iter().any(|a| {
                a["attachment_types"]
                    .as_array()
                    .is_some_and(|ts| ts.iter().any(|t| t == "deferred_tools_delta"))
            })
        };

        let (pool, atts, tst) = run(undeferred_body());
        assert_eq!(pool, 0, "没有延迟工具就没有工具池变更");
        assert!(!atts.is_empty() && !has_delta(&atts), "附件里不该有 deferred_tools_delta");
        assert!(!tst.is_empty());
        assert!(
            tst.iter().all(|m| m["reason"] == "not_registered" && m["enabled"] == false),
            "{tst:?}"
        );
        assert!(tst.iter().all(|m| m["mcpToolCount"] == 2), "{tst:?}");

        let (pool, atts, tst) = run(tool_search_body());
        assert!(pool >= 1, "延迟形态照发工具池变更");
        assert!(has_delta(&atts));
        assert!(
            tst.iter().all(|m| m["reason"] == "tst_enabled" && m["enabled"] == true),
            "{tst:?}"
        );

        // 只有占位、没有 ToolSearch：工具池两样跟着延迟声明走仍发，搜索模式却不算启用。
        let (pool, atts, tst) = run(cc_body(true));
        assert!(pool >= 1 && has_delta(&atts));
        assert!(
            tst.iter().all(|m| m["reason"] == "not_registered" && m["enabled"] == false),
            "{tst:?}"
        );
    }

    /// 工具长度表的 hash 是长度表 JSON 的 sha256 前 12 位：无工具时是 sha256("{}") 的前缀
    /// `44136fa355b3`，`cap/2.1.260-1` 那 16 个工具的表算出来是 `65b78f5c8f58`。
    #[test]
    fn tool_schema_hash_is_over_the_length_table() {
        assert_eq!(&sha256_hex(b"{}")[..12], "44136fa355b3");
        let table = r#"{"Agent":3078,"Artifact":37405,"AskUserQuestion":4926,"Bash":2352,"Edit":993,"ListAgents":1180,"Read":1617,"ReportFindings":2206,"ScheduleWakeup":4660,"SendFeedback":5537,"ShareOnboardingGuide":1326,"Skill":1832,"ToolSearch":1469,"Workflow":5384,"Write":668}"#;
        assert_eq!(&sha256_hex(table.as_bytes())[..12], "65b78f5c8f58");
        let s = parse_shape(&cc_body(true)).unwrap();
        assert_eq!(s.tools_hash, &sha256_hex(s.tool_lens.as_bytes())[..12]);
    }

    /// 续轮（tool_result）：工具事件补在这条之前、depth +1、首字在工具之后。
    #[test]
    fn tool_use_continuation_emits_tool_events_and_deepens_the_chain() {
        let t = Telemetry::default();
        // 第一条 tool_use 收尾、没有正文。
        let mut first = call(cc_body(true), "req_1", "tool_use");
        first.text_chars = 0;
        first.started_at = frozen_now() - Duration::from_secs(20);
        t.ingest(first);
        // 续轮：上一条 assistant 是 Bash 调用，末条是 tool_result。
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la","description":"list"}}]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"total 8\nfile"}]});
        let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
        second.started_at = frozen_now() - Duration::from_secs(10);
        second.ua_out = "claude-cli/2.1.260 (external, cli)".into();
        t.ingest(second);

        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let by_name = |n: &str| -> Vec<Value> {
            p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
        };
        let granted = by_name("tengu_tool_use_granted_in_config");
        assert_eq!(granted.len(), 1, "auto 模式下每个工具一条");
        assert_eq!(granted[0]["toolName"], "Bash");
        assert_eq!(granted[0]["messageID"], "msg_011Cedjuoa4oBPzoB2CSUNEB");
        let allowed = by_name("tengu_tool_use_can_use_tool_allowed");
        assert_eq!(allowed[0]["requestId"], "req_1", "指上一条回复");
        assert_eq!(allowed[0]["queryDepth"], 0);
        let bash = by_name("tengu_bash_tool_command_executed");
        assert_eq!(bash[0]["tool_use_id"], "t1");
        assert_eq!(bash[0]["stdout_length"], "total 8\nfile\n".len(), "原始输出末尾的换行照算");
        assert_eq!(bash[0]["bash_argv0"], "ls");
        assert_eq!(bash[0]["bash_command_class"], "file_search");
        let ok = by_name("tengu_tool_use_success");
        assert_eq!(ok[0]["bashCommandLen"], "ls -la".len());
        assert_eq!(ok[0]["toolResultSizeBytes"], "total 8\nfile".len());
        assert!(
            by_name("tengu_feature_ok")
                .iter()
                .any(|m| m["feature_name"] == "shell_snapshot_create")
        );
        assert_eq!(by_name("tengu_query_before_attachments")[0]["toolResultsCount"], 1);
        // 归一化计数：续轮 = post + 5 + 第几次输入(1) + 续轮次数(1)；apiSystemMessageCount = 1 + 1。
        let pre = by_name("tengu_api_before_normalize");
        let post = by_name("tengu_api_after_normalize");
        assert_eq!(pre[1]["preNormalizedMessageCount"], 3 + 5 + 1 + 1);
        assert_eq!(post[1]["apiSystemMessageCount"], 1 + 1);
        let queries = by_name("tengu_api_query");
        assert_eq!(queries[0]["queryDepth"], 0);
        assert_eq!(queries[1]["queryDepth"], 1, "续轮 depth +1");
        assert_eq!(queries[1]["queryChainId"], queries[0]["queryChainId"], "同一轮同一条链");
        assert_eq!(queries[1]["previousRequestId"], "req_1");
        // 完成时刻相减：第一条 20s 前发、6.1s 跑完；第二条 10s 前发、6.1s 跑完 → 10.0s，
        // 而不是工具执行那 3.9s 的空档。
        let gap = by_name("tengu_api_success")[1]["timeSinceLastApiCallMs"].as_u64().unwrap();
        assert!((9_900..=10_100).contains(&gap), "{gap}");
        let first_text = by_name("tengu_turn_first_text");
        assert_eq!(first_text.len(), 1, "第一条没有正文，首字落在续轮");
        assert_eq!(first_text[0]["first_text_path"], "after_tool_use");
        assert_eq!(first_text[0]["requests_before_first_text"], 2);
        assert_eq!(first_text[0]["tool_calls_before_first_text"], 1);
        let successes = by_name("tengu_api_success");
        assert!(
            successes[0].get("thinkingContentLength").is_none(),
            "没有思考块就不带，哪怕整条没有正文"
        );
        assert!(successes[1].get("thinkingContentLength").is_none());
        assert_eq!(successes[1]["systemPromptSource"], "live_unrecorded", "2.1.258 以上才有");
        let ends = by_name("tengu_turn_end");
        assert_eq!(ends.len(), 1, "tool_use 那条不结束这一轮");
        assert!(ends[0]["duration_ms"].as_i64().unwrap() >= 10_000, "整轮时长，从提交算起");
        // active_time：cli 只算 end_turn 收尾的那条（6.118s），tool_use 那条不算；user 是
        // 敲那 11 个字（`hello there`）的时长估算 0.8 + 0.1×11 = 1.9s。
        let m = metrics_body(&p.metrics, "2.1.260", "team", None);
        let active = m["metrics"][3]["data_points"].as_array().unwrap();
        let by_type = |ty: &str| {
            active.iter().find(|d| d["attributes"]["type"] == ty).unwrap()["value"]
                .as_f64()
                .unwrap()
        };
        assert!((by_type("cli") - 6.118).abs() < 0.01, "{}", by_type("cli"));
        assert!((by_type("user") - 1.9).abs() < 0.01, "{}", by_type("user"));
        // durationMsIncludingRetries 比 durationMs 多 1–5ms，按 request-id 稳定。
        let s0 = &successes[0];
        let d =
            s0["durationMsIncludingRetries"].as_i64().unwrap() - s0["durationMs"].as_i64().unwrap();
        assert!((1..=5).contains(&d), "{d}");
        assert!(p.dd.iter().any(|d| d["message"] == "tengu_tool_use_success"));
        assert!(p.dd.iter().any(
            |d| d["message"] == "tengu_bash_tool_command_executed" && d["tool_use_id"] == "t1"
        ));
        assert_eq!(by_name("tengu_input_prompt").len(), 1, "续轮不是新输入");
    }

    /// 猜下一句：带工具、末条用户消息以 `[SUGGESTION MODE:` 开头。自己算一轮，接在主线程后。
    #[test]
    fn prompt_suggestion_is_its_own_auxiliary_turn() {
        let t = Telemetry::default();
        let mut main = call(cc_body(true), "req_main", "end_turn");
        main.started_at = frozen_now() - Duration::from_secs(20);
        t.ingest(main);
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["messages"][2] = json!({"role":"user","content":"[SUGGESTION MODE: Suggest what the user might naturally type next into Claude Code.]\n\nFIRST: ..."});
        body["system"][0]["text"] = json!(
            "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=b6499; cc_prev_req=req_main;"
        );
        let mut sugg = call(body.to_string().into_bytes(), "req_sugg", "end_turn");
        sugg.started_at = frozen_now() - Duration::from_secs(5);
        sugg.ua_out = "claude-cli/2.1.260 (external, cli)".into();
        t.ingest(sugg);
        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let metas: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_success")
            .map(|(_, e)| meta_of(e))
            .collect();
        let s = &metas[1];
        assert_eq!(s["querySource"], "prompt_suggestion");
        assert_eq!(s["queryDepth"], 2, "主线程 depth 0 + 2");
        assert_ne!(s["queryChainId"], metas[0]["queryChainId"], "自己一条链");
        assert_eq!(s["previousRequestId"], "req_main");
        assert!(s.get("is_default_model").is_none());
        assert_eq!(s["effort_level"], "high");
        assert_eq!(
            s["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622",
            "沿用主线程的 prompt"
        );
        // 归一化计数用主线程最后一条的 depth（0），不是自己 +2 过的那个：
        // pre = 3 + 5 + 1 + 0，apiSystemMessageCount = 1 + 0。
        let pre: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_before_normalize")
            .map(|(_, e)| meta_of(e))
            .collect();
        assert_eq!(pre[1]["preNormalizedMessageCount"], 3 + 5 + 1);
        let post: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_after_normalize")
            .map(|(_, e)| meta_of(e))
            .collect();
        assert_eq!(post[1]["apiSystemMessageCount"], 1);
        let bp: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_cache_breakpoints")
            .map(|(_, e)| meta_of(e))
            .collect();
        assert_eq!(bp.last().unwrap()["skipCacheWrite"], true);
        let fork = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_fork_agent_query").unwrap();
        let fm = meta_of(&fork.1);
        assert_eq!(fm["forkLabel"], "prompt_suggestion");
        assert_eq!(fm["queryChainId"], metas[0]["queryChainId"], "fork 统计引用父链");
        assert_eq!(fm["inputTokens"], 2);
        let ends: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_turn_end")
            .map(|(_, e)| meta_of(e))
            .collect();
        assert_eq!(ends.len(), 2);
        assert_eq!(ends[1]["query_source_category"], "auxiliary");
        assert!(p.dd.iter().any(|d| d["feature_name"] == "prompt_suggestion_generate"));
        assert_eq!(
            p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_prompt_suggestion").count(),
            1,
            "只有首轮那条 suppressed"
        );
        assert_eq!(
            p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_tip_shown").count(),
            1,
            "猜下一句没有 tips"
        );
        assert_eq!(p.metrics.len(), 2);
        assert_eq!(p.metrics[1].category, "auxiliary");
    }

    /// 会话标题生成：无链、default 权限、无缓存、边界缺失、成功后一条 title_generated。
    #[test]
    fn session_title_generation_is_a_chainless_side_query() {
        let t = Telemetry::default();
        let mut main = call(cc_body(true), "req_main", "end_turn");
        main.started_at = frozen_now() - Duration::from_secs(20);
        t.ingest(main);
        let body = json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 32000,
            "stream": true,
            "thinking": {"type": "disabled"},
            "system": [
                {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.ced; cc_entrypoint=cli; cch=b1b2c;"},
                {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
                {"type":"text","text":"You are naming a coding session so the user can pick it out of a long list of sessions."}
            ],
            "messages": [{"role":"user","content":[{"type":"text","text":"<session>\nhi\n</session>\n\nWrite the title"}]}],
            "metadata": {"user_id": "{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"}
        });
        let mut title = call(body.to_string().into_bytes(), "req_title", "end_turn");
        title.started_at = frozen_now() - Duration::from_secs(5);
        title.resp_model = Some("claude-haiku-4-5-20251001".into());
        // haiku 那条不带 context-1m，也就没有 `[1m]` 展示名。
        title.betas =
            Some("claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14".into());
        title.cache_read_tokens = 0;
        title.cache_creation_tokens = 0;
        title.input_tokens = 896;
        t.ingest(title);
        // 侧查询会先扣住等下一条主线程；这里没有，走超时补发。
        t.gc(Instant::now() + Duration::from_secs(config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1));
        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let query = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_query")
            .map(|(_, e)| meta_of(e))
            .nth(1)
            .unwrap();
        assert_eq!(query["querySource"], "generate_session_title");
        assert!(query.get("queryChainId").is_none() && query.get("queryDepth").is_none());
        assert_eq!(query["permissionMode"], "default");
        assert_eq!(query["thinkingType"], "disabled");
        assert!(query.get("effortValue").is_none());
        assert!(query.get("previousRequestId").is_none());
        let success = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_success")
            .map(|(_, e)| meta_of(e))
            .nth(1)
            .unwrap();
        assert_eq!(success["model"], "claude-haiku-4-5-20251001");
        assert!(success.get("preNormalizedModel").is_none());
        assert_eq!(success["messageTokens"], 0);
        assert!(success.get("effort_level").is_none() && success.get("is_default_model").is_none());
        assert!(success.get("prompt_cache_ttl").is_none());
        assert_eq!(success["toolSchemasHash"], "44136fa355b3");
        assert!(success.get("timeSinceLastApiCallMs").is_some());
        assert!(success.get("systemPromptSource").is_none());
        let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
        assert_eq!(
            names.iter().filter(|n| **n == "tengu_sysprompt_missing_boundary_marker").count(),
            2
        );
        assert!(names.contains(&"tengu_session_title_generated"));
        assert_eq!(names.iter().filter(|n| **n == "tengu_turn_end").count(), 1, "标题那条不算一轮");
        assert_eq!(
            names.iter().filter(|n| **n == "tengu_tool_schema_sizes").count(),
            2,
            "工具集从 16 个变成 0 个"
        );
        let bp: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_cache_breakpoints")
            .map(|(_, e)| meta_of(e))
            .collect();
        assert_eq!(bp.last().unwrap()["cachingEnabled"], false);
        assert!(p.metrics[1].effort.is_none(), "没有 effort 属性");
        let m = metrics_body(&p.metrics, "2.1.260", "team", None);
        let cost = m["metrics"][1]["data_points"].as_array().unwrap();
        assert!(cost.iter().any(|d| d["attributes"]["query_source"] == "auxiliary"
            && d["attributes"].get("effort").is_none()));
        // 标题那串事件的顶层 model 与 Datadog 的 model 都是会话主模型，不是 haiku。
        let title_ev =
            p.events.iter().find(|(_, e)| ev_name(e) == "tengu_session_title_generated").unwrap();
        assert_eq!(title_ev.1["event_data"]["model"], "claude-opus-5[1m]");
        // Datadog：api_success 那条的 `model` 被 meta 里这条请求的模型盖掉（官方同样如此），
        // 其余条目（如 api_request）用会话主模型。
        let title_dd =
            p.dd.iter()
                .find(|d| d["message"] == "tengu_api_success" && d["request_id"] == "req_title")
                .unwrap();
        assert_eq!(title_dd["model"], "claude-haiku-4-5", "DD 去掉日期后缀");
        assert!(title_dd["ddtags"].as_str().unwrap().contains("model:claude-haiku-4-5,"));
        // event_logging 里标题的 api 事件顶层 model 是它自己的 haiku（跟 meta），其余事件是主模型。
        let title_query =
            p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_api_query").nth(1).unwrap();
        assert_eq!(title_query.1["event_data"]["model"], "claude-haiku-4-5-20251001");
        let title_success =
            p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_api_success").nth(1).unwrap();
        assert_eq!(title_success.1["event_data"]["model"], "claude-haiku-4-5-20251001");
        let schema_events: Vec<&(DateTime<Utc>, Value)> =
            p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_tool_schema_sizes").collect();
        assert_eq!(schema_events[1].1["event_data"]["model"], "claude-opus-5[1m]");
        let api_requests: Vec<&Value> =
            p.dd.iter().filter(|d| d["feature_name"] == "api_request").collect();
        assert_eq!(api_requests.len(), 2);
        assert_eq!(api_requests[1]["model"], "claude-opus-5", "标题那次的 feature_ok 用会话主模型");
        assert!(api_requests[1]["ddtags"].as_str().unwrap().contains("model:claude-opus-5,"));
        drop(st);

        // 第二轮主线程回来：工具集和第一轮一样，不再重发 schema（标题那条空表不算污染）。
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["system"][0]["text"] = json!(
            "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=f850a; cc_prev_req=req_main; cc_prompt_id=16d7a19d-7939-4638-9703-b31d2fc92661;"
        );
        let mut third = call(body.to_string().into_bytes(), "req_main2", "end_turn");
        third.started_at = frozen_now() - Duration::from_secs(2);
        t.ingest(third);
        let st = t.0.state.lock();
        let n = st.pending[&key()]
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_tool_schema_sizes")
            .count();
        assert_eq!(n, 2, "官方整个会话就两条");
    }

    /// 主线程请求末尾挂着一条 `role:"system"` 附件消息（`cap/2.1.260-2` 三条都是）：判新输入
    /// 与续轮都要跳过它。
    #[test]
    fn trailing_system_message_does_not_hide_the_prompt_or_the_tool_result() {
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["messages"].as_array_mut().unwrap().push(json!({"role":"system","content":[{"type":"text","text":"<system-reminder>tokens</system-reminder>"}]}));
        let s = parse_shape(body.to_string().as_bytes()).unwrap();
        assert!(s.new_prompt, "尾部 system 不算末条");
        assert_eq!(s.prompt_len, "hello there".len());

        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"pwd"}}]});
        body["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"system","content":[{"type":"text","text":"reminder"}]}));
        let s = parse_shape(body.to_string().as_bytes()).unwrap();
        assert!(!s.new_prompt);
        assert_eq!(s.tool_uses.len(), 1, "隔着尾部 system 也认得出 assistant→tool_result");
        assert_eq!(s.tool_uses[0].name, "Bash");

        // 第二次输入带尾部 system：是新输入，chain 换、depth 归零、input_prompt 计到 2。
        let t = Telemetry::default();
        let mut first = call(cc_body(true), "req_1", "end_turn");
        first.started_at = frozen_now() - Duration::from_secs(30);
        t.ingest(first);
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["system"][0]["text"] = json!(
            "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=f850a; cc_prev_req=req_1; cc_prompt_id=16d7a19d-7939-4638-9703-b31d2fc92661;"
        );
        body["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"system","content":[{"type":"text","text":"reminder"}]}));
        let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
        second.started_at = frozen_now() - Duration::from_secs(10);
        t.ingest(second);
        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let prompts: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_input_prompt")
            .map(|(_, e)| meta_of(e))
            .collect();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[1]["prompt_index"], 2);
        assert_eq!(prompts[1]["is_wakeup"], false);
        assert_eq!(prompts[1]["cc_prompt_id"], "16d7a19d-7939-4638-9703-b31d2fc92661");
        let queries: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_query")
            .map(|(_, e)| meta_of(e))
            .collect();
        assert_ne!(queries[0]["queryChainId"], queries[1]["queryChainId"]);
        assert_eq!(queries[1]["queryDepth"], 0);
        assert_eq!(queries[1]["previousRequestId"], "req_1");
        assert!(
            p.events.iter().any(|(_, e)| ev_name(e) == "tengu_paste_text"),
            "第二次输入走 prompt_next 模板"
        );
    }

    /// 标题生成先扣住，等同会话下一条主线程请求带来新一轮 prompt id 再补发；等不到就超时按
    /// 现有 id 发。
    #[test]
    fn side_queries_wait_for_the_new_prompt_id() {
        fn title_body() -> Vec<u8> {
            json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 32000,
                "stream": true,
                "thinking": {"type": "disabled"},
                "system": [
                    {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.ced; cc_entrypoint=cli; cch=b1b2c;"},
                    {"type":"text","text":"You are naming a coding session so the user can pick it out of a long list of sessions."}
                ],
                "messages": [{"role":"user","content":"<session>\nhi\n</session>\n\nWrite the title"}],
                "metadata": {"user_id": "{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"}
            })
            .to_string()
            .into_bytes()
        }
        let t = Telemetry::default();
        let mut first = call(cc_body(true), "req_1", "end_turn");
        first.started_at = frozen_now() - Duration::from_secs(30);
        t.ingest(first);
        let mut title = call(title_body(), "req_title", "end_turn");
        title.started_at = frozen_now() - Duration::from_secs(12);
        title.betas = Some("claude-code-20250219,oauth-2025-04-20".into());
        t.ingest(title);
        {
            let st = t.0.state.lock();
            assert_eq!(st.sessions[&key()].deferred.len(), 1, "扣住了");
            assert!(
                !st.pending[&key()]
                    .events
                    .iter()
                    .any(|(_, e)| ev_name(e) == "tengu_session_title_generated")
            );
        }
        // 主线程新一轮到了：标题那条补发，prompt id 是新一轮的。
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["system"][0]["text"] = json!(
            "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=f850a; cc_prev_req=req_1; cc_prompt_id=16d7a19d-7939-4638-9703-b31d2fc92661;"
        );
        let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
        second.started_at = frozen_now() - Duration::from_secs(11);
        t.ingest(second);
        {
            let st = t.0.state.lock();
            assert!(st.sessions[&key()].deferred.is_empty());
            let p = &st.pending[&key()];
            let title_success = p
                .events
                .iter()
                .filter(|(_, e)| ev_name(e) == "tengu_api_success")
                .map(|(_, e)| meta_of(e))
                .find(|m| m["querySource"] == "generate_session_title")
                .expect("标题那条补发了");
            assert_eq!(title_success["cc_prompt_id"], "16d7a19d-7939-4638-9703-b31d2fc92661");
            // 完成时刻相减：标题（12s 前发、6.1s 跑完 → 5.9s 前完成）− 首条（30s 前发 →
            // 23.9s 前完成）= 18.0s；不是后来那条主线程。
            let title_gap = title_success["timeSinceLastApiCallMs"].as_u64().unwrap();
            assert!((17_900..=18_100).contains(&title_gap), "{title_gap}");
            // 主线程第二条（11s 前发 → 4.9s 前完成）距离**标题**的完成（5.9s 前）= 1.0s，
            // 标题虽然被扣住了，完成时刻照样算进去。
            let main_success = p
                .events
                .iter()
                .filter(|(_, e)| ev_name(e) == "tengu_api_success")
                .map(|(_, e)| meta_of(e))
                .find(|m| m["requestId"] == "req_2")
                .unwrap();
            let main_gap = main_success["timeSinceLastApiCallMs"].as_u64().unwrap();
            assert!((900..=1_100).contains(&main_gap), "{main_gap}");
            assert!(p.events.iter().any(|(_, e)| ev_name(e) == "tengu_session_title_generated"));
        }
        // Datadog 那份按发生顺序发：标题（先完成）排在后到的主线程之前，尽管它是补发入队的。
        let dd_due = t.take_due(
            Instant::now() + Duration::from_secs(config::TELEMETRY_DATADOG_FLUSH_SECS + 1),
        );
        assert_eq!(dd_due.len(), 1);
        let dd = &dd_due[0].dd;
        assert!(dd_due[0].events.is_empty(), "事件那路 30s 才到，这里只取 Datadog");
        let pos = |rid: &str| {
            dd.iter()
                .position(|d| d["message"] == "tengu_api_success" && d["request_id"] == rid)
                .unwrap()
        };
        assert!(pos("req_title") < pos("req_2"), "标题完成在前");
        // 超时路径：再来一条标题、没有主线程跟上，gc 到 10s 后按现有 id 发。
        let mut title2 = call(title_body(), "req_title2", "end_turn");
        title2.betas = Some("claude-code-20250219,oauth-2025-04-20".into());
        t.ingest(title2);
        let now = Instant::now();
        t.gc(now);
        assert_eq!(t.0.state.lock().sessions[&key()].deferred.len(), 1, "还没到 10s");
        t.gc(now + Duration::from_secs(config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1));
        let st = t.0.state.lock();
        assert!(st.sessions[&key()].deferred.is_empty());
        let n = st.pending[&key()]
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_session_title_generated")
            .count();
        assert_eq!(n, 2);
    }

    /// 拿真实抓包的请求体重新算长度表：Artifact 37405、toolsCharLength 74633、
    /// toolSchemasHash 65b78f5c8f58、requestBodyChars 101459（`cap/2.1.260-2/00057` 的
    /// `tengu_api_success`）。抓包目录不入库，本地没有就跳过。
    #[test]
    fn lengths_match_the_capture_when_it_is_present() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/cap/2.1.260-2/00057_174302.569.req.raw");
        let Ok(raw) = std::fs::read(path) else {
            eprintln!("skipped: {path} not present");
            return;
        };
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("http headers") + 4;
        let body = &raw[sep..];
        let s = parse_shape(body).expect("parses");
        let lens: Value = serde_json::from_str(&s.tool_lens).unwrap();
        assert_eq!(lens["Artifact"], 37405);
        assert_eq!(lens["Agent"], 3078);
        assert_eq!(lens["Bash"], 2352);
        assert_eq!(s.tools_chars, 74633);
        assert_eq!(s.tools_hash, "65b78f5c8f58");
        assert_eq!(js_len(std::str::from_utf8(body).unwrap()), 101459);
        assert_eq!(s.input_text_chars, 14203);
        assert_eq!(s.estimated_tokens, 4735);

        // 同一会话的其它三条：续轮（含 tool_use 块）、猜下一句、标题生成。
        let expect = [
            ("00061_174309.489.req.raw", 15163usize, 5055usize),
            ("00063_174319.456.req.raw", 17539, 5847),
            ("00058_174302.401.req.raw", 221, 55),
        ];
        for (file, chars, est) in expect {
            let path = format!("{}/cap/2.1.260-2/{file}", env!("CARGO_MANIFEST_DIR"));
            let raw = std::fs::read(&path).unwrap();
            let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let s = parse_shape(&raw[sep..]).unwrap();
            assert_eq!(s.input_text_chars, chars, "{file}");
            assert_eq!(s.estimated_tokens, est, "{file}");
        }
    }

    /// 链的权威源是**请求自己带的那份**，不是回程时另算的会话状态。
    ///
    /// 出站体的 billing header 里那个 `cc_prev_req` 与遥测的 `previousRequestId` 说的是
    /// 同一件事——`cap/2.1.260-2` 三条续轮逐字相同，官方两处出自同一个 `requestJournal`。
    /// luban 这边两者的更新路径不同（前者在 `ReqLog::drop` 里同步写，后者走 ingest 队列），
    /// 让遥测复述请求自己说过的话，两份就不可能对不上。
    /// `diagnostics.previous_message_id` 同理。
    #[test]
    fn the_chain_fields_come_from_the_request_itself() {
        const PREV_REQ: &str = "req_011CeiBW8Yx9A2uzWiCBsJsU";
        const PREV_MSG: &str = "msg_011CeiBWZwBH2rmLqr63MhHD";
        let t = Telemetry::default();
        // 会话首条：体里没有 `cc_prev_req`，链上没有上一条。
        t.ingest(call(cc_body(true), "req_1", "end_turn"));
        // 第二条：体里自报了一个**与会话状态不同**的 `cc_prev_req` 和 `previous_message_id`。
        // 会话状态此刻记着 `req_1` / `msg_011Ced…`，两者都该被体里那份顶掉。
        let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        v["system"][0]["text"] = json!(format!(
            "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=b6499; \
             cc_prompt_id=6c079143-0c53-4c48-817d-105460b3f622; cc_prev_req={PREV_REQ};"
        ));
        v["diagnostics"] = json!({ "previous_message_id": PREV_MSG });
        // 换个模型让缓存诊断那条事件发出来（它带 `previousMessageId`）。
        v["model"] = json!("claude-sonnet-5");
        let mut c = call(serde_json::to_vec(&v).unwrap(), "req_2", "end_turn");
        c.resp_model = Some("claude-sonnet-5".into());
        t.ingest(c);

        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        let meta = |name: &str, nth: usize| -> Value {
            let (_, e) = p.events.iter().filter(|(_, e)| ev_name(e) == name).nth(nth).unwrap();
            let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
            let raw =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
            serde_json::from_slice::<Value>(&raw).unwrap()
        };
        assert!(meta("tengu_api_query", 0).get("previousRequestId").is_none(), "首条没有上一条");
        assert_eq!(
            meta("tengu_api_query", 1)["previousRequestId"],
            PREV_REQ,
            "以体里自报的 cc_prev_req 为准，而不是会话状态里的 req_1"
        );
        assert_eq!(meta("tengu_api_success", 1)["previousRequestId"], PREV_REQ);
        assert_eq!(
            meta("tengu_prompt_cache_diagnosis_received", 0)["previousMessageId"],
            PREV_MSG,
            "以体里 diagnostics.previous_message_id 为准"
        );
    }

    /// 体里没有 `cc_prev_req` 时仍回落到会话状态：会话首轮、没有 billing header 的来访、
    /// 关掉链注入的那些都走这条路。
    #[test]
    fn the_chain_falls_back_to_session_state_without_a_declared_prev() {
        let t = Telemetry::default();
        t.ingest(call(cc_body(true), "req_1", "end_turn"));
        t.ingest(call(cc_body(false), "req_2", "end_turn"));
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        let (_, e) =
            p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_api_query").nth(1).unwrap();
        let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        let m: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(m["previousRequestId"], "req_1", "体里没自报就用会话状态那条链");
    }

    /// 处理顺序必须等于**入队顺序**（= `ReqLog::drop` 顺序 = 响应完成顺序）。
    ///
    /// 此前每条调用各起一个 `spawn_blocking`，那是往一个几百线程的池子里扔任务，前后脚
    /// 提交的两条谁先跑没有保证；而 `process` 里一多半状态是按顺序累积的。这里从**多个
    /// 线程**并发 `record`，再断言那条 `previousRequestId` 链严丝合缝——乱序处理会让链
    /// 在某一处指回更早的一条，或者干脆指向自己后面那条。
    #[test]
    fn the_ingest_queue_processes_calls_in_the_order_they_were_recorded() {
        const N: usize = 64;
        // 体要够大，`process` 里那趟 JSON 解析才占得住时间——生产快过消费，队列上才真的
        // 会同时压着好几条。体小的话每条 record 都在下一条入队前就处理完了，什么都测不出来。
        let big = {
            let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
            v["system"][3]["text"] = json!("While auto mode is active: ".repeat(8_000));
            serde_json::to_vec(&v).unwrap()
        };
        // 先把 N 条都造好：构造的开销留在计时之外，`record` 那一串才是背靠背的。
        let calls: Vec<ApiCall> =
            (0..N).map(|i| call(big.clone(), &format!("req_{i:02}"), "end_turn")).collect();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .max_blocking_threads(8)
            .enable_all()
            .build()
            .unwrap();
        let t = Telemetry::default();
        rt.block_on(async {
            // 入队是同步的，故入队顺序就是这里的循环顺序。
            for c in calls {
                t.record(c);
            }
            // 等队列排空：最多等 30 秒，正常是几百毫秒。
            for _ in 0..3_000 {
                let done = {
                    let st = t.0.state.lock();
                    st.pending.get(&key()).is_some_and(|p| {
                        p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_api_query").count()
                            == N
                    })
                };
                if done {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("ingest queue did not drain");
        });

        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        let chain: Vec<Option<String>> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_query")
            .map(|(_, e)| {
                let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
                let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                    .unwrap();
                let m: Value = serde_json::from_slice(&raw).unwrap();
                m.get("previousRequestId").and_then(|v| v.as_str()).map(str::to_string)
            })
            .collect();
        assert_eq!(chain.len(), N);
        assert_eq!(chain[0], None, "首条没有上一条");
        for (i, prev) in chain.iter().enumerate().skip(1) {
            assert_eq!(
                prev.as_deref(),
                Some(format!("req_{:02}", i - 1)).as_deref(),
                "第 {i} 条的 previousRequestId 应当是紧挨着的上一条"
            );
        }
    }

    /// `toolUseContentLengths`：回复里带工具调用时，`tengu_api_success` 多一个字段，值是
    /// 「工具名 → 入参 JSON 字符数」那张表**序列化成的字符串**（与 `toolSchemaCharLengths`
    /// 同一种写法），一个工具都没有时整个字段不出现。
    #[test]
    fn tool_use_content_lengths_ride_along_with_the_success() {
        let t = Telemetry::default();
        let mut c = call(cc_body(true), "req_tu", "tool_use");
        c.tool_use_lens = vec![("Bash".into(), 169), ("Read".into(), 42)];
        t.ingest(c);
        t.ingest(call(cc_body(true), "req_plain", "end_turn"));
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        let successes: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_success")
            .map(|(_, e)| {
                let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
                let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                    .unwrap();
                serde_json::from_slice::<Value>(&raw).unwrap()
            })
            .collect();
        assert_eq!(successes[0]["toolUseContentLengths"], r#"{"Bash":169,"Read":42}"#);
        assert!(
            successes[1].get("toolUseContentLengths").is_none(),
            "回复里没有 tool_use 块的话整个字段不出现"
        );
    }

    /// 失败请求走 `tengu_api_error` 而不是 `tengu_api_success`：分类按状态码、文案取上游
    /// 原文，收尾多一条 `tengu_feature_bad{api_request}` 与一条
    /// `terminal_reason: "api_error"` 的 `tengu_turn_end`；成功那条独有的东西一样都没有。
    #[test]
    fn a_failed_call_reports_an_api_error_instead_of_a_success() {
        let t = Telemetry::default();
        t.ingest(failed(
            call(cc_body(true), "req_429", "end_turn"),
            Some(429),
            "rate_limit_error",
            "This request would exceed your organization's rate limit",
        ));
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).expect("入队");
        let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
        assert!(names.contains(&"tengu_api_error"));
        assert!(!names.contains(&"tengu_api_success"), "两条互斥");
        assert!(!names.contains(&"tengu_tool_schema_sizes"), "长度表只在成功那条里发");
        let meta_of = |name: &str| {
            let (_, e) = p.events.iter().find(|(_, e)| ev_name(e) == name).unwrap();
            let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
            let raw =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
            serde_json::from_slice::<Value>(&raw).unwrap()
        };
        let err = meta_of("tengu_api_error");
        assert_eq!(err["errorType"], "rate_limit");
        assert_eq!(err["status"], "429", "状态码是十进制串，不是数字");
        assert_eq!(err["error"], "This request would exceed your organization's rate limit");
        assert_eq!(err["model"], "claude-opus-5[1m]");
        assert_eq!(err["attempt"], 1);
        assert_eq!(err["provider"], "firstParty");
        assert_eq!(err["requestId"], "req_429");
        assert_eq!(err["clientRequestId"], "3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e");
        assert_eq!(err["querySource"], "repl_main_thread");
        assert_eq!(err["queryDepth"], 0);
        assert_eq!(err["requestBodyEncoding"], "identity");
        assert!(err.get("inputTokens").is_none(), "失败那条没有任何用量字段");
        assert!(err.get("costUSD").is_none());
        // 收尾：feature_bad + api_error 两条都进 Datadog，但没有 feature_ok{api_request}。
        let bad = meta_of("tengu_feature_bad");
        assert_eq!(bad["feature_name"], "api_request");
        assert_eq!(bad["error_code"], "api_request_retry_exhausted");
        assert!(p.dd.iter().any(|d| d["message"] == "tengu_api_error"));
        assert!(p.dd.iter().any(|d| d["message"] == "tengu_feature_bad"));
        // 这批里所有 feature 事件的名字：`api_request` 只能以 bad 的形式出现一次。
        let features = |name: &str| -> Vec<String> {
            p.events
                .iter()
                .filter(|(_, e)| ev_name(e) == name)
                .filter_map(|(_, e)| {
                    let b64 = e["event_data"]["additional_metadata"].as_str()?;
                    let raw =
                        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                            .ok()?;
                    let v: Value = serde_json::from_slice(&raw).ok()?;
                    Some(v["feature_name"].as_str()?.to_string())
                })
                .collect()
        };
        assert!(
            !features("tengu_feature_ok").contains(&"api_request".to_string()),
            "官方的 feature_ok{{api_request}} 是成功回包才打的"
        );
        assert!(
            !features("tengu_feature_ok").contains(&"turn".to_string()),
            "失败的那轮没有 feature_ok{{turn}}，也没有 stop hook"
        );
        assert!(!features("tengu_feature_ok").contains(&"hook_stop_handler".to_string()));
        // 一轮到此为止，但走的是 api_error 那条。
        assert_eq!(p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_turn_end").count(), 1);
        let end = meta_of("tengu_turn_end");
        assert_eq!(end["terminal_reason"], "api_error");
        assert_eq!(end["error_kind"], "rate_limit");
        assert_eq!(end["is_error"], true);
        // 指标：会话照数、active_time 照记，但 cost/token 一个数据点都没有。
        let m = metrics_body(&p.metrics, "2.1.260", "team", None);
        let names: Vec<&str> =
            m["metrics"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"claude_code.session.count"));
        assert!(names.contains(&"claude_code.active_time.total"));
        assert!(!names.contains(&"claude_code.cost.usage"), "失败请求不进 cost");
        assert!(!names.contains(&"claude_code.token.usage"), "失败请求不进 token");
    }

    /// 连 `ReqLog` 都没建起来的那两条路（连接层失败、401 换号）由
    /// [`Capture::record_failure`] 就地补一条失败遥测：响应侧的量一概没有，
    /// 事件链与正常路径上那条 `tengu_api_error` 同一套。
    #[test]
    fn a_capture_can_report_a_failure_without_a_response() {
        let t = Telemetry::default();
        let cap = |sink: Telemetry| Capture {
            sink,
            account_uuid: Some("9922ef8e-7945-4f5a-ab4f-cf5f521531df".into()),
            org_type: Some("claude_team".into()),
            body: Bytes::from(cc_body(true)),
            betas: Some("claude-code-20250219,oauth-2025-04-20".into()),
            session_header: None,
            client_request_id: Some("3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e".into()),
            agent: AgentHeaders::default(),
            organization_id: None,
            started_at: frozen_now() - Duration::from_secs(3),
        };
        // 连接层就失败：没有状态码、没有上游 request-id。
        cap(t.clone()).record_failure(
            7,
            "claude-cli/2.1.260 (external, cli)".into(),
            1_200,
            None,
            CallFailure {
                status: None,
                error_type: None,
                message: "error sending request: connection refused".into(),
                in_band: false,
            },
        );
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).expect("入队");
        let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
        assert!(names.contains(&"tengu_api_error"));
        assert!(!names.contains(&"tengu_api_success"));
        let (_, e) = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_error").unwrap();
        let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        let err: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(err["errorType"], "connection_error");
        assert!(err.get("status").is_none(), "连接层失败没有 HTTP 状态码");
        assert!(err.get("requestId").is_none(), "上游 request-id 都没拿到");
        assert_eq!(err["clientRequestId"], "3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e");
        assert_eq!(err["durationMs"], 1_200);
        // 请求侧的量照旧从出站体算（那份 body 确实拼好了、只是没发出去）。
        assert_eq!(err["messageCount"], 3);
        assert!(err.get("inputTokens").is_none(), "响应侧一个字段都没有");
        // 一轮到此为止，走 api_error 那条。
        let (_, end) = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_turn_end").unwrap();
        let b64 = end["event_data"]["additional_metadata"].as_str().unwrap();
        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        let end: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(end["terminal_reason"], "api_error");
        assert_eq!(end["error_kind"], "connection_error");
        // 请求没发出去，没有响应头可取组织 id；这个号之前也没有过带那个头的响应，
        // 于是 `auth` 里只有 account——官方对连不上的那类同样拿不到更多。
        assert!(e["event_data"]["auth"].get("organization_uuid").is_none());
    }

    /// 401 那条路（换号/回 403/原样透传三条出路都绕开 `ReqLog::drop`）报的是带状态码的
    /// `auth_error`，且**响应头里的组织 id 要带进 `auth` 块**。
    ///
    /// 组织 id 遥测这边按凭证缓存过一份，同一个号之前有过一条带这个头的响应就还补得上；
    /// 但一个进程里头一条就是早退 401 的号没有那份缓存，`auth` 里就会整个少掉
    /// `organization_uuid`——而订阅/团队账号官方每条事件都带。所以 401 那条路要把响应头
    /// 上那份传下来（连接层失败那条没有响应，只能靠缓存）。
    #[test]
    fn a_401_early_return_still_reports_an_auth_error() {
        let t = Telemetry::default();
        Capture {
            sink: t.clone(),
            account_uuid: Some("9922ef8e-7945-4f5a-ab4f-cf5f521531df".into()),
            org_type: Some("claude_team".into()),
            body: Bytes::from(cc_body(true)),
            betas: None,
            session_header: None,
            client_request_id: None,
            agent: AgentHeaders::default(),
            organization_id: Some("09520b85-f6b6-432f-97e2-6ecb804a083f".into()),
            started_at: frozen_now() - Duration::from_secs(2),
        }
        .record_failure(
            7,
            "claude-cli/2.1.260 (external, cli)".into(),
            800,
            Some("req_401".into()),
            CallFailure {
                status: Some(401),
                error_type: Some("authentication_error".into()),
                message: "OAuth token has been revoked".into(),
                in_band: false,
            },
        );
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        let (_, e) = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_error").unwrap();
        let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        let err: Value = serde_json::from_slice(&raw).unwrap();
        // `token_revoked` 在官方 `ate()` 里排在 401 → auth_error 之前。
        assert_eq!(err["errorType"], "token_revoked");
        assert_eq!(err["status"], "401");
        assert_eq!(err["requestId"], "req_401");
        assert_eq!(err["error"], "OAuth token has been revoked");
        // 这个号在这个进程里还没有过任何一条成功响应，`auth` 里那个组织 id 只能来自
        // 这一发 401 自己的响应头。
        assert_eq!(
            e["event_data"]["auth"]["organization_uuid"], "09520b85-f6b6-432f-97e2-6ecb804a083f",
            "早退 401 的事件也要带上组织 id"
        );
        assert_eq!(e["event_data"]["auth"]["account_uuid"], "9922ef8e-7945-4f5a-ab4f-cf5f521531df");
    }

    /// 流内错误（`event: error` 裹在 200 里）：SDK 那头没有状态码，官方报
    /// `in_band_<上游 type>`，`status` 字段整个不出现。
    #[test]
    fn an_in_band_stream_error_is_reported_as_in_band() {
        let t = Telemetry::default();
        t.ingest(failed(
            call(cc_body(true), "req_ib", "end_turn"),
            None,
            "overloaded_error",
            "Overloaded",
        ));
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        let (_, e) = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_error").unwrap();
        let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        let err: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(err["errorType"], "in_band_overloaded_error");
        assert!(err.get("status").is_none(), "流内错误没有 HTTP 状态码");
    }

    /// `errorType` 的分类顺序与官方 `ate()` 一致：429 在「4xx」之前、529/overloaded 在
    /// 「5xx」之前，非重试类的 400 报 `client_error` 且 `error_code` 换成 non_retryable。
    #[test]
    fn error_kinds_follow_the_official_classifier() {
        let f = |status: Option<u16>, etype: &str, msg: &str| CallFailure {
            status,
            error_type: (!etype.is_empty()).then(|| etype.to_string()),
            message: msg.to_string(),
            in_band: false,
        };
        assert_eq!(error_kind(&f(Some(429), "rate_limit_error", "")), "rate_limit");
        assert_eq!(error_kind(&f(Some(529), "overloaded_error", "")), "server_overload");
        assert_eq!(error_kind(&f(Some(500), "overloaded_error", "")), "server_overload");
        assert_eq!(error_kind(&f(Some(500), "api_error", "")), "server_error");
        assert_eq!(error_kind(&f(Some(401), "authentication_error", "")), "auth_error");
        assert_eq!(error_kind(&f(Some(403), "permission_error", "")), "auth_error");
        assert_eq!(error_kind(&f(Some(413), "", "")), "request_too_large");
        assert_eq!(
            error_kind(&f(Some(400), "invalid_request_error", "prompt is too long: 1 tokens")),
            "prompt_too_long"
        );
        assert_eq!(
            error_kind(&f(
                Some(400),
                "invalid_request_error",
                "text content blocks must be non-empty"
            )),
            "empty_text_block"
        );
        assert_eq!(
            error_kind(&f(Some(404), "not_found_error", "model: claude-nope")),
            "model_not_found"
        );
        assert_eq!(error_kind(&f(Some(400), "invalid_request_error", "whatever")), "client_error");
        assert_eq!(error_kind(&f(None, "", "connection reset")), "connection_error");
        assert_eq!(api_request_error_code(&f(Some(429), "", "")), "api_request_retry_exhausted");
        assert_eq!(api_request_error_code(&f(Some(500), "", "")), "api_request_retry_exhausted");
        assert_eq!(api_request_error_code(&f(None, "", "")), "api_request_retry_exhausted");
        assert_eq!(api_request_error_code(&f(Some(400), "", "")), "api_request_non_retryable");
    }

    /// `active_time.total{type:user}` 是**打字时长**，与 API 花了多久无关：估算
    /// `0.8 + 0.1 × 字数`，再按「上一次提交到这次提交」的窗口截断。
    ///
    /// 三份抓包（`cap/2.1.260-2`）的实测值：2 字 → 0.878、2 字 → 1.118、
    /// 3 字 + 20 字 → 3.988。这里复现第三份那个两轮会话。
    #[test]
    fn user_active_time_tracks_the_typed_length() {
        let t = Telemetry::default();
        let prompt = |text: &str| {
            let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
            v["messages"] = json!([{"role":"user","content":[{"type":"text","text":text}]}]);
            serde_json::to_vec(&v).unwrap()
        };
        // 第一轮：3 个字（`hii`），窗口是进程起点到提交那 3.085s → 0.8 + 0.3 = 1.1。
        let mut first = call(prompt("hii"), "req_1", "end_turn");
        first.started_at = frozen_now() - Duration::from_millis(9_800);
        first.total_ms = 3_779;
        t.ingest(first);
        // 第二轮：20 个字，两次提交相隔 6.06s，估算 0.8 + 2.0 = 2.8 < 6.06，取估算值。
        let mut second = call(prompt("hilele what can u do"), "req_2", "end_turn");
        second.started_at = frozen_now() - Duration::from_millis(9_800 - 6_060);
        second.total_ms = 6_338;
        t.ingest(second);

        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        let m = metrics_body(&p.metrics, "2.1.260", "team", None);
        let active = m["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "claude_code.active_time.total")
            .unwrap()["data_points"]
            .as_array()
            .unwrap()
            .clone();
        let by_type = |ty: &str| {
            active.iter().find(|d| d["attributes"]["type"] == ty).unwrap()["value"]
                .as_f64()
                .unwrap()
        };
        assert!((by_type("user") - 3.9).abs() < 0.05, "{}", by_type("user"));
        // cli 仍是两条 end_turn 的时长之和（抓包 10.053）。
        assert!((by_type("cli") - 10.117).abs() < 0.05, "{}", by_type("cli"));
    }

    /// 用户输入窗口比估算值短时按窗口截断：粘一大段再立刻回车，打字时长不可能超过
    /// 「上次提交到这次提交」那段真实时间。
    #[test]
    fn user_active_time_is_capped_by_the_window() {
        let t = Telemetry::default();
        let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        v["messages"] = json!([{"role":"user","content":[{"type":"text","text":"x".repeat(500)}]}]);
        let mut c = call(serde_json::to_vec(&v).unwrap(), "req_paste", "end_turn");
        c.started_at = frozen_now() - Duration::from_secs(5);
        t.ingest(c);
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        // 估算 0.8 + 50 = 50.8s，但会话起点只比提交早 3.1s。
        assert!((p.metrics[0].user_secs - 3.085).abs() < 0.05, "{}", p.metrics[0].user_secs);
    }

    /// 工具入参那几样的解析规则（`cap/auto-2.1.285-20260930` 的官方取值），不靠抓包目录。
    #[test]
    fn tool_input_helpers_follow_the_official_values() {
        let p = bash_profile("git log --oneline | head -3");
        assert_eq!((p.command_type.as_str(), p.class, p.argv0.as_str()), ("git", "vcs", "git"));
        assert_eq!((p.last_argv0.as_str(), p.subcommand.as_deref()), ("head", Some("log")));
        assert!(p.has_pipe && !p.has_chain && p.simple_commands == 2);
        let p = bash_profile("cd /x && find . -name '*.py' | xargs wc -l");
        assert_eq!((p.argv0.as_str(), p.last_argv0.as_str(), p.simple_commands), ("find", "wc", 2));
        let p = bash_profile("python3 -m unittest -v test_calc 2>&1; echo \"---\"");
        assert_eq!(
            (p.command_type.as_str(), p.class, p.argv0.as_str()),
            ("python3", "lang_runtime", "python")
        );
        assert!(p.has_chain && !p.has_redirect, "2>&1 不算重定向");
        assert!(!bash_profile("ls; head -5 README* 2>/dev/null").has_redirect);
        assert!(bash_profile("echo >> README.md").has_redirect);
        assert!(bash_profile("[ -n \"$(tail -c1 x)\" ] && echo").has_subshell);
        // 只读：工作目录外的路径不算。
        let cwd = Some("/w");
        assert!(bash_read_only(&bash_profile("ls -la"), cwd));
        assert!(bash_read_only(&bash_profile("grep -rn x /w/src"), cwd));
        assert!(!bash_read_only(&bash_profile("cat ~/.codex/config.toml"), cwd));
        assert!(!bash_read_only(&bash_profile("python3 calc.py"), cwd));
        assert!(!bash_read_only(&bash_profile("rm test_calc.py"), cwd));
        // 结果末尾客户端追加的提醒不计，输出自己的末行换行照算。
        assert_eq!(
            strip_trailing_reminder("a\nb\n\n<system-reminder>\nx\n</system-reminder>"),
            "a\nb\n"
        );
        assert_eq!(exit_code_of("Exit code 2\nboom"), 2);
        assert_eq!(exit_code_of("boom"), 1);
        assert_eq!(read_content_bytes("     1\tab\n     2\tcd"), 5);
        assert_eq!(file_ext("/a/b/calc.py"), "py");
        assert_eq!(file_ext("/a/.env"), "");
        // Edit 的入参补上默认的 replace_all。
        let edit = json!({"file_path": "/a", "old_string": "x", "new_string": "y"});
        assert_eq!(tool_input_len("Edit", &edit), edit.to_string().len() + 20);
        // 猜下一句被忽略：相似度是敲的字数 ÷ 建议的字数。
        let shown = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let submit: DateTime<Utc> = (shown + Duration::from_millis(4_066)).into();
        let m = ignored_suggestion_meta("req_1", shown, 28, submit, 79);
        assert_eq!(m["timeToIgnoreMs"], 4_066);
        assert!((m["similarity"].as_f64().unwrap() - 79.0 / 28.0).abs() < 1e-9);
    }

    #[test]
    fn dd_model_drops_the_date_suffix() {
        assert_eq!(dd_model_short("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
        assert_eq!(dd_model_short("claude-opus-5"), "claude-opus-5");
        assert_eq!(dd_model_short("claude-fable-5-1"), "claude-fable-5-1");
    }

    #[test]
    fn camel_to_snake_matches_datadog_spelling() {
        assert_eq!(camel_to_snake("costUSD"), "cost_u_s_d");
        assert_eq!(camel_to_snake("isTTY"), "is_t_t_y");
        assert_eq!(camel_to_snake("preNormalizedModel"), "pre_normalized_model");
        assert_eq!(camel_to_snake("stop_reason"), "stop_reason");
    }

    #[test]
    fn session_betas_is_a_filtered_subset_in_header_order() {
        let header = "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,server-side-fallback-2026-07-01,fallback-credit-2026-06-01,afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
        assert_eq!(
            session_betas(header),
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07",
            "cap/2.1.258/00020 那条 opus 事件的 betas"
        );
        // haiku 主线程（`cap/2.1.285/00051` 的出站头）→ 同批事件里的会话级 betas（`00060`）。
        let haiku = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
        assert_eq!(
            session_betas(haiku),
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05"
        );
        // opus-4-6（`cap/2.1.285/00083`）少 `mid-conversation-system`，会话级那份跟着少（`00090`）。
        let opus46 = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
        assert_eq!(
            session_betas(opus46),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05"
        );
    }

    #[test]
    fn version_comes_from_the_outbound_ua() {
        assert_eq!(version_from_ua("claude-cli/2.1.260 (external, cli)"), "2.1.260");
        assert_eq!(version_from_ua("claude-code/2.1.258"), "2.1.258");
        assert_eq!(version_from_ua("curl/8.0"), config::CC_VERSION_BASE);
    }

    /// Datadog 那份日志与 event_logging 是**同一套阶段规则**：启动早期两项都没有，界面起来
    /// 后只有 `renderer_mode`，用户提交后才多 `prompt_id`。
    ///
    /// 依据 `cap/2.1.260-2/00017` 一批 80 条：59 条两项皆无（`tengu_started`/`tengu_init`/
    /// `tengu_timer`/大部分 `tengu_feature_ok`）、7 条只有 `renderer_mode`、16 条两者都有。
    /// 原先这两项无条件写，于是每个会话有近六十条启动日志带着「界面模式」和一个当时还不
    /// 存在的 prompt id。
    #[test]
    fn datadog_entries_follow_the_same_metadata_stages() {
        let id = identity();
        let ctx = EventCtx { model: "claude-opus-5", betas: "a", prompt_id: "p", uptime_secs: 1.0 };
        let at = |stage| id.dd_entry_at(stage, "tengu_started", &ctx, "claude-opus-5", json!({}));

        let early = at(super::MetaStage::Startup);
        assert_eq!(early["renderer_mode"], Value::Null, "启动早期没有界面");
        assert_eq!(early["prompt_id"], Value::Null, "也还没有用户输入");
        assert_eq!(early["session_id"], id.session_id, "别的公共字段照旧");

        let mid = at(super::MetaStage::Renderer);
        assert_eq!(mid["renderer_mode"], "default");
        assert_eq!(mid["prompt_id"], Value::Null, "界面起来了但还没提交");

        let late = at(super::MetaStage::Prompt);
        assert_eq!(late["renderer_mode"], "default");
        assert_eq!(late["prompt_id"], "p");
    }

    /// `-p` 不起终端界面：两路都不写 `renderer_mode`，`cc_prompt_id` / `prompt_id` 照常分阶段
    /// （`cap/auto-2.1.285-20260930` 九个 `-p` 会话：事件 1751 条、Datadog 920 条一条都没有它）。
    #[test]
    fn sdk_sessions_never_report_a_renderer_mode() {
        let id = Identity { sdk: true, ..identity() };
        let ctx = EventCtx { model: "claude-opus-5", betas: "a", prompt_id: "p", uptime_secs: 1.0 };
        for stage in [super::MetaStage::Renderer, super::MetaStage::Prompt] {
            let dd = id.dd_entry_at(stage, "tengu_started", &ctx, "claude-opus-5", json!({}));
            let b64 = id.metadata_b64_at(stage, "p", json!({}));
            let meta: Value = serde_json::from_slice(&STANDARD.decode(b64).unwrap()).unwrap();
            assert_eq!(dd["renderer_mode"], Value::Null, "{stage:?}");
            assert_eq!(meta["renderer_mode"], Value::Null, "{stage:?}");
        }
        let prompt = id.metadata_b64_at(super::MetaStage::Prompt, "p", json!({}));
        let meta: Value = serde_json::from_slice(&STANDARD.decode(prompt).unwrap()).unwrap();
        assert_eq!(meta["cc_prompt_id"], "p", "提交之后照样带 prompt id");
    }

    #[test]
    fn metadata_is_standard_base64_with_padding() {
        let id = identity();
        let at = |stage| {
            let b64 = id.metadata_b64_at(stage, "p", json!({"feature_name":"notification_show"}));
            let decoded = STANDARD.decode(&b64).expect("standard base64");
            (b64, serde_json::from_slice::<Value>(&decoded).unwrap())
        };
        // 三个阶段各写几项，见 [`super::MetaStage`]。
        let (_, early) = at(super::MetaStage::Startup);
        assert_eq!(early["renderer_mode"], Value::Null, "启动早期没有界面");
        assert_eq!(early["cc_prompt_id"], Value::Null, "也还没有用户输入");
        assert_eq!(early["subscription_type"], "team");
        let (_, mid) = at(super::MetaStage::Renderer);
        assert_eq!(mid["renderer_mode"], "default");
        assert_eq!(mid["cc_prompt_id"], Value::Null, "界面起来了但还没提交");
        let (b64, v) = at(super::MetaStage::Prompt);
        assert_eq!(v["renderer_mode"], "default");
        assert_eq!(v["subscription_type"], "team");
        assert_eq!(v["cc_prompt_id"], "p");
        assert_eq!(v["feature_name"], "notification_show");
        // 抓包里的那串正好以 `=` 收尾；url-safe 无填充版本会解不出来。
        assert!(b64.ends_with('=') || b64.len().is_multiple_of(4));
    }

    #[test]
    fn event_carries_org_and_account_in_auth() {
        let id = identity();
        let ctx =
            EventCtx { model: "claude-opus-5[1m]", betas: "a,b", prompt_id: "p", uptime_secs: 9.5 };
        let ev = id.event("tengu_api_query", Utc::now(), &ctx, json!({}));
        let d = &ev["event_data"];
        assert_eq!(d["event_name"], "tengu_api_query");
        assert_eq!(d["auth"]["organization_uuid"], "09520b85-f6b6-432f-97e2-6ecb804a083f");
        assert_eq!(d["auth"]["account_uuid"], "9922ef8e-7945-4f5a-ab4f-cf5f521531df");
        assert_eq!(d["env"]["build_time"], "2026-09-01T21:54:40Z", "2.1.258 的构建时间");
        assert_eq!(d["model"], "claude-opus-5[1m]");
        let proc: Value =
            serde_json::from_slice(&STANDARD.decode(d["process"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(proc["uptime"], 9.5);
    }

    #[test]
    fn dd_entry_flattens_meta_and_tags_provider() {
        let id = identity();
        let ctx = EventCtx { model: "claude-opus-5", betas: "a", prompt_id: "p", uptime_secs: 1.0 };
        let e = id.dd_entry(
            "tengu_api_success",
            &ctx,
            "claude-opus-5",
            snake_flat(&json!({"requestId":"req_1","costUSD":0.5,"provider":"firstParty","cc_prompt_id":"x"})),
        );
        assert_eq!(e["request_id"], "req_1");
        assert_eq!(e["cost_u_s_d"], 0.5);
        assert!(e.get("cc_prompt_id").is_none(), "base 已有 prompt_id");
        assert_eq!(e["prompt_id"], "p");
        assert!(
            e["ddtags"].as_str().unwrap().contains("provider:firstParty,subscription_type:team")
        );
        assert_eq!(e["user_bucket"], 15);
    }

    #[test]
    fn parse_shape_reads_the_cc_body() {
        let s = parse_shape(&cc_body(true)).unwrap();
        assert_eq!(s.model, "claude-opus-5");
        assert_eq!(s.messages_len, 3);
        assert!(s.new_prompt);
        assert_eq!(s.prompt_len, "hello there".len());
        assert_eq!(s.cc_prompt_id.as_deref(), Some("6c079143-0c53-4c48-817d-105460b3f622"));
        assert!(!s.is_subagent);
        assert_eq!(s.system_blocks, 4);
        assert_eq!(s.sys0_len, 132, "billing header 那块的长度与抓包一致");
        assert_eq!(s.tools_count, 2);
        assert_eq!(s.deferred_tools, 1);
        assert_eq!(s.mcp_tools, 0);
        assert_eq!(s.tools_hash.len(), 12);
        assert_eq!(s.thinking_type, "adaptive");
        assert_eq!(s.effort.as_deref(), Some("high"));
        assert_eq!(s.permission_mode, "auto");
        assert!(s.cache_ttl_1h);
        assert_eq!(s.device_id.as_deref().map(str::len), Some(64));
        assert_eq!(s.session_id.as_deref(), Some("4dc73702-d904-4887-809d-17b93cc5357c"));
        let cont = parse_shape(&cc_body(false)).unwrap();
        assert!(!cont.new_prompt, "tool_result 续轮不是新输入");
    }

    #[test]
    fn full_chain_for_a_main_thread_call_and_continuation() {
        let t = Telemetry::default();
        t.ingest(call(cc_body(true), "req_1", "tool_use"));
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).expect("queued under the credential + session");
        let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
        assert!(names.contains(&"tengu_api_query"));
        assert!(names.contains(&"tengu_api_success"));
        assert!(names.contains(&"tengu_input_prompt"), "新输入才有");
        assert!(names.contains(&"tengu_turn_first_text"));
        assert!(!names.contains(&"tengu_turn_end"), "stop_reason=tool_use 这一轮还没结束");
        assert!(names.contains(&"tengu_tool_schema_sizes"), "首次见到这套工具");
        let success = p
            .events
            .iter()
            .find(|(_, e)| e["event_data"]["event_name"] == "tengu_api_success")
            .unwrap();
        let meta: Value = serde_json::from_slice(
            &STANDARD
                .decode(success.1["event_data"]["additional_metadata"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(meta["requestId"], "req_1");
        assert_eq!(meta["model"], "claude-opus-5");
        assert_eq!(meta["preNormalizedModel"], "claude-opus-5[1m]", "context-1m beta 还原 [1m]");
        assert_eq!(meta["cachedInputTokens"], 26736);
        assert_eq!(meta["uncachedInputTokens"], 8729);
        assert_eq!(meta["messageTokens"], 0, "首条没有上一轮");
        assert!(meta.get("previousRequestId").is_none());
        assert_eq!(meta["gzipSkipReason"], "below_min_size", "请求体不到 4 KiB");
        assert_eq!(meta["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622");
        // api_success 顶层 model 跟 meta 走，是规范名；api_query 则是展示名。
        assert_eq!(success.1["event_data"]["model"], "claude-opus-5");
        let query = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_query").unwrap();
        assert_eq!(query.1["event_data"]["model"], "claude-opus-5[1m]");
        // API 事件（query/success）报的是**这条请求的完整 beta 串**，含 `effort` 这种
        // 只在请求头上出现的项；界面事件（turn_end 等）报会话级那份，见
        // [`super::Identity::event`]。`cap/2.1.260-2/00016` 同一批里两者取值不同。
        const FULL: &str = "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
             interleaved-thinking-2025-05-14,effort-2025-11-24";
        // 会话级那份**始终带 `redact-thinking`**，哪怕请求头里没有——见 [`super::session_betas`]。
        const SESSION: &str = "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
             interleaved-thinking-2025-05-14,redact-thinking-2026-02-12";
        assert_eq!(success.1["event_data"]["betas"], FULL, "api_success 报完整串");
        assert_eq!(query.1["event_data"]["betas"], FULL, "api_query 也报完整串");
        let other = p
            .events
            .iter()
            .find(|(_, e)| !matches!(ev_name(e), "tengu_api_query" | "tengu_api_success"))
            .expect("这一批里总有别的事件");
        assert_eq!(
            other.1["event_data"]["betas"],
            SESSION,
            "非 API 事件（{}）报会话级那份",
            ev_name(&other.1)
        );
        let dd_success = p.dd.iter().find(|d| d["message"] == "tengu_api_success").unwrap();
        assert_eq!(dd_success["betas"], FULL, "Datadog 的 api_success 同样是完整串");
        assert_eq!(
            success.1["event_data"]["auth"]["organization_uuid"],
            "09520b85-f6b6-432f-97e2-6ecb804a083f"
        );
        assert_eq!(p.dd.iter().filter(|d| d["message"] == "tengu_api_success").count(), 1);
        drop(st);

        // 续轮：tool_result 收尾，end_turn → turn_end；previousRequestId 串上一条。
        // 第二条在第一条结束之后才发出（第一条 8s 前发、跑了 6.1s）。
        let mut second = call(cc_body(false), "req_2", "end_turn");
        second.started_at = frozen_now();
        t.ingest(second);
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        let success2 = p
            .events
            .iter()
            .filter(|(_, e)| e["event_data"]["event_name"] == "tengu_api_success")
            .nth(1)
            .unwrap();
        let meta2: Value = serde_json::from_slice(
            &STANDARD
                .decode(success2.1["event_data"]["additional_metadata"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(meta2["previousRequestId"], "req_1");
        // 上一条 input + cache_read + cache_creation + output：`cap/2.1.258` 那条正是 35498。
        assert_eq!(meta2["messageTokens"], 2 + 26736 + 8729 + 31);
        assert!(meta2.get("timeSinceLastApiCallMs").is_some());
        let names2: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
        assert_eq!(names2.iter().filter(|n| **n == "tengu_turn_end").count(), 1);
        assert_eq!(
            names2.iter().filter(|n| **n == "tengu_input_prompt").count(),
            1,
            "续轮不算新输入"
        );
        assert_eq!(
            names2.iter().filter(|n| **n == "tengu_tool_schema_sizes").count(),
            1,
            "工具没变不再报"
        );
        assert_eq!(p.metrics.len(), 2);
        assert!(p.metrics[0].new_session && !p.metrics[1].new_session);
    }

    #[test]
    fn helper_calls_skip_turn_events_and_subagents_are_flagged() {
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["tools"] = json!([]);
        let t = Telemetry::default();
        t.ingest(call(body.to_string().into_bytes(), "req_h", "end_turn"));
        let st = t.0.state.lock();
        let names: Vec<String> =
            st.pending[&key()].events.iter().map(|(_, e)| ev_name(e).to_string()).collect();
        assert!(!names.iter().any(|n| n == "tengu_turn_end"));
        assert!(!names.iter().any(|n| n == "tengu_input_prompt"));
        assert!(names.iter().any(|n| n == "tengu_api_success"));
        drop(st);

        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["system"][0]["text"] = json!(
            "x-anthropic-billing-header: cc_version=2.1.260.660; cc_entrypoint=cli; cch=590f3; cc_is_subagent=true;"
        );
        let s = parse_shape(body.to_string().as_bytes()).unwrap();
        assert!(s.is_subagent);
        assert!(s.cc_prompt_id.is_none());
    }

    #[test]
    fn calls_without_identity_are_ignored() {
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body.as_object_mut().unwrap().remove("metadata");
        let t = Telemetry::default();
        let mut c = call(body.to_string().into_bytes(), "req_x", "end_turn");
        c.session_header = Some("s".into());
        t.ingest(c);
        assert!(t.0.state.lock().pending.is_empty(), "没有 device_id 就不报");
    }

    #[test]
    fn take_due_respects_the_three_cadences() {
        let t = Telemetry::default();
        t.ingest(call(cc_body(true), "req_1", "end_turn"));
        let now = Instant::now();
        assert!(t.take_due(now).is_empty(), "刚攒下，什么都还没到期");
        let later = now + Duration::from_secs(config::TELEMETRY_DATADOG_FLUSH_SECS + 1);
        let due = t.take_due(later);
        assert_eq!(due.len(), 1);
        assert!(due[0].events.is_empty() && !due[0].dd.is_empty() && due[0].metrics.is_none());
        let later = now + Duration::from_secs(config::TELEMETRY_EVENT_FLUSH_SECS + 1);
        let due = t.take_due(later);
        assert_eq!(due.len(), 1);
        assert!(!due[0].events.is_empty() && due[0].dd.is_empty());
        // 事件按时间排好序。
        let ts: Vec<&str> = due[0].events.iter().map(ev_ts).collect();
        let mut sorted = ts.clone();
        sorted.sort();
        assert_eq!(ts, sorted);
        assert_eq!(due[0].version, "2.1.258");
        let later = now + Duration::from_secs(config::TELEMETRY_METRICS_FLUSH_SECS + 1);
        let due = t.take_due(later);
        let m = due[0].metrics.as_ref().expect("metrics due");
        let names: Vec<&str> =
            m["metrics"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "claude_code.session.count",
                "claude_code.cost.usage",
                "claude_code.token.usage",
                "claude_code.active_time.total"
            ]
        );
        let cost = &m["metrics"][1]["data_points"][0];
        assert_eq!(cost["attributes"]["organization.id"], "09520b85-f6b6-432f-97e2-6ecb804a083f");
        assert_eq!(cost["attributes"]["model"], "claude-opus-5[1m]");
        assert_eq!(cost["attributes"]["query_source"], "main");
        assert_eq!(cost["value"], 0.18);
        assert_eq!(m["metrics"][2]["data_points"].as_array().unwrap().len(), 4);
        assert!(t.take_due(later + Duration::from_secs(1)).is_empty(), "取空后不再有东西");
        assert_eq!(t.org_uuid(7).as_deref(), Some("09520b85-f6b6-432f-97e2-6ecb804a083f"));
    }

    /// 读一份抓包请求：请求体与出站 `anthropic-beta`。文件不在（打包的源码里没有 `cap/`）返回 `None`。
    fn cap_request(rel: &str) -> Option<(Vec<u8>, Option<String>)> {
        let dir = format!("{}/cap/{rel}", env!("CARGO_MANIFEST_DIR"));
        let (dir, prefix) = dir.rsplit_once('/').unwrap();
        let path =
            std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".req.raw"))
            })?;
        let raw = std::fs::read(path).ok()?;
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
        let head = std::str::from_utf8(&raw[..sep]).ok()?;
        let betas = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("anthropic-beta: ").or_else(|| l.strip_prefix("Anthropic-Beta: "))
            })
            .map(str::to_string);
        Some((raw[sep..].to_vec(), betas))
    }

    /// `omittedBytes` 是 `system[1..]` 与 `tools` 两段 JSON 的长度之和（`cap/2.1.277/00031`
    /// 那条续用线程报 85845）；末条 assistant 之后的条数即 `deltaMessageCount`。
    #[test]
    fn omitted_bytes_match_the_capture_when_it_is_present() {
        let Some((body, _)) = cap_request("2.1.277/00031_") else {
            eprintln!("skipped: cap/2.1.277 not present");
            return;
        };
        let s = parse_shape(&body).unwrap();
        assert_eq!(s.omitted_bytes, 85845);
        assert_eq!(s.after_last_assistant, 2);
    }

    /// 回放 `cap/2.1.280` 的七条主线程请求（每轮换一次模型、前三轮 auto 后四轮 default），
    /// tether 判定、无状态标记、快照来源、每轮输入那几条新事件逐条对官方批次。
    #[test]
    fn main_thread_replay_matches_the_2_1_280_capture() {
        const FILES: [&str; 7] =
            ["00021_", "00029_", "00033_", "00038_", "00065_", "00068_", "00073_"];
        let mut reqs = Vec::new();
        for f in FILES {
            let Some(r) = cap_request(&format!("2.1.280/{f}")) else {
                eprintln!("skipped: cap/2.1.280 not present");
                return;
            };
            reqs.push(r);
        }
        let session = parse_shape(&reqs[0].0).unwrap().session_id.unwrap();
        let t = Telemetry::default();
        let base = frozen_now() - Duration::from_secs(600);
        for (i, (body, betas)) in reqs.into_iter().enumerate() {
            let model = parse_shape(&body).unwrap().model;
            let mut c = call(body, &format!("req_{i}"), "end_turn");
            c.betas = betas;
            c.ua_out = "claude-cli/2.1.280 (external, cli)".into();
            c.resp_model = Some(model);
            c.started_at = base + Duration::from_secs(20 * i as u64);
            t.ingest(c);
        }
        let st = t.0.state.lock();
        let p = st.pending.get(&(7, session)).expect("queued");
        let metas = |n: &str| -> Vec<Value> {
            p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
        };
        let col = |v: &[Value], k: &str| -> Vec<Value> { v.iter().map(|m| m[k].clone()).collect() };
        let b = |xs: &[bool]| -> Vec<Value> { xs.iter().map(|x| json!(x)).collect() };

        let queries = metas("tengu_api_query");
        assert_eq!(
            col(&queries, "permissionMode"),
            ["auto", "auto", "auto", "default", "default", "default", "default"].map(|s| json!(s)),
            "auto 模式的提示在消息里"
        );

        let dec = metas("tengu_tether_decision");
        assert_eq!(dec.len(), 7);
        assert_eq!(col(&dec, "reason")[0], "first_request");
        assert!(col(&dec, "reason")[1..].iter().all(|r| r == "config_changed"));
        assert!(col(&dec, "decision").iter().all(|d| d == "create"));
        assert_eq!(col(&dec, "messageCount"), [2, 5, 8, 9, 16, 19, 22].map(|n| json!(n)));
        assert_eq!(col(&dec, "prevMessageCount"), [0, 2, 5, 8, 9, 16, 19].map(|n| json!(n)));
        assert_eq!(col(&dec, "changedModel"), b(&[false, true, true, true, true, true, true]));
        assert_eq!(col(&dec, "changedEffort"), b(&[false, true, false, true, true, true, false]));
        assert_eq!(
            col(&dec, "changedLatchedHeaders"),
            b(&[false, false, false, true, false, false, false])
        );
        assert!(col(&dec, "changedTools").iter().all(|v| v == false), "延迟工具不进长度表");
        assert_eq!(
            col(&dec, "modelHeldStateless"),
            b(&[true, true, false, false, true, true, false])
        );
        assert_eq!(
            col(&dec, "classifierHeldStateless"),
            b(&[true, true, true, false, false, false, false])
        );
        assert_eq!(metas("tengu_tether_echo_audit").len(), 6, "首条没有回声审计");
        let live = metas("tengu_tether_live_outcome");
        assert_eq!(
            col(&live, "sentThreadType"),
            ["none", "none", "none", "create", "none", "none", "create"].map(|s| json!(s))
        );
        assert!(col(&live, "omittedBytes").iter().all(|v| v == 0), "没有续用线程的");

        let ok = metas("tengu_api_success");
        assert_eq!(col(&ok, "systemPromptSource")[0], "live_recorded");
        assert!(col(&ok, "systemPromptSource")[1..].iter().all(|s| s == "from_snapshot"));
        let hashes = col(&ok, "snapshotHash");
        assert!(hashes.iter().all(|h| h == &hashes[0] && h.as_str().unwrap().len() == 12));
        assert!(col(&ok, "turn_origin").iter().all(|o| o == "human"));
        assert!(ok.iter().all(|m| m["firstContentMs"].as_i64().unwrap() >= 1800));
        assert!(ok.iter().all(|m| m["clientRequestId"] == "3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e"));

        let inputs = metas("tengu_input_prompt");
        assert_eq!(inputs.len(), 7);
        assert!(inputs[3].get("effort_level").is_none(), "haiku 没有 effort");
        assert_eq!(metas("tengu_sleepy_snowflake_applied").len(), 4, "每个模型一次");
        assert_eq!(metas("tengu_auto_mode_git_state_probe").len(), 3, "只在 auto 下");
        assert_eq!(metas("tengu_declared_tool_set_held").len(), 7);

        // Datadog 那份带 tether 三条，判定那条的 ddtags 按字母序多出 decision / reason。
        let dd_dec: Vec<&Value> =
            p.dd.iter().filter(|d| d["message"] == "tengu_tether_decision").collect();
        assert_eq!(dd_dec.len(), 7);
        assert!(
            dd_dec[1]["ddtags"]
                .as_str()
                .unwrap()
                .contains("client_type:cli,decision:create,entrypoint:cli")
        );
        assert!(
            dd_dec[1]["ddtags"]
                .as_str()
                .unwrap()
                .contains("platform:darwin,reason:config_changed,subscription_type")
        );
        assert_eq!(p.dd.iter().filter(|d| d["message"] == "tengu_tether_live_outcome").count(), 7);
    }

    /// 回放 `cap/2.1.285` 的 11 条主线程请求（一个会话里 `/model` 轮流切 11 个模型、前三轮
    /// auto），逐条对官方批次：只有 fable-5-1 钉成无状态、auto 不再钉；`toolAdditionHistory`
    /// 跟着 `mid-conversation-tool-changes` 走；tether 两条与 `api_success` 的新字段落在官方
    /// 位置（键序取自 `00040` 批次）；默认 effort 按模型报。
    #[test]
    fn main_thread_replay_matches_the_2_1_285_capture() {
        const FILES: [&str; 11] = [
            "00030_", "00039_", "00045_", "00051_", "00055_", "00061_", "00067_", "00072_",
            "00077_", "00083_", "00088_",
        ];
        let mut reqs = Vec::new();
        for f in FILES {
            let Some(r) = cap_request(&format!("2.1.285/{f}")) else {
                eprintln!("skipped: cap/2.1.285 not present");
                return;
            };
            reqs.push(r);
        }
        let session = parse_shape(&reqs[0].0).unwrap().session_id.unwrap();
        let t = Telemetry::default();
        let base = frozen_now() - Duration::from_secs(900);
        for (i, (body, betas)) in reqs.into_iter().enumerate() {
            let model = parse_shape(&body).unwrap().model;
            let mut c = call(body, &format!("req_{i}"), "end_turn");
            c.betas = betas;
            c.ua_out = "claude-cli/2.1.285 (external, cli)".into();
            c.resp_model = Some(model);
            c.started_at = base + Duration::from_secs(20 * i as u64);
            t.ingest(c);
        }
        let st = t.0.state.lock();
        let p = st.pending.get(&(7, session)).expect("queued");
        let metas = |n: &str| -> Vec<Value> {
            p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
        };
        let col = |v: &[Value], k: &str| -> Vec<Value> { v.iter().map(|m| m[k].clone()).collect() };
        let b = |xs: &[bool]| -> Vec<Value> { xs.iter().map(|x| json!(x)).collect() };
        let keys = |m: &Value| -> Vec<String> {
            m.as_object().unwrap().keys().filter(|k| !k.starts_with("cc_")).cloned().collect()
        };
        let subseq = |got: &[String], want: &[&str]| {
            let mut it = got.iter();
            want.iter().all(|w| it.any(|g| g == w))
        };

        let dec = metas("tengu_tether_decision");
        assert_eq!(dec.len(), 11);
        let only_fable =
            b(&[false, true, false, false, false, false, false, false, false, false, false]);
        assert_eq!(col(&dec, "modelHeldStateless"), only_fable);
        assert!(col(&dec, "classifierHeldStateless").iter().all(|v| v == false), "auto 不再钉");
        assert!(col(&dec, "creditRetryStateless").iter().all(|v| v == false));
        assert!(subseq(
            &keys(&dec[0]),
            &["classifierHeldStateless", "creditRetryStateless", "dropHeldStateless"]
        ));

        let live = metas("tengu_tether_live_outcome");
        assert_eq!(
            col(&live, "sentThreadType"),
            [
                "create", "none", "create", "create", "create", "create", "create", "create",
                "create", "create", "create"
            ]
            .map(|s| json!(s)),
            "按请求体：只有 fable-5-1 那条没写 thread"
        );
        assert_eq!(
            col(&live, "toolAdditionHistory"),
            b(&[true, true, false, false, false, true, true, true, false, false, false])
        );
        assert!(subseq(
            &keys(&live[0]),
            &[
                "classifierHeldStateless",
                "creditRetryStateless",
                "toolResultClearingHeldStateless",
                "serverToolHistory"
            ]
        ));

        let held = metas("tengu_declared_tool_set_held");
        assert_eq!(held.len(), 11);
        assert!(
            held.iter()
                .all(|m| m["noDeferredChannel"] == false && m["unclassifiedDepartures"] == 0)
        );

        let ok = metas("tengu_api_success");
        assert_eq!(ok.len(), 11);
        // `cap/2.1.285/00040` 那条 opus-5-5 的键序（去掉公共头三项与本会话没有的字段）。
        let want = [
            "model",
            "dispatch",
            "betas",
            "echoWireToolInputs",
            "messageCount",
            "messageTokens",
            "inputTokens",
            "outputTokens",
            "cachedInputTokens",
            "uncachedInputTokens",
            "uncoveredTailReason",
            "uncoveredTailMessages",
            "resentTailTokensEst",
            "sentOnceTailTokensEst",
            "unexcusedTailTokensEst",
            "plainInputExcessTokens",
            "durationMs",
            "durationMsIncludingRetries",
            "attempt",
            "ttftMs",
            "firstContentMs",
            "queryOverheadMs",
            "buildAgeMins",
            "provider",
            "requestId",
            "stop_reason",
            "effort_level",
            "turn_origin",
            "is_default_model",
            "default_model",
            "is_default_effort",
            "default_effort_level",
            "costUSD",
            "querySource",
            "requestBodyChars",
            "requestPrepareMs",
            "gzipSkipReason",
            "fastMode",
        ];
        for m in &ok {
            assert!(subseq(&keys(m), &want) || m.get("effort_level").is_none(), "{m}");
            assert_eq!(m["dispatch"], "v2d");
            assert_eq!(m["uncoveredTailReason"], "none");
            assert_eq!(m["plainInputExcessTokens"], m["inputTokens"]);
            let q = m["queryOverheadMs"].as_i64().unwrap();
            assert!((6..=46).contains(&q), "{q}");
        }
        assert_eq!(
            col(&ok, "default_effort_level"),
            [
                json!("medium"),
                json!("high"),
                json!("medium"),
                Value::Null,
                json!("high"),
                json!("high"),
                json!("high"),
                json!("high"),
                json!("xhigh"),
                json!("high"),
                json!("high")
            ]
        );
        // Datadog 那份的 ddtags 多 `uncovered_tail_reason`，落在 subscription_type 与 user_bucket 之间。
        let dd_ok: Vec<&Value> =
            p.dd.iter().filter(|d| d["message"] == "tengu_api_success").collect();
        assert_eq!(dd_ok.len(), 11);
        let tags = dd_ok[0]["ddtags"].as_str().unwrap();
        assert!(tags.contains(",uncovered_tail_reason:none,user_bucket:15,"), "{tags}");
        let sub = tags.find("subscription_type:").unwrap();
        assert!(sub < tags.find("uncovered_tail_reason:").unwrap(), "{tags}");
    }

    /// 回放 `cap/2.1.285` 第二批（`00113` 起：主线程工具调用与 thread 续轮、claude-code-guide
    /// 子代理首轮与续轮、WebFetch 之后的无工具页面处理、子代理摘要、task 通知那一轮）：
    /// 分类、未覆盖尾段的四种取值、续轮的 `deltaMessageCount` 逐条对官方批次。
    #[test]
    fn tool_and_subagent_replay_matches_the_2_1_285_capture() {
        // 每条的停止原因与回复里的工具调用（工具名 → 入参字符数），取自各自的响应。
        type Step = (&'static str, &'static str, &'static [(&'static str, usize)]);
        const FILES: [Step; 18] = [
            ("00113", "tool_use", &[("Skill", 156)]),
            ("00115", "end_turn", &[]),
            ("00118", "tool_use", &[("Agent", 913)]),
            ("00120", "tool_use", &[("WebFetch", 207)]),
            ("00121", "end_turn", &[]),
            ("00125", "end_turn", &[]),
            ("00127", "tool_use", &[("WebFetch", 493)]),
            ("00131", "tool_use", &[("Read", 192), ("WebFetch", 144)]),
            ("00134", "end_turn", &[]),
            ("00135", "end_turn", &[]),
            ("00136", "tool_use", &[("WebFetch", 149)]),
            ("00140", "end_turn", &[]),
            ("00142", "tool_use", &[("WebFetch", 353)]),
            ("00144", "end_turn", &[]),
            ("00147", "end_turn", &[]),
            ("00148", "end_turn", &[]),
            ("00149", "end_turn", &[]),
            ("00158", "end_turn", &[]),
        ];
        let t = Telemetry::default();
        let base = frozen_now() - Duration::from_secs(900);
        let mut session = String::new();
        for (i, (f, stop, tools)) in FILES.iter().enumerate() {
            let rel = format!("2.1.285/{f}_");
            let Some((body, betas)) = cap_request(&rel) else {
                eprintln!("skipped: cap/2.1.285 not present");
                return;
            };
            let shape = parse_shape(&body).unwrap();
            session = shape.session_id.clone().unwrap();
            let mut c = call(body, &format!("req_{f}"), stop);
            c.tool_use_lens = tools.iter().map(|(n, l)| (n.to_string(), *l)).collect();
            c.betas = betas;
            c.ua_out = "claude-cli/2.1.285 (external, cli)".into();
            c.resp_model = Some(shape.model.clone());
            c.message_id = Some(format!("msg_{f}"));
            c.started_at = base + Duration::from_secs(10 * i as u64);
            c.client_request_id = cap_header(&rel, "x-client-request-id");
            c.agent = AgentHeaders {
                agent_id: cap_header(&rel, "x-claude-code-agent-id"),
                agent_type: cap_header(&rel, "x-claude-code-agent-type"),
                request_class: cap_header(&rel, "x-claude-code-request-class"),
            };
            t.ingest(c);
        }
        let st = t.0.state.lock();
        let p = st.pending.get(&(7, session)).expect("queued");
        let ok: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_success")
            .map(|(_, e)| meta_of(e))
            .collect();
        let by_source =
            |src: &str| -> Vec<&Value> { ok.iter().filter(|m| m["querySource"] == src).collect() };
        let count = |n: &str| p.events.iter().filter(|(_, e)| ev_name(e) == n).count();
        let features = |f: &str| {
            p.events
                .iter()
                .filter(|(_, e)| {
                    ev_name(e) == "tengu_feature_ok" && meta_of(e)["feature_name"] == f
                })
                .count()
        };

        // thread 续轮的工具事件链配回来了：主线程 Skill、Agent 各一次，子代理九次（八次
        // WebFetch、一次 Read），条数与官方批次相同。
        let tool_uses: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_tool_use_success")
            .map(|(_, e)| meta_of(e))
            .collect();
        let names: Vec<&str> = tool_uses.iter().map(|m| m["toolName"].as_str().unwrap()).collect();
        assert_eq!(names.iter().filter(|n| **n == "WebFetch").count(), 8, "{names:?}");
        assert_eq!(names.iter().filter(|n| **n == "Read").count(), 1, "{names:?}");
        assert_eq!(names.iter().filter(|n| **n == "Skill" || **n == "Agent").count(), 2);
        assert!(tool_uses.iter().all(|m| m.get("toolResultTokensEst").is_some()));
        // 权限：非 auto 的 Skill 走弹框，其余十个走配置放行，键序官方那样。
        assert_eq!(count("tengu_tool_use_show_permission_request"), 1);
        assert_eq!(count("tengu_tool_use_granted_in_prompt_temporary"), 1);
        assert_eq!(count("tengu_tool_use_granted_in_config"), 10);
        assert_eq!(count("tengu_tool_use_can_use_tool_allowed"), 11);
        let granted = p
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_tool_use_granted_in_config")
            .map(|(_, e)| meta_of(e))
            .unwrap();
        let keys: Vec<&str> = granted.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys[keys.len() - 4..], ["messageID", "isMcp", "toolName", "sandboxEnabled"]);
        // 工具跑完那条 `tool_<名>`，Agent / Skill 各有前置的一条。
        assert_eq!(features("tool_web_fetch"), 8);
        assert_eq!(features("tool_read"), 1);
        assert_eq!(features("tool_agent"), 1);
        assert_eq!(features("tool_skill"), 1);
        assert_eq!(features("subagent_launch"), 1);
        assert_eq!(features("skill_invoke"), 1);
        // 那条 58.9KB 的 WebFetch 结果落了盘：原始大小报回去，另有一条落盘事件。
        assert_eq!(count("tengu_tool_result_persisted"), 1);
        assert!(tool_uses.iter().any(|m| m["toolResultWillPersist"] == true));
        // 子代理首步前主线程选定子代理、支线上解析模型；首两步的上下文记录与回放。
        assert_eq!(count("tengu_agent_tool_selected"), 1);
        assert_eq!(features("subagent_model_resolve"), 1);
        // 这段回放里 00113 就是会话首次输入：主线程模板那组一记一放、第二条（00115）再放一次，
        // 子代理首步一记一放、第二步再放一次。
        assert_eq!(count("tengu_reminder_fold_recorded"), 2);
        assert_eq!(count("tengu_reminder_fold_replayed"), 4);
        assert_eq!(count("tengu_wire_tool_input_echo_replayed"), 4);
        // 离开回顾（00158）是一轮辅助调用，收尾照官方。
        assert_eq!(by_source("away_summary").len(), 1);
        assert_eq!(features("away_summary_generate"), 1);
        // 子代理与它的摘要报 claude-code-guide 自带的 dontAsk。
        for m in by_source("agent_summary") {
            assert_eq!(m["permissionMode"], "dontAsk");
        }

        // WebFetch 页面处理：四条，挂在子代理那条支线上但没有链、没有 queryOverheadMs，
        // 不带断点 → `caching_off`，尾段就是整段消息。
        let wfa = by_source("web_fetch_apply");
        assert_eq!(wfa.len(), 4, "{:?}", ok.iter().map(|m| &m["querySource"]).collect::<Vec<_>>());
        for m in &wfa {
            assert!(m.get("queryChainId").is_none() && m.get("queryOverheadMs").is_none(), "{m}");
            assert_eq!(m["uncoveredTailReason"], "caching_off");
            assert_eq!(m["uncoveredTailMessages"], 1);
            assert!(m.get("plainInputExcessTokens").is_none());
            assert_eq!(m["permissionMode"], "default");
        }
        // 子代理摘要：fork 那种尾段。
        let summary = by_source("agent_summary");
        assert_eq!(summary.len(), 2);
        for m in &summary {
            assert_eq!(m["uncoveredTailReason"], "fork_tail_skip_cache_write");
            assert_eq!(m["uncoveredTailMessages"], 1);
            assert!(m.get("plainInputExcessTokens").is_none());
        }
        // thread 续轮：主线程四条（00115、00118、00121、00149）、子代理五条都是 `threaded_continue`；
        // 两条首轮（00113、00120）是 `none` 并报 `plainInputExcessTokens`。
        let main = by_source("repl_main_thread");
        let sub = by_source("agent:builtin:claude-code-guide");
        assert_eq!((main.len(), sub.len()), (5, 6));
        let reasons = |v: &[&Value]| -> Vec<Value> {
            v.iter().map(|m| m["uncoveredTailReason"].clone()).collect()
        };
        assert_eq!(
            reasons(&main),
            [
                "none",
                "threaded_continue",
                "threaded_continue",
                "threaded_continue",
                "threaded_continue"
            ]
            .map(|s| json!(s))
        );
        assert_eq!(reasons(&sub)[0], "none");
        assert!(reasons(&sub)[1..].iter().all(|r| r == "threaded_continue"));
        assert!(main[0].get("plainInputExcessTokens").is_some());
        assert!(main[1].get("plainInputExcessTokens").is_none());

        // tether：续轮的 deltaMessageCount 是增量体的条数——主线程 `[2, 2, 2, 1]`（最后那条 task
        // 通知轮只有一条消息），子代理恒 1（`00122`、`00137`、`00146`、`00151` 批次）。
        let live: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_tether_live_outcome")
            .map(|(_, e)| meta_of(e))
            .collect();
        let deltas = |cat: &str| -> Vec<Value> {
            live.iter()
                .filter(|m| m["sentThreadType"] == "continue" && m["sourceCategory"] == cat)
                .map(|m| m["deltaMessageCount"].clone())
                .collect()
        };
        assert_eq!(deltas("main"), [2, 2, 2, 1].map(|n| json!(n)));
        assert_eq!(deltas("subagent"), [1, 1, 1, 1, 1].map(|n| json!(n)));
        assert!(live.iter().any(|m| m["sentThreadType"] == "continue"));

        // 续轮前那条附件事件带 2.1.285 的两项。
        let att: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_attachments")
            .map(|(_, e)| meta_of(e))
            .collect();
        for m in &att {
            assert_eq!(m["attachment_token_estimates"].as_array().unwrap().last().unwrap(), "23");
            assert!(m["query_source"].is_string(), "{m}");
        }
    }

    /// **对照工具，默认不跑**：把 `LUBAN_REPLAY_CALLS`（抓包提出来的一串调用，见 scratchpad 里
    /// 的 `calls.py`）按原时刻喂进遥测，把生成的 event_logging 事件与 Datadog 条目写到
    /// `LUBAN_REPLAY_OUT`，给脚本逐段与官方批次比。
    /// `cargo test dump_telemetry_replay -- --ignored --nocapture`。
    #[test]
    #[ignore]
    fn dump_telemetry_replay() {
        let (Ok(inp), Ok(out)) =
            (std::env::var("LUBAN_REPLAY_CALLS"), std::env::var("LUBAN_REPLAY_OUT"))
        else {
            eprintln!("set LUBAN_REPLAY_CALLS / LUBAN_REPLAY_OUT");
            return;
        };
        let calls: Vec<Value> = serde_json::from_slice(&std::fs::read(inp).unwrap()).unwrap();
        let t = Telemetry::default();
        let s = |c: &Value, k: &str| c[k].as_str().map(str::to_string);
        for c in &calls {
            let mut a = call(c["body"].as_str().unwrap().as_bytes().to_vec(), "x", "end_turn");
            a.betas = s(c, "betas");
            a.ua_out = s(c, "ua").unwrap_or_default();
            a.session_header = s(c, "session_header");
            a.organization_id = s(c, "organization_id");
            a.started_at =
                SystemTime::UNIX_EPOCH + Duration::from_millis(c["started_ms"].as_u64().unwrap());
            a.ttft_ms = c["ttft_ms"].as_u64();
            a.total_ms = c["total_ms"].as_u64().unwrap_or(1000);
            a.request_id = s(c, "request_id");
            a.client_request_id = s(c, "client_request_id");
            a.agent = AgentHeaders {
                agent_id: s(c, "agent_id"),
                agent_type: s(c, "agent_type"),
                request_class: s(c, "request_class"),
            };
            a.message_id = s(c, "message_id");
            a.stop_reason = s(c, "stop_reason");
            a.resp_model = s(c, "resp_model");
            a.input_tokens = c["input_tokens"].as_i64().unwrap_or(0);
            a.output_tokens = c["output_tokens"].as_i64().unwrap_or(0);
            a.cache_read_tokens = c["cache_read"].as_i64().unwrap_or(0);
            a.cache_creation_tokens = c["cache_creation"].as_i64().unwrap_or(0);
            a.text_chars = c["text_chars"].as_u64().unwrap_or(0) as usize;
            a.reply_input_chars = a.text_chars;
            a.thinking_chars = c["thinking_chars"].as_u64().unwrap_or(0) as usize;
            a.saw_thinking = c["saw_thinking"].as_bool().unwrap_or(false);
            a.tool_use_lens = c["tool_use_lens"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| (x[0].as_str().unwrap().to_string(), x[1].as_u64().unwrap() as usize))
                .collect();
            a.cost_usd = c["cost_usd"].as_f64();
            a.aborted = c["aborted"].as_bool().unwrap_or(false);
            a.tool_calls = c["tool_calls"]
                .as_array()
                .map(|v| {
                    v.iter()
                        .map(|x| ToolCall {
                            id: x[0].as_str().unwrap_or("").to_string(),
                            name: x[1].as_str().unwrap_or("").to_string(),
                            input: x[2].clone(),
                            verdict: x[3].as_str().map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default();
            t.ingest(a);
        }
        let st = t.0.state.lock();
        let mut events = Vec::new();
        let mut dd = Vec::new();
        for p in st.pending.values() {
            for (ts, e) in &p.events {
                let d = &e["event_data"];
                let meta = d
                    .get("additional_metadata")
                    .and_then(|m| m.as_str())
                    .and_then(|m| STANDARD.decode(m).ok())
                    .and_then(|m| serde_json::from_slice::<Value>(&m).ok())
                    .unwrap_or(Value::Null);
                events.push(json!({
                    "ts": ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    "type": e["event_type"],
                    "name": d.get("event_name").or_else(|| d.get("experiment_id")),
                    "meta": meta,
                    "top": {"model": d.get("model"), "agent_id": d.get("agent_id"), "betas": d.get("betas")},
                    "sid": d.get("session_id"),
                    "parent": d.get("parent_session_id"),
                }));
            }
            dd.extend(p.dd.iter().cloned());
        }
        std::fs::write(out, serde_json::to_vec(&json!({"events": events, "dd": dd})).unwrap())
            .unwrap();
    }

    /// 出站版本决定用哪份模板：2.1.285 起是那份（启动段有 `managed_config_ready`、首次输入有
    /// 上下文宣告、收尾没有 `tip_shown`），之前的仍是 2.1.260 那份。Datadog 里
    /// `is_claude_ai_auth` 紧跟 `betas`。
    #[test]
    fn the_template_follows_the_outbound_version() {
        let names_for = |ua: &str| {
            let t = Telemetry::default();
            let mut c = call(cc_body(true), "req_tpl", "end_turn");
            c.ua_out = ua.into();
            t.ingest(c);
            let st = t.0.state.lock();
            let p = st.pending.get(&key()).expect("queued");
            let names: Vec<String> = p.events.iter().map(|(_, e)| ev_name(e).to_string()).collect();
            let dd_keys: Vec<String> = p.dd[0].as_object().unwrap().keys().cloned().collect();
            (names, dd_keys)
        };
        let (new, dd) = names_for("claude-cli/2.1.285 (external, cli)");
        for n in
            ["tengu_managed_config_ready", "tengu_context_announcement", "tengu_prompt_suggestion"]
        {
            assert!(new.iter().any(|x| x == n), "{n}");
        }
        assert!(!new.iter().any(|x| x == "tengu_tip_shown"));
        let at = |k: &str| dd.iter().position(|x| x == k).unwrap();
        assert_eq!(at("is_claude_ai_auth"), at("betas") + 1);
        let (old, _) = names_for("claude-cli/2.1.280 (external, cli)");
        assert!(old.iter().any(|x| x == "tengu_tip_shown"), "2.1.280 仍是 2.1.260 那份模板");
        assert!(!old.iter().any(|x| x == "tengu_managed_config_ready"));
    }

    /// 同一配置下消息只增不减：`continue/append`；实际走没走线程看请求体。
    #[test]
    fn same_config_continuation_continues_the_tether_thread() {
        // 去掉 auto 模式的提示，免得分类器把它钉成无状态。
        let plain = |last_user_text| {
            String::from_utf8(cc_body(last_user_text))
                .unwrap()
                .replace("While auto mode is active: rules", "rules")
                .into_bytes()
        };
        let t = Telemetry::default();
        let mut first = call(plain(true), "req_1", "tool_use");
        first.ua_out = "claude-cli/2.1.280 (external, cli)".into();
        t.ingest(first);
        let mut body: Value = serde_json::from_slice(&plain(false)).unwrap();
        body["messages"].as_array_mut().unwrap().splice(
            0..0,
            [
                json!({"role":"user","content":"earlier"}),
                json!({"role":"assistant","content":"ok"}),
            ],
        );
        let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
        second.ua_out = "claude-cli/2.1.280 (external, cli)".into();
        let expect_omitted = parse_shape(&second.body).unwrap().omitted_bytes;
        t.ingest(second);
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        let metas = |n: &str| -> Vec<Value> {
            p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
        };
        let dec = metas("tengu_tether_decision");
        assert_eq!(
            (dec[1]["decision"].as_str(), dec[1]["reason"].as_str()),
            (Some("continue"), Some("append"))
        );
        assert_eq!(dec[1]["turnsInThread"], 2);
        assert_eq!(dec[1]["deltaMessageCount"], 2);
        // 判定是接着用，但请求体没写 `thread`（模拟路径就是这样）：实际发出的是无线程的全量，
        // 照请求体报 none、什么都没省。
        let live = metas("tengu_tether_live_outcome");
        assert_eq!(live[1]["engineDecision"], "continue");
        assert_eq!(live[1]["sentThreadType"], "none");
        assert_eq!(live[1]["planReason"], "none");
        assert_eq!(live[1]["omittedBytes"], 0);
        assert!(expect_omitted > 0);
        assert_eq!(live[1]["deltaMessageCount"], 1);
        assert_eq!(metas("tengu_tether_echo_audit")[0]["turnsReceived"], 2);
    }

    /// 旧版本（出站 UA 2.1.258）不带 2.1.270 起才有的字段与事件。
    #[test]
    fn older_versions_keep_the_old_layout() {
        let t = Telemetry::default();
        t.ingest(call(cc_body(true), "req_1", "end_turn"));
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).unwrap();
        let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
        for n in [
            "tengu_tether_decision",
            "tengu_declared_tool_set_held",
            "tengu_sleepy_snowflake_applied",
        ] {
            assert!(!names.contains(&n), "{n}");
        }
        let ok = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_success").unwrap();
        let m = meta_of(&ok.1);
        assert!(m.get("firstContentMs").is_none() && m.get("snapshotHash").is_none());
        assert!(m.get("turn_origin").is_none());
    }

    /// 请求头里的一项（大小写不敏感）。
    fn cap_header(rel: &str, name: &str) -> Option<String> {
        let dir = format!("{}/cap/{rel}", env!("CARGO_MANIFEST_DIR"));
        let (dir, prefix) = dir.rsplit_once('/').unwrap();
        let path =
            std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".req.raw"))
            })?;
        let raw = std::fs::read(path).ok()?;
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
        std::str::from_utf8(&raw[..sep]).ok()?.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }

    /// 回放 `cap/2.1.280` 第二个会话：主线程拉起一个内置 Explore 子代理（6 条请求 + 1 条摘要
    /// 请求），按官方的完成先后喂进来，子代理那条支线的事件逐项对官方批次。
    #[test]
    fn subagent_replay_matches_the_2_1_280_capture() {
        // (文件, 回复的 stop_reason, 回复里调了哪些工具)
        const CALLS: [(&str, &str, &[&str]); 11] = [
            ("00161_", "end_turn", &[]),
            ("00164_", "tool_use", &["Agent"]),
            ("00166_", "end_turn", &[]),
            ("00165_", "tool_use", &["Bash"]),
            ("00168_", "end_turn", &[]),
            ("00170_", "tool_use", &["Bash"]),
            ("00171_", "tool_use", &["Bash"]),
            ("00173_", "tool_use", &["Bash"]),
            ("00174_", "end_turn", &[]),
            ("00175_", "tool_use", &["SubagentHandback"]),
            ("00178_", "end_turn", &[]),
        ];
        const AGENT: &str = "a51764a248f499f13";
        let t = Telemetry::default();
        let base = frozen_now() - Duration::from_secs(600);
        let mut session = String::new();
        for (i, (f, stop, tools)) in CALLS.iter().enumerate() {
            let rel = format!("2.1.280/{f}");
            let Some((body, betas)) = cap_request(&rel) else {
                eprintln!("skipped: cap/2.1.280 not present");
                return;
            };
            let shape = parse_shape(&body).unwrap();
            session = shape.session_id.clone().unwrap();
            let mut c = call(body, &format!("req_{f}"), stop);
            c.betas = betas;
            c.ua_out = "claude-cli/2.1.280 (external, cli)".into();
            c.resp_model = Some(shape.model.clone());
            c.message_id = Some(format!("msg_{f}"));
            c.started_at = base + Duration::from_secs(10 * i as u64);
            c.tool_use_lens = tools.iter().map(|n| (n.to_string(), 100)).collect();
            c.client_request_id = cap_header(&rel, "x-client-request-id");
            c.agent = AgentHeaders {
                agent_id: cap_header(&rel, "x-claude-code-agent-id"),
                agent_type: cap_header(&rel, "x-claude-code-agent-type"),
                request_class: cap_header(&rel, "x-claude-code-request-class"),
            };
            t.ingest(c);
        }
        let st = t.0.state.lock();
        let p = st.pending.get(&(7, session)).expect("queued");
        let of_agent = |e: &Value| e["event_data"]["agent_id"] == AGENT;
        let sub = |n: &str| -> Vec<Value> {
            p.events
                .iter()
                .filter(|(_, e)| ev_name(e) == n && of_agent(e))
                .map(|(_, e)| meta_of(e))
                .collect()
        };
        let col = |v: &[Value], k: &str| -> Vec<Value> { v.iter().map(|m| m[k].clone()).collect() };

        // 只有支线上的事件带 agent_id；主线程那几条一律不带。
        for (_, e) in &p.events {
            let d = &e["event_data"];
            if d.get("agent_id").is_some() {
                assert_eq!(d["agent_type"], "subagent");
            }
        }
        let main_queries: Vec<&Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_query" && !of_agent(e))
            .map(|(_, e)| e)
            .collect();
        assert_eq!(main_queries.len(), 4, "主线程三条 + 猜下一句");
        assert!(main_queries.iter().all(|e| e["event_data"].get("agent_id").is_none()));

        let q = sub("tengu_api_query");
        assert_eq!(
            col(&q, "querySource"),
            [
                "agent:builtin:Explore",
                "agent:builtin:Explore",
                "agent:builtin:Explore",
                "agent:builtin:Explore",
                "agent_summary",
                "agent:builtin:Explore",
                "agent:builtin:Explore"
            ]
            .map(|s| json!(s))
        );
        assert_eq!(col(&q, "queryDepth"), [2, 3, 4, 5, 3, 6, 7].map(|n| json!(n)));
        let chains = col(&q, "queryChainId");
        assert!([0, 1, 2, 3, 5, 6].iter().all(|&i| chains[i] == chains[0]), "整条支线一个链");
        assert_ne!(chains[4], chains[0], "摘要请求另起链");

        let ok = sub("tengu_api_success");
        assert_eq!(ok[0]["invokingRequestId"], "req_00164_", "拉起它的是调了 Agent 的那条");
        assert_eq!(ok[0]["invocationKind"], "spawn");
        assert!(ok[1].get("invokingRequestId").is_none());
        assert!(ok.iter().all(|m| m.get("is_default_model").is_none()));
        assert_eq!(ok[0]["systemPromptSource"], "live_recorded");
        assert!(ok[1..].iter().all(|m| m["systemPromptSource"] == "from_snapshot"));
        let main_ok: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_success" && !of_agent(e))
            .map(|(_, e)| meta_of(e))
            .collect();
        assert_ne!(ok[0]["snapshotHash"], main_ok[0]["snapshotHash"], "支线自己一份快照");
        let explore: Vec<&Value> =
            ok.iter().filter(|m| m["querySource"] == "agent:builtin:Explore").collect();
        assert!(explore.iter().all(|m| m["attributionAgent"] == "Explore"));
        let summary = ok.iter().find(|m| m["querySource"] == "agent_summary").unwrap();
        assert!(summary.get("attributionAgent").is_none());
        assert_eq!(ok[0]["prompt_cache_ttl_reason"], "default");
        assert_eq!(
            ok[1]["messageTokens"],
            ok[0]["inputTokens"].as_i64().unwrap() + 26736 + 8729 + 31
        );

        // 规范化前后与系统提示词：没有分界标记，块数恒报 6。
        let pre = col(&sub("tengu_api_before_normalize"), "preNormalizedMessageCount");
        let post = sub("tengu_api_after_normalize");
        assert_eq!(pre[..6], [9, 15, 19, 23, 25, 27].map(|n| json!(n)));
        assert_eq!(
            col(&post, "postNormalizedMessageCount")[..5],
            [2, 5, 8, 11, 11].map(|n| json!(n))
        );
        assert_eq!(col(&post, "apiSystemMessageCount")[..5], [1, 2, 3, 4, 4].map(|n| json!(n)));
        assert!(sub("tengu_sysprompt_boundary_found").is_empty());
        let missing = sub("tengu_sysprompt_missing_boundary_marker");
        assert_eq!(missing.len(), 14);
        assert!(missing.iter().all(|m| m["promptBlockCount"] == 6));

        // tether：支线首条另起，之后接着用；摘要请求没有。
        let dec = sub("tengu_tether_decision");
        assert_eq!(dec.len(), 6);
        assert_eq!(dec[0]["reason"], "first_request");
        assert!(dec[1..].iter().all(|d| d["decision"] == "continue" && d["reason"] == "append"));
        assert_eq!(col(&dec, "turnsInThread"), [1, 2, 3, 4, 5, 6].map(|n| json!(n)));
        assert!(dec.iter().all(|d| d["sourceCategory"] == "subagent"));
        assert_eq!(sub("tengu_tether_echo_audit").len(), 5);

        // 支线里的工具续轮与攒附件：深度是上一条的。
        let before = sub("tengu_query_before_attachments");
        assert_eq!(col(&before, "queryDepth")[..3], [2, 3, 4].map(|n| json!(n)));
        let tool_ok = sub("tengu_tool_use_success");
        assert!(
            tool_ok
                .iter()
                .all(|m| m["subagent_type"] == "Explore" && m["is_built_in_agent"] == true)
        );
        assert_eq!(sub("tengu_auto_mode_git_state_probe").len(), 7, "auto 模式下每条请求前一次");

        // 摘要请求与支线收尾。
        let ends = sub("tengu_turn_end");
        assert_eq!(
            col(&ends, "query_source"),
            ["agent_summary", "agent:builtin:Explore"].map(|s| json!(s))
        );
        assert!(ends.iter().all(|e| e["is_subagent"] == true));
        assert_eq!(ends[1]["query_source_category"], "subagent");
        let fork = sub("tengu_fork_agent_query");
        assert_eq!(fork[0]["forkLabel"], "agent_summary");
        assert_eq!(fork[0]["queryChainId"], chains[0]);
        assert_eq!(fork[0]["relayEligible"], false);
        let done = sub("tengu_agent_tool_completed");
        assert_eq!(done[0]["assistant_message_count"], 6);
        assert_eq!(done[0]["total_tool_uses"], 5);
        assert_eq!(done[0]["agent_type"], "Explore");
        assert_eq!(sub("tengu_cache_eviction_hint")[0]["scope"], "subagent_end");

        // Datadog：支线条目带 agent_id；api_success 不带工具长度表的 hash。
        assert!(p.dd.iter().any(|d| d["agent_id"] == AGENT && d["message"] == "tengu_api_success"));
        assert!(
            p.dd.iter()
                .filter(|d| d["message"] == "tengu_api_success")
                .all(|d| d.get("tool_schemas_hash").is_none())
        );
    }

    /// 线程增量请求（`thread.type: continue`，体里没有 tools、只有新增的两条消息）按上一条
    /// 全量补回客户端视角：`cap/2.1.277` sonnet 那一对，官方报 messageCount 8、toolsCount 19、
    /// 实际走线程续用、省掉 85845 字节。
    #[test]
    fn thread_continuations_are_filled_from_the_thread_base() {
        let mut calls = Vec::new();
        for (f, stop) in [("00031_", "tool_use"), ("00035_", "tool_use")] {
            let rel = format!("2.1.277/{f}");
            let Some((body, betas)) = cap_request(&rel) else {
                eprintln!("skipped: cap/2.1.277 not present");
                return;
            };
            let mut c = call(body, &format!("req_{f}"), stop);
            c.betas = betas;
            c.ua_out = "claude-cli/2.1.280 (external, cli)".into();
            c.agent.request_class = cap_header(&rel, "x-claude-code-request-class");
            // 00031 那条回复：正文 38 字 + `Bash` 与入参 242（响应解压后逐块数的）。
            c.reply_input_chars = 280;
            calls.push(c);
        }
        let session = parse_shape(&calls[0].body).unwrap().session_id.unwrap();
        assert_eq!(parse_shape(&calls[1].body).unwrap().tools_count, 0, "增量请求体里没有工具");
        let t = Telemetry::default();
        for c in calls {
            t.ingest(c);
        }
        let st = t.0.state.lock();
        let p = st.pending.get(&(7, session)).unwrap();
        let metas = |n: &str| -> Vec<Value> {
            p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
        };
        let q = metas("tengu_api_query");
        assert_eq!(q[1]["querySource"], "repl_main_thread", "不是辅助调用");
        assert_eq!(q[1]["messagesLength"], 8);
        let ok = metas("tengu_api_success");
        assert_eq!(ok[1]["toolsCount"], 19);
        // 输入长度含服务端持有、体里没有的那条回复。
        assert_eq!(ok[1]["inputTextCharLength"], 56026);
        assert_eq!(ok[1]["estimatedInputTokens"], 18676);
        let live = metas("tengu_tether_live_outcome");
        assert_eq!(live[0]["sentThreadType"], "create");
        assert_eq!(live[1]["sentThreadType"], "continue");
        assert_eq!(live[1]["omittedBytes"], 85845);
        let dec = metas("tengu_tether_decision");
        assert_eq!(
            (dec[1]["decision"].as_str(), dec[1]["deltaMessageCount"].as_u64()),
            (Some("continue"), Some(3))
        );
    }

    /// 这一轮是谁发起的以请求自己的 `cc_turn_origin` 为准：`cap/2.1.280` 00179 是同伴会话
    /// 发来的（peer）、00180 是后台任务的完成通知（task_notification），官方分别报 `peer` /
    /// `task-notification`、`prompt_source: system`，peer 那条不带 `prompt_index` 也不占号。
    #[test]
    fn turn_origin_follows_the_billing_header() {
        let t = Telemetry::default();
        let mut session = String::new();
        for (i, f) in ["00161_", "00179_", "00180_"].into_iter().enumerate() {
            let rel = format!("2.1.280/{f}");
            let Some((body, betas)) = cap_request(&rel) else {
                eprintln!("skipped: cap/2.1.280 not present");
                return;
            };
            session = parse_shape(&body).unwrap().session_id.unwrap();
            let mut c = call(body, &format!("req_{f}"), "end_turn");
            c.betas = betas;
            c.ua_out = "claude-cli/2.1.280 (external, cli)".into();
            c.agent.request_class = cap_header(&rel, "x-claude-code-request-class");
            c.started_at = frozen_now() - Duration::from_secs(300 - 60 * i as u64);
            t.ingest(c);
        }
        let st = t.0.state.lock();
        let p = st.pending.get(&(7, session)).unwrap();
        let metas = |n: &str| -> Vec<Value> {
            p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
        };
        let inputs = metas("tengu_input_prompt");
        let col = |k: &str| -> Vec<Value> { inputs.iter().map(|m| m[k].clone()).collect() };
        assert_eq!(col("turn_origin"), ["human", "peer", "task-notification"].map(|s| json!(s)));
        assert_eq!(col("prompt_source"), ["typed", "system", "system"].map(|s| json!(s)));
        assert_eq!(col("prompt_index"), [json!(1), Value::Null, json!(2)], "peer 不占号");
        assert!(inputs.iter().all(|m| m["is_wakeup"] == false));
        let ok = metas("tengu_api_success");
        assert_eq!(
            ok.iter().map(|m| m["turn_origin"].clone()).collect::<Vec<_>>(),
            ["human", "peer", "task-notification"].map(|s| json!(s))
        );
    }

    /// auto 模式只认客户端注入的两种位置；用户正文里引用、工具输出里恰好有、assistant 复述
    /// 这段话都不算。
    #[test]
    fn auto_mode_markers_are_only_read_from_injected_reminders() {
        let body = |msgs: Value| {
            json!({ "model": "claude-sonnet-5", "messages": msgs, "system": [{"type":"text","text":"x"}] })
                .to_string()
                .into_bytes()
        };
        let mode = |msgs: Value| parse_shape(&body(msgs)).unwrap().permission_mode;
        let quoted = "文档里写着 While auto mode is active: 你可以……";
        // 用户引用、工具输出、assistant 复述：都不改模式。
        assert_eq!(
            mode(json!([
                {"role":"user","content":[{"type":"text","text":quoted}]},
                {"role":"assistant","content":[{"type":"text","text":"While auto mode is active: noted"}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"<system-reminder>\nWhile auto mode is active: x"}]}
            ])),
            "default"
        );
        // 注入的提示块与 system 消息：算。
        let enter = json!({"role":"user","content":[
            {"type":"text","text":"<system-reminder>\nWhile auto mode is active:\n\nrules</system-reminder>"},
            {"type":"text","text":"hi"}
        ]});
        assert_eq!(mode(json!([enter.clone()])), "auto");
        assert_eq!(
            mode(
                json!([{"role":"system","content":"# Environment\n... While auto mode is active: ..."}])
            ),
            "auto"
        );
        // 进入之后，工具输出里出现退出那段文字：仍是 auto；注入的退出提示才算退出。
        assert_eq!(
            mode(json!([
                enter.clone(),
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"## Exited Auto Mode\nYou have exited"}]}
            ])),
            "auto"
        );
        assert_eq!(
            mode(json!([
                enter,
                {"role":"user","content":[{"type":"text","text":"<system-reminder>\n## Exited Auto Mode\n\nYou have exited auto mode.</system-reminder>"}]}
            ])),
            "default"
        );
    }

    /// 会话首轮就是 peer：它不占号（下一次用户输入仍是 1），它的续轮也不再被当成新输入，
    /// 首轮那串只发一次、跟着真正的首轮走。
    #[test]
    fn a_peer_opening_turn_does_not_take_a_prompt_number() {
        let t = Telemetry::default();
        let mut session = String::new();
        let steps = [
            ("00179_", "tool_use", true),
            ("00179_", "end_turn", false),
            ("00161_", "end_turn", true),
        ];
        for (i, (f, stop, fresh)) in steps.into_iter().enumerate() {
            let rel = format!("2.1.280/{f}");
            let Some((body, betas)) = cap_request(&rel) else {
                eprintln!("skipped: cap/2.1.280 not present");
                return;
            };
            // 第二步是 peer 那一轮的工具续轮：把末条消息换成 tool_result。
            let body = if fresh {
                body
            } else {
                let mut v: Value = serde_json::from_slice(&body).unwrap();
                let msgs = v["messages"].as_array_mut().unwrap();
                msgs.push(json!({"role":"assistant","content":[{"type":"tool_use","id":"tp","name":"Bash","input":{}}]}));
                msgs.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"tp","content":"ok"}]}));
                v.to_string().into_bytes()
            };
            session = parse_shape(&body).unwrap().session_id.unwrap();
            let mut c = call(body, &format!("req_{i}"), stop);
            c.betas = betas;
            c.ua_out = "claude-cli/2.1.280 (external, cli)".into();
            c.started_at = frozen_now() - Duration::from_secs(300 - 60 * i as u64);
            t.ingest(c);
        }
        let st = t.0.state.lock();
        let p = st.pending.get(&(7, session)).unwrap();
        let inputs: Vec<Value> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_input_prompt")
            .map(|(_, e)| meta_of(e))
            .collect();
        assert_eq!(inputs.len(), 2, "peer 的续轮不是新输入");
        assert_eq!(inputs[0]["turn_origin"], "peer");
        assert!(inputs[0].get("prompt_index").is_none());
        assert_eq!(inputs[1]["turn_origin"], "human");
        assert_eq!(inputs[1]["prompt_index"], 1, "peer 不占号");
        let first_prompt_only = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_policy_limits_cache_state_at_first_prompt")
            .count();
        assert_eq!(first_prompt_only, 1, "首轮那串只发一次");
    }
}
