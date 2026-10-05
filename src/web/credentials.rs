//! 凭证管理接口：列表、设备 / 会话绑定、删除与逐项 / 批量设置。

use super::*;

// ---------- 凭证管理 ----------

/// 列出全部凭证（token 已脱敏）。
pub(super) async fn list_credentials(
    State(state): State<AppState>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    blocking(move || credential_views(&state)).await
}

/// [`list_credentials`] 的同步部分：十几条聚合查询，须在阻塞线程池里跑，见 [`blocking`]。
fn credential_views(state: &AppState) -> Result<Json<Vec<CredentialView>>, ApiError> {
    let list = state.store.list().map_err(internal)?;
    let counts = state.store.device_counts().map_err(internal)?;
    let session_counts = state.store.session_counts().map_err(internal)?;
    let quotas = state.store.latest_quotas().map_err(internal)?;
    let last_used = state.store.last_used().map_err(internal)?;
    let costs = state.store.cost_by_cred().map_err(internal)?;
    let rpm = state.store.recent_rpm().map_err(internal)?;
    let mut denials = state.store.all_model_denials().map_err(internal)?;
    let bans = state.store.ban_counts().map_err(internal)?;
    let defaults = DefaultLimits::of(&state.store);
    let proxy_ids = saved_proxy_ids(state)?;
    let views = list
        .iter()
        .map(|c| {
            CredentialView::new(
                c,
                counts.get(&c.id).copied().unwrap_or(0),
                session_counts.get(&c.id).copied().unwrap_or(0),
                defaults,
            )
            .with_ban_count(bans.get(&c.id).copied().unwrap_or(0))
            .with_proxy_ids(&proxy_ids)
            .with_cooldown(
                state.store.rate_limited_secs(c.id),
                state.store.rate_limited_models(c.id),
            )
            .with_denials(denials.remove(&c.id).unwrap_or_default())
            .with_stats(
                quotas.get(&c.id).cloned(),
                last_used.get(&c.id).copied(),
                costs.get(&c.id).copied().unwrap_or(0.0),
                // 窗口内一条流水都没有的账号不在 map 里，就是 0 RPM。
                rpm.get(&c.id).copied().unwrap_or(0),
            )
        })
        .collect();
    Ok(Json(views))
}

/// 代理池 URL → id。
fn saved_proxy_ids(state: &AppState) -> Result<std::collections::HashMap<String, i64>, ApiError> {
    Ok(state.store.list_proxies().map_err(internal)?.into_iter().map(|p| (p.url, p.id)).collect())
}

/// 列出某凭证当前绑定的设备明细（按最近活跃倒序）。
///
/// 口径与卡片上的「设备 x/y」一致：只含 TTL 内仍活跃的绑定，所以条数必然等于 x。
pub(super) async fn list_credential_devices(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<store::DeviceBinding>>, ApiError> {
    // 凭证不存在时给 404：否则前端会把「账号已被删掉」显示成「该账号没有设备」。
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let devices = blocking(move || state.store.list_devices(id).map_err(internal)).await?;
    Ok(Json(devices))
}

/// 手动解除某设备与该凭证的绑定，立即腾出一个设备名额。
///
/// 「解绑」不等于「拉黑」：该设备的下一次请求会重新走选号，名额没满时完全可能又落回同一个
/// 账号。要把设备挡在外面得靠设备上限，不是这个接口。
///
/// 绑定不存在同样给 404（而非静默 ok）：明细是前端缓存的，设备可能已被 TTL 回收或已换到别的
/// 账号，静默成功会让人以为解绑生效、实际点了个空。
pub(super) async fn unbind_credential_device(
    State(state): State<AppState>,
    Path((id, device_id)): Path<(i64, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    if !state.store.unbind_device(id, &device_id).map_err(internal)? {
        return Err((
            StatusCode::NOT_FOUND,
            "device binding not found (it may have expired or moved to another credential)".into(),
        ));
    }
    tracing::info!(cred_id = id, device_id = %device_id, "device binding removed manually");
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 列出某凭证当前绑定的**模拟会话**明细（按最近活跃倒序）。
///
/// 口径与卡片上的「会话 x/y」一致：只含 TTL 内仍活跃的绑定。只有走模拟路径、没有设备身份的
/// 来访会出现在这里（键是它自带的会话 id，或缓存前缀加首条用户消息的指纹），见
/// `store::Select::session_key`。
pub(super) async fn list_credential_sessions(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<store::SessionBinding>>, ApiError> {
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let sessions = blocking(move || state.store.list_sessions(id).map_err(internal)).await?;
    Ok(Json(sessions))
}

/// 一条会话的历史事件（最近的在前，最多 100 条，保留 7 天）。事件跨账号：改绑之后记在新账号
/// 上，按会话键查全部列出，路径上的账号只用来判存在。
pub(super) async fn list_session_events(
    State(state): State<AppState>,
    Path((id, session_key)): Path<(i64, String)>,
) -> Result<Json<Vec<store::SessionEvent>>, ApiError> {
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let events =
        blocking(move || state.store.session_events(&session_key).map_err(internal)).await?;
    Ok(Json(events))
}

/// 某账号某槽位（即某个上游会话 id）先后被哪些会话用过：这个槽位上的事件，以及从它离开的
/// 改绑与换槽位，最近的在前。
pub(super) async fn list_slot_events(
    State(state): State<AppState>,
    Path((id, slot)): Path<(i64, i64)>,
) -> Result<Json<Vec<store::SessionEvent>>, ApiError> {
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let events = blocking(move || state.store.slot_events(id, slot).map_err(internal)).await?;
    Ok(Json(events))
}

/// 一键清掉该凭证的全部模拟会话绑定，返回清掉的条数。同样不是拉黑：名额腾出来，下一条
/// 请求照常重新选号（多半又落回这个号）。
pub(super) async fn clear_credential_sessions(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let removed = state.store.unbind_all_sessions(id).map_err(internal)?;
    tracing::info!(cred_id = id, removed, "all session bindings removed manually");
    Ok(Json(serde_json::json!({ "ok": true, "removed": removed })))
}

/// 手动解除某模拟会话与该凭证的绑定，立即腾出一个会话名额。语义同 [`unbind_credential_device`]：
/// 解绑不是拉黑，下一条请求会重新选号。
pub(super) async fn unbind_credential_session(
    State(state): State<AppState>,
    Path((id, session_key)): Path<(i64, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    if !state.store.unbind_session(id, &session_key).map_err(internal)? {
        return Err((
            StatusCode::NOT_FOUND,
            "session binding not found (it may have expired or moved to another credential)".into(),
        ));
    }
    tracing::info!(cred_id = id, session_key = %session_key, "session binding removed manually");
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 删除一条凭证。用量流水不删，随保留期自然裁掉，理由见 [`CredentialStore::remove`]。
pub(super) async fn delete_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = state.store.clone();
    let removed = blocking(move || store.remove(&[id]).map_err(internal)).await?;
    if removed == 0 {
        return Err(not_found());
    }
    tracing::info!(cred_id = id, "credential deleted");
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(super) struct SetDisabledReq {
    disabled: bool,
}

/// 启用/停用一条凭证。
pub(super) async fn set_disabled(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetDisabledReq>,
) -> Result<Json<CredentialView>, ApiError> {
    if !state.store.set_disabled(id, req.disabled).map_err(internal)? {
        return Err(not_found());
    }
    view_of(&state, id).await
}

#[derive(Deserialize)]
pub(super) struct SetPriorityReq {
    priority: i64,
}

/// 设置优先级。
pub(super) async fn set_priority(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetPriorityReq>,
) -> Result<Json<CredentialView>, ApiError> {
    check_priority(req.priority)?;
    if !state.store.set_priority(id, req.priority).map_err(internal)? {
        return Err(not_found());
    }
    view_of(&state, id).await
}

/// 优先级只收 [`PRIORITY_MIN`]..=[`PRIORITY_MAX`]，越界直接拒，不悄悄截断。
fn check_priority(priority: i64) -> Result<(), ApiError> {
    if !(PRIORITY_MIN..=PRIORITY_MAX).contains(&priority) {
        return Err(bad_request(format!(
            "priority must be between {PRIORITY_MIN} and {PRIORITY_MAX}"
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct SetPrioritiesReq {
    /// 待调整的账号 id 列表。
    ids: Vec<i64>,
    /// 统一设置的优先级（数值小者优先）。与 `delta` 二选一。
    priority: Option<i64>,
    /// 各自升降的档数（负数 = 提高），越界截到边界。与 `priority` 二选一。
    delta: Option<i64>,
}

/// 批量调整优先级：统一调到同一档（`priority`），或各自升降若干档（`delta`，保留
/// 选中账号之间的先后顺序，碰到 P0/P4 的截住）。返回更新后的整份列表。
pub(super) async fn set_priorities(
    State(state): State<AppState>,
    Json(req): Json<SetPrioritiesReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    match (req.priority, req.delta) {
        (Some(priority), None) => {
            check_priority(priority)?;
            let n = state.store.set_priorities(&req.ids, priority).map_err(internal)?;
            tracing::info!(count = n, priority, "priority set in bulk");
        }
        (None, Some(delta)) => {
            let span = PRIORITY_MAX - PRIORITY_MIN;
            if delta == 0 || !(-span..=span).contains(&delta) {
                return Err(bad_request(format!("delta must be non-zero and within ±{span}")));
            }
            let n = state.store.shift_priorities(&req.ids, delta).map_err(internal)?;
            tracing::info!(count = n, delta, "priority shifted in bulk");
        }
        _ => return Err(bad_request("provide exactly one of priority or delta")),
    }
    list_credentials(State(state)).await
}

/// 批量操作的公共入参：待处理的账号 id 列表。
#[derive(Deserialize)]
pub(super) struct IdsReq {
    ids: Vec<i64>,
}

/// 校验批量入参并返回 id 列表；空列表视为客户端错误而非静默 no-op。
pub(super) fn check_ids(ids: &[i64]) -> Result<(), ApiError> {
    if ids.is_empty() {
        return Err(bad_request("select at least one credential"));
    }
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct SetDeviceLimitsReq {
    ids: Vec<i64>,
    /// 三态同单账号接口：`> 0` 独立上限；`0` 跟随全局默认；`< 0` 明确不限。
    device_limit: i64,
}

/// 批量设置设备数上限，返回更新后的整份列表。
pub(super) async fn set_device_limits(
    State(state): State<AppState>,
    Json(req): Json<SetDeviceLimitsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    // 负值统一收敛为 -1，与单账号接口保持一致。
    let limit = if req.device_limit < 0 { -1 } else { req.device_limit };
    let n = state.store.set_device_limits(&req.ids, limit).map_err(internal)?;
    tracing::info!(count = n, device_limit = limit, "device limit set in bulk");
    list_credentials(State(state)).await
}

#[derive(Deserialize)]
pub(super) struct SetSessionLimitsReq {
    ids: Vec<i64>,
    /// 三态同单账号接口：`> 0` 独立上限；`0` 跟随全局默认；`< 0` 明确不限。
    session_limit: i64,
}

/// 批量设置模拟会话数上限，返回更新后的整份列表。
pub(super) async fn set_session_limits(
    State(state): State<AppState>,
    Json(req): Json<SetSessionLimitsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let limit = if req.session_limit < 0 { -1 } else { req.session_limit };
    let n = state.store.set_session_limits(&req.ids, limit).map_err(internal)?;
    tracing::info!(count = n, session_limit = limit, "session limit set in bulk");
    list_credentials(State(state)).await
}

#[derive(Deserialize)]
pub(super) struct SetRpmLimitsReq {
    ids: Vec<i64>,
    /// 三态同单账号接口：`> 0` 独立上限；`0` 跟随全局默认；`< 0` 明确不限。
    rpm_limit: i64,
}

/// 批量设置账号 RPM 上限，返回更新后的整份列表。
pub(super) async fn set_rpm_limits(
    State(state): State<AppState>,
    Json(req): Json<SetRpmLimitsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let limit = if req.rpm_limit < 0 { -1 } else { req.rpm_limit };
    let n = state.store.set_rpm_limits(&req.ids, limit).map_err(internal)?;
    tracing::info!(count = n, rpm_limit = limit, "rpm limit set in bulk");
    list_credentials(State(state)).await
}

#[derive(Deserialize)]
pub(super) struct SetQuotaPausePctsManyReq {
    ids: Vec<i64>,
    /// 两档三态同单账号接口 [`SetCredentialQuotaPausePctReq`]：`null` 跟随全局、`0` 不停、
    /// `1..=100` 独立阈值；整份覆盖，两档都要传。
    #[serde(default)]
    quota_pause_pct: Option<i64>,
    #[serde(default)]
    quota_pause_pct_7d: Option<i64>,
}

/// 批量设置账号自己的提前停调度阈值，返回更新后的整份列表。
pub(super) async fn set_quota_pause_pcts_many(
    State(state): State<AppState>,
    Json(req): Json<SetQuotaPausePctsManyReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let short = req.quota_pause_pct.map(|p| p.clamp(0, 100));
    let long = req.quota_pause_pct_7d.map(|p| p.clamp(0, 100));
    let n = state.store.set_quota_pause_pcts_many(&req.ids, short, long).map_err(internal)?;
    tracing::info!(
        count = n,
        quota_pause_pct = ?short,
        quota_pause_pct_7d = ?long,
        "per-account quota-threshold pause set in bulk"
    );
    list_credentials(State(state)).await
}

#[derive(Deserialize)]
pub(super) struct SetDisabledManyReq {
    ids: Vec<i64>,
    disabled: bool,
}

/// 批量启用/停用，返回更新后的整份列表。
pub(super) async fn set_disabled_many(
    State(state): State<AppState>,
    Json(req): Json<SetDisabledManyReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let n = state.store.set_disabled_many(&req.ids, req.disabled).map_err(internal)?;
    tracing::info!(count = n, disabled = req.disabled, "enabled/disabled in bulk");
    list_credentials(State(state)).await
}

/// 批量删除（连带清设备绑定；用量流水不删，随保留期自然裁掉），返回删除后的整份列表。
///
/// 用 POST 而非 DELETE：带请求体的 DELETE 在部分代理/客户端上会被丢掉 body。
pub(super) async fn delete_credentials(
    State(state): State<AppState>,
    Json(req): Json<IdsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let store = state.store.clone();
    let n = blocking(move || store.remove(&req.ids).map_err(internal)).await?;
    tracing::info!(count = n, "credentials deleted in bulk");
    list_credentials(State(state)).await
}

#[derive(Deserialize)]
pub(super) struct SetLabelReq {
    label: String,
}

/// 重命名。
pub(super) async fn set_label(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetLabelReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let label = req.label.trim();
    if label.is_empty() {
        return Err(bad_request("the name must not be empty"));
    }
    if !state.store.set_label(id, label).map_err(internal)? {
        return Err(not_found());
    }
    view_of(&state, id).await
}

#[derive(Deserialize)]
pub(super) struct SetDeviceLimitReq {
    /// 设备数上限三态：`> 0` 本账号独立上限；`0` 跟随全局默认；`< 0` 本账号明确不限。
    device_limit: i64,
}

/// 设置设备数上限。
pub(super) async fn set_device_limit(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetDeviceLimitReq>,
) -> Result<Json<CredentialView>, ApiError> {
    // 负值统一收敛为 -1，避免库里出现各式各样的“不限”取值。
    let limit = if req.device_limit < 0 { -1 } else { req.device_limit };
    if !state.store.set_device_limit(id, limit).map_err(internal)? {
        return Err(not_found());
    }
    view_of(&state, id).await
}

#[derive(Deserialize)]
pub(super) struct SetSessionLimitReq {
    /// 模拟会话数上限三态：`> 0` 本账号独立上限；`0` 跟随全局默认；`< 0` 本账号明确不限。
    session_limit: i64,
}

/// 设置模拟会话数上限。
pub(super) async fn set_session_limit(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetSessionLimitReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let limit = if req.session_limit < 0 { -1 } else { req.session_limit };
    if !state.store.set_session_limit(id, limit).map_err(internal)? {
        return Err(not_found());
    }
    view_of(&state, id).await
}

#[derive(Deserialize)]
pub(super) struct SetRpmLimitReq {
    /// RPM 上限三态：`> 0` 本账号独立上限；`0` 跟随全局默认；`< 0` 本账号明确不限。
    rpm_limit: i64,
}

/// 设置该账号每分钟最多转发多少条请求。
///
/// 计数在进程内存里，改完即时生效；已经记在窗口里的那些不会因为调高上限而消失，
/// 也不会因为调低而被追认——只影响之后的判定。
pub(super) async fn set_rpm_limit(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetRpmLimitReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let limit = if req.rpm_limit < 0 { -1 } else { req.rpm_limit };
    if !state.store.set_rpm_limit(id, limit).map_err(internal)? {
        return Err(not_found());
    }
    tracing::info!(cred_id = id, rpm_limit = limit, "rpm limit set");
    view_of(&state, id).await
}

#[derive(Deserialize)]
pub(super) struct SetCredentialQuotaPausePctReq {
    /// **5h 窗口**这一档：`null`（或不传）= 跟随全局；`0` = 本账号这一档不停；`1..=100` =
    /// 本账号独立阈值。后端夹到 0~100。
    #[serde(default)]
    quota_pause_pct: Option<i64>,
    /// **7d 窗口**那一档，同上。两档**都要传**：这是整份覆盖，不传即视为「跟随全局」——
    /// 与全局那个接口「不传 = 保持现值」的约定不同，因为这里的三态里「跟随」本身就是 null。
    #[serde(default)]
    quota_pause_pct_7d: Option<i64>,
}

/// 设置该账号自己的「额度用到多少就提前停调度」阈值，覆盖全局的
/// [`crate::store::QUOTA_PAUSE_PCT`] / [`crate::store::QUOTA_PAUSE_PCT_7D`]。判定见
/// [`crate::proxy::park_if_quota_nearly_exhausted`]，下一条带限流头的响应起生效；已经按
/// 旧阈值停下的号不会因为调高阈值而自动回池——到点自恢复、手动启用或连通性测试照旧。
pub(super) async fn set_credential_quota_pause_pct(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetCredentialQuotaPausePctReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let short = req.quota_pause_pct.map(|p| p.clamp(0, 100));
    let long = req.quota_pause_pct_7d.map(|p| p.clamp(0, 100));
    if !state.store.set_quota_pause_pcts(id, short, long).map_err(internal)? {
        return Err(not_found());
    }
    tracing::info!(
        cred_id = id,
        quota_pause_pct = ?short,
        quota_pause_pct_7d = ?long,
        "per-account quota-threshold pause changed"
    );
    view_of(&state, id).await
}

#[derive(Deserialize)]
pub(super) struct SetProxyReq {
    /// 代理 URL；`null` 或空串表示清除（改回直连）。
    proxy: Option<String>,
}

/// 设置/清除某个账号专用的出站代理。
///
/// 配好之后这个号的**全部**出站流量都走它：转发、token 刷新、profile、连通性测试。
/// 校验放在入库之前——存进去一条建不出客户端的代理，故障要等到下次真有请求选中这个号
/// 才暴露，那时现场只剩一条「这个号所有请求都失败」。
pub(super) async fn set_proxy(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetProxyReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    let proxy = match req.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            Some(crate::clients::validate_proxy(raw).map_err(|e| bad_request(format!("{e:#}")))?)
        }
        None => None,
    };
    if !state.store.set_proxy(id, proxy.as_deref()).map_err(internal)? {
        return Err(not_found());
    }
    // 丢掉旧代理那份缓存客户端，否则它的连接池还会继续把请求送去老代理——改完之后
    // 「看着已经换了、实际还在走旧的」是这类缓存最典型的坑。
    if let Some(old) = cred.proxy.as_deref() {
        state.clients.forget(old);
    }
    // 新代理自动入池：下次其他账号能直接从池里选，不必再手打。
    if let Some(ref url) = proxy {
        state.store.ensure_proxy_in_pool(url);
    }
    tracing::info!(
        cred_id = id, cred = %cred.label,
        proxy = %proxy.as_deref().unwrap_or("<direct>"),
        "credential proxy updated"
    );
    view_of(&state, id).await
}

/// 读取单条并转为脱敏视图（含已绑定设备数）。额度与 RPM 读的是只读连接，故经 [`blocking`]。
pub(super) async fn view_of(state: &AppState, id: i64) -> Result<Json<CredentialView>, ApiError> {
    let state = state.clone();
    blocking(move || credential_view(&state, id)).await
}

/// [`view_of`] 的同步部分。
pub(super) fn credential_view(state: &AppState, id: i64) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    let count = state.store.device_count(id).map_err(internal)?;
    let session_count = state.store.session_count(id).map_err(internal)?;
    // 单账号视图只查这一个 id：此前调的是三个「全库聚合」再 remove 一条，改一次开关就要把
    // usage_logs 整表聚合三遍。
    let quota = state.store.latest_quota(id).map_err(internal)?;
    let last_used = state.store.last_used_at(id).map_err(internal)?;
    let cost_total = state.store.cost_of(id).map_err(internal)?;
    let rpm = state.store.recent_rpm_of(id).map_err(internal)?;
    let denials = state.store.denied_models(id).map_err(internal)?;
    Ok(Json(
        CredentialView::new(&cred, count, session_count, DefaultLimits::of(&state.store))
            .with_proxy_ids(&saved_proxy_ids(state)?)
            .with_cooldown(
                state.store.rate_limited_secs(cred.id),
                state.store.rate_limited_models(cred.id),
            )
            .with_denials(denials)
            .with_stats(quota, last_used, cost_total, rpm),
    ))
}
