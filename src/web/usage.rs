//! 用量流水、凭证统计与封号事件的查询接口。

use super::*;

// ---------- 用量日志 ----------

#[derive(Deserialize, Default)]
pub(super) struct UsageQuery {
    /// 只看这个账号的流水；仅全局的 `/usage` 认它（按号的 `/credentials/{id}/usage` 以路径为准）。
    ///
    /// 与按号接口分开存在，是因为那个接口在账号不存在时给 404，而已删账号的流水会留到保留期满
    /// （见 [`CredentialStore::remove`]）：趋势拆分表里点已删账号那一行，得走这里才看得到。
    #[serde(default)]
    pub(super) cred_id: Option<i64>,
    /// 返回条数上限（默认 100，最多 1000；按号查时默认 25、最多 200）。
    #[serde(default)]
    pub(super) limit: Option<i64>,
    /// 跳过前多少条（页码 × 每页条数）。
    #[serde(default)]
    pub(super) offset: Option<i64>,
    /// 翻页锚点：只取 id ≤ 它的记录。首次不传，之后把响应里的 `anchor` 原样带回来。
    /// 理由见 [`store::UsageLogQuery`]。
    #[serde(default)]
    pub(super) until: Option<i64>,
    /// 只看这一个请求 id（luban 回在 `X-Oneapi-Request-Id` 上的那个，New API 日志里的
    /// `upstream_request_id`）。精确匹配，空白视同不筛。
    #[serde(default)]
    pub(super) request_id: Option<String>,
    /// 只看这个模型（精确匹配）；趋势对话框的拆分表点进来带的。
    #[serde(default)]
    pub(super) model: Option<String>,
    /// 只看最近这么多小时；不传为不限（受流水保留期约束）。
    #[serde(default)]
    pub(super) hours: Option<i64>,
    /// 只看这条模拟会话的请求（`session_bindings.session_key`，精确匹配，空白视同不筛）；
    /// 名额对话框里会话那一行点「看请求」带的。
    #[serde(default)]
    pub(super) session_key: Option<String>,
    /// 只看这个会话 id 的请求：**出站与来访两侧任一命中**（走模拟时两者不是同一个 uuid，
    /// 见 `store::Forensics::session_id_in`）。请求查询里贴一个 uuid 进来走的就是它。
    #[serde(default)]
    pub(super) session_id: Option<String>,
}

/// 一页流水 + 整个集合的口径。前端要靠 `total` 算页数、靠 `anchor` 把整轮翻页钉在同一快照上。
#[derive(serde::Serialize)]
pub(super) struct UsagePage {
    /// 满足筛选（含 `until` 上界）的总条数。
    pub(super) total: i64,
    /// 同一集合的花费合计（USD）。
    total_cost: f64,
    /// 本轮翻页的锚点：请求里带了 `until` 就是它，否则是当前最大 id；空集为 null。
    anchor: Option<i64>,
    pub(super) logs: Vec<store::UsageLog>,
}

/// 列出最近的用量日志（按时间倒序）。
///
/// 代理和用户只查得到本人名下的号的流水：带了别人的 `cred_id` 回 404，按请求 id / 会话 id
/// 查时结果也只在本人的号里找（[`store::UsageLogQuery::owner_id`]）。
pub(super) async fn list_usage(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<UsagePage>, ApiError> {
    let owner = actor.scope().owner();
    if let (Some(owner), Some(cred)) = (owner, q.cred_id)
        && state.store.credential_owner(cred).map_err(internal)? != Some(owner)
    {
        return Err(not_found());
    }
    blocking(move || usage_page(&state, q.cred_id, owner, &q, 100, 1000)).await
}

/// 列出某凭证的请求流水（按时间倒序，页码翻页）。
///
/// 与卡片上那些聚合数的口径**不同**，这一点得记清楚：卡片的累计花费、设备请求数读的是
/// 终身账本（`credential_stats` / `device_costs`），而流水只保留近期（见
/// [`store::CredentialStore::prune_usage_logs`]）。于是「明细合计 < 卡片上的累计」是正常的，
/// 不是哪一边算错了。
pub(super) async fn list_credential_usage(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<UsagePage>, ApiError> {
    // 与设备明细同口径：凭证不存在给 404，免得前端把「账号已被删」显示成「没有请求」。
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    blocking(move || usage_page(&state, Some(id), None, &q, 25, 200)).await
}

#[derive(Serialize)]
pub(super) struct CredentialStatsResp {
    since: i64,
    bucket_secs: i64,
    #[serde(flatten)]
    stats: store::CredentialStats,
}

/// 单个账号的用量统计：按小时 / 按日的时间序列，加按模型、设备、客户端、状态码的拆分。
/// 参数同趋势接口（`hours` / `bucket_secs` / `tz_offset_secs`）。不进 [`cached_metrics`]：
/// 只扫一个号的流水，而且只有详情页打开时才会被拉。
pub(super) async fn get_credential_stats(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<CredentialStatsResp>, ApiError> {
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let (since, bucket_secs, tz) = q.normalized();
    let stats = blocking(move || {
        state.store.credential_stats(id, since, bucket_secs, tz, 20).map_err(internal)
    })
    .await?;
    Ok(Json(CredentialStatsResp { since, bucket_secs, stats }))
}

/// 两条流水接口共用的取页逻辑：先按 `until`（没有就现取一个）钉住快照，再在同一条件下
/// 取统计与当页记录。
///
/// **统计与记录必须同锚点**：先算 total 再另取一次 max(id) 当锚点的话，两次之间新写入的
/// 请求会让 total 比锚点下真正翻得到的条数多，最后一页于是空着。
fn usage_page(
    state: &AppState,
    cred_id: Option<i64>,
    owner_id: Option<i64>,
    q: &UsageQuery,
    default_limit: i64,
    max_limit: i64,
) -> Result<Json<UsagePage>, ApiError> {
    let limit = q.limit.unwrap_or(default_limit).clamp(1, max_limit);
    let offset = q.offset.unwrap_or(0).max(0);
    let since = q.hours.map(|h| {
        let max_hours = store::USAGE_LOG_RETENTION_SECS / 3600;
        chrono::Utc::now().timestamp() - h.clamp(1, max_hours) * 3600
    });
    let mut filter = store::UsageLogQuery {
        cred_id,
        until_id: q.until,
        offset,
        limit,
        request_id: q.request_id.clone(),
        model: q.model.clone(),
        since,
        session_key: q.session_key.clone(),
        session_id: q.session_id.clone(),
        owner_id,
    };
    let stats = state.store.usage_log_stats(filter.clone()).map_err(internal)?;
    // 首次请求没有锚点，就用这一刻的最大 id 当锚点——统计与记录都在它之下，两者自洽。
    filter.until_id = q.until.or(stats.max_id);
    let anchor = filter.until_id;
    let logs = state.store.query_usage_logs(filter).map_err(internal)?;
    Ok(Json(UsagePage { total: stats.total, total_cost: stats.cost_usd, anchor, logs }))
}

#[derive(Deserialize)]
pub(super) struct BanEventsQuery {
    #[serde(default)]
    cred_id: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

/// 封号事件列表（新的在前），见 [`store::CredentialStore::record_ban`]。
///
/// 已删账号的事件照常返回：事件按 cred_id 存、不随删号消失——死号最容易被清理，而清理的
/// 瞬间恰是最需要留下它的时候。
///
/// 代理和用户只能查本人名下某个号的（必须带 `cred_id`）；admin 与访客可以不带、看全部。
pub(super) async fn list_ban_events(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<BanEventsQuery>,
) -> Result<Json<Vec<store::BanEvent>>, ApiError> {
    if let Scope::Owner(owner) = actor.scope() {
        let owned = match q.cred_id {
            Some(id) => state.store.credential_owner(id).map_err(internal)? == Some(owner),
            None => false,
        };
        if !owned {
            return Err(not_found());
        }
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let events =
        blocking(move || state.store.list_ban_events(q.cred_id, limit).map_err(internal)).await?;
    Ok(Json(events))
}

#[derive(Deserialize)]
pub(super) struct FrozenLogsQuery {
    /// 返回条数上限（默认 100，最多 1000）。
    #[serde(default)]
    limit: Option<i64>,
    /// 跳过前多少条（页码 × 每页条数）。
    #[serde(default)]
    offset: Option<i64>,
}

/// 一页冻结流水 + 该事件冻结的总条数。翻页锚点这里不需要：冻结表写完就不再变。
#[derive(serde::Serialize)]
pub(super) struct FrozenLogPage {
    total: i64,
    logs: Vec<store::UsageLog>,
}

/// 某封号事件冻结下来的一页流水（时间正序）：封前 7 天 + 封后 10 分钟内到达的该号全部请求，
/// 带取证列（出口代理、形态摘要、上游错误文案、第三方判定、改写标签）。
///
/// 一次封号常冻下上千行、几十 MB，整份一次吐出去页面要卡住半天，所以按页给；要整份的
/// （下载取证包）由前端连着翻完再拼。
pub(super) async fn list_ban_event_logs(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<FrozenLogsQuery>,
) -> Result<Json<FrozenLogPage>, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0).max(0);
    let (total, logs) =
        blocking(move || state.store.frozen_usage_logs(id, limit, offset).map_err(internal))
            .await?;
    Ok(Json(FrozenLogPage { total, logs }))
}
