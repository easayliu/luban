//! 身份与事件构造（保活与逐请求两路共用）

use super::*;

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
    pub(super) fn entrypoint(&self) -> &'static str {
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
    pub(super) fn metadata_b64_at(
        &self,
        stage: MetaStage,
        prompt_id: &str,
        extra: Value,
    ) -> String {
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
    pub(super) fn event_at(
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
    pub(super) fn dd_entry_at(
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
pub(super) fn dd_model_short(model: &str) -> String {
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

/// `tengu_api_success` 的 meta 转成 Datadog 扁平字段：键 camel → snake，去掉 base 已有的三项。
pub(super) fn snake_flat(meta: &Value) -> Value {
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
