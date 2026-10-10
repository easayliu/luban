//! 凭证管理接口：列表、设备 / 会话绑定、删除与逐项 / 批量设置。

use super::*;

// ---------- 凭证管理 ----------

/// 列出全部凭证（token 已脱敏）。
///
/// 代理和用户只列自己名下的号；admin 与访客看全部，并带上每个号的主人用户名。
pub(super) async fn list_credentials(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    let scope = actor.scope();
    let store = &state.store;
    // 额度快照那条最重（逐号加 7 天窗口内的流水），和其余各项并发跑；其余的都是小表或走索引的
    // 窄查询，照旧逐个跑——全部并发会一下占掉连接池的十几条连接，和转发路径抢。
    let rest = async {
        Ok::<_, anyhow::Error>((
            store.list_scoped(scope).await?,
            match scope {
                Scope::All => Some(store.owner_names().await?),
                Scope::Owner(_) => None,
            },
            store.credential_group_map().await?,
            store.device_counts().await?,
            store.session_counts().await?,
            store.last_used().await?,
            store.cost_by_cred().await?,
            store.recent_rpm().await?,
            store.all_model_denials().await?,
            store.proxy_ids_by_owner().await?,
        ))
    };
    let (quotas, rest) = tokio::try_join!(store.latest_quotas_cached(), rest).map_err(internal)?;
    let (
        list,
        owners,
        mut groups,
        counts,
        session_counts,
        last_used,
        costs,
        rpm,
        mut denials,
        proxy_ids,
    ) = rest;
    let defaults = DefaultLimits::of(&state.store);
    let views = list
        .iter()
        .map(|c| {
            CredentialView::new(
                c,
                counts.get(&c.id).copied().unwrap_or(0),
                session_counts.get(&c.id).copied().unwrap_or(0),
                defaults,
            )
            .with_proxy_ids(&proxy_ids)
            .with_owner_name(owners.as_ref())
            .with_groups(groups.remove(&c.id).unwrap_or_default())
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

/// 代理池 (主人, URL) → id：同一个地址不同的人各存一条，号对应的是它主人池里那条。
async fn saved_proxy_ids(
    state: &AppState,
) -> Result<std::collections::HashMap<(i64, String), i64>, ApiError> {
    state.store.proxy_ids_by_owner().await.map_err(internal)
}

/// 列出某凭证当前绑定的设备明细（按最近活跃倒序）。
///
/// 口径与卡片上的「设备 x/y」一致：只含 TTL 内仍活跃的绑定，所以条数必然等于 x。
pub(super) async fn list_credential_devices(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<store::DeviceBinding>>, ApiError> {
    // 凭证不存在时给 404：否则前端会把「账号已被删掉」显示成「该账号没有设备」。
    if state.store.get(id).await.map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let mut devices = state.store.list_devices(id).await.map_err(internal)?;
    // 代理和用户：全池合计不给，见 [`store::DeviceBinding::cost_usd_all`]。
    if actor.scope().owner().is_some() {
        for d in &mut devices {
            d.cost_usd_all = None;
        }
    }
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
    if state.store.get(id).await.map_err(internal)?.is_none() {
        return Err(not_found());
    }
    if !state.store.unbind_device(id, &device_id).await.map_err(internal)? {
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
    if state.store.get(id).await.map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let sessions = state.store.list_sessions(id).await.map_err(internal)?;
    Ok(Json(sessions))
}

/// 一条会话的历史事件（最近的在前，最多 100 条，保留 7 天）。事件跨账号：改绑之后记在新账号
/// 上，按会话键查全部列出，路径上的账号只用来判存在。
///
/// 代理和用户只看得到落在本人号上的事件，见 [`events_for`]。
pub(super) async fn list_session_events(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, session_key)): Path<(i64, String)>,
) -> Result<Json<Vec<store::SessionEvent>>, ApiError> {
    if state.store.get(id).await.map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let events = state.store.session_events(&session_key).await.map_err(internal)?;
    let events = events_for(&state, actor.scope(), events).await?;
    Ok(Json(events))
}

/// 按可见范围收窄会话事件：事件跨账号（改绑前后记在两个号上），同一个会话可能先后落在
/// 不同人的号上。代理和用户只留至少有一端是本人号的事件，另一端若是别人的号，id 与名称
/// 一并抹掉——只告诉他「换到了别处 / 从别处换来」，不告诉是谁的哪个号。
async fn events_for(
    state: &AppState,
    scope: Scope,
    events: Vec<store::SessionEvent>,
) -> Result<Vec<store::SessionEvent>, ApiError> {
    let Scope::Owner(_) = scope else { return Ok(events) };
    let owned: std::collections::HashSet<i64> =
        state.store.list_scoped(scope).await.map_err(internal)?.iter().map(|c| c.id).collect();
    let mine = |id: Option<i64>| id.is_some_and(|id| owned.contains(&id));
    Ok(events
        .into_iter()
        .filter(|e| mine(e.cred_id) || mine(e.prev_cred_id))
        .map(|mut e| {
            if !mine(e.cred_id) {
                e.cred_id = None;
                e.cred_label = None;
            }
            if !mine(e.prev_cred_id) {
                e.prev_cred_id = None;
                e.prev_cred_label = None;
            }
            e
        })
        .collect())
}

/// 某账号某槽位（即某个上游会话 id）先后被哪些会话用过：这个槽位上的事件，以及从它离开的
/// 改绑与换槽位，最近的在前。
pub(super) async fn list_slot_events(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, slot)): Path<(i64, i64)>,
) -> Result<Json<Vec<store::SessionEvent>>, ApiError> {
    if state.store.get(id).await.map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let events = state.store.slot_events(id, slot).await.map_err(internal)?;
    let events = events_for(&state, actor.scope(), events).await?;
    Ok(Json(events))
}

/// 一键清掉该凭证的全部模拟会话绑定，返回清掉的条数。同样不是拉黑：名额腾出来，下一条
/// 请求照常重新选号（多半又落回这个号）。
pub(super) async fn clear_credential_sessions(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get(id).await.map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let removed = state.store.unbind_all_sessions(id).await.map_err(internal)?;
    tracing::info!(cred_id = id, removed, "all session bindings removed manually");
    Ok(Json(serde_json::json!({ "ok": true, "removed": removed })))
}

/// 手动解除某模拟会话与该凭证的绑定，立即腾出一个会话名额。语义同 [`unbind_credential_device`]：
/// 解绑不是拉黑，下一条请求会重新选号。
pub(super) async fn unbind_credential_session(
    State(state): State<AppState>,
    Path((id, session_key)): Path<(i64, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get(id).await.map_err(internal)?.is_none() {
        return Err(not_found());
    }
    if !state.store.unbind_session(id, &session_key).await.map_err(internal)? {
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
    let removed = store.remove(&[id]).await.map_err(internal)?;
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
    if !state.store.set_disabled(id, req.disabled).await.map_err(internal)? {
        return Err(not_found());
    }
    view_of(&state, id).await
}

#[derive(Deserialize)]
pub(super) struct SetPriorityReq {
    pub(super) priority: i64,
}

/// 设置优先级。代理和用户最高只能调到 [`MemberCaps::min_priority`]（已经更高的只能往下调）。
pub(super) async fn set_priority(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<SetPriorityReq>,
) -> Result<Json<CredentialView>, ApiError> {
    check_priority(req.priority)?;
    // 不能高过 P2，也不能比现在更高：admin 给的 P0 往下调一档（到 P1）照样可以。「比现在」
    // 的判断与写入在同一条语句里，免得读完之后 admin 恰好改了档、这里再按旧值写回去。
    let floor = member_caps(&state, &actor).map_or(PRIORITY_MIN, |c| c.min_priority);
    if !state.store.set_priority(id, req.priority, floor).await.map_err(internal)? {
        return Err(match state.store.get(id).await.map_err(internal)? {
            Some(_) => stricter_only(Knob::Priority),
            None => not_found(),
        });
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

// ---------- 代理和用户只能往紧里调 ----------

/// 代理和用户改自己号的调度参数时能到的边：只能比全局更保守，不能更宽。
///
/// 这几项决定全池的流量往哪个号上走：P0 跨档先用尽、名额不限的号会把同组的流量吸过去，
/// 而账单按号主算——号主把自己的号调到 P0、上限放开，就能把别人的流量抢过来，号一出事
/// 下游的请求也跟着扎堆报错。admin 不受限（给某个号主的号单独放宽照样可以）。
///
/// 只在写入时核对：admin 之后把全局默认调紧，号主先前设的值不追溯。
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct MemberCaps {
    /// 能选的最高档（数值最小）：[`PRIORITY_DEFAULT`]，即 P2。
    pub(crate) min_priority: i64,
    /// 设备 / 会话 / RPM 上限换算成生效值后的天花板；`0` 为全局不限，这时随便设（含「不限」）。
    pub(crate) device_limit: i64,
    pub(crate) session_limit: i64,
    pub(crate) rpm_limit: i64,
    /// 提前停调度阈值（两档）换算成生效值后的天花板；`0` 为全局这一档不停，这时随便设。
    pub(crate) quota_pause_pct: i64,
    pub(crate) quota_pause_pct_7d: i64,
}

impl MemberCaps {
    pub(crate) fn of(store: &store::CredentialStore) -> Self {
        Self {
            min_priority: PRIORITY_DEFAULT,
            device_limit: store.default_device_limit().max(0),
            session_limit: store.default_session_limit().max(0),
            rpm_limit: store.default_rpm_limit().max(0),
            quota_pause_pct: store.quota_pause_pct(),
            quota_pause_pct_7d: store.quota_pause_pct_7d(),
        }
    }

    /// 三态上限（`> 0` 独立 / `0` 跟随全局 / `< 0` 不限）换算成生效值后不超过天花板
    /// （`0` 为不设边）。三种上限的三态语义相同，见 [`store::effective_device_limit`]。
    fn allows_limit(&self, knob: Knob, limit: i64) -> bool {
        let cap = match knob {
            Knob::DeviceLimit => self.device_limit,
            Knob::SessionLimit => self.session_limit,
            Knob::RpmLimit => self.rpm_limit,
            Knob::Priority | Knob::QuotaPause => unreachable!("not a three-state limit"),
        };
        cap == 0 || (1..=cap).contains(&store::effective_device_limit(limit, cap))
    }
}

/// 代理和用户受 [`MemberCaps`] 约束的几项。
#[derive(Debug, Clone, Copy)]
enum Knob {
    Priority,
    DeviceLimit,
    SessionLimit,
    RpmLimit,
    QuotaPause,
}

impl Knob {
    fn name(self) -> &'static str {
        match self {
            Knob::Priority => "priority",
            Knob::DeviceLimit => "device limit",
            Knob::SessionLimit => "session limit",
            Knob::RpmLimit => "rpm limit",
            Knob::QuotaPause => "quota pause threshold",
        }
    }
}

/// admin 为 None（不受限），其余人回此刻的 [`MemberCaps`]。
fn member_caps(state: &AppState, actor: &Actor) -> Option<MemberCaps> {
    (!actor.is_admin()).then(|| MemberCaps::of(&state.store))
}

/// 代理和用户改单个号时，那个号此刻的样子（admin 不查，回 None）：原样保留的值不算放宽——
/// admin 给的「不限」「不停」，号主改别的项时照原值带回来不该被拒。
async fn current_for_member(
    state: &AppState,
    actor: &Actor,
    id: i64,
) -> Result<Option<Credential>, ApiError> {
    if actor.is_admin() {
        return Ok(None);
    }
    state.store.get(id).await.map_err(internal)?.ok_or_else(not_found).map(Some)
}

/// 核对一项三态上限（设备 / 会话 / RPM），回要不要写：admin 一律写；代理和用户与 `current`
/// （单号接口给这个号的现值，批量不给）相同时**不写**、也不核对，否则换算后不能比全局宽。
/// 单号与批量接口共用这一处。
///
/// 原值不写而不是「放行后照写」：读现值与写入之间 admin 可能刚把它收紧，照写就把 admin 收回
/// 的「不限」又写了回去。不写就没有这个窗口——值本来就没变。
fn check_member_limit(
    state: &AppState,
    actor: &Actor,
    knob: Knob,
    limit: i64,
    current: Option<i64>,
) -> Result<bool, ApiError> {
    match member_caps(state, actor) {
        None => Ok(true),
        Some(_) if current == Some(limit) => Ok(false),
        Some(c) if c.allows_limit(knob, limit) => Ok(true),
        Some(_) => Err(stricter_only(knob)),
    }
}

/// 核对两档提前停调度阈值（`None` 跟随全局 / `0` 不停 / `1..=100`），各档外层 `None` 表示这一档
/// 不写、不核对。要写的那档换算成生效值后不超过全局（全局这一档不停时不设边）。
fn check_member_pcts(
    state: &AppState,
    actor: &Actor,
    short: Option<Option<i64>>,
    long: Option<Option<i64>>,
) -> Result<(), ApiError> {
    let Some(c) = member_caps(state, actor) else { return Ok(()) };
    let within = |pct: Option<Option<i64>>, cap: i64| match pct {
        None => true,
        Some(pct) => cap == 0 || (1..=cap).contains(&store::effective_quota_pause_pct(pct, cap)),
    };
    if within(short, c.quota_pause_pct) && within(long, c.quota_pause_pct_7d) {
        Ok(())
    } else {
        Err(stricter_only(Knob::QuotaPause))
    }
}

fn stricter_only(knob: Knob) -> ApiError {
    (
        StatusCode::FORBIDDEN,
        format!(
            "{}: agents and users can only set values stricter than the global default",
            knob.name()
        ),
    )
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
    Extension(actor): Extension<Actor>,
    Json(req): Json<SetPrioritiesReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    match (req.priority, req.delta) {
        (Some(priority), None) => {
            check_priority(priority)?;
            if member_caps(&state, &actor).is_some_and(|c| priority < c.min_priority) {
                return Err(stricter_only(Knob::Priority));
            }
            let n = state.store.set_priorities(&req.ids, priority).await.map_err(internal)?;
            tracing::info!(count = n, priority, "priority set in bulk");
        }
        (None, Some(delta)) => {
            let span = PRIORITY_MAX - PRIORITY_MIN;
            if delta == 0 || !(-span..=span).contains(&delta) {
                return Err(bad_request(format!("delta must be non-zero and within ±{span}")));
            }
            // 代理和用户往上调最多到 P2；原本就在 P2 之上的（admin 给的）保持不动、不往下压。
            let floor = member_caps(&state, &actor).map_or(PRIORITY_MIN, |c| c.min_priority);
            let n = state.store.shift_priorities(&req.ids, delta, floor).await.map_err(internal)?;
            tracing::info!(count = n, delta, "priority shifted in bulk");
        }
        _ => return Err(bad_request("provide exactly one of priority or delta")),
    }
    list_credentials(State(state), Extension(actor)).await
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
    Extension(actor): Extension<Actor>,
    Json(req): Json<SetDeviceLimitsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    // 负值统一收敛为 -1，与单账号接口保持一致。
    let limit = if req.device_limit < 0 { -1 } else { req.device_limit };
    check_member_limit(&state, &actor, Knob::DeviceLimit, limit, None)?;
    let n = state.store.set_device_limits(&req.ids, limit).await.map_err(internal)?;
    tracing::info!(count = n, device_limit = limit, "device limit set in bulk");
    list_credentials(State(state), Extension(actor)).await
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
    Extension(actor): Extension<Actor>,
    Json(req): Json<SetSessionLimitsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let limit = if req.session_limit < 0 { -1 } else { req.session_limit };
    check_member_limit(&state, &actor, Knob::SessionLimit, limit, None)?;
    let n = state.store.set_session_limits(&req.ids, limit).await.map_err(internal)?;
    tracing::info!(count = n, session_limit = limit, "session limit set in bulk");
    list_credentials(State(state), Extension(actor)).await
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
    Extension(actor): Extension<Actor>,
    Json(req): Json<SetRpmLimitsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let limit = if req.rpm_limit < 0 { -1 } else { req.rpm_limit };
    check_member_limit(&state, &actor, Knob::RpmLimit, limit, None)?;
    let n = state.store.set_rpm_limits(&req.ids, limit).await.map_err(internal)?;
    tracing::info!(count = n, rpm_limit = limit, "rpm limit set in bulk");
    list_credentials(State(state), Extension(actor)).await
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
    Extension(actor): Extension<Actor>,
    Json(req): Json<SetQuotaPausePctsManyReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let short = req.quota_pause_pct.map(|p| p.clamp(0, 100));
    let long = req.quota_pause_pct_7d.map(|p| p.clamp(0, 100));
    check_member_pcts(&state, &actor, Some(short), Some(long))?;
    let n = state.store.set_quota_pause_pcts_many(&req.ids, short, long).await.map_err(internal)?;
    tracing::info!(
        count = n,
        quota_pause_pct = ?short,
        quota_pause_pct_7d = ?long,
        "per-account quota-threshold pause set in bulk"
    );
    list_credentials(State(state), Extension(actor)).await
}

#[derive(Deserialize)]
pub(super) struct SetDisabledManyReq {
    ids: Vec<i64>,
    disabled: bool,
}

/// 批量启用/停用，返回更新后的整份列表。
pub(super) async fn set_disabled_many(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<SetDisabledManyReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let n = state.store.set_disabled_many(&req.ids, req.disabled).await.map_err(internal)?;
    tracing::info!(count = n, disabled = req.disabled, "enabled/disabled in bulk");
    list_credentials(State(state), Extension(actor)).await
}

/// 批量删除（连带清设备绑定；用量流水不删，随保留期自然裁掉），返回删除后的整份列表。
///
/// 用 POST 而非 DELETE：带请求体的 DELETE 在部分代理/客户端上会被丢掉 body。
pub(super) async fn delete_credentials(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<IdsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let store = state.store.clone();
    let n = store.remove(&req.ids).await.map_err(internal)?;
    tracing::info!(count = n, "credentials deleted in bulk");
    list_credentials(State(state), Extension(actor)).await
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
    if !state.store.set_label(id, label).await.map_err(internal)? {
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
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<SetDeviceLimitReq>,
) -> Result<Json<CredentialView>, ApiError> {
    // 负值统一收敛为 -1，避免库里出现各式各样的“不限”取值。
    let limit = if req.device_limit < 0 { -1 } else { req.device_limit };
    let current = current_for_member(&state, &actor, id).await?;
    let write = check_member_limit(
        &state,
        &actor,
        Knob::DeviceLimit,
        limit,
        current.map(|c| c.device_limit),
    )?;
    if write && !state.store.set_device_limit(id, limit).await.map_err(internal)? {
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
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<SetSessionLimitReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let limit = if req.session_limit < 0 { -1 } else { req.session_limit };
    let current = current_for_member(&state, &actor, id).await?;
    let write = check_member_limit(
        &state,
        &actor,
        Knob::SessionLimit,
        limit,
        current.map(|c| c.session_limit),
    )?;
    if write && !state.store.set_session_limit(id, limit).await.map_err(internal)? {
        return Err(not_found());
    }
    view_of(&state, id).await
}

#[derive(Deserialize)]
pub(super) struct SetRpmLimitReq {
    /// RPM 上限三态：`> 0` 本账号独立上限；`0` 跟随全局默认；`< 0` 本账号明确不限。
    pub(super) rpm_limit: i64,
}

/// 设置该账号每分钟最多转发多少条请求。
///
/// 计数在进程内存里，改完即时生效；已经记在窗口里的那些不会因为调高上限而消失，
/// 也不会因为调低而被追认——只影响之后的判定。
pub(super) async fn set_rpm_limit(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<SetRpmLimitReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let limit = if req.rpm_limit < 0 { -1 } else { req.rpm_limit };
    let current = current_for_member(&state, &actor, id).await?;
    let write =
        check_member_limit(&state, &actor, Knob::RpmLimit, limit, current.map(|c| c.rpm_limit))?;
    if write && !state.store.set_rpm_limit(id, limit).await.map_err(internal)? {
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
    pub(super) quota_pause_pct: Option<i64>,
    /// **7d 窗口**那一档，同上。两档**都要传**：这是整份覆盖，不传即视为「跟随全局」——
    /// 与全局那个接口「不传 = 保持现值」的约定不同，因为这里的三态里「跟随」本身就是 null。
    #[serde(default)]
    pub(super) quota_pause_pct_7d: Option<i64>,
}

/// 设置该账号自己的「额度用到多少就提前停调度」阈值，覆盖全局的
/// [`crate::store::QUOTA_PAUSE_PCT`] / [`crate::store::QUOTA_PAUSE_PCT_7D`]。判定见
/// [`crate::proxy::park_if_quota_nearly_exhausted`]，下一条带限流头的响应起生效；已经按
/// 旧阈值停下的号不会因为调高阈值而自动回池——到点自恢复、手动启用或连通性测试照旧。
pub(super) async fn set_credential_quota_pause_pct(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<SetCredentialQuotaPausePctReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let short = req.quota_pause_pct.map(|p| p.clamp(0, 100));
    let long = req.quota_pause_pct_7d.map(|p| p.clamp(0, 100));
    // 代理和用户：与现值相同的那一档不写（理由见 [`check_member_limit`]）。两档是整份提交的，
    // 只改一档时另一档原样带回来，不写它就不会把这期间 admin 刚改的值盖回去。
    let current = current_for_member(&state, &actor, id).await?;
    let (short_w, long_w) = match &current {
        None => (Some(short), Some(long)),
        Some(c) => (
            (short != c.quota_pause_pct).then_some(short),
            (long != c.quota_pause_pct_7d).then_some(long),
        ),
    };
    check_member_pcts(&state, &actor, short_w, long_w)?;
    if (short_w.is_some() || long_w.is_some())
        && !state.store.set_quota_pause_pcts_partial(id, short_w, long_w).await.map_err(internal)?
    {
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
    pub(super) proxy: Option<String>,
}

/// 设置/清除某个账号专用的出站代理。
///
/// 配好之后这个号的**全部**出站流量都走它：转发、token 刷新、profile、连通性测试。
/// 校验放在入库之前——存进去一条建不出客户端的代理，故障要等到下次真有请求选中这个号
/// 才暴露，那时现场只剩一条「这个号所有请求都失败」。
pub(super) async fn set_proxy(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<SetProxyReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).await.map_err(internal)?.ok_or_else(not_found)?;
    let proxy = match req.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            Some(crate::clients::validate_proxy(raw).map_err(|e| bad_request(format!("{e:#}")))?)
        }
        None => None,
    };
    if let Some(url) = &proxy {
        check_proxy_for(&state, &actor, url).await?;
    }
    if !state.store.set_proxy(id, proxy.as_deref()).await.map_err(internal)? {
        return Err(not_found());
    }
    // 丢掉旧代理那份缓存客户端，否则它的连接池还会继续把请求送去老代理——改完之后
    // 「看着已经换了、实际还在走旧的」是这类缓存最典型的坑。
    if let Some(old) = cred.proxy.as_deref() {
        state.clients.forget(old);
    }
    // 新代理自动入池（号主人的池子）：下次其他账号能直接从池里选，不必再手打。
    if let Some(ref url) = proxy {
        state.store.ensure_proxy_in_pool(cred.owner_id.unwrap_or(actor.id), url).await;
    }
    tracing::info!(
        cred_id = id, cred = %cred.label,
        proxy = %proxy.as_deref().unwrap_or("<direct>"),
        "credential proxy updated"
    );
    view_of(&state, id).await
}

/// 读取单条并转为脱敏视图（含已绑定设备数）。
pub(super) async fn view_of(state: &AppState, id: i64) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).await.map_err(internal)?.ok_or_else(not_found)?;
    let count = state.store.device_count(id).await.map_err(internal)?;
    let session_count = state.store.session_count(id).await.map_err(internal)?;
    // 单账号视图只查这一个 id：此前调的是三个「全库聚合」再 remove 一条，改一次开关就要把
    // usage_logs 整表聚合三遍。
    let quota = state.store.latest_quota(id).await.map_err(internal)?;
    let last_used = state.store.last_used_at(id).await.map_err(internal)?;
    let cost_total = state.store.cost_of(id).await.map_err(internal)?;
    let rpm = state.store.recent_rpm_of(id).await.map_err(internal)?;
    let denials = state.store.denied_models(id).await.map_err(internal)?;
    let groups = state.store.credential_groups(id).await.map_err(internal)?;
    Ok(Json(
        CredentialView::new(&cred, count, session_count, DefaultLimits::of(&state.store))
            .with_proxy_ids(&saved_proxy_ids(state).await?)
            .with_groups(groups)
            .with_cooldown(
                state.store.rate_limited_secs(cred.id),
                state.store.rate_limited_models(cred.id),
            )
            .with_denials(denials)
            .with_stats(quota, last_used, cost_total, rpm),
    ))
}

#[derive(Deserialize)]
pub(super) struct SetGroupsReq {
    group_ids: Vec<i64>,
}

#[derive(Deserialize)]
pub(super) struct SetGroupsManyReq {
    ids: Vec<i64>,
    group_ids: Vec<i64>,
}

/// 改一个号所在的分组（整体替换，至少一个）。代理和用户只能选开放给自己的分组，admin 随便选。
pub(super) async fn set_credential_groups(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<SetGroupsReq>,
) -> Result<Json<CredentialView>, ApiError> {
    if state.store.get(id).await.map_err(internal)?.is_none() {
        return Err(not_found());
    }
    check_selectable(&state, &actor, &req.group_ids).await?;
    state
        .store
        .set_credential_groups(&[id], &req.group_ids)
        .await
        .map_err(internal)?
        .map_err(group_error)?;
    tracing::info!(cred_id = id, groups = ?req.group_ids, by = %actor.username, "credential groups updated");
    view_of(&state, id).await
}

/// 批量改分组：口径同 [`set_credential_groups`]。
pub(super) async fn set_credentials_groups(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<SetGroupsManyReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    check_selectable(&state, &actor, &req.group_ids).await?;
    state
        .store
        .set_credential_groups(&req.ids, &req.group_ids)
        .await
        .map_err(internal)?
        .map_err(group_error)?;
    tracing::info!(count = req.ids.len(), groups = ?req.group_ids, by = %actor.username, "credential groups updated in bulk");
    list_credentials(State(state), Extension(actor)).await
}
