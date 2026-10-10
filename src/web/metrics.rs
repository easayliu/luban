//! 实时指标与趋势：缓存、TTFT、用量明细、本地拒绝统计。

use super::*;

// ---------- 实时指标 ----------

/// 整个代理此刻的两个实时数，见 [`get_metrics`]。
#[derive(Serialize)]
pub(super) struct MetricsResp {
    /// 全局 RPM：最近 60 秒转发的请求总数，恒等于各账号 RPM 之和
    /// （见 [`store::CredentialStore::total_rpm`]）。
    rpm: i64,
    /// 在途请求数：已进入转发入口、响应尚未走完的那些（流式回复整段传输期间都算）。
    in_flight: i64,
    /// RPM 的统计窗口（秒），固定 60；前端据此写文案，不必两边各写死一个 60。
    window_secs: i64,
}

/// 读取实时指标。**刻意与账号列表分开**：这两个数几秒就变一次，值得单独用一个便宜的接口
/// 高频轮询，而账号列表那个响应要跑十几条聚合查询，按同样频率拉只是白烧数据库。
pub(super) async fn get_metrics(
    State(state): State<AppState>,
) -> Result<Json<MetricsResp>, ApiError> {
    let store = state.store.clone();
    let rpm = store.total_rpm().await.map_err(internal)?;
    Ok(Json(MetricsResp {
        rpm,
        in_flight: state.in_flight.load(std::sync::atomic::Ordering::Relaxed).max(0),
        window_secs: store::RPM_WINDOW_SECS,
    }))
}

// ---------- 缓存 & TTFT 趋势 ----------

/// 趋势接口的查询串：回看多少小时、桶宽、本地时区偏移。桶宽与偏移由前端按它要画的格子
/// 给（逐小时一格给 3600，逐天一格给 86400 加本地零点的偏移）。数据来自 15 分钟一格的预聚合，
/// 故桶宽向上取整到 15 分钟的整数倍、偏移对齐到 15 分钟、窗口起点向上对齐到 15 分钟（见
/// [`store::series_grid`]）；响应里的 `since` / `bucket_secs` 回的是规整之后实际用的值。
#[derive(Deserialize)]
pub(super) struct SeriesQuery {
    #[serde(default = "default_series_hours")]
    hours: i64,
    #[serde(default = "default_bucket_secs")]
    bucket_secs: i64,
    #[serde(default)]
    tz_offset_secs: i64,
}

fn default_series_hours() -> i64 {
    7 * 24
}

fn default_bucket_secs() -> i64 {
    3600
}

impl SeriesQuery {
    /// `(since, bucket_secs, tz_offset_secs)`：先钳到合法范围，再规整成预聚合拼得出的网格。
    pub(super) fn normalized(&self) -> (i64, i64, i64) {
        let max_hours = store::USAGE_LOG_RETENTION_SECS / 3600;
        let hours = self.hours.clamp(1, max_hours);
        let since = chrono::Utc::now().timestamp() - hours * 3600;
        let bucket = self.bucket_secs.clamp(60, store::USAGE_LOG_RETENTION_SECS);
        let tz = self.tz_offset_secs.clamp(-14 * 3600, 14 * 3600);
        store::series_grid(since, bucket, tz)
    }
}

/// 趋势接口的短期响应缓存：键是接口名加规整后的参数，20 秒内同一份问题直接回上次的答案。
/// 概览页每分钟拉四条趋势（缓存 / 延迟各 24h 与 7d），多开几个后台标签页就是成倍的 7 天
/// 扫描；而这些数一分钟内本来也不会变到值得重算。条目按参数组合计，超过 64 条整个清掉
/// （参数都钳过范围，正常只有几种组合）。
type MetricsCacheMap =
    std::collections::HashMap<String, (std::time::Instant, Arc<serde_json::Value>)>;

static METRICS_CACHE: std::sync::LazyLock<std::sync::Mutex<MetricsCacheMap>> =
    std::sync::LazyLock::new(Default::default);

const METRICS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(20);

/// 按键取缓存，没有或过期就算一次并存起来。算的那一步在锁外，几个标签页同时撞上会各算各的，
/// 但接下来 20 秒都省了。回给 axum 的是 `Value` 的一份克隆：几 KB 的 JSON，比整段扫描
/// 便宜几个数量级；`Arc<Value>` 本身不能直接序列化（serde 的 rc 特性没开）。
pub(super) async fn cached_metrics<T, F>(
    key: String,
    compute: F,
) -> Result<Json<serde_json::Value>, ApiError>
where
    T: Serialize,
    F: std::future::Future<Output = Result<T, ApiError>>,
{
    let now = std::time::Instant::now();
    if let Some((at, v)) = METRICS_CACHE.lock().unwrap().get(&key)
        && now.duration_since(*at) < METRICS_CACHE_TTL
    {
        return Ok(Json((**v).clone()));
    }
    let computed = compute.await?;
    let value = Arc::new(serde_json::to_value(computed).map_err(|e| internal(anyhow::anyhow!(e)))?);
    let mut cache = METRICS_CACHE.lock().unwrap();
    if cache.len() >= 64 {
        cache.clear();
    }
    cache.insert(key, (now, Arc::clone(&value)));
    Ok(Json((*value).clone()))
}

#[derive(Serialize)]
struct CacheSeriesResp {
    since: i64,
    bucket_secs: i64,
    points: Vec<store::CacheBucket>,
    /// 整个窗口的合计。
    summary: store::CacheBucket,
    /// 近 60 分钟的合计（起点向上对齐到 15 分钟）。
    recent: store::CacheBucket,
}

pub(super) async fn get_cache_series(
    State(state): State<AppState>,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (since, bucket_secs, tz) = q.normalized();
    // 键里放小时数而不是 since：since 每秒都在变，放进去缓存永远命不中。
    let key = format!("cache:{}:{bucket_secs}:{tz}", q.hours);
    cached_metrics(key, async move {
        let store::CacheReport { points, summary, recent } =
            state.store.cache_report(since, bucket_secs, tz).await.map_err(internal)?;
        Ok(CacheSeriesResp { since, bucket_secs, points, summary, recent })
    })
    .await
}

#[derive(Serialize)]
struct TtftSeriesResp {
    since: i64,
    bucket_secs: i64,
    points: Vec<store::TtftBucket>,
    /// 整个窗口的分位与吞吐（合并整窗口的延迟分布再取分位，不是各桶的平均）。
    summary: store::TtftBucket,
    /// 近 60 分钟的分位与吞吐。
    recent: store::TtftBucket,
}

pub(super) async fn get_ttft_series(
    State(state): State<AppState>,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (since, bucket_secs, tz) = q.normalized();
    let key = format!("ttft:{}:{bucket_secs}:{tz}", q.hours);
    cached_metrics(key, async move {
        let store::TtftReport { points, summary, recent } =
            state.store.ttft_report(since, bucket_secs, tz).await.map_err(internal)?;
        Ok(TtftSeriesResp { since, bucket_secs, points, summary, recent })
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct BreakdownQuery {
    #[serde(default = "default_series_hours")]
    hours: i64,
    /// `model` 或 `account`。
    #[serde(default = "default_breakdown_by")]
    by: String,
}

fn default_breakdown_by() -> String {
    "model".into()
}

#[derive(Serialize)]
struct BreakdownResp {
    since: i64,
    by: String,
    rows: Vec<store::BreakdownRow>,
    /// 全部分组（不止返回的前 12 行）缓存省下的钱合计（USD），见 `BreakdownRow::cache_saved_usd`。
    cache_saved_usd_total: f64,
}

#[derive(Deserialize)]
pub(super) struct RejectionsQuery {
    #[serde(default = "default_rejections_hours")]
    hours: i64,
}

fn default_rejections_hours() -> i64 {
    1
}

#[derive(Serialize)]
struct RejectionKind {
    kind: String,
    count: i64,
}

#[derive(Serialize)]
struct RejectionsResp {
    since: i64,
    total: i64,
    rows: Vec<RejectionKind>,
}

/// 近几小时本地拒绝的条数，按原因分类：设备 / 会话 / RPM 三道闸加了之后，被拒的请求只在流水里
/// 记成本地 429，概览得有个数才知道有号被打满。同样过 20 秒缓存。
pub(super) async fn get_rejections(
    State(state): State<AppState>,
    Query(q): Query<RejectionsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let max_hours = store::USAGE_LOG_RETENTION_SECS / 3600;
    let hours = q.hours.clamp(1, max_hours);
    let since = chrono::Utc::now().timestamp() - hours * 3600;
    cached_metrics(format!("rejections:{hours}"), async move {
        let rows: Vec<RejectionKind> = state
            .store
            .local_rejections(since)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|(kind, count)| RejectionKind { kind, count })
            .collect();
        let total = rows.iter().map(|r| r.count).sum();
        Ok(RejectionsResp { since, total, rows })
    })
    .await
}

/// 这段时间按模型或按账号拆开的延迟与缓存：趋势对话框下面那张「谁在拖后腿」的表。
/// 这一条要回表读模型、账号、状态与全部 token 列，是三条里最贵的，同样过 20 秒缓存。
pub(super) async fn get_usage_breakdown(
    State(state): State<AppState>,
    Query(q): Query<BreakdownQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let max_hours = store::USAGE_LOG_RETENTION_SECS / 3600;
    let hours = q.hours.clamp(1, max_hours);
    let since = chrono::Utc::now().timestamp() - hours * 3600;
    let by = match q.by.as_str() {
        "account" => store::BreakdownBy::Account,
        _ => store::BreakdownBy::Model,
    };
    let by_name = if by == store::BreakdownBy::Account { "account" } else { "model" };
    cached_metrics(format!("breakdown:{hours}:{by_name}"), async move {
        // 合计要覆盖全部分组，先不截断；前端只拿前 12 行。
        let mut rows =
            state.store.usage_breakdown(since, by, usize::MAX).await.map_err(internal)?;
        let cache_saved_usd_total = rows.iter().map(|r| r.cache_saved_usd).sum();
        rows.truncate(12);
        Ok(BreakdownResp { since, by: by_name.into(), rows, cache_saved_usd_total })
    })
    .await
}
