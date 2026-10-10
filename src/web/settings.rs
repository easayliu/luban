//! 接入设置的读取：设置与转发开关的视图、官方最新版本的同步。

use super::*;

// ---------- 接入设置 ----------

#[derive(Serialize)]
pub(super) struct SettingsResp {
    /// `LUBAN_API_KEY` / `--api-key` 设的接入 Key（只在 `env_managed` 时有值）。
    api_key: Option<String>,
    /// 是否设了 `LUBAN_API_KEY` / `--api-key`。
    env_managed: bool,
    /// 转发是否要求带接入 Key：设了环境变量那把、库里有 Key，或者配过（全删了也算）。
    api_keys_required: bool,
    /// 设备绑定有效期（秒）；0 表示永不过期。
    device_binding_ttl_secs: i64,
    /// 软绑定保留期（秒）：超过有效期的绑定不再占名额，但这段时间内设备回来仍优先回原号。
    /// 0 表示永久保留。
    device_binding_retention_secs: i64,
    /// 模拟会话绑定有效期（秒）；0 表示永不过期。与设备的分开配。
    session_binding_ttl_secs: i64,
    /// 模拟会话软绑定保留期（秒）；0 表示永久保留。
    session_binding_retention_secs: i64,
    /// 全局默认设备数上限；0 表示默认不限。账号未单独配置时套用它。
    default_device_limit: i64,
    /// 全局默认会话数上限；0 表示默认不限。账号未单独配置时套用它。见
    /// `store::Select::per_session`。
    default_session_limit: i64,
    /// 带设备身份的来访按会话占名额、设备上限不生效（由转发设置里的设备指纹归一化与身份伪装
    /// 决定，见 [`store::ForwardFlags::devices_by_session`]），设置页据此提示。
    devices_by_session: bool,
    /// 全局默认账号 RPM 上限（最近 60 秒最多转发多少条）；0 表示默认不限。
    /// 账号未单独配置时套用它。
    default_rpm_limit: i64,
    /// 每设备 RPM 上限（单台设备最近 60 秒最多转发多少条）；0 表示不限。全局一个值。
    device_rpm_limit: i64,
    /// 每会话 RPM 上限（单个会话最近 60 秒最多转发多少条）；0 表示不限。全局一个值。
    /// 与 [`Self::device_rpm_limit`] 两个粒度并存，见 [`crate::store::SESSION_RPM_LIMIT`]。
    session_rpm_limit: i64,
    /// 每会话并发在途上限（单个会话同时在飞的最大请求数）；0 表示不限。
    session_concurrency_limit: i64,
    /// 是否要求请求携带有效设备身份（`metadata.user_id`）；关闭后放行裸客户端。
    require_device_id: bool,
    /// 允许接入的最低 Claude Code 客户端版本；空串表示不限。只卡 UA 自报 `claude-cli/<版本>`
    /// 的请求，见 [`crate::store::MIN_CLIENT_VERSION`]。
    min_client_version: String,
    /// 已知的官方最新 Claude Code 版本（`主.次.修`）；空串表示还没学到、也没手动填。
    /// 自动从 `downloads.claude.ai` 学（只升不降）并落库；网页可手动填、可删。来访 UA 自报
    /// 高于 `max(它, cc_version_base)` 的版本不当官方客户端。见 [`crate::store::LATEST_CC_RELEASE`]。
    latest_cc_release: String,
    /// 上限的写死兜底值：抓包证实存在的最新官方版本
    /// （[`crate::config::CC_LATEST_KNOWN_RELEASE`]）。字段名沿用旧的，前端按它显示「退回基线」；
    /// 它**不再是**模拟路径的版本（那是 [`crate::config::CC_VERSION_BASE`]，可以更旧）。
    cc_version_base: String,
    /// 登录时实际申请的 OAuth scope（空格分隔）；恒为非空——没配就是
    /// [`crate::config::SCOPES`]。
    oauth_scopes: String,
    /// 默认 scope 串（[`crate::config::SCOPES`]），供网页判断当前是不是默认值。
    oauth_scopes_default: String,
    /// 精简 scope 串（[`crate::config::SCOPES_MINIMAL`]），网页上「只要必需项」那一档。
    oauth_scopes_minimal: String,
    /// 单凭证裸请求速率上限（窗口内条数）；0 表示不限。
    bare_rate_limit: i64,
    /// 裸请求速率窗口（秒），默认 60。
    bare_rate_window_secs: i64,
    /// 上游 429 时最多换几个号重试；0 表示不重试。
    rate_limit_retry_max: i64,
    /// **5h 窗口**的使用率到多少百分比就提前把号挪出调度池；0 表示关闭（收到 429 才停）。
    /// 见 [`crate::proxy::park_if_quota_nearly_exhausted`]。
    quota_pause_pct: i64,
    /// **7d 窗口**的同一档阈值，另算；0（默认）= 不按周用量停号，见
    /// [`crate::store::QUOTA_PAUSE_PCT_7D`]。
    quota_pause_pct_7d: i64,
    /// 转发形态开关（默认全开）。
    #[serde(flatten)]
    forwarding: ForwardingResp,
}

/// 转发形态开关的对外形态；字段名与 [`crate::store::ForwardFlags`] 一一对应。
#[derive(Serialize)]
struct ForwardingResp {
    /// 改写 `metadata.user_id` 的 account_uuid/device_id。
    spoof_identity: bool,
    /// 来访自带 `device_id` 时要不要换成派生值（[`Self::spoof_identity`] 的子项）。
    spoof_device_id: bool,
    /// 设备指纹只取平台（arch/os），不含客户端原始 device_id（[`Self::spoof_device_id`] 的子项）。
    normalize_device_fp: bool,
    /// 给 `x-anthropic-billing-header` 补 `cch`。
    billing_cch: bool,
    /// 真实 CC 来访的 body 被改写后按最终出站字节重算 `cch`。
    cch_real_recompute: bool,
    /// 模拟请求的 `cch` 按出站字节算真值（关则随机值）。
    cch_sim_compute: bool,
    /// 补齐客户端未携带的 `accept-encoding`/`anthropic-version`/`x-client-request-id`。
    fill_client_headers: bool,
    /// 合并并按官方顺序重排 `anthropic-beta`（含塞入 oauth beta）。
    merge_beta: bool,
    /// 把 `system` 对齐成官方订阅客户端的 4 块形态（拆/并块 + 块数封顶 4）。
    system_shape: bool,
    /// 按官方拼写与顺序发出头名。
    orig_header_case: bool,
    /// 上游拒绝 thinking 块签名时，降级历史 thinking 后重试一次。
    thinking_signature_retry: bool,
    /// 上游拒绝 `redacted_thinking` 块的密文时，降级历史 thinking 后重试一次。
    redacted_thinking_retry: bool,
    /// 非 Claude Code 客户端的请求，按官方抓包形态模拟成 CC 请求。
    simulate_cc: bool,
    /// 模拟路径补齐官方 `system` 第四块（[`Self::simulate_cc`] 的子项）。
    simulate_full_system: bool,
    /// 模拟路径给不带 `tools` 的来访也补官方工具（[`Self::simulate_cc`] 的子项）。
    fill_absent_tools: bool,
    /// 模拟路径注入的官方工具去掉 Artifact / ListAgents / SendFeedback（[`Self::simulate_cc`] 的子项）。
    sim_trim_tools: bool,
    /// 模拟路径只注 `system[0]` billing header、其余注入全部跳过（[`Self::simulate_cc`] 的子项，实验性）。
    sim_billing_only: bool,
    /// billing-only 下保留模拟请求自带的 `metadata.user_id`；关掉即剥离。
    sim_billing_keep_user_id: bool,
    /// billing-only 下保留真实客户端自带的 `metadata.user_id`；关掉即剥离。
    real_billing_keep_user_id: bool,
    /// 模拟路径的主线程按官方 message threads 形态写 `thread`（[`Self::simulate_cc`] 的子项）。
    sim_message_threads: bool,
    /// 已是 CC 形态但不带 `metadata.user_id` 的请求，补一份官方形态的身份。
    fill_metadata: bool,
    /// 上游回 429 时给该号打冷却并换号重试。
    rate_limit_retry: bool,
    /// 官方基座那块的缓存断点带不带 `scope:"global"`（跨账号共享基座缓存）。
    cache_scope_global: bool,
    /// 缓存断点写不写 `ttl:"1h"`（对齐官方；关掉即沿用客户端自己传的时长）。
    cache_ttl_1h: bool,
    /// 工具声明补不补 `eager_input_streaming: true`（只补抓包证实过的版本 × 模型 × 用途）。
    eager_tool_streaming: bool,
    /// 非流式 `/v1/messages` 改成流式发给上游，再把 SSE 聚合回整段 JSON 给客户端。
    nonstream_as_sse: bool,
    /// 剥掉官方客户端从不发送的顶层字段（缺省语义的 `tool_choice`、`thinking.display`）。
    strip_extra_fields: bool,
    /// 把会被上游判成第三方应用的工具名换成假名转发，回程再还原。
    tool_name_mimic: bool,
    /// 模拟路径下是否注入 thinking（及配套的 context_management）。
    inject_thinking: bool,
    /// 本地拒绝带 OpenAI 格式转换残留的请求，不修补不转发。
    reject_openai_shape: bool,
    /// 会话 id 头体不一致时本地拒绝，不替客户端选一个。
    reject_session_conflict: bool,
    /// 本地就地回答探针 / 探活类请求（200 + 一句「OK」，头上标 x-luban-local），不转发。
    reject_probes: bool,
    /// 探针拒绝严格模式：ping 不要求无 tools，新增「短开场」判据。默认关。
    reject_probes_strict: bool,
    /// 本地拦下上游分类器已拒答过的那条提示词的逐字重发，原样回放上游那次的响应（200）；
    /// 出站带 fallbacks 的不拦。
    reject_refusals: bool,
    /// 本地拒绝上游回过 200 却零输出的请求类（403）。
    reject_empty_replies: bool,
    /// 从上游 400 学请求形态错误并在本地拒掉同样的组合；关掉即不学也不拦。
    reject_learned_shapes: bool,
    /// 替每条转发的 `/v1/messages` 上报官方客户端形态的遥测。
    api_telemetry: bool,
    /// 保活循环里的空闲遥测（版本检查事件 + Datadog + GrowthBook 画像）。
    keepalive_telemetry: bool,
    /// fable 族主线程请求补官方那份服务端 refusal fallback（拒答时上游换 opus-5 重跑）。
    fable_refusal_fallback: bool,
    /// opus-5 族主线程请求补 luban 自定的 refusal fallback 链（4.8 → 4.6）；实验开关，默认关。
    opus_refusal_fallback: bool,
}

impl From<crate::store::ForwardFlags> for ForwardingResp {
    fn from(f: crate::store::ForwardFlags) -> Self {
        Self {
            spoof_identity: f.spoof_identity,
            spoof_device_id: f.spoof_device_id,
            normalize_device_fp: f.normalize_device_fp,
            billing_cch: f.billing_cch,
            cch_real_recompute: f.cch_real_recompute,
            cch_sim_compute: f.cch_sim_compute,
            fill_client_headers: f.fill_client_headers,
            merge_beta: f.merge_beta,
            system_shape: f.system_shape,
            orig_header_case: f.orig_header_case,
            thinking_signature_retry: f.thinking_signature_retry,
            redacted_thinking_retry: f.redacted_thinking_retry,
            simulate_cc: f.simulate_cc,
            simulate_full_system: f.simulate_full_system,
            fill_absent_tools: f.fill_absent_tools,
            sim_trim_tools: f.sim_trim_tools,
            sim_billing_only: f.sim_billing_only,
            sim_billing_keep_user_id: f.sim_billing_keep_user_id,
            real_billing_keep_user_id: f.real_billing_keep_user_id,
            sim_message_threads: f.sim_message_threads,
            fill_metadata: f.fill_metadata,
            rate_limit_retry: f.rate_limit_retry,
            cache_scope_global: f.cache_scope_global,
            cache_ttl_1h: f.cache_ttl_1h,
            eager_tool_streaming: f.eager_tool_streaming,
            nonstream_as_sse: f.nonstream_as_sse,
            strip_extra_fields: f.strip_extra_fields,
            tool_name_mimic: f.tool_name_mimic,
            inject_thinking: f.inject_thinking,
            reject_openai_shape: f.reject_openai_shape,
            reject_session_conflict: f.reject_session_conflict,
            reject_probes: f.reject_probes,
            reject_probes_strict: f.reject_probes_strict,
            reject_refusals: f.reject_refusals,
            reject_empty_replies: f.reject_empty_replies,
            reject_learned_shapes: f.reject_learned_shapes,
            api_telemetry: f.api_telemetry,
            keepalive_telemetry: f.keepalive_telemetry,
            fable_refusal_fallback: f.fable_refusal_fallback,
            opus_refusal_fallback: f.opus_refusal_fallback,
        }
    }
}

/// 读 settings 里的 `latest_cc_release`。值写坏了（手改库、旧文件）按没有处理并告警——一个解析
/// 不了的上限等于没有上限，比静默忽略更该让人看见。
pub(super) fn read_latest_release_setting(
    store: &CredentialStore,
) -> Result<Option<oauth::ReleaseVersion>> {
    let Some(raw) = store.get_setting(store::LATEST_CC_RELEASE)? else { return Ok(None) };
    let v = oauth::parse_release_body(&raw);
    if v.is_none() {
        tracing::warn!(value = %raw, "settings.latest_cc_release is not a version number, ignoring");
    }
    Ok(v)
}

/// 启动时把 settings 里的 `latest_cc_release` 同步进进程内缓存（库为准）。
///
/// 导入设置、网页手动改/删**不走这个**：那两处要在同一把锁里先写库再同步，见
/// [`oauth::ReleaseCache::sync_from_store`]。
pub(super) async fn sync_latest_release_from_store(store: &CredentialStore) {
    if let Err(e) =
        oauth::LATEST_RELEASE.sync_from_store(async { read_latest_release_setting(store) }).await
    {
        tracing::warn!(error = %e, "failed to read settings.latest_cc_release");
    }
}

pub(super) async fn settings_resp(state: &AppState) -> SettingsResp {
    let device_binding_ttl_secs = state.store.device_binding_ttl();
    let device_binding_retention_secs = state.store.device_binding_retention();
    let session_binding_ttl_secs = state.store.session_binding_ttl();
    let session_binding_retention_secs = state.store.session_binding_retention();
    let default_device_limit = state.store.default_device_limit();
    let default_session_limit = state.store.default_session_limit();
    let devices_by_session = state.store.forward_flags().devices_by_session();
    let default_rpm_limit = state.store.default_rpm_limit();
    let device_rpm_limit = state.store.device_rpm_limit();
    let session_rpm_limit = state.store.session_rpm_limit();
    let session_concurrency_limit = state.store.session_concurrency_limit();
    let require_device_id = state.store.require_device_id();
    let min_client_version = state.store.min_client_version().unwrap_or_default();
    let latest_cc_release =
        oauth::LATEST_RELEASE.get().map(oauth::release_string).unwrap_or_default();
    let cc_version_base = crate::config::CC_LATEST_KNOWN_RELEASE.to_string();
    let oauth_scopes = state.store.oauth_scopes();
    let bare_rate_limit = state.store.bare_rate_limit();
    let bare_rate_window_secs = state.store.bare_rate_window_secs();
    let rate_limit_retry_max = state.store.rate_limit_retry_max() as i64;
    let quota_pause_pct = state.store.quota_pause_pct();
    let quota_pause_pct_7d = state.store.quota_pause_pct_7d();
    let forwarding = state.store.forward_flags().into();
    if let Some(k) = &state.client_key {
        return SettingsResp {
            api_key: Some(k.to_string()),
            env_managed: true,
            api_keys_required: true,
            device_binding_ttl_secs,
            device_binding_retention_secs,
            session_binding_ttl_secs,
            session_binding_retention_secs,
            default_device_limit,
            default_session_limit,
            devices_by_session,
            default_rpm_limit,
            device_rpm_limit,
            session_rpm_limit,
            session_concurrency_limit,
            require_device_id,
            min_client_version,
            latest_cc_release: latest_cc_release.clone(),
            cc_version_base: cc_version_base.clone(),
            oauth_scopes,
            oauth_scopes_default: crate::config::SCOPES.to_string(),
            oauth_scopes_minimal: crate::config::SCOPES_MINIMAL.to_string(),
            bare_rate_limit,
            bare_rate_window_secs,
            rate_limit_retry_max,
            quota_pause_pct,
            quota_pause_pct_7d,
            forwarding,
        };
    }
    let api_key = state
        .store
        .get_setting(crate::store::CLIENT_API_KEY)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    SettingsResp {
        api_key,
        env_managed: false,
        api_keys_required: state.store.api_keys_required().await.unwrap_or(true),
        device_binding_ttl_secs,
        device_binding_retention_secs,
        session_binding_ttl_secs,
        session_binding_retention_secs,
        default_device_limit,
        default_session_limit,
        devices_by_session,
        default_rpm_limit,
        device_rpm_limit,
        session_rpm_limit,
        session_concurrency_limit,
        require_device_id,
        min_client_version,
        latest_cc_release,
        cc_version_base,
        oauth_scopes,
        oauth_scopes_default: crate::config::SCOPES.to_string(),
        oauth_scopes_minimal: crate::config::SCOPES_MINIMAL.to_string(),
        bare_rate_limit,
        bare_rate_window_secs,
        rate_limit_retry_max,
        quota_pause_pct,
        quota_pause_pct_7d,
        forwarding,
    }
}

/// 读取接入设置。
pub(super) async fn get_settings(State(state): State<AppState>) -> Json<SettingsResp> {
    Json(settings_resp(&state).await)
}
