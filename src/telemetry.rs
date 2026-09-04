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

use std::collections::HashMap;
use std::sync::Arc;
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
}

static TEMPLATE: std::sync::LazyLock<Template> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("assets/cc_telemetry_template.json"))
        .expect("assets/cc_telemetry_template.json must parse")
});

/// 官方 Datadog 那份日志只收这几类事件（`cap/2.1.258` 四批 285 条与 `cap/2.1.260-1` 六批
/// 对照：`tengu_feature_ok` 全部、`tengu_api_success`、启动那几条，其余只进 event_logging）。
const DD_EVENT_NAMES: &[&str] = &[
    "tengu_feature_ok",
    "tengu_api_success",
    "tengu_started",
    "tengu_timer",
    "tengu_init",
    "tengu_mcp_sdk_generation",
    "tengu_mcp_server_connection_succeeded",
    "tengu_exit",
    "tengu_tool_use_success",
    "tengu_bash_tool_command_executed",
];

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
            let mut meta = substitute(&e.meta, subst);
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
            events.push((t, id.event(&e.name, t, &c, meta.clone())));
            if DD_EVENT_NAMES.contains(&e.name.as_str()) {
                dd.push(id.dd_entry(&e.name, &c, resp_model, snake_flat(&meta)));
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
#[derive(Debug, Clone)]
pub struct Identity {
    pub session_id: String,
    /// sha256 hex，64 位。
    pub device_id: String,
    pub account_uuid: String,
    /// `/v1/messages` 响应头 `anthropic-organization-id`；一次都还没见过时为 `None`，
    /// 此时 `auth` 块只带 `account_uuid`。
    pub organization_uuid: Option<String>,
    pub subscription_type: String,
    /// 客户端版本（`2.1.258`），与出站 UA 一致。
    pub version: String,
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
        json!({
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
        })
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
    /// url-safe 无填充是另一种编码，一眼可辨）。前三项固定，`extra` 追加在后。
    pub fn metadata_b64(&self, prompt_id: &str, extra: Value) -> String {
        let mut m = Map::new();
        m.insert("renderer_mode".into(), "default".into());
        m.insert("subscription_type".into(), self.subscription_type.clone().into());
        m.insert("cc_prompt_id".into(), prompt_id.into());
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                m.insert(k.clone(), v.clone());
            }
        }
        STANDARD.encode(Value::Object(m).to_string())
    }

    /// 一条 `ClaudeCodeInternalEvent`。
    pub fn event(&self, name: &str, ts: DateTime<Utc>, ctx: &EventCtx<'_>, extra: Value) -> Value {
        // 顶层 `model` 跟事件自己的 meta.model 走（api_query 是这条请求的展示名、api_success
        // 是规范名，标题生成那条就是 haiku），没有 meta.model 的事件才用会话主模型
        // （`cap/2.1.260-2`：title_generated / tool_schema_sizes 顶层都是 `claude-opus-5[1m]`）。
        let model = extra.get("model").and_then(|m| m.as_str()).unwrap_or(ctx.model);
        json!({
            "event_type": "ClaudeCodeInternalEvent",
            "event_data": {
                "event_name": name,
                "client_timestamp": ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "model": model,
                "session_id": &self.session_id,
                "user_type": "external",
                "betas": ctx.betas,
                "env": self.env_block(),
                "entrypoint": "cli",
                "is_interactive": true,
                "client_type": "cli",
                "process": process_b64(ctx.uptime_secs),
                "additional_metadata": self.metadata_b64(ctx.prompt_id, extra),
                "auth": self.auth_block(),
                "event_id": uuid_v4(),
                "device_id": &self.device_id
            }
        })
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
    pub fn dd_entry(&self, message: &str, ctx: &EventCtx<'_>, model: &str, extra: Value) -> Value {
        let s = |v: &str| Value::String(v.to_string());
        // 附加字段平铺在公共字段之后，同名会盖掉：`tengu_api_success` 的 meta 自带 `model`
        // （这条请求实际用的规范名），于是 DD 那份的 `model` 与 `ddtags` 都跟它走——
        // `cap/2.1.258/00019` 里会话模型是 `claude-opus-5[1m]`，api_success 那条却是
        // `model:claude-opus-5`，正是被 meta 盖掉的结果。其它事件没有 meta.model，用会话主模型。
        // Datadog 那份对 meta.model 还会去掉日期后缀：标题那条是 `claude-haiku-4-5`
        // （`cap/2.1.260-2/00062`），opus 没有后缀所以看不出来。
        let short = extra.get("model").and_then(|m| m.as_str()).map(dd_model_short);
        let model = short.as_deref().unwrap_or(model);
        let provider_tag = extra
            .get("provider")
            .and_then(|p| p.as_str())
            .map(|p| format!("provider:{p},"))
            .unwrap_or_default();
        let mut m = Map::new();
        m.insert("ddsource".into(), s("nodejs"));
        m.insert(
            "ddtags".into(),
            s(&format!(
                "event:{message},arch:arm64,client_type:cli,entrypoint:cli,model:{model},\
                 platform:darwin,{provider_tag}subscription_type:{},user_bucket:15,\
                 user_type:external,version:{v},version_base:{v}",
                self.subscription_type,
                v = self.version,
            )),
        );
        m.insert("message".into(), s(message));
        m.insert("service".into(), s("claude-code"));
        m.insert("hostname".into(), s("claude-code"));
        m.insert("env".into(), s("external"));
        m.insert("model".into(), s(model));
        m.insert("session_id".into(), s(&self.session_id));
        m.insert("user_type".into(), s("external"));
        m.insert("betas".into(), s(ctx.betas));
        m.insert("entrypoint".into(), s("cli"));
        m.insert("is_interactive".into(), s("true"));
        m.insert("client_type".into(), s("cli"));
        m.insert("process_metrics".into(), process_metrics(ctx.uptime_secs));
        for k in ["swe_bench_run_id", "swe_bench_instance_id", "swe_bench_task_id"] {
            m.insert(k.into(), s(""));
        }
        m.insert("subscription_type".into(), s(&self.subscription_type));
        m.insert("renderer_mode".into(), s("default"));
        m.insert("prompt_id".into(), s(ctx.prompt_id));
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
            ("is_claude_ai_auth", true),
        ] {
            m.insert(k.into(), Value::Bool(v));
        }
        m.insert("version".into(), s(&self.version));
        m.insert("version_base".into(), s(&self.version));
        m.insert("build_time".into(), s(self.build_time()));
        m.insert("deployment_environment".into(), s("unknown-darwin"));
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

/// base64 编码的 `process`（标准字典、带填充，同 [`Identity::metadata_b64`]）。
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
    header
        .split(',')
        .map(str::trim)
        .filter(|b| config::TELEMETRY_SESSION_BETA_PREFIXES.iter().any(|p| b.starts_with(p)))
        .collect::<Vec<_>>()
        .join(",")
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
    client
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", format!("claude-code/{version}"))
        .header("x-service-name", "claude-code")
        .header("Accept", "application/json, text/plain, */*")
        .json(&json!({ "events": events }))
        .send()
        .await
        .ok()
        .map(|r| r.status().as_u16())
}

/// Datadog 日志摄入。真实客户端用 axios 直发，不带 Authorization。
pub async fn post_datadog(client: &wreq::Client, entries: &[Value]) -> Option<u16> {
    client
        .post(config::DATADOG_INTAKE_URL)
        .header("DD-API-KEY", config::DATADOG_API_KEY)
        .header("User-Agent", config::DATADOG_USER_AGENT)
        .header("Accept", "application/json, text/plain, */*")
        .header("Accept-Encoding", "gzip, compress, deflate, br")
        .json(&entries)
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
    client
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", format!("claude-code/{version}"))
        .header("Accept", "application/json, text/plain, */*")
        .json(body)
        .send()
        .await
        .ok()
        .map(|r| r.status().as_u16())
}

// ---------- 逐请求：转发路径交过来的一条 API 调用 ----------

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
    pub thinking_chars: usize,
    pub cost_usd: Option<f64>,
    pub speed: Option<String>,
}

/// 转发路径在建 `ReqLog` 时先攒好的那部分（响应侧的量在流结束时才有）。
pub struct Capture {
    pub sink: Telemetry,
    pub account_uuid: Option<String>,
    pub org_type: Option<String>,
    pub body: Bytes,
    pub betas: Option<String>,
    pub session_header: Option<String>,
    pub organization_id: Option<String>,
    pub started_at: SystemTime,
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
}

impl Kind {
    fn query_source(self) -> &'static str {
        match self {
            Kind::Main => "repl_main_thread",
            Kind::Subagent => "agent:general-purpose",
            Kind::Suggestion => "prompt_suggestion",
            Kind::Title => "generate_session_title",
            Kind::Helper => "compact",
        }
    }
    fn category(self) -> &'static str {
        match self {
            Kind::Main => "main",
            Kind::Subagent => "subagent",
            Kind::Suggestion | Kind::Title | Kind::Helper => "auxiliary",
        }
    }
    /// 带 `queryChainId` / `queryDepth` 的那几类；标题那类查询没有链。
    fn has_chain(self) -> bool {
        matches!(self, Kind::Main | Kind::Subagent | Kind::Suggestion)
    }
    /// 有基座提示词（`tengu_sysprompt_boundary_found`）；侧查询的 system 没有边界标记。
    fn has_boundary(self) -> bool {
        matches!(self, Kind::Main | Kind::Subagent | Kind::Suggestion)
    }
}

/// `2.1.260` 这种三段版本号是否不低于 `min`。
fn version_at_least(v: &str, min: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parse(v) >= parse(min)
}

/// 上一条回复里的一次工具调用（续轮请求的倒数第二条 assistant 消息里的 `tool_use` 块）。
#[derive(Debug, Clone)]
struct ToolUse {
    id: String,
    name: String,
    /// `input` 的 JSON 字符数。
    input_len: usize,
    /// Bash 的 `command` 长度（其它工具为 0）。
    command_len: usize,
    /// 对应 `tool_result` 的内容字符数。
    result_len: usize,
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
    /// system 里有会话标题生成的指令。
    title: bool,
    /// 续轮请求：上一条 assistant 消息里的工具调用，配上末条消息里的 tool_result 大小。
    tool_uses: Vec<ToolUse>,
    assistant_messages: usize,
    device_id: Option<String>,
    session_id: Option<String>,
    account_uuid: Option<String>,
}

/// 内容块数组里 text 的字符数（tool_result 的 content 既可能是字符串也可能是块数组）。
fn content_chars(c: &Value) -> usize {
    match c {
        Value::String(s) => js_len(s),
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

fn tool_uses_of(messages: &[Value]) -> Vec<ToolUse> {
    let (Some(prev), Some(last)) = last_two_non_system(messages) else { return vec![] };
    if prev.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return vec![];
    }
    let results: Vec<(&str, usize)> = last
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                .filter_map(|b| {
                    let id = b.get("tool_use_id")?.as_str()?;
                    Some((id, b.get("content").map(content_chars).unwrap_or(0)))
                })
                .collect()
        })
        .unwrap_or_default();
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
                    let result_len =
                        results.iter().find(|(rid, _)| *rid == id).map_or(0, |(_, n)| *n);
                    Some(ToolUse {
                        id,
                        name,
                        input_len: input.to_string().len(),
                        command_len,
                        result_len,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn sha256_hex(data: &[u8]) -> String {
    crate::credentials::hex_lower(&Sha256::digest(data))
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

/// 末条消息是不是一次新的用户输入：role 为 user，且 content 里没有 `tool_result`。
fn last_is_new_prompt(messages: &[Value]) -> (bool, usize) {
    let (_, Some(last)) = last_two_non_system(messages) else { return (true, 0) };
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return (false, 0);
    }
    match last.get("content") {
        Some(Value::String(s)) => (true, js_len(s)),
        Some(Value::Array(blocks)) => {
            if blocks.iter().any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
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
                shape.cc_prompt_id = first
                    .split(';')
                    .map(str::trim)
                    .find_map(|kv| kv.strip_prefix("cc_prompt_id="))
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                shape.is_subagent = first.contains("cc_is_subagent=true");
            }
        }
        let n = texts.len();
        if n >= 2 {
            shape.static_len = js_len(texts[n - 2]);
        }
        if let Some(last) = texts.last() {
            shape.dynamic_len = js_len(last);
        }
        if texts.iter().any(|t| t.contains("auto mode is active")) {
            shape.permission_mode = "auto";
        }
        shape.title = texts.iter().any(|t| t.contains("You are naming a coding session"));
    }

    // tools：数量、JSON 长度、逐个长度表（整个工具对象的紧凑 JSON 长度：抓包里 Agent 3078 /
    // Bash 2352 与 `cap/2.1.260-2/00057` 逐个对得上）、hash、deferred 数。
    //
    // `toolSchemasHash` 是**长度表那串 JSON** 的 sha256 前 12 位，而不是工具数组的：无工具时
    // 官方报 `44136fa355b3`，正是 sha256("{}") 的前缀（`cap/2.1.260-2` 标题生成那条）。
    let mut lens = Map::new();
    if let Some(tools) = v.get("tools").and_then(|t| t.as_array()) {
        shape.tools_count = tools.len();
        for t in tools {
            // 延迟加载的占位工具计入 `toolsCount` / `deferredToolsCount`，但不进长度表：官方
            // 16 个工具的表只有 15 项，`toolsCharLength` 也不含它（差的 204 正是占位那条）。
            if t.get("defer_loading").and_then(|d| d.as_bool()) == Some(true) {
                shape.deferred_tools += 1;
                continue;
            }
            if let Some(name) = t.get("name").and_then(|n| n.as_str()) {
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
        shape.estimated_tokens = if shape.model.contains("haiku") {
            (shape.input_text_chars as f64 / 4.0).round() as usize
        } else {
            shape.input_text_chars.div_ceil(3)
        };
        (shape.new_prompt, shape.prompt_len) = last_is_new_prompt(msgs);
        if !shape.new_prompt {
            shape.tool_uses = tool_uses_of(msgs);
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
    }
    // 缓存断点的 ttl：任一处写了 1h 就算 1h。
    shape.cache_ttl_1h = body.windows(10).any(|w| w == b"\"ttl\":\"1h\"");
    Some(shape)
}

/// 一个遥测会话（同一凭证 + 同一 `session_id`）跨请求要记住的东西。
struct Session {
    /// 会话「进程」的起点：首条请求前几秒（客户端启动到第一次提交之间的那段）。
    started_wall: SystemTime,
    last_seen: Instant,
    prompt_index: u32,
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
    /// 本轮已经出现过正文（`tengu_turn_first_text` 只发一次）。
    turn_text_seen: bool,
    /// 本轮已执行的工具调用数。
    turn_tool_calls: u32,
    /// 会话里已经发过 `shell_snapshot_create`（首次 Bash 才有）。
    shell_snapshot_done: bool,
    /// 任一类请求最近一次结束的时刻（`timeSinceLastApiCallMs`）。
    last_call_end: Option<SystemTime>,
    /// 上一条**主线程**请求的 token 总量（input + cache_read + cache_creation + output）：
    /// `messageTokens` 报的是「对话此刻的 token 数」，抓包里三条续轮逐一对得上。
    prev_total_input: i64,
    /// 会话里第一条请求的展示模型名，作 `default_model`。
    default_model: String,
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
struct CallMetric {
    session_id: String,
    device_id: String,
    account_uuid: String,
    model: String,
    category: &'static str,
    /// 没有 `output_config.effort` 的侧查询不带 `effort` 属性（抓包里标题生成那组没有）。
    effort: Option<String>,
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
    /// 新会话待做的启动握手（policy_limits / settings / eval / bootstrap …那串 GET），
    /// 由 [`run_flusher`] 取走执行。
    handshakes: Vec<Handshake>,
}

/// 一个新会话要做的启动握手：真实客户端每次拉起进程都会用当前账号打这一串端点
/// （`cap/2.1.260-1` 17:14:56–17:15:05），luban 替它补上，身份取该会话的。
pub struct Handshake {
    pub cred_id: i64,
    pub snapshot: SessionSnapshot,
    /// bootstrap 的 `model=` 参数：规范名。
    pub model: String,
}

/// 逐请求遥测的汇聚点：转发路径往里 [`Telemetry::record`]，[`run_flusher`] 定时取走发出。
#[derive(Clone, Default)]
pub struct Telemetry(Arc<parking_lot::Mutex<State>>);

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
    pub fn record(&self, call: ApiCall) {
        let me = self.clone();
        let work = move || me.ingest(call);
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn_blocking(work);
            }
            Err(_) => work(),
        }
    }

    /// 某凭证最近一次响应头里的 `anthropic-organization-id`（保活事件的 `auth` 块用）。
    pub fn org_uuid(&self, cred_id: i64) -> Option<String> {
        self.0.lock().org_uuid.get(&cred_id).cloned()
    }

    /// 取走待做的启动握手。
    pub fn take_handshakes(&self) -> Vec<Handshake> {
        std::mem::take(&mut self.0.lock().handshakes)
    }

    /// 某凭证最近活跃的真实会话（`max_idle` 内有过请求的那些里最新的一个）；没有则 `None`。
    /// 保活拿它把空闲事件挂到真实会话上，见 [`SessionSnapshot`]。
    pub fn latest_session(&self, cred_id: i64, max_idle: Duration) -> Option<SessionSnapshot> {
        let st = self.0.lock();
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

    fn ingest(&self, call: ApiCall) {
        let mut st = self.0.lock();
        Self::process(&mut st, call, true, None);
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
        let identity = Identity {
            session_id: session_id.clone(),
            device_id: device_id.clone(),
            account_uuid: account_uuid.clone(),
            organization_uuid: st.org_uuid.get(&call.cred_id).cloned(),
            subscription_type: subscription_type(call.org_type.as_deref()).to_string(),
            version: version.clone(),
        };

        let kind = if shape.is_subagent {
            Kind::Subagent
        } else if shape.suggestion && shape.tools_count > 0 {
            Kind::Suggestion
        } else if shape.title && shape.tools_count == 0 {
            Kind::Title
        } else if shape.tools_count == 0 {
            Kind::Helper
        } else {
            Kind::Main
        };
        // 侧查询（标题生成等）按 `default` 权限模式跑，不管主线程是不是 auto。
        if matches!(kind, Kind::Title | Kind::Helper) {
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
            sess.deferred.push((call, prev_end, now));
            return;
        }
        // 同一个 id 在按退出收尾之后再出现 = `--resume`：新进程从头计数，只在指标上标 resume。
        let resumed = is_new_session && st.ended.remove(&key).is_some();
        let sess = st.sessions.entry(key.clone()).or_insert_with(|| Session {
            started_wall: call.started_at - Duration::from_millis(3_100),
            last_seen: now,
            prompt_index: 0,
            prompt_id: String::new(),
            chain_id: String::new(),
            last_main_request_id: None,
            last_main_message_id: None,
            last_main_depth: 0,
            turn_depth: 0,
            turn_started: None,
            turn_text_seen: false,
            turn_tool_calls: 0,
            shell_snapshot_done: false,
            last_call_end: None,
            prev_total_input: 0,
            default_model: display_model.clone(),
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
        });
        sess.last_seen = now;
        // 一轮结束（`end_turn`）才有 stop hook 与 turn_end；`tool_use` 是同一轮的中间步。
        let turn_over = call.stop_reason.as_deref().is_none_or(|s| s != "tool_use");
        let emit_first_turn = kind == Kind::Main && turn_over && !sess.first_turn_done;
        if emit_first_turn {
            sess.first_turn_done = true;
        }
        sess.version = version.clone();
        sess.betas = session_betas(call.betas.as_deref().unwrap_or(""));
        // 新一轮用户输入（只有主线程算）：prompt 计数 +1、换 prompt_id（优先用 billing header
        // 里客户端自己的）与 queryChainId、depth 归零。tool_result 续轮沿用上一轮的，depth +1。
        // 侧查询（标题、猜下一句）不动这些计数。
        let is_main = kind == Kind::Main;
        let new_prompt = is_main && (shape.new_prompt || sess.prompt_index == 0);
        if new_prompt {
            sess.prompt_index += 1;
            sess.chain_id = uuid_v4();
            sess.turn_depth = 0;
            sess.turn_started = Some(call.started_at - Duration::from_millis(15));
            sess.turn_text_seen = false;
            sess.turn_tool_calls = 0;
        }
        if let Some(pid) = shape.cc_prompt_id.clone() {
            sess.prompt_id = pid;
        } else if sess.prompt_id.is_empty() {
            sess.prompt_id = uuid_v4();
        }
        let prompt_id = sess.prompt_id.clone();
        // 主线程与猜下一句走 `previousRequestId` 链；标题那类没有。
        let previous_request_id =
            kind.has_chain().then(|| sess.last_main_request_id.clone()).flatten();
        let prev_main_message_id = sess.last_main_message_id.clone();
        let prev_main_depth = sess.last_main_depth;
        // `timeSinceLastApiCallMs` = **这条完成时刻 − 上一条完成时刻**（不分主线程/侧查询，
        // 按完成先后）。`cap/2.1.260-2`：标题 09.490−06.166=3323、主线程 12.019−09.490=2529、
        // 续轮 19.458−12.019=7439、猜下一句 21.385−19.458=1927，全部对上；用「这条开始」算
        // 的话并发的标题与主线程会出负数、续轮只剩工具执行那一秒。
        let this_end = call.started_at + Duration::from_millis(call.total_ms);
        let prev_end: Option<SystemTime> = prev_end_override.or(sess.last_call_end);
        let time_since_last =
            prev_end.and_then(|t| this_end.duration_since(t).ok()).map(|d| d.as_millis() as u64);
        let message_tokens = if kind.has_chain() { sess.prev_total_input } else { 0 };
        let default_model = sess.default_model.clone();
        // 事件顶层 `model` 与 Datadog 的 `model` 是**会话主模型**（用户设置的那个），侧查询
        // 自己用的 haiku 只出现在 api 事件的 meta 里。
        let session_model = sess.last_model.clone().unwrap_or_else(|| sess.default_model.clone());
        let model_changed =
            is_main && sess.last_model.as_deref().is_some_and(|m| m != display_model);
        let previous_message_id = sess.last_message_id.clone();
        let tools_slot =
            if kind.has_boundary() { &sess.tools_hash_main } else { &sess.tools_hash_side };
        let tools_changed = tools_slot.as_deref() != Some(shape.tools_hash.as_str());
        let counted = sess.counted;
        let started_wall = sess.started_wall;
        // queryDepth：主线程本轮第几次请求；猜下一句 = 主线程最后一次 + 2（抓包：0→2、1→3）；
        // 子代理记 1。
        // 主线程当前的链：猜下一句的 fork 统计里引用的是这条父链。
        let main_chain = sess.chain_id.clone();
        let (chain_id, query_depth) = match kind {
            Kind::Main => (sess.chain_id.clone(), sess.turn_depth),
            Kind::Suggestion => (uuid_v4(), sess.last_main_depth + 2),
            Kind::Subagent => (uuid_v4(), 1),
            Kind::Title | Kind::Helper => (String::new(), 0),
        };
        let turn_started: DateTime<Utc> =
            sess.turn_started.unwrap_or(call.started_at - Duration::from_millis(15)).into();
        let first_text_in_turn = is_main && call.text_chars > 0 && !sess.turn_text_seen;
        // 首字之前跑过的工具数 = 本轮此前的 + 这条续轮带回来的。
        let tool_calls_before = sess.turn_tool_calls + shape.tool_uses.len() as u32;
        // 用户这次输入前花的时间：上一条结束到这次提交（封顶 5s）；首次输入没有上一条，取 0.9s。
        let user_secs = if !new_prompt {
            0.0
        } else if sess.prompt_index <= 1 {
            0.9
        } else {
            let submit = call.started_at - Duration::from_millis(15);
            prev_end
                .and_then(|e| submit.duration_since(e).ok())
                .map_or(0.9, |d| d.as_secs_f64().min(5.0))
        };
        let shell_snapshot_first = is_main
            && !sess.shell_snapshot_done
            && shape.tool_uses.iter().any(|t| t.name == "Bash");
        let prompt_index = sess.prompt_index.max(1);

        // 更新会话状态给下一条用。
        // 补发的侧查询比后来的主线程请求结束得早，别把「最近一次结束」往回拨。
        sess.last_call_end = Some(sess.last_call_end.map_or(this_end, |e| e.max(this_end)));
        sess.last_message_id = call.message_id.clone().or(sess.last_message_id.take());
        if is_main {
            sess.last_main_request_id =
                call.request_id.clone().or(sess.last_main_request_id.take());
            sess.last_main_message_id =
                call.message_id.clone().or(sess.last_main_message_id.take());
            sess.last_main_depth = sess.turn_depth;
            sess.prev_total_input = call.input_tokens
                + call.cache_read_tokens
                + call.cache_creation_tokens
                + call.output_tokens;
            sess.last_model = Some(display_model.clone());
            sess.turn_tool_calls += shape.tool_uses.len() as u32;
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
        if kind.has_boundary() {
            sess.tools_hash_main = Some(shape.tools_hash.clone());
        } else {
            sess.tools_hash_side = Some(shape.tools_hash.clone());
        }
        sess.counted = true;
        let ctx_model = if is_main { display_model.clone() } else { session_model };
        let dd_model = ctx_model.trim_end_matches("[1m]").to_string();

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
        let betas_full = call.betas.clone().unwrap_or_default();
        let betas_session = session_betas(&betas_full);
        let ctx = |dt: DateTime<Utc>| EventCtx {
            model: &ctx_model,
            betas: &betas_session,
            prompt_id: &prompt_id,
            uptime_secs: uptime(dt),
        };
        let build_age_mins = {
            let bt = DateTime::parse_from_rfc3339(identity.build_time())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or(t0);
            (t0 - bt).num_minutes().max(0)
        };
        let query_source = kind.query_source();
        let cache_ttl = if shape.cache_ttl_1h { "1h" } else { "5m" };
        let effort = shape.effort.clone();
        let effort_value = effort.clone().unwrap_or_else(|| "high".to_string());
        // 2.1.260 起 `tengu_api_success` 多了 `systemPromptSource`。
        let modern = version_at_least(&version, "2.1.260");
        // 链路字段的写法：有链的带 chain/depth，没链的一律不带。
        let chain_fields = |obj: &mut Map<String, Value>| {
            if kind.has_chain() {
                obj.insert("queryChainId".into(), json!(&chain_id));
                obj.insert("queryDepth".into(), json!(query_depth));
            }
        };

        let mut events: Vec<(DateTime<Utc>, Value)> = Vec::with_capacity(16);
        let mut dd: Vec<Value> = Vec::with_capacity(6);
        let mut push = |dt: DateTime<Utc>, name: &str, extra: Value| {
            events.push((dt, identity.event(name, dt, &ctx(dt), extra)));
        };
        let feature = |name: &str| json!({ "feature_name": name });

        // 静态模板那几串攒在这里，最后再并进 `events`/`dd`（`push` 闭包还借着它们）。
        let mut tpl_events: Vec<(DateTime<Utc>, Value)> = Vec::new();
        let mut tpl_dd: Vec<Value> = Vec::new();
        let setting = model_setting(&display_model);
        let subst = Subst {
            version: &version,
            model: &display_model,
            model_setting: &setting,
            permission_mode: shape.permission_mode,
            resumed,
            prompt_index,
        };
        let mut take_tpl = |tpl: &[TplEvent], anchor: DateTime<Utc>| {
            let (ev, d) = emit_template(tpl, anchor, &identity, ctx, &dd_model, &subst);
            tpl_events.extend(ev);
            tpl_dd.extend(d);
        };
        // 新会话：进程启动那串（120 多条），锚在首条 api_query 前 1.3s 起。
        if is_new_session {
            take_tpl(&TEMPLATE.startup, t0);
        }
        if new_prompt {
            if prompt_index <= 1 {
                take_tpl(&TEMPLATE.prompt, t0);
                take_tpl(&TEMPLATE.first_prompt, t0);
            } else {
                take_tpl(&TEMPLATE.prompt_next, t0);
            }
            push(
                ms(t0, -15),
                "tengu_input_prompt",
                json!({
                    "is_negative": false,
                    "is_keep_going": false,
                    // 会话里第一次输入是把进程从等待里叫醒的那一次（两份抓包都是首次 true）。
                    "is_wakeup": prompt_index == 1,
                    "prompt_index": prompt_index,
                    "prompt_length": shape.prompt_len,
                    "prompt_source": "typed",
                    "effort_level": &effort_value
                }),
            );
        }

        // 续轮：上一条回复里的工具调用在两次请求之间执行，把权限判定、执行、附件计算那串
        // 补在这条请求之前（`cap/2.1.260-2` 09:43:12–09:43:13）。权限判定发生在上一条回复
        // 流到工具块时，时间戳落在上一条结束之前。
        if is_main && !new_prompt && !shape.tool_uses.is_empty() {
            let prev_end_dt: DateTime<Utc> =
                prev_end.unwrap_or(call.started_at - Duration::from_secs(1)).into();
            // 工具是上一条主线程回复产生的：事件里的 requestId / messageID 都指上一条。
            let prev_req = previous_request_id.clone().unwrap_or_default();
            let prev_msg = prev_main_message_id.clone().unwrap_or_default();
            let n = shape.tool_uses.len() as i64;
            for (i, tu) in shape.tool_uses.iter().enumerate() {
                let i = i as i64;
                if shape.permission_mode == "auto" {
                    let tg = ms(prev_end_dt, -6 - 3 * (n - i));
                    push(
                        tg,
                        "tengu_tool_use_granted_in_config",
                        json!({
                            "messageID": &prev_msg,
                            "toolName": &tu.name,
                            "isMcp": false,
                            "sandboxEnabled": false,
                            "destructive_category": "none",
                            "destructive_target_scope": "none",
                            "git_destructive_target": "none",
                            "permission_mode": shape.permission_mode
                        }),
                    );
                    push(ms(tg, 1), "tengu_feature_ok", feature("permission_auto_approve_config"));
                    dd.push(identity.dd_entry(
                        "tengu_feature_ok",
                        &ctx(ms(tg, 1)),
                        &dd_model,
                        feature("permission_auto_approve_config"),
                    ));
                    push(
                        ms(tg, 2),
                        "tengu_tool_use_can_use_tool_allowed",
                        json!({
                            "messageID": &prev_msg,
                            "toolName": &tu.name,
                            "queryChainId": &chain_id,
                            "queryDepth": prev_main_depth,
                            "requestId": &prev_req
                        }),
                    );
                }
                // 工具执行：按调用数把上一条结束到这条发出之间的时间均分。
                let gap = (t0 - prev_end_dt).num_milliseconds().max(50);
                let t_done = ms(prev_end_dt, gap * (i + 1) / (n + 1));
                let duration = (gap / (n + 1) - 12).max(1);
                if tu.name == "Bash" {
                    if shell_snapshot_first && i == 0 {
                        push(ms(t_done, -40), "tengu_feature_ok", feature("shell_snapshot_create"));
                        dd.push(identity.dd_entry(
                            "tengu_feature_ok",
                            &ctx(ms(t_done, -40)),
                            &dd_model,
                            feature("shell_snapshot_create"),
                        ));
                    }
                    let bash = json!({
                        "command_type": "other",
                        "stdout_length": tu.result_len,
                        "stderr_length": 0,
                        "exit_code": 0,
                        "interrupted": false,
                        "executor_shell": "zsh",
                        "executor_shell_overridden": false,
                        "sandboxed": false,
                        "sandbox_enabled": false,
                        "dangerously_disable_sandbox": false,
                        "user_typed_shell_dispatch": false,
                        "filesystem_policy": "strict",
                        "call_origin": "local",
                        "had_sandbox_violation": false,
                        "was_backgrounded": false,
                        "tool_use_id": &tu.id,
                        "destructive_category": "none",
                        "destructive_target_scope": "none",
                        "git_destructive_target": "none",
                        "permission_mode": shape.permission_mode
                    });
                    push(t_done, "tengu_bash_tool_command_executed", bash.clone());
                    dd.push(identity.dd_entry(
                        "tengu_bash_tool_command_executed",
                        &ctx(t_done),
                        &dd_model,
                        snake_flat(&bash),
                    ));
                    push(t_done, "tengu_feature_ok", feature("tool_bash"));
                    dd.push(identity.dd_entry(
                        "tengu_feature_ok",
                        &ctx(t_done),
                        &dd_model,
                        feature("tool_bash"),
                    ));
                }
                let mut success = json!({
                    "messageID": &prev_msg,
                    "toolName": &tu.name,
                    "isMcp": false,
                    "effort_level": &effort_value,
                    "durationMs": duration,
                    "rssDeltaBytes": 1_081_344,
                    "heapUsedDeltaBytes": 3_887_104,
                    "externalDeltaBytes": 2_870_395,
                    "preToolHookDurationMs": 0,
                    "permissionDurationMs": 11,
                    "toolResultSizeBytes": tu.result_len,
                    "toolInputSizeBytes": tu.input_len
                });
                if tu.name == "Bash" {
                    success["bashCommandLen"] = json!(tu.command_len);
                }
                success["queryChainId"] = json!(&chain_id);
                success["queryDepth"] = json!(prev_main_depth);
                success["requestId"] = json!(&prev_req);
                push(t_done, "tengu_tool_use_success", success.clone());
                dd.push(identity.dd_entry(
                    "tengu_tool_use_success",
                    &ctx(t_done),
                    &dd_model,
                    snake_flat(&success),
                ));
            }
            // 工具都跑完，攒附件再发下一条。
            let ta = ms(t0, -4);
            for label in ["agent_pending_messages", "memory_update"] {
                push(
                    ta,
                    "tengu_attachment_compute_duration",
                    json!({ "label": label, "duration_ms": 0, "attachment_size_bytes": 0, "attachment_count": 0 }),
                );
            }
            push(ta, "tengu_attachments", json!({ "attachment_types": ["total_tokens_reminder"] }));
            let results = shape.tool_uses.len();
            push(
                ta,
                "tengu_query_before_attachments",
                json!({
                    "messagesForQueryCount": shape.messages_len + 3,
                    "assistantMessagesCount": shape.assistant_messages,
                    "toolResultsCount": results,
                    "queryChainId": &chain_id,
                    "queryDepth": query_depth
                }),
            );
            push(
                ms(ta, 1),
                "tengu_query_after_attachments",
                json!({
                    "totalToolResultsCount": results + 1,
                    "fileChangeAttachmentCount": 0,
                    "queryChainId": &chain_id,
                    "queryDepth": query_depth
                }),
            );
        }

        // 客户端内部的消息条数比 API 那份多（harness 注入的 system-reminder 等）：
        // `cap/2.1.260-2`：8→2、12→5、16→8、18→10，即 post + 5 + 第几次输入 + 本轮已有的
        // 续轮次数；`apiSystemMessageCount` = 第几次输入 + 本轮已有的续轮次数（1、2、3、3）。
        // 「本轮已有的续轮次数」主线程就是这条自己的 depth，猜下一句则是主线程最后一条的
        // depth（它自己的 depth 是 +2 过的，不能拿来算）。侧查询没有这些，pre == post、0。
        let turn_extra = if is_main { query_depth } else { prev_main_depth } as usize;
        let (pre_count, api_system) = if kind.has_boundary() {
            (
                shape.messages_len + 5 + prompt_index as usize + turn_extra,
                prompt_index as usize + turn_extra,
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
        let breakpoints = json!({
            "totalMessageCount": shape.messages_len,
            "cachingEnabled": shape.has_cache_control,
            "skipCacheWrite": kind == Kind::Suggestion,
            "forkPointPinned": pinned,
            "markerCount": if pinned { 2 } else { 1 }
        });
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
        if shape.sys0_len > 0 {
            push(
                t0,
                "tengu_sysprompt_block",
                json!({ "length": shape.sys0_len, "hash": &shape.sys0_hash }),
            );
        }
        if kind.has_boundary() && shape.system_blocks >= 2 {
            let boundary = json!({
                "blockCount": shape.system_blocks,
                "staticBlockLength": shape.static_len,
                "dynamicBlockLength": shape.dynamic_len
            });
            push(t0, "tengu_sysprompt_boundary_found", boundary.clone());
            push(t0, "tengu_sysprompt_boundary_found", boundary);
        } else if shape.system_blocks > 0 {
            let missing = json!({ "promptBlockCount": shape.system_blocks });
            push(t0, "tengu_sysprompt_missing_boundary_marker", missing.clone());
            push(t0, "tengu_sysprompt_missing_boundary_marker", missing);
        }
        // 续轮与侧查询在 api_query 时也判一次工具搜索模式（首次输入的那条在模板里）。
        if !new_prompt {
            push(
                t0,
                "tengu_tool_search_mode_decision",
                json!({
                    "enabled": shape.tools_count > 0,
                    "mode": "tst",
                    "reason": if shape.tools_count > 0 { "tst_enabled" } else { "no_tools_in_request" },
                    "checkedModel": &display_model,
                    "mcpToolCount": if shape.tools_count > 0 { 2 } else { 0 },
                    "mcpNonBlocking": false,
                    "userType": "external"
                }),
            );
        }
        push(ms(t0, 1), "tengu_api_cache_breakpoints", breakpoints);

        // 首字节到达。
        push(t_first, "tengu_feature_ok", feature("api_request"));
        dd.push(identity.dd_entry(
            "tengu_feature_ok",
            &ctx(t_first),
            &dd_model,
            feature("api_request"),
        ));
        if first_text_in_turn {
            let t_paint = ms(t_first, 30);
            push(
                t_paint,
                "tengu_turn_first_text",
                json!({
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
                }),
            );
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

        // 收尾：tengu_api_success（键序照抓包）。
        let mut success = Map::new();
        let mut put = |k: &str, v: Value| {
            success.insert(k.to_string(), v);
        };
        put("model", json!(&resp_model));
        if has_1m {
            put("preNormalizedModel", json!(&display_model));
        }
        put("betas", json!(&betas_full));
        put("messageCount", json!(shape.messages_len));
        put("messageTokens", json!(message_tokens));
        put("inputTokens", json!(call.input_tokens));
        put("outputTokens", json!(call.output_tokens));
        put("cachedInputTokens", json!(call.cache_read_tokens));
        put("uncachedInputTokens", json!(call.cache_creation_tokens));
        put("durationMs", json!(total));
        // 比 durationMs 多出的那 1–5ms 是客户端重试包装层的开销，官方五条是 +3/+5/+1/+1/+4；
        // 按 request-id 取个稳定的零头，别恒等。
        let retry_pad = call
            .request_id
            .as_deref()
            .map(|r| r.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b))))
            .unwrap_or(0)
            % 5;
        put("durationMsIncludingRetries", json!(total + 1 + i64::from(retry_pad)));
        put("attempt", json!(1));
        put("ttftMs", json!(ttft));
        put("buildAgeMins", json!(build_age_mins));
        put("provider", json!("firstParty"));
        put("requestId", json!(call.request_id.as_deref().unwrap_or("")));
        put("stop_reason", json!(call.stop_reason.as_deref().unwrap_or("end_turn")));
        if let Some(e) = &effort {
            put("effort_level", json!(e));
        }
        // 只有主线程（和子代理）报「是不是默认模型/默认 effort」；猜下一句和标题那类不报。
        if matches!(kind, Kind::Main | Kind::Subagent) {
            put("is_default_model", json!(display_model == default_model));
            put("default_model", json!(&default_model));
            if let Some(e) = &effort {
                put("is_default_effort", json!(true));
                put("default_effort_level", json!(e));
            }
        }
        put("costUSD", json!(call.cost_usd.unwrap_or(0.0)));
        put("didFallBackToNonStreaming", json!(false));
        put("isNonInteractiveSession", json!(false));
        put("print", json!(false));
        put("isTTY", json!(true));
        put("querySource", json!(query_source));
        if kind.has_chain() {
            put("queryChainId", json!(&chain_id));
            put("queryDepth", json!(query_depth));
        }
        put("permissionMode", json!(shape.permission_mode));
        put("globalCacheStrategy", json!("system_prompt"));
        if shape.has_cache_control {
            put("prompt_cache_ttl", json!(cache_ttl));
            put("prompt_cache_ttl_reason", json!("subscriber"));
        }
        put("textContentLength", json!(call.text_chars));
        // 没有正文（tool_use 收尾）或真有思考内容时才带思考长度，抓包里三种情形都对得上。
        if call.thinking_chars > 0 || call.text_chars == 0 {
            put("thinkingContentLength", json!(call.thinking_chars));
        }
        put("narrationBlockCount", json!(0));
        put("imageBlockCount", json!(shape.image_blocks));
        put("imageTotalPixels", json!(0));
        put("imageTotalBytes", json!(shape.image_bytes));
        put("documentBlockCount", json!(shape.doc_blocks));
        put("documentTotalBytes", json!(shape.doc_bytes));
        put("inputTextCharLength", json!(shape.input_text_chars));
        put("estimatedInputTokens", json!(shape.estimated_tokens));
        put("systemCharLength", json!(shape.system_chars));
        if modern && kind.has_boundary() {
            put("systemPromptSource", json!("live_unrecorded"));
        }
        put("toolsCharLength", json!(shape.tools_chars));
        put("toolsCount", json!(shape.tools_count));
        put("deferredToolsCount", json!(shape.deferred_tools));
        put("toolSchemasHash", json!(&shape.tools_hash));
        put("requestBodyEncoding", json!("identity"));
        put(
            "requestBodyChars",
            json!(std::str::from_utf8(&call.body).map(js_len).unwrap_or(call.body.len())),
        );
        put("gzipSkipReason", json!("proxy"));
        put("fastMode", json!(shape.fast_mode));
        if let Some(prev) = &previous_request_id {
            put("previousRequestId", json!(prev));
        }
        if let Some(ms_since) = time_since_last {
            put("timeSinceLastApiCallMs", json!(ms_since));
        }
        let success = Value::Object(success);
        push(t_end, "tengu_api_success", success.clone());
        dd.push(identity.dd_entry(
            "tengu_api_success",
            &ctx(t_end),
            &dd_model,
            snake_flat(&success),
        ));

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
        if kind == Kind::Title {
            push(t_end, "tengu_session_title_generated", json!({ "success": true }));
        }

        // 一轮结束（`end_turn`）才有 stop hook 与 turn_end；`tool_use` 是同一轮的中间步。
        let turn_end = |terminal: &str, duration: i64| {
            json!({
                "terminal_reason": terminal,
                "is_error": false,
                "is_subagent": kind == Kind::Subagent,
                "goal_active": false,
                "duration_ms": duration,
                "query_source": query_source,
                "query_source_category": kind.category()
            })
        };
        if is_main && turn_over {
            take_tpl(&TEMPLATE.turn, t_end);
            if emit_first_turn {
                take_tpl(&TEMPLATE.first_turn, t_end);
            }
            let t1 = ms(t_end, 1);
            let t2 = ms(t_end, 2);
            push(t1, "tengu_feature_ok", feature("hook_stop_handler"));
            push(t2, "tengu_feature_ok", feature("turn"));
            push(
                t2,
                "tengu_turn_end",
                turn_end("completed", (t2 - turn_started).num_milliseconds().max(total)),
            );
            dd.push(identity.dd_entry(
                "tengu_feature_ok",
                &ctx(t1),
                &dd_model,
                feature("hook_stop_handler"),
            ));
            dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t2), &dd_model, feature("turn")));
        }
        // 猜下一句：自己就是一轮（`cap/2.1.260-2` 09:43:21），收尾多一条 fork 统计，没有 tips。
        if kind == Kind::Suggestion {
            let t1 = ms(t_end, 1);
            let total_in = call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
            let hit_rate =
                if total_in > 0 { call.cache_read_tokens as f64 / total_in as f64 } else { 0.0 };
            for name in ["hook_stop_handler", "prompt_suggestion_generate", "turn"] {
                push(t1, "tengu_feature_ok", feature(name));
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t1), &dd_model, feature(name)));
            }
            push(
                t1,
                "tengu_fork_agent_query",
                json!({
                    "forkLabel": "prompt_suggestion",
                    "querySource": "prompt_suggestion",
                    "durationMs": total + 7,
                    "messageCount": 1,
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
                }),
            );
            push(t1, "tengu_turn_end", turn_end("completed", total + 7));
        }

        // `take_tpl` 借着 `tpl_events`/`tpl_dd`，到这里已经不再用它，可以并进主队列。
        events.extend(tpl_events);
        dd.extend(tpl_dd);

        // 新会话：把启动握手那串 GET 交给发送循环去做（用该凭证的 token 与代理）。
        if is_new_session {
            st.handshakes.push(Handshake {
                cred_id: call.cred_id,
                snapshot: SessionSnapshot {
                    session_id: session_id.clone(),
                    device_id: device_id.clone(),
                    account_uuid: account_uuid.clone(),
                    version: version.clone(),
                    model: display_model.clone(),
                    betas: betas_session.clone(),
                    prompt_id: prompt_id.clone(),
                    started_wall,
                },
                model: resp_model.clone(),
            });
        }

        // ---- 入队 ----
        let pending = st.pending.entry((call.cred_id, session_id.clone())).or_default();
        pending.version = version;
        pending.subscription_type = identity.subscription_type.clone();
        pending.identity = Some(identity.clone());
        pending.model = display_model.clone();
        pending.betas = betas_session.clone();
        pending.prompt_id = prompt_id.clone();
        pending.started_wall = Some(started_wall);
        pending.events_since.get_or_insert(now);
        pending.events.extend(events);
        pending.dd_since.get_or_insert(now);
        pending.dd.extend(dd);
        pending.metrics_since.get_or_insert(now);
        pending.metrics.push(CallMetric {
            session_id,
            device_id,
            account_uuid,
            model: display_model,
            category: kind.category(),
            effort: shape.effort.clone(),
            cost: call.cost_usd.unwrap_or(0.0),
            input: call.input_tokens,
            output: call.output_tokens,
            cache_read: call.cache_read_tokens,
            cache_creation: call.cache_creation_tokens,
            cli_secs: if is_main && turn_over { call.total_ms as f64 / 1000.0 } else { 0.0 },
            user_secs,
            new_session: is_new_session && !counted,
            resumed,
        });

        // 主线程请求到了：新一轮的 prompt id 已经写进会话，把扣住的侧查询补发出去。
        if is_main {
            let deferred = st
                .sessions
                .get_mut(&key)
                .map(|s| std::mem::take(&mut s.deferred))
                .unwrap_or_default();
            for (c, prev_end, _) in deferred {
                Self::process(st, c, false, prev_end);
            }
        }
    }

    /// 取走到期该发的：事件攒满 [`config::TELEMETRY_EVENT_FLUSH_SECS`]、Datadog 攒满
    /// [`config::TELEMETRY_DATADOG_FLUSH_SECS`]、指标攒满 [`config::TELEMETRY_METRICS_FLUSH_SECS`]，
    /// 或任一路条数到了 [`config::TELEMETRY_BATCH_MAX`]。每个会话一份 [`Flush`]，同一张凭证
    /// 的多个会话各发各的。
    pub fn take_due(&self, now: Instant) -> Vec<Flush> {
        let mut st = self.0.lock();
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
        let mut st = self.0.lock();
        // 扣住太久的侧查询：没等到主线程请求，按会话现有的 prompt id 补发。
        let hold = Duration::from_secs(config::TELEMETRY_SIDE_QUERY_HOLD_SECS);
        let stale: Vec<(i64, String)> = st
            .sessions
            .iter()
            .filter(|(_, s)| s.deferred.iter().any(|(_, _, at)| now.duration_since(*at) >= hold))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            let deferred = st
                .sessions
                .get_mut(&k)
                .map(|s| std::mem::take(&mut s.deferred))
                .unwrap_or_default();
            for (c, prev_end, _) in deferred {
                Self::process(&mut st, c, false, prev_end);
            }
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
        for (k, _) in &expired {
            st.ended.insert(k.clone(), now);
        }
        for ((cred_id, session_id), s) in expired {
            let identity = Identity {
                session_id: session_id.clone(),
                device_id: s.device_id,
                account_uuid: s.account_uuid,
                organization_uuid: st.org_uuid.get(&cred_id).cloned(),
                subscription_type: s.subscription_type,
                version: s.version.clone(),
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
        a.insert("start_type".into(), json!(if c.resumed { "resume" } else { "fresh" }));
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
    };
    for c in calls {
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
    metrics.push(json!({
        "name": "claude_code.cost.usage",
        "description": "Cost of the Claude Code session",
        "unit": "USD",
        "data_points": cost_points
    }));
    metrics.push(json!({
        "name": "claude_code.token.usage",
        "description": "Number of tokens used",
        "unit": "tokens",
        "data_points": token_points
    }));
    metrics.push(json!({
        "name": "claude_code.active_time.total",
        "description": "Total active time in seconds",
        "unit": "s",
        "data_points": active_points
    }));
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
        for h in t.take_handshakes() {
            let Ok(Some(cred)) = store.get(h.cred_id) else { continue };
            if cred.is_banned() {
                continue;
            }
            let Ok(client) = clients.for_credential(&cred) else { continue };
            let Ok(crate::store::TokenAttempt::Ready(token)) =
                crate::store::ensure_fresh_token(&store, &clients, &cred).await
            else {
                continue;
            };
            let org = t.org_uuid(h.cred_id);
            crate::oauth::session_handshake(&client, &token, &cred, h, org).await;
        }
        for f in t.take_due(now) {
            let Ok(Some(cred)) = store.get(f.cred_id) else { continue };
            if cred.is_banned() {
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

    fn identity() -> Identity {
        Identity {
            session_id: "111e3644-948f-43fb-9bc2-cac60e65fd32".into(),
            device_id: "b9".repeat(32),
            account_uuid: "9922ef8e-7945-4f5a-ab4f-cf5f521531df".into(),
            organization_uuid: Some("09520b85-f6b6-432f-97e2-6ecb804a083f".into()),
            subscription_type: "team".into(),
            version: "2.1.258".into(),
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
            started_at: SystemTime::now() - Duration::from_secs(8),
            ttft_ms: Some(1800),
            total_ms: 6118,
            request_id: Some(request_id.into()),
            message_id: Some("msg_011Cedjuoa4oBPzoB2CSUNEB".into()),
            stop_reason: Some(stop.into()),
            resp_model: Some("claude-opus-5".into()),
            input_tokens: 2,
            output_tokens: 31,
            cache_read_tokens: 26736,
            cache_creation_tokens: 8729,
            text_chars: 87,
            thinking_chars: 0,
            cost_usd: Some(0.18),
            speed: None,
        }
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
    fn new_session_gets_the_startup_burst_and_a_handshake() {
        let t = Telemetry::default();
        t.ingest(call(cc_body(true), "req_1", "end_turn"));
        {
            let st = t.0.lock();
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
            assert_eq!(meta["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622");
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
            assert_eq!(st.handshakes.len(), 1);
            assert_eq!(st.handshakes[0].snapshot.session_id, SESSION);
            assert_eq!(st.handshakes[0].model, "claude-opus-5");
        }
        assert_eq!(t.take_handshakes().len(), 1);
        assert!(t.take_handshakes().is_empty());

        // 同一会话第二轮：有输入串、没有启动串与首轮那串。
        let mut second = call(cc_body(true), "req_2", "end_turn");
        second.started_at = SystemTime::now();
        t.ingest(second);
        let st = t.0.lock();
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
        assert!(st.handshakes.is_empty(), "握手只排一次");
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
            let st = t.0.lock();
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
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14"
        );
        assert_eq!(s.prompt_id, "6c079143-0c53-4c48-817d-105460b3f622");
        assert!(t.latest_session(8, Duration::from_secs(3600)).is_none(), "别的凭证没有");
        assert!(t.latest_session(7, Duration::ZERO).is_none(), "超过闲置上限就不算近期");
    }

    /// 会话闲置到期 = 客户端退出：补退出事件链，三路立刻到期一起发（`cap/2.1.260-1`）。
    #[test]
    fn idle_session_ends_like_a_client_exit() {
        let t = Telemetry::default();
        // 一分钟前发的：首轮那串版本检查（api_success 后 6.9s）得落在「退出」之前，真实
        // 情形下退出离最后一条请求至少 3 小时。
        let mut last = call(cc_body(true), "req_last", "end_turn");
        last.started_at = SystemTime::now() - Duration::from_secs(60);
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
        back.started_at = SystemTime::now();
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
        third.started_at = SystemTime::now();
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
        assert!(t.0.lock().pending.is_empty());
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
        first.started_at = SystemTime::now() - Duration::from_secs(20);
        t.ingest(first);
        // 续轮：上一条 assistant 是 Bash 调用，末条是 tool_result。
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la","description":"list"}}]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"total 8\nfile"}]});
        let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
        second.started_at = SystemTime::now() - Duration::from_secs(10);
        second.ua_out = "claude-cli/2.1.260 (external, cli)".into();
        t.ingest(second);

        let st = t.0.lock();
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
        assert_eq!(bash[0]["stdout_length"], "total 8\nfile".len());
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
        assert_eq!(successes[0]["thinkingContentLength"], 0, "无正文时带思考长度 0");
        assert!(successes[1].get("thinkingContentLength").is_none());
        assert_eq!(successes[1]["systemPromptSource"], "live_unrecorded", "2.1.258 以上才有");
        let ends = by_name("tengu_turn_end");
        assert_eq!(ends.len(), 1, "tool_use 那条不结束这一轮");
        assert!(ends[0]["duration_ms"].as_i64().unwrap() >= 10_000, "整轮时长，从提交算起");
        // active_time：cli 只算 end_turn 收尾的那条（6.118s），tool_use 那条不算；user 是首次
        // 输入的 0.9s。
        let m = metrics_body(&p.metrics, "2.1.260", "team", None);
        let active = m["metrics"][3]["data_points"].as_array().unwrap();
        let by_type = |ty: &str| {
            active.iter().find(|d| d["attributes"]["type"] == ty).unwrap()["value"]
                .as_f64()
                .unwrap()
        };
        assert!((by_type("cli") - 6.118).abs() < 0.01, "{}", by_type("cli"));
        assert!((by_type("user") - 0.9).abs() < 0.01);
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
        main.started_at = SystemTime::now() - Duration::from_secs(20);
        t.ingest(main);
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        body["messages"][2] = json!({"role":"user","content":"[SUGGESTION MODE: Suggest what the user might naturally type next into Claude Code.]\n\nFIRST: ..."});
        body["system"][0]["text"] = json!(
            "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=b6499; cc_prev_req=req_main;"
        );
        let mut sugg = call(body.to_string().into_bytes(), "req_sugg", "end_turn");
        sugg.started_at = SystemTime::now() - Duration::from_secs(5);
        sugg.ua_out = "claude-cli/2.1.260 (external, cli)".into();
        t.ingest(sugg);
        let st = t.0.lock();
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
        main.started_at = SystemTime::now() - Duration::from_secs(20);
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
        title.started_at = SystemTime::now() - Duration::from_secs(5);
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
        let st = t.0.lock();
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
        third.started_at = SystemTime::now() - Duration::from_secs(2);
        t.ingest(third);
        let st = t.0.lock();
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
        first.started_at = SystemTime::now() - Duration::from_secs(30);
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
        second.started_at = SystemTime::now() - Duration::from_secs(10);
        t.ingest(second);
        let st = t.0.lock();
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
        first.started_at = SystemTime::now() - Duration::from_secs(30);
        t.ingest(first);
        let mut title = call(title_body(), "req_title", "end_turn");
        title.started_at = SystemTime::now() - Duration::from_secs(12);
        title.betas = Some("claude-code-20250219,oauth-2025-04-20".into());
        t.ingest(title);
        {
            let st = t.0.lock();
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
        second.started_at = SystemTime::now() - Duration::from_secs(11);
        t.ingest(second);
        {
            let st = t.0.lock();
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
        assert_eq!(t.0.lock().sessions[&key()].deferred.len(), 1, "还没到 10s");
        t.gc(now + Duration::from_secs(config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1));
        let st = t.0.lock();
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
    }

    #[test]
    fn version_comes_from_the_outbound_ua() {
        assert_eq!(version_from_ua("claude-cli/2.1.260 (external, cli)"), "2.1.260");
        assert_eq!(version_from_ua("claude-code/2.1.258"), "2.1.258");
        assert_eq!(version_from_ua("curl/8.0"), config::CC_VERSION_BASE);
    }

    #[test]
    fn metadata_is_standard_base64_with_padding() {
        let id = identity();
        let b64 = id.metadata_b64("p", json!({"feature_name":"notification_show"}));
        let decoded = STANDARD.decode(&b64).expect("standard base64");
        let v: Value = serde_json::from_slice(&decoded).unwrap();
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
        let st = t.0.lock();
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
        assert_eq!(meta["gzipSkipReason"], "proxy");
        assert_eq!(meta["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622");
        // api_success 顶层 model 跟 meta 走，是规范名；api_query 则是展示名。
        assert_eq!(success.1["event_data"]["model"], "claude-opus-5");
        let query = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_query").unwrap();
        assert_eq!(query.1["event_data"]["model"], "claude-opus-5[1m]");
        assert_eq!(
            success.1["event_data"]["betas"],
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14"
        );
        assert_eq!(
            success.1["event_data"]["auth"]["organization_uuid"],
            "09520b85-f6b6-432f-97e2-6ecb804a083f"
        );
        assert_eq!(p.dd.iter().filter(|d| d["message"] == "tengu_api_success").count(), 1);
        drop(st);

        // 续轮：tool_result 收尾，end_turn → turn_end；previousRequestId 串上一条。
        // 第二条在第一条结束之后才发出（第一条 8s 前发、跑了 6.1s）。
        let mut second = call(cc_body(false), "req_2", "end_turn");
        second.started_at = SystemTime::now();
        t.ingest(second);
        let st = t.0.lock();
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
        let st = t.0.lock();
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
        assert!(t.0.lock().pending.is_empty(), "没有 device_id 就不报");
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
}
