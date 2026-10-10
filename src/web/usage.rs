//! 用量流水与凭证统计的查询接口。

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
    /// 满足筛选（含 `until` 上界）的总条数。请求带了 `until`（翻后续页）时为 null：同一个锚点
    /// 下集合不变，沿用第一页拿到的即可，不再每翻一页把整个集合重新数一遍、加一遍。
    pub(super) total: Option<i64>,
    /// 同一集合的花费合计（USD），何时为 null 同 `total`。
    total_cost: Option<f64>,
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
        && state.store.credential_owner(cred).await.map_err(internal)? != Some(owner)
    {
        return Err(not_found());
    }
    usage_page(&state, q.cred_id, owner, &q, 100, 1000, owner.is_some()).await
}

/// 列出某凭证的请求流水（按时间倒序，页码翻页）。
///
/// 与卡片上那些聚合数的口径**不同**，这一点得记清楚：卡片的累计花费、设备请求数读的是
/// 终身账本（`credential_stats` / `device_costs`），而流水只保留近期（见
/// [`store::CredentialStore::prune_usage_logs`]）。于是「明细合计 < 卡片上的累计」是正常的，
/// 不是哪一边算错了。
pub(super) async fn list_credential_usage(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<UsagePage>, ApiError> {
    // 与设备明细同口径：凭证不存在给 404，免得前端把「账号已被删」显示成「没有请求」。
    if state.store.get(id).await.map_err(internal)?.is_none() {
        return Err(not_found());
    }
    usage_page(&state, Some(id), None, &q, 25, 200, actor.scope().owner().is_some()).await
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
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<CredentialStatsResp>, ApiError> {
    if state.store.get(id).await.map_err(internal)?.is_none() {
        return Err(not_found());
    }
    let (since, bucket_secs, tz) = q.normalized();
    let mut stats =
        state.store.credential_stats(id, since, bucket_secs, tz, 20).await.map_err(internal)?;
    // 按设备、按客户端的拆分是下游使用者的设备 id 与 UA，号主不该看到，理由见 [`for_owner`]。
    if actor.scope().owner().is_some() {
        stats.by_device.clear();
        stats.by_client.clear();
    }
    Ok(Json(CredentialStatsResp { since, bucket_secs, stats }))
}

/// 给号主（代理和用户）看的流水：只留这个号自己的事——时间、模型、状态、用量、费用、
/// 额度头、走的出口、上游错误类型。
///
/// 抹掉的两类：一是**下游使用者**的身份——来访 UA、设备 id、会话 id，以及出站那份派生的
/// 设备 / 会话 id；二是 **luban 的改写细节**——走没走模拟、为什么走、请求体结构摘要、改写
/// 标签。上游错误原文与零输出的回复片段也抹掉：前者个别会把请求体回显进来，后者就是模型
/// 输出，都是下游的内容。号主是外部的人，这些看到了既泄露下游，也泄露手法。
fn for_owner(mut log: store::UsageLog) -> store::UsageLog {
    log.device_id = None;
    log.ua = None;
    log.ua_out = None;
    let f = &mut log.forensics;
    f.simulated = false;
    f.sim_reason = None;
    f.shape = None;
    f.session_id = None;
    f.session_id_in = None;
    f.session_key = None;
    f.device_id_out = None;
    f.error_message = None;
    f.rewrites = None;
    f.response_excerpt = None;
    log
}

/// 两条流水接口共用的取页逻辑：先按 `until`（没有就现取一个）钉住快照，再在同一条件下
/// 取统计与当页记录。
///
/// **统计与记录必须同锚点**：先算 total 再另取一次 max(id) 当锚点的话，两次之间新写入的
/// 请求会让 total 比锚点下真正翻得到的条数多，最后一页于是空着。
async fn usage_page(
    state: &AppState,
    cred_id: Option<i64>,
    owner_id: Option<i64>,
    q: &UsageQuery,
    default_limit: i64,
    max_limit: i64,
    owner_view: bool,
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
    // 带了锚点就是在翻同一轮的后续页：集合没变，统计沿用第一页的（前端留着），不重算——
    // 无筛选时这是一次全表的 COUNT/SUM，每翻一页都扫一遍不值当。
    let (total, total_cost) = if q.until.is_some() {
        (None, None)
    } else {
        let stats = state.store.usage_log_stats(filter.clone()).await.map_err(internal)?;
        // 首次请求没有锚点，就用这一刻的最大 id 当锚点——统计与记录都在它之下，两者自洽。
        filter.until_id = stats.max_id;
        (Some(stats.total), Some(stats.cost_usd))
    };
    let anchor = filter.until_id;
    let mut logs = state.store.query_usage_logs(filter).await.map_err(internal)?;
    if owner_view {
        logs = logs.into_iter().map(for_owner).collect();
    }
    Ok(Json(UsagePage { total, total_cost, anchor, logs }))
}
