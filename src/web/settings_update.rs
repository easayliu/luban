//! 接入设置的逐项写入接口。

use super::*;

/// 写一项非负整数设置（负数按 0 存，0 一律表示「不限 / 永不过期」），返回实际写入的值。
async fn save_nonneg(state: &AppState, key: &str, value: i64) -> Result<i64, ApiError> {
    let value = value.max(0);
    state.store.set_setting(key, &value.to_string()).await.map_err(internal)?;
    Ok(value)
}

#[derive(Deserialize)]
pub(super) struct SetDeviceTtlReq {
    /// 设备绑定有效期（秒）；0（或负数）表示永不过期。
    device_binding_ttl_secs: i64,
}

/// 设置设备绑定有效期（秒）。
pub(super) async fn set_device_ttl(
    State(state): State<AppState>,
    Json(req): Json<SetDeviceTtlReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    save_nonneg(&state, crate::store::DEVICE_BINDING_TTL, req.device_binding_ttl_secs).await?;
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetDeviceRetentionReq {
    /// 软绑定保留期（秒）；0（或负数）表示永久保留。
    device_binding_retention_secs: i64,
}

/// 设置软绑定保留期（秒）。
///
/// 不在这里校验「必须 >= 有效期」：两个设置各存各的，比较放在选路时做
/// （见 [`crate::store::effective_retention`]），免得改动顺序还得先改大的那个。
pub(super) async fn set_device_retention(
    State(state): State<AppState>,
    Json(req): Json<SetDeviceRetentionReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    save_nonneg(&state, crate::store::DEVICE_BINDING_RETENTION, req.device_binding_retention_secs)
        .await?;
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetSessionTtlReq {
    /// 模拟会话绑定有效期（秒）；0（或负数）表示永不过期。
    session_binding_ttl_secs: i64,
}

/// 设置模拟会话绑定有效期（秒）。
pub(super) async fn set_session_ttl(
    State(state): State<AppState>,
    Json(req): Json<SetSessionTtlReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    save_nonneg(&state, crate::store::SESSION_BINDING_TTL, req.session_binding_ttl_secs).await?;
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetSessionRetentionReq {
    /// 模拟会话软绑定保留期（秒）；0（或负数）表示永久保留。
    session_binding_retention_secs: i64,
}

/// 设置模拟会话软绑定保留期（秒）；与设备那条一样，不在这里校验「必须 >= 有效期」。
pub(super) async fn set_session_retention(
    State(state): State<AppState>,
    Json(req): Json<SetSessionRetentionReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    save_nonneg(
        &state,
        crate::store::SESSION_BINDING_RETENTION,
        req.session_binding_retention_secs,
    )
    .await?;
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetDefaultDeviceLimitReq {
    /// 全局默认设备数上限；0（或负数）表示默认不限。
    default_device_limit: i64,
}

/// 设置全局默认设备数上限（账号自身未单独配置时生效）。
pub(super) async fn set_default_device_limit(
    State(state): State<AppState>,
    Json(req): Json<SetDefaultDeviceLimitReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    save_nonneg(&state, crate::store::DEFAULT_DEVICE_LIMIT, req.default_device_limit).await?;
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetDefaultSessionLimitReq {
    /// 全局默认模拟会话数上限；0（或负数）表示默认不限。
    default_session_limit: i64,
}

/// 设置全局默认模拟会话数上限（账号自身未单独配置时生效）。
pub(super) async fn set_default_session_limit(
    State(state): State<AppState>,
    Json(req): Json<SetDefaultSessionLimitReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    save_nonneg(&state, crate::store::DEFAULT_SESSION_LIMIT, req.default_session_limit).await?;
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetDefaultRpmLimitReq {
    /// 全局默认账号 RPM 上限；0（或负数）表示默认不限。
    default_rpm_limit: i64,
}

/// 设置全局默认账号 RPM 上限（账号自身未单独配置时生效）。
pub(super) async fn set_default_rpm_limit(
    State(state): State<AppState>,
    Json(req): Json<SetDefaultRpmLimitReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let limit = save_nonneg(&state, crate::store::DEFAULT_RPM_LIMIT, req.default_rpm_limit).await?;
    tracing::info!(limit, "default rpm limit changed");
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetDeviceRpmLimitReq {
    /// 每设备 RPM 上限；0（或负数）表示不限。
    device_rpm_limit: i64,
}

/// 设置每设备 RPM 上限：单台设备最近 60 秒最多转发多少条，超了直接 429，不换号。
///
/// 与账号 RPM 各算各的：一条请求先过设备这道闸，再在选号时过账号那道。
pub(super) async fn set_device_rpm_limit(
    State(state): State<AppState>,
    Json(req): Json<SetDeviceRpmLimitReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let limit = save_nonneg(&state, crate::store::DEVICE_RPM_LIMIT, req.device_rpm_limit).await?;
    tracing::info!(limit, "per-device rpm limit changed");
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetSessionRpmLimitReq {
    /// 每会话 RPM 上限；0（或负数）表示不限。
    session_rpm_limit: i64,
}

/// 设置每会话 RPM 上限：单个会话最近 60 秒最多转发多少条，超了直接 429，不换号。
///
/// 与设备 RPM 是同一件事的两个粒度，两道都要配（只配一边各有各的口子，见
/// [`crate::store::SESSION_RPM_LIMIT`]）。这里只落设置，不去校正「会话上限比设备上限还大」
/// 这类配法：那是运维的判断，代替他改数字比让他看见自己配了什么更糟。
pub(super) async fn set_session_rpm_limit(
    State(state): State<AppState>,
    Json(req): Json<SetSessionRpmLimitReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let limit = save_nonneg(&state, crate::store::SESSION_RPM_LIMIT, req.session_rpm_limit).await?;
    tracing::info!(limit, "per-session rpm limit changed");
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetSessionConcurrencyLimitReq {
    session_concurrency_limit: i64,
}

pub(super) async fn set_session_concurrency_limit(
    State(state): State<AppState>,
    Json(req): Json<SetSessionConcurrencyLimitReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let limit =
        save_nonneg(&state, crate::store::SESSION_CONCURRENCY_LIMIT, req.session_concurrency_limit)
            .await?;
    tracing::info!(limit, "per-session concurrency limit changed");
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetBareRateLimitReq {
    /// 单凭证在窗口内允许的裸请求条数；0（或负数）表示不限。
    bare_rate_limit: i64,
    /// 窗口秒数；缺省或 `<= 0` 时保持现值不动（不因为改上限就把窗口重置成默认值）。
    bare_rate_window_secs: Option<i64>,
}

/// 设置裸请求速率上限（每个凭证各算各的，只统计无 `metadata.user_id` 的请求）。
///
/// 计数在进程内存里，改上限即时生效；窗口只在显式给出正数时才写，避免前端只想调上限却把
/// 窗口顺手清成默认值。
pub(super) async fn set_bare_rate_limit(
    State(state): State<AppState>,
    Json(req): Json<SetBareRateLimitReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let limit = req.bare_rate_limit.max(0);
    state
        .store
        .set_setting(crate::store::BARE_RATE_LIMIT, &limit.to_string())
        .await
        .map_err(internal)?;
    if let Some(window) = req.bare_rate_window_secs.filter(|w| *w > 0) {
        state
            .store
            .set_setting(crate::store::BARE_RATE_WINDOW_SECS, &window.to_string())
            .await
            .map_err(internal)?;
    }
    tracing::info!(limit, window = ?req.bare_rate_window_secs, "bare-request rate limit changed");
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetRateLimitRetryMaxReq {
    /// 上游 429 时最多换几个号重试；0 表示不重试，上限由后端夹到 10。
    rate_limit_retry_max: i64,
}

/// 设置上游 429 的换号重试次数（开关另见转发形态里的 `rate_limit_retry`）。
pub(super) async fn set_rate_limit_retry_max(
    State(state): State<AppState>,
    Json(req): Json<SetRateLimitRetryMaxReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let n = req.rate_limit_retry_max.clamp(0, 10);
    state
        .store
        .set_setting(crate::store::RATE_LIMIT_RETRY_MAX, &n.to_string())
        .await
        .map_err(internal)?;
    tracing::info!(retry_max = n, "upstream-429 credential-swap retry cap changed");
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetQuotaPausePctReq {
    /// **5h 窗口**的阈值：使用率到多少百分比就提前停调度；0 表示关闭（收到 429 才停），
    /// 后端夹到 0~100。
    quota_pause_pct: i64,
    /// **7d 窗口**的阈值，另算；0 = 不按周用量停号。不传 = 保持现值（前端只改一档时不必
    /// 回传另一档，与裸请求速率那对字段同一个约定）。
    #[serde(default)]
    quota_pause_pct_7d: Option<i64>,
}

/// 设置「额度用到多少就提前停调度」的阈值，见
/// [`crate::proxy::park_if_quota_nearly_exhausted`]。与 429 冷却同受转发形态里的
/// `rate_limit_retry` 总开关。
///
/// 5h 与 7d 是**两档**、各存各的：同一个百分比在两个窗口上的后果差着数量级，7d 那档默认关
/// （见 [`crate::store::QUOTA_PAUSE_PCT_7D`]）。
pub(super) async fn set_quota_pause_pct(
    State(state): State<AppState>,
    Json(req): Json<SetQuotaPausePctReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let pct = req.quota_pause_pct.clamp(0, 100);
    state
        .store
        .set_setting(crate::store::QUOTA_PAUSE_PCT, &pct.to_string())
        .await
        .map_err(internal)?;
    // 7d 那档只在传了的时候写：两档各存各的，前端拨一档不该顺手把另一档也覆盖成默认值。
    let pct_7d = req.quota_pause_pct_7d.map(|p| p.clamp(0, 100));
    if let Some(p) = pct_7d {
        state
            .store
            .set_setting(crate::store::QUOTA_PAUSE_PCT_7D, &p.to_string())
            .await
            .map_err(internal)?;
    }
    tracing::info!(
        quota_pause_pct = pct,
        quota_pause_pct_7d = ?pct_7d,
        "quota-threshold scheduling pause changed"
    );
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetRequireDeviceIdReq {
    /// 是否要求请求携带有效设备身份。
    required: bool,
}

/// 开关设备身份校验：关闭后，无 `metadata.user_id` 的请求不再 403，而是以
/// 「不绑定、不占设备名额」的方式转发（也无法被身份伪装）。
pub(super) async fn set_require_device_id(
    State(state): State<AppState>,
    Json(req): Json<SetRequireDeviceIdReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let value = if req.required { "true" } else { "false" };
    state.store.set_setting(crate::store::REQUIRE_DEVICE_ID, value).await.map_err(internal)?;
    tracing::info!(required = req.required, "device identity check toggled");
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetMinClientVersionReq {
    /// 最低 Claude Code 客户端版本（`2.1.220`、`2.1`、`2` 都收）；空串表示不限。
    min_client_version: String,
}

/// 设置最低客户端版本闸：UA 自报 `claude-cli/<版本>` 且低于此值的请求直接 403。
///
/// 只收能解析的版本串——写错一个字（`v2.1`、`最新版`）在代理侧会被当成「没配」而静默放行，
/// 那时网页上明明写着一个值、闸却没开，是最难查的一种。故在入口处直接回 400。
pub(super) async fn set_min_client_version(
    State(state): State<AppState>,
    Json(req): Json<SetMinClientVersionReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let version = req.min_client_version.trim();
    if version.is_empty() {
        state.store.delete_setting(crate::store::MIN_CLIENT_VERSION).await.map_err(internal)?;
        tracing::info!("minimum client version cleared");
        return Ok(Json(settings_resp(&state).await));
    }
    if crate::proxy::parse_version(version).is_none() {
        return Err(bad_request(
            "the minimum client version must look like 2.1.220 (2 and 2.1 are accepted too)",
        ));
    }
    state.store.set_setting(crate::store::MIN_CLIENT_VERSION, version).await.map_err(internal)?;
    tracing::info!(version, "minimum client version changed");
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetLatestCcReleaseReq {
    /// 官方最新 Claude Code 版本，严格 `主.次.修`；空串表示删掉手动/学到的值，等下次自动学。
    latest_cc_release: String,
}

/// 手动指定（或删掉）已知的官方最新 Claude Code 版本。
///
/// 场景：官方刚发了新版、保活还没轮到下一次检查，而用新版的真实客户端已经在被判成冒充；
/// 或 `downloads.claude.ai` 从某个出口拉不到。填进去立刻生效，之后自动检查学到更新的照样
/// 覆盖（只升不降）。删掉就退回写死的基线，等下一次检查再学。
///
/// 只收严格三段数字：这是官方发布清单的形态，写成 `2.1` 在这里没有含义。库先写、缓存后同步，
/// 与 [`sync_latest_release_from_store`] 的口径一致。
pub(super) async fn set_latest_cc_release(
    State(state): State<AppState>,
    Json(req): Json<SetLatestCcReleaseReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let version = req.latest_cc_release.trim();
    // 写库放进缓存的串行锁里（闭包），别先写库再同步：后台一笔迟到的落库会把这里刚写的盖掉。
    if version.is_empty() {
        oauth::LATEST_RELEASE
            .sync_from_store(async {
                state.store.delete_setting(store::LATEST_CC_RELEASE).await?;
                Ok::<_, anyhow::Error>(None)
            })
            .await
            .map_err(internal)?;
        tracing::info!("latest Claude Code release cleared; will relearn on the next check");
        return Ok(Json(settings_resp(&state).await));
    }
    let Some(v) = oauth::parse_release_body(version) else {
        return Err(bad_request("the latest Claude Code release must look like 2.1.260"));
    };
    oauth::LATEST_RELEASE
        .sync_from_store(async {
            state.store.set_setting(store::LATEST_CC_RELEASE, &oauth::release_string(v)).await?;
            Ok::<_, anyhow::Error>(Some(v))
        })
        .await
        .map_err(internal)?;
    tracing::info!(version = %oauth::release_string(v), "latest Claude Code release set manually");
    Ok(Json(settings_resp(&state).await))
}

#[derive(Deserialize)]
pub(super) struct SetOAuthScopesReq {
    /// 登录时申请的 scope（空格分隔）；空串表示回到默认的 [`crate::config::SCOPES`]。
    oauth_scopes: String,
}

/// 设置登录时申请的 OAuth scope。
///
/// 只影响之后新加的账号：已存下来的凭证按当初授权的范围来。刷新发的是固定的
/// [`crate::config::REFRESH_SCOPES`]（官方那五项），不读这一项——所以选了精简 scope 的号
/// 会在第一次刷新后被扩回五项。
///
/// **不校验**，写什么存什么、原样发给上游（只做空白规整与去重，见
/// [`crate::config::normalize_scopes`]）。这个框存在的意义就是拿来试上游认哪些 scope，
/// 在这里替它判合法性只会把探边界的值挡在外面；真不认时同意页会明说（如
/// `Missing scope parameter`），那比我们猜的白名单准。
pub(super) async fn set_oauth_scopes(
    State(state): State<AppState>,
    Json(req): Json<SetOAuthScopesReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let scopes = crate::config::normalize_scopes(&req.oauth_scopes);
    if scopes.is_empty() {
        state.store.delete_setting(crate::store::OAUTH_SCOPES).await.map_err(internal)?;
        tracing::info!(scopes = %crate::config::SCOPES, "oauth scopes reset to the default");
        return Ok(Json(settings_resp(&state).await));
    }
    state.store.set_setting(crate::store::OAUTH_SCOPES, &scopes).await.map_err(internal)?;
    tracing::info!(scopes = %scopes, "oauth scopes changed");
    Ok(Json(settings_resp(&state).await))
}

/// 转发形态开关的改动请求：**只有出现的字段会被写入**，其余保持原值。
/// 前端每次拨一个开关就只带那一个字段，不必回传全量、也不会互相覆盖。
#[derive(Deserialize)]
pub(super) struct SetForwardingReq {
    spoof_identity: Option<bool>,
    spoof_device_id: Option<bool>,
    normalize_device_fp: Option<bool>,
    billing_cch: Option<bool>,
    cch_real_recompute: Option<bool>,
    cch_sim_compute: Option<bool>,
    fill_client_headers: Option<bool>,
    merge_beta: Option<bool>,
    system_shape: Option<bool>,
    orig_header_case: Option<bool>,
    thinking_signature_retry: Option<bool>,
    redacted_thinking_retry: Option<bool>,
    simulate_cc: Option<bool>,
    simulate_full_system: Option<bool>,
    fill_absent_tools: Option<bool>,
    sim_trim_tools: Option<bool>,
    sim_billing_only: Option<bool>,
    sim_billing_keep_user_id: Option<bool>,
    real_billing_keep_user_id: Option<bool>,
    sim_message_threads: Option<bool>,
    fill_metadata: Option<bool>,
    rate_limit_retry: Option<bool>,
    cache_scope_global: Option<bool>,
    cache_ttl_1h: Option<bool>,
    eager_tool_streaming: Option<bool>,
    nonstream_as_sse: Option<bool>,
    strip_extra_fields: Option<bool>,
    tool_name_mimic: Option<bool>,
    inject_thinking: Option<bool>,
    reject_openai_shape: Option<bool>,
    reject_session_conflict: Option<bool>,
    reject_probes: Option<bool>,
    reject_probes_strict: Option<bool>,
    reject_refusals: Option<bool>,
    reject_empty_replies: Option<bool>,
    reject_learned_shapes: Option<bool>,
    api_telemetry: Option<bool>,
    keepalive_telemetry: Option<bool>,
    fable_refusal_fallback: Option<bool>,
    opus_refusal_fallback: Option<bool>,
}

/// 逐项开关转发形态改动。全关即「零改写直接转发」——实测上游唯一必需的是注入
/// `Authorization`，这些开关都只影响与官方客户端的形态贴合度，见
/// [`crate::store::ForwardFlags`]。
pub(super) async fn set_forwarding(
    State(state): State<AppState>,
    Json(req): Json<SetForwardingReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    use crate::store::{
        API_TELEMETRY, CCH_REAL_RECOMPUTE, CCH_SIM_COMPUTE, EAGER_TOOL_STREAMING,
        FABLE_REFUSAL_FALLBACK, FILL_ABSENT_TOOLS, FILL_CLIENT_HEADERS, FILL_METADATA,
        INJECT_THINKING, KEEPALIVE_TELEMETRY, MERGE_BETA, NONSTREAM_AS_SSE, NORMALIZE_DEVICE_FP,
        OPUS_REFUSAL_FALLBACK, ORIG_HEADER_CASE, RATE_LIMIT_RETRY, REAL_BILLING_KEEP_USER_ID,
        REDACTED_THINKING_RETRY, REJECT_EMPTY_REPLIES, REJECT_LEARNED_SHAPES, REJECT_OPENAI_SHAPE,
        REJECT_PROBES, REJECT_PROBES_STRICT, REJECT_REFUSALS, REJECT_SESSION_CONFLICT,
        SIM_BILLING_KEEP_USER_ID, SIM_BILLING_ONLY, SIM_MESSAGE_THREADS, SIM_TRIM_TOOLS,
        SIMULATE_CC, SIMULATE_FULL_SYSTEM, SPOOF_BILLING_CCH, SPOOF_DEVICE_ID,
        SPOOF_IDENTITY_ENABLED, STRIP_EXTRA_FIELDS, SYSTEM_CACHE_SCOPE, SYSTEM_CACHE_TTL,
        SYSTEM_SHAPE, THINKING_SIGNATURE_RETRY, TOOL_NAME_MIMIC,
    };
    let items = [
        (SPOOF_IDENTITY_ENABLED, req.spoof_identity),
        (SPOOF_DEVICE_ID, req.spoof_device_id),
        (NORMALIZE_DEVICE_FP, req.normalize_device_fp),
        (SPOOF_BILLING_CCH, req.billing_cch),
        (CCH_REAL_RECOMPUTE, req.cch_real_recompute),
        (CCH_SIM_COMPUTE, req.cch_sim_compute),
        (FILL_CLIENT_HEADERS, req.fill_client_headers),
        (MERGE_BETA, req.merge_beta),
        (SYSTEM_SHAPE, req.system_shape),
        (ORIG_HEADER_CASE, req.orig_header_case),
        (THINKING_SIGNATURE_RETRY, req.thinking_signature_retry),
        (REDACTED_THINKING_RETRY, req.redacted_thinking_retry),
        (SIMULATE_CC, req.simulate_cc),
        (SIMULATE_FULL_SYSTEM, req.simulate_full_system),
        (FILL_ABSENT_TOOLS, req.fill_absent_tools),
        (SIM_TRIM_TOOLS, req.sim_trim_tools),
        (SIM_BILLING_ONLY, req.sim_billing_only),
        (SIM_BILLING_KEEP_USER_ID, req.sim_billing_keep_user_id),
        (REAL_BILLING_KEEP_USER_ID, req.real_billing_keep_user_id),
        (SIM_MESSAGE_THREADS, req.sim_message_threads),
        (FILL_METADATA, req.fill_metadata),
        (RATE_LIMIT_RETRY, req.rate_limit_retry),
        (SYSTEM_CACHE_SCOPE, req.cache_scope_global),
        (SYSTEM_CACHE_TTL, req.cache_ttl_1h),
        (EAGER_TOOL_STREAMING, req.eager_tool_streaming),
        (NONSTREAM_AS_SSE, req.nonstream_as_sse),
        (STRIP_EXTRA_FIELDS, req.strip_extra_fields),
        (TOOL_NAME_MIMIC, req.tool_name_mimic),
        (INJECT_THINKING, req.inject_thinking),
        (REJECT_OPENAI_SHAPE, req.reject_openai_shape),
        (REJECT_SESSION_CONFLICT, req.reject_session_conflict),
        (REJECT_PROBES, req.reject_probes),
        (REJECT_PROBES_STRICT, req.reject_probes_strict),
        (REJECT_REFUSALS, req.reject_refusals),
        (REJECT_EMPTY_REPLIES, req.reject_empty_replies),
        (REJECT_LEARNED_SHAPES, req.reject_learned_shapes),
        (API_TELEMETRY, req.api_telemetry),
        (KEEPALIVE_TELEMETRY, req.keepalive_telemetry),
        (FABLE_REFUSAL_FALLBACK, req.fable_refusal_fallback),
        (OPUS_REFUSAL_FALLBACK, req.opus_refusal_fallback),
    ];
    for (key, value) in items.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))) {
        state
            .store
            .set_setting(key, if value { "true" } else { "false" })
            .await
            .map_err(internal)?;
        tracing::info!(key, enabled = value, "forwarding shape toggle changed");
    }
    Ok(Json(settings_resp(&state).await))
}
