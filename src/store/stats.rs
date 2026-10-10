//! 统计报表：RPM、费用、缓存与 TTFT 趋势、用量明细、凭证统计。

use super::*;

/// 一批流水的整体口径：条数、花费合计、以及可作翻页锚点的最大 id。
///
/// 缓存命中率趋势里的一个**小时桶**：`ts` 是这一小时的起点（Unix 秒）。
///
/// 回两个原始数，比率由界面算：一个 300 token 的小时里的「命中 0%」与 17K 前缀那种
/// 小时里的「命中 94%」是两件事，光看比率判断不了。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheBucket {
    pub ts: i64,
    /// 全部输入 token（含缓存命中与缓存写入）。
    pub input_tokens: i64,
    /// 其中来自缓存的部分（`cache_read_tokens`）。
    pub cached_tokens: i64,
    /// 其中写进缓存的部分（`cache_creation_tokens`，旧行按 5m + 1h 两段之和）。命中率一个数
    /// 分不出「没命中」和「没写入」，拆开才知道该查什么：写入多命中少是前缀每轮在变，
    /// 写入命中都少是客户端根本没标断点。
    pub written_tokens: i64,
}

impl CacheBucket {
    pub(super) fn empty(ts: i64) -> Self {
        Self { ts, input_tokens: 0, cached_tokens: 0, written_tokens: 0 }
    }
}

/// TTFT（首字时延）趋势里的一个**小时桶**。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TtftBucket {
    pub ts: i64,
    /// 算术平均，留给老口径对照；偶发的几十秒超时会把它拉高，看 p50 / p95。
    pub avg_ms: i64,
    /// 中位数（nearest-rank）。
    pub p50_ms: i64,
    /// 95 分位（nearest-rank）。
    pub p95_ms: i64,
    /// 参与统计的成功请求数（status = 200 且记了 TTFT）。
    pub count: i64,
    /// 输出吞吐（token / 秒）：`Σ output_tokens / Σ (total_ms − ttft_ms)`，只算两者都有且
    /// 生成阶段时长为正的请求；没有这样的请求为 `None`。
    pub tokens_per_sec: Option<f64>,
}

/// 趋势接口的一次返回：各桶、整窗口合计、近 60 分钟合计。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheReport {
    pub points: Vec<CacheBucket>,
    pub summary: CacheBucket,
    pub recent: CacheBucket,
}

/// 同上，延迟那份。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TtftReport {
    pub points: Vec<TtftBucket>,
    pub summary: TtftBucket,
    pub recent: TtftBucket,
}

/// 一条请求里缓存省下的钱：命中与写入都按原价算一遍减去实际计价。命中省 0.9 倍输入价，
/// 5 分钟档写入多付 0.25 倍、1 小时档多付 1 倍；模型认不出价目时记 0。
pub(super) fn cache_saved_usd(
    model: Option<&str>,
    plain: i64,
    cached: i64,
    creation: Option<i64>,
    c5: Option<i64>,
    c1: Option<i64>,
) -> f64 {
    use crate::pricing::{Usage, estimate_usd};
    let written = creation.unwrap_or(c5.unwrap_or(0) + c1.unwrap_or(0));
    let actual = estimate_usd(Usage {
        model,
        input_tokens: Some(plain),
        cache_read_tokens: Some(cached),
        cache_creation_total: creation,
        cache_5m_tokens: c5,
        cache_1h_tokens: c1,
        ..Default::default()
    });
    let baseline = estimate_usd(Usage {
        model,
        input_tokens: Some(plain + cached + written),
        ..Default::default()
    });
    match (actual, baseline) {
        (Some(a), Some(b)) => b - a,
        _ => 0.0,
    }
}

/// 拆分维度，见 [`CredentialStore::usage_breakdown`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakdownBy {
    Model,
    Account,
}

/// 按模型或按账号拆开的一行：这段时间里它的请求数、延迟分位、吞吐与缓存三段 token。
/// 池子平均看不出「谁在拖后腿」，这张表就是给这个问题的。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BreakdownRow {
    /// 模型名，或凭证 id 的十进制串。
    pub key: String,
    /// 展示名：模型名本身，或凭证的 label（已删的号是 `#<id>`）。
    pub label: String,
    /// 按账号拆时该号的套餐（`Max 5x` / `Pro` …）；按模型拆或号已删为 `None`。Max 号和 Pro 号
    /// 上游的排队本来就不同，混在一起比延迟没有意义。
    pub tier: Option<String>,
    /// 这段时间里的全部请求数（含失败的）。
    pub requests: i64,
    /// 缓存给这一组省下的钱（USD）：把命中与写入都按原价算一遍再减去实际——命中按十分之一
    /// 计价省下来的，减掉写入按 1.25 倍（1 小时档 2 倍）多付的。可能为负：只写不命中就是亏。
    /// 模型认不出价目的行不计。
    pub cache_saved_usd: f64,
    /// 延迟统计（只算成功且记了 TTFT 的请求），`ts` 一律是窗口起点。
    pub latency: TtftBucket,
    /// 缓存三段 token（所有请求）。
    pub cache: CacheBucket,
}

/// 单账号用量统计的一格：一个时间桶，或整个窗口的合计（`ts` 是桶起点 / 窗口起点）。
///
/// token 四项与官方 `usage` 同口径、互不重叠，**不加权**；费用是写流水时按价目表估的等价 API
/// 费用，模型认不出价目的记录按 0 计。
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct CredentialStatsBucket {
    pub ts: i64,
    /// 全部请求数（含失败与本地拒绝）。
    pub requests: i64,
    /// 其中非 2xx 的条数。
    pub errors: i64,
    /// 其中 luban 本地拒掉、没发到上游的条数（`rewrites` 以 `rejected_locally` 开头）。
    pub rejected: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

impl CredentialStatsBucket {
    pub(super) fn add(&mut self, row: &CredentialStatsBucket) {
        self.requests += row.requests;
        self.errors += row.errors;
        self.rejected += row.rejected;
        self.input_tokens += row.input_tokens;
        self.output_tokens += row.output_tokens;
        self.cache_write_tokens += row.cache_write_tokens;
        self.cache_read_tokens += row.cache_read_tokens;
        self.cost_usd += row.cost_usd;
    }
}

/// 单账号按某个维度拆开的一组。`key` 是模型名 / device_id / 来访 UA / 状态码的原值，
/// 缺失时为空串（没带设备身份的裸请求、没带 UA 的来访）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct CredentialStatsGroup {
    pub key: String,
    pub requests: i64,
    pub errors: i64,
    /// 四项 token 之和（同 [`CredentialStatsBucket`] 的口径）。
    pub tokens: i64,
    pub cost_usd: f64,
    /// 这一组最近一条请求的时刻（Unix 秒）。
    pub last_ts: i64,
}

impl CredentialStatsGroup {
    pub(super) fn add(&mut self, row: &CredentialStatsBucket) {
        self.requests += row.requests;
        self.errors += row.errors;
        self.tokens +=
            row.input_tokens + row.output_tokens + row.cache_write_tokens + row.cache_read_tokens;
        self.cost_usd += row.cost_usd;
        self.last_ts = self.last_ts.max(row.ts);
    }
}

/// [`CredentialStore::credential_stats`] 的结果。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CredentialStats {
    /// 有请求的桶（空桶不返回，前端自己补齐）。
    pub points: Vec<CredentialStatsBucket>,
    /// 整个窗口的合计。
    pub summary: CredentialStatsBucket,
    pub by_model: Vec<CredentialStatsGroup>,
    pub by_device: Vec<CredentialStatsGroup>,
    pub by_client: Vec<CredentialStatsGroup>,
    pub by_status: Vec<CredentialStatsGroup>,
}

/// 把汇总格子按展示桶（桶宽与时区偏移见 [`display_grid`]）合并，按时间升序。
pub(super) fn regroup<'a>(
    cells: impl IntoIterator<Item = &'a RollupCell>,
    bucket_secs: i64,
    tz_offset_secs: i64,
) -> std::collections::BTreeMap<i64, RollupAgg> {
    let (bucket_secs, tz_offset_secs) = display_grid(bucket_secs, tz_offset_secs);
    let mut out: std::collections::BTreeMap<i64, RollupAgg> = Default::default();
    for c in cells {
        out.entry(display_bucket(c.bucket, bucket_secs, tz_offset_secs)).or_default().add(&c.agg);
    }
    out
}

/// 一组格子的合计。
pub(super) fn total<'a>(cells: impl IntoIterator<Item = &'a RollupCell>) -> RollupAgg {
    let mut out = RollupAgg::default();
    for c in cells {
        out.add(&c.agg);
    }
    out
}

use std::collections::{BTreeMap, HashMap};

use super::CredentialStore;
use super::{RPM_WINDOW_SECS, RollupAgg, RollupCell, RollupDim, first_bucket};
use anyhow::Result;
use sqlx::{PgPool, Row};

impl CredentialStore {
    /// 每个凭证最近 60 秒的请求数，即当前 RPM（cred_id → 条数）。窗口内没有请求的凭证
    /// **不出现**在结果里，调用方按 0 处理。
    ///
    /// 口径与 `requests_5h`/`requests_7d` 完全一致——数的是 `usage_logs` 的流水条数，失败的
    /// （4xx/5xx）同样计入，只是窗口固定为 60 秒。**刻意不复用内存里的限流计数器**：它只数
    /// 裸请求，且重启即清零，拿来当 RPM 会系统性地偏小。
    pub async fn recent_rpm(&self) -> Result<HashMap<i64, i64>> {
        // 时间下界用库的时钟，与写入侧（insert_usage_log_at）同源：两边若各取各的时钟，
        // 机器时间稍有偏差就会把刚写进去的那几条数丢或多数。
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT cred_id, COUNT(*) FROM usage_logs
              WHERE ts >= unixepoch() - $1 AND cred_id IS NOT NULL
              GROUP BY cred_id",
        )
        .bind(RPM_WINDOW_SECS)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// **全局 RPM**：最近 60 秒经 luban 转发的请求总数。
    ///
    /// 口径与 [`Self::recent_rpm`] 逐条对齐（同一张表、同一个窗口、同样只数落到某个账号头上
    /// 的那些），所以它恒等于各账号 RPM 之和。代价是没选到号就失败的请求不计入：它们压根
    /// 没发出去。
    pub async fn total_rpm(&self) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM usage_logs WHERE ts >= unixepoch() - $1 AND cred_id IS NOT NULL",
        )
        .bind(RPM_WINDOW_SECS)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 一批号合计的当前 RPM：代理和用户看的「实时流量」，只数自己名下的号。口径同
    /// [`Self::total_rpm`]，所以 `ids` 取全部号时两者相等。
    pub async fn rpm_of_creds(&self, ids: &[i64]) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM usage_logs WHERE cred_id = ANY($1) AND ts >= unixepoch() - $2",
        )
        .bind(ids)
        .bind(RPM_WINDOW_SECS)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 单个凭证当前的 RPM；口径同 [`Self::recent_rpm`]，无请求时为 0。
    pub async fn recent_rpm_of(&self, cred_id: i64) -> Result<i64> {
        // 走 idx_usage_logs_cred_usage 的 (cred_id, ts) 前缀，直接定位到该号最近 60 秒那一小段。
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM usage_logs WHERE cred_id = $1 AND ts >= unixepoch() - $2",
        )
        .bind(cred_id)
        .bind(RPM_WINDOW_SECS)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 每个凭证最近一次被使用（有转发记录）的时间（cred_id → Unix 秒）。读账本，
    /// 不扫流水——流水会被裁剪，账本才是终身口径（下同，cost_by_cred / cost_of 亦然）。
    pub async fn last_used(&self) -> Result<HashMap<i64, i64>> {
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT cred_id, last_used_at FROM credential_stats WHERE last_used_at IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// 单个凭证最近一次被使用的时间；无记录时为 `None`。口径同 [`Self::last_used`]。
    pub async fn last_used_at(&self, cred_id: i64) -> Result<Option<i64>> {
        // 账本行可能还不存在（该凭证从未有过流水），optional 后拍平。
        let ts: Option<Option<i64>> =
            sqlx::query_scalar("SELECT last_used_at FROM credential_stats WHERE cred_id = $1")
                .bind(cred_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(ts.flatten())
    }

    /// 每个凭证累计的等价 API 费用（cred_id → USD 合计）。
    pub async fn cost_by_cred(&self) -> Result<HashMap<i64, f64>> {
        let rows: Vec<(i64, f64)> =
            sqlx::query_as("SELECT cred_id, cost_total_usd FROM credential_stats")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    /// 单个凭证累计的等价 API 费用（USD）；无记录时为 0。口径同 [`Self::cost_by_cred`]。
    pub async fn cost_of(&self, cred_id: i64) -> Result<f64> {
        Ok(sqlx::query_scalar(
            "SELECT COALESCE((SELECT cost_total_usd FROM credential_stats WHERE cred_id = $1), 0)",
        )
        .bind(cred_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 缓存命中率趋势的桶。回三个原始数（输入、命中、写入），比率由前端算。
    ///
    /// `bucket_secs` 是桶宽；`tz_offset_secs` 是本地时区相对 UTC 的偏移，按天分桶时桶边界
    /// 落在**本地**零点上。两者都规整到 15 分钟的整数倍（见 `display_grid`）。读预聚合。
    pub async fn cache_series(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<Vec<CacheBucket>> {
        let cells = self.rollup_cells(RollupDim::All, since).await?;
        Ok(regroup(&cells, bucket_secs, tz_offset_secs)
            .into_iter()
            .filter(|(_, agg)| agg.input_tokens > 0)
            .map(|(ts, agg)| agg.cache(ts))
            .collect())
    }

    /// 趋势接口一次要的三样：各桶、整窗口合计、近 60 分钟合计。整窗口合计直接把各桶加起来
    /// （桶是窗口的划分），近 1 小时按库的时钟另取一段（与写入侧同源）。
    pub async fn cache_report(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<CacheReport> {
        let points = self.cache_series(since, bucket_secs, tz_offset_secs).await?;
        let summary = points.iter().fold(CacheBucket::empty(since), |mut acc, b| {
            acc.input_tokens += b.input_tokens;
            acc.cached_tokens += b.cached_tokens;
            acc.written_tokens += b.written_tokens;
            acc
        });
        let now = db_now(&self.pool).await?;
        let recent = self.cache_summary(now - 3600).await?;
        Ok(CacheReport { points, summary, recent })
    }

    /// `since` 起到现在的缓存三段 token 合计（一个桶），`ts` 是 `since`。
    pub async fn cache_summary(&self, since: i64) -> Result<CacheBucket> {
        Ok(total(&self.rollup_cells(RollupDim::All, since).await?).cache(since))
    }

    /// 趋势接口一次要的三样：各桶、整窗口、近 60 分钟——读一次，分三路汇总。
    pub async fn ttft_report(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<TtftReport> {
        let recent_since = db_now(&self.pool).await? - 3600;
        // 近 1 小时通常落在窗口里；窗口比它还短时多读那一段，再按各自的起点分开。
        let cells = self.rollup_cells(RollupDim::All, since.min(recent_since)).await?;
        let window: Vec<&RollupCell> =
            cells.iter().filter(|c| c.bucket >= first_bucket(since)).collect();
        let recent = cells.iter().filter(|c| c.bucket >= first_bucket(recent_since));
        Ok(TtftReport {
            points: regroup(window.iter().copied(), bucket_secs, tz_offset_secs)
                .into_iter()
                .filter(|(_, agg)| agg.lat_count > 0)
                .map(|(ts, agg)| agg.latency(ts))
                .collect(),
            summary: total(window.iter().copied()).latency(since),
            recent: total(recent).latency(recent_since),
        })
    }

    /// TTFT（首字时延）趋势的桶：每桶平均、p50、p95、请求数与输出吞吐。
    #[cfg(test)]
    pub async fn ttft_series(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<Vec<super::TtftBucket>> {
        Ok(self.ttft_report(since, bucket_secs, tz_offset_secs).await?.points)
    }

    /// `since` 起到现在的延迟汇总（一个桶），`ts` 是 `since`。线上走 [`Self::ttft_report`]
    /// 一次拿齐，这个只给测试核对。
    #[cfg(test)]
    pub async fn ttft_summary(&self, since: i64) -> Result<super::TtftBucket> {
        Ok(total(&self.rollup_cells(RollupDim::All, since).await?).latency(since))
    }

    /// `since` 起按模型或按账号拆开的用量：每组的请求数、延迟分位与吞吐、缓存三段 token，
    /// 按请求数降序，最多 `limit` 行。读对应那一维的预聚合。
    pub async fn usage_breakdown(
        &self,
        since: i64,
        by: BreakdownBy,
        limit: usize,
    ) -> Result<Vec<BreakdownRow>> {
        let dim = match by {
            BreakdownBy::Model => RollupDim::Model,
            BreakdownBy::Account => RollupDim::Cred,
        };
        // 账号名与套餐档：账号表就几十行，一次读进来。
        let creds: HashMap<String, (String, Option<String>)> = if by == BreakdownBy::Account {
            let rows: Vec<(i64, String, Option<String>)> =
                sqlx::query_as("SELECT id, label, tier FROM credentials")
                    .fetch_all(&self.pool)
                    .await?;
            rows.into_iter().map(|(id, label, tier)| (id.to_string(), (label, tier))).collect()
        } else {
            HashMap::new()
        };
        // 键 → (窗口内最早那一桶记下的名字, 汇总)。格子按桶升序来，第一次见到的就是最早的。
        let mut groups: HashMap<String, (String, RollupAgg)> = HashMap::new();
        for c in self.rollup_cells(dim, since).await? {
            groups.entry(c.key).or_insert_with(|| (c.label, RollupAgg::default())).1.add(&c.agg);
        }
        let mut out: Vec<BreakdownRow> = groups
            .into_iter()
            .map(|(key, (recorded, agg))| {
                let (label, tier) = match by {
                    BreakdownBy::Model => (key.clone(), None),
                    BreakdownBy::Account => match creds.get(&key) {
                        Some((label, tier)) => (label.clone(), tier.clone()),
                        // 已删账号退回汇总里记下的名字（窗口内最早那一桶第一条流水的），
                        // 那也没有就记 `#id`。
                        None if !recorded.is_empty() => (recorded, None),
                        None => (format!("#{}", if key.is_empty() { "?" } else { &key }), None),
                    },
                };
                BreakdownRow {
                    label,
                    tier,
                    requests: agg.requests,
                    cache_saved_usd: agg.saved_usd,
                    latency: agg.latency(since),
                    cache: agg.cache(since),
                    key,
                }
            })
            .collect();
        out.sort_by(|a, b| b.requests.cmp(&a.requests).then_with(|| a.key.cmp(&b.key)));
        out.truncate(limit);
        Ok(out)
    }

    /// 单个账号 `since` 起的用量统计：按时间分桶（桶宽与时区偏移同 [`Self::ttft_report`]）、
    /// 整个窗口的合计，以及按模型 / 设备 / 来访客户端 / 状态码四个维度拆开的分组（各自按请求数
    /// 降序、最多 `group_limit` 组）。一次扫描把这几路都汇总出来——走 `idx_usage_logs_cred_usage`
    /// 的前缀卡住账号与起点，在 Rust 里聚合比拼五条 GROUP BY 省一半扫描。
    pub async fn credential_stats(
        &self,
        cred_id: i64,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
        group_limit: usize,
    ) -> Result<CredentialStats> {
        let bucket_secs = bucket_secs.max(1);
        let rows = sqlx::query(
            "SELECT ts, status, COALESCE(model, ''), COALESCE(device_id, ''), COALESCE(ua, ''),
                    COALESCE(input_tokens, 0), COALESCE(output_tokens, 0),
                    cache_creation_tokens, cache_5m_tokens, cache_1h_tokens,
                    COALESCE(cache_read_tokens, 0), COALESCE(cost_usd, 0),
                    CASE WHEN rewrites LIKE 'rejected_locally%' THEN 1::BIGINT ELSE 0::BIGINT END
               FROM usage_logs
              WHERE cred_id = $1 AND ts >= $2",
        )
        .bind(cred_id)
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        let mut buckets: BTreeMap<i64, CredentialStatsBucket> = Default::default();
        let mut summary = CredentialStatsBucket { ts: since, ..Default::default() };
        let mut by_model: HashMap<String, CredentialStatsGroup> = Default::default();
        let mut by_device: HashMap<String, CredentialStatsGroup> = Default::default();
        let mut by_client: HashMap<String, CredentialStatsGroup> = Default::default();
        let mut by_status: HashMap<String, CredentialStatsGroup> = Default::default();
        for r in &rows {
            let ts: i64 = r.try_get(0)?;
            let status: i64 = r.try_get(1)?;
            let model: String = r.try_get(2)?;
            let device: String = r.try_get(3)?;
            let ua: String = r.try_get(4)?;
            let creation: Option<i64> = r.try_get(7)?;
            let c5: Option<i64> = r.try_get(8)?;
            let c1: Option<i64> = r.try_get(9)?;
            let row = CredentialStatsBucket {
                ts,
                requests: 1,
                errors: i64::from(!(200..300).contains(&status)),
                rejected: r.try_get(12)?,
                input_tokens: r.try_get(5)?,
                output_tokens: r.try_get(6)?,
                // 老记录只有细分档没有合计列时，拿两档相加兜底。
                cache_write_tokens: creation.unwrap_or(c5.unwrap_or(0) + c1.unwrap_or(0)),
                cache_read_tokens: r.try_get(10)?,
                cost_usd: r.try_get(11)?,
            };
            let bucket =
                ((ts + tz_offset_secs).div_euclid(bucket_secs)) * bucket_secs - tz_offset_secs;
            buckets
                .entry(bucket)
                .or_insert(CredentialStatsBucket { ts: bucket, ..Default::default() })
                .add(&row);
            summary.add(&row);
            for (groups, key) in [
                (&mut by_model, model),
                (&mut by_device, device),
                (&mut by_client, ua),
                (&mut by_status, status.to_string()),
            ] {
                groups
                    .entry(key.clone())
                    .or_insert_with(|| CredentialStatsGroup { key, ..Default::default() })
                    .add(&row);
            }
        }
        let ranked = |groups: HashMap<String, CredentialStatsGroup>| {
            let mut out: Vec<_> = groups.into_values().collect();
            out.sort_by(|a, b| b.requests.cmp(&a.requests).then_with(|| a.key.cmp(&b.key)));
            out.truncate(group_limit);
            out
        };
        Ok(CredentialStats {
            points: buckets.into_values().collect(),
            summary,
            by_model: ranked(by_model),
            by_device: ranked(by_device),
            by_client: ranked(by_client),
            by_status: ranked(by_status),
        })
    }

    /// `since` 起本地拒绝的条数，按原因分类（`rewrites` 里 `rejected_locally:<kind>` 的 kind；
    /// 没分类的算 `other`），按条数降序。给概览「近 1 小时被拒了多少、为什么」用。
    pub async fn local_rejections(&self, since: i64) -> Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT rewrites, COUNT(*) FROM usage_logs
              WHERE ts >= $1 AND rewrites LIKE 'rejected_locally%'
              GROUP BY rewrites",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        let mut counts: HashMap<String, i64> = Default::default();
        for (tag, n) in rows {
            let kind =
                tag.split_once(':').map(|(_, k)| k.to_string()).unwrap_or_else(|| "other".into());
            *counts.entry(kind).or_default() += n;
        }
        let mut out: Vec<(String, i64)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }
}

/// 库的当前时刻（`unixepoch()`），与写入侧的默认时间戳同源。
async fn db_now(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT unixepoch()").fetch_one(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::super::usage::tests::{db_now, log_row, store_with};
    use super::super::{Forensics, UsageLogQuery, UsageRecord};
    use super::*;

    /// RPM 只数最近 60 秒，且不跨账号；窗口外的老流水与从未发过请求的号都不得混进来。
    ///
    /// 时间基准取的是库里的 `unixepoch()`（与写入侧同源），所以这条用例也顺带钉住
    /// 「两边同一个时钟」。
    #[sqlx::test]
    async fn recent_rpm_counts_only_the_last_minute(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let now = db_now(&store).await;
        let hit = async |cred_id, ts| {
            let rec = UsageRecord { cred_id: Some(cred_id), ..Default::default() };
            store.insert_usage_log_at(&rec, Some(ts)).await.unwrap();
        };
        hit(a, now).await;
        hit(a, now - 30).await;
        hit(a, now - 120).await; // 窗口外
        hit(b, now - 5).await;

        let rpm = store.recent_rpm().await.unwrap();
        assert_eq!(rpm.get(&a).copied(), Some(2), "两分钟前那条不在 60 秒窗口内");
        assert_eq!(rpm.get(&b).copied(), Some(1), "RPM 不得跨账号串");
        assert_eq!(store.recent_rpm_of(a).await.unwrap(), 2, "单账号入口须与批量口径一致");
        assert_eq!(store.recent_rpm_of(b).await.unwrap(), 1);

        // 从未发过请求的号压根不进 map（调用方按 0 处理），单账号入口直接给 0。
        let c = store.insert("c", None, "tc", "rc", 0, None, None, 1).await.unwrap().id;
        assert_eq!(store.recent_rpm().await.unwrap().get(&c), None);
        assert_eq!(store.recent_rpm_of(c).await.unwrap(), 0);

        // 全局 RPM 必须恰好是各账号之和。
        assert_eq!(store.total_rpm().await.unwrap(), 3);
        assert_eq!(
            store.total_rpm().await.unwrap(),
            store.recent_rpm().await.unwrap().values().sum::<i64>(),
            "全局与逐账号必须同口径（同一张表、同一个窗口）"
        );

        // 没落到任何账号头上的流水（选号前就失败的那些）不计入——它们压根没发出去。
        store
            .insert_usage_log_at(&UsageRecord { cred_id: None, ..Default::default() }, Some(now))
            .await
            .unwrap();
        assert_eq!(store.total_rpm().await.unwrap(), 3, "无账号的流水不进全局 RPM");

        // 按一批号合计：只数这批，和逐账号同口径。
        assert_eq!(store.rpm_of_creds(&[a]).await.unwrap(), 2);
        assert_eq!(store.rpm_of_creds(&[a, b, c]).await.unwrap(), 3);
        assert_eq!(store.rpm_of_creds(&[]).await.unwrap(), 0, "名下没有号就是 0");
    }

    /// 单账号的「最近使用 / 累计费用」与全量聚合同口径，无日志时分别是 None 与 0。
    #[sqlx::test]
    async fn single_cred_stats_match_batch(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        log_row(&store, a, 1_000, 1.5, None, None).await;
        log_row(&store, a, 2_000, 2.5, None, None).await;
        log_row(&store, b, 3_000, 7.0, None, None).await;
        let c = store.insert("c", None, "tc", "rc", 0, None, None, 1).await.unwrap().id; // 从未被用过

        let last = store.last_used().await.unwrap();
        let costs = store.cost_by_cred().await.unwrap();
        for id in [a, b] {
            assert_eq!(store.last_used_at(id).await.unwrap(), last.get(&id).copied());
            assert_eq!(store.cost_of(id).await.unwrap(), costs[&id]);
        }
        assert_eq!(store.last_used_at(a).await.unwrap(), Some(2_000));
        assert_eq!(store.cost_of(a).await.unwrap(), 4.0);
        assert_eq!(store.last_used_at(c).await.unwrap(), None, "无日志时是 None 而非 0");
        assert_eq!(store.cost_of(c).await.unwrap(), 0.0);
    }

    /// 分位数来自指数直方图（OTel scale 5）：与 nearest-rank 精确值的相对误差在 1.1% 以内。
    pub(in crate::store) fn assert_close(est: i64, exact: i64) {
        let tol = (exact as f64 * 0.011).max(1.0);
        assert!((est - exact).abs() as f64 <= tol, "估计 {est} 偏离精确值 {exact} 超过 1.1%");
    }

    /// 延迟与缓存的趋势口径：分位数 nearest-rank（直方图近似）、桶边界按时区偏移切、
    /// 吞吐只算生成阶段、汇总对整窗口算而不是各桶平均；按模型 / 按账号的拆分同一套数。
    /// 全部读预聚合（usage_rollup），不扫流水。
    ///
    /// SQLite 版还用 EXPLAIN QUERY PLAN 钉住「按 (dim, bucket) 主键范围扫」，PG 版不移植那一条
    /// （主键就是 (dim, bucket, key)，计划随统计信息变）。
    #[sqlx::test]
    async fn latency_and_cache_series_use_percentiles_and_local_buckets(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let now = db_now(&store).await;
        // 最近那批记录（[last-104, last]）必须落在同一个小时桶里、又在真实时间的近一小时内。
        // 整点后十来分钟内照 now-600 写会跨过整点，那时就整批放到上一个小时的末尾。
        let hour_start = now - now.rem_euclid(3600);
        let last = if now - hour_start >= 700 { now - 500 } else { hour_start - 1 };
        let rec = |cred: i64,
                   model: &str,
                   status: u16,
                   ttft: i64,
                   total: i64,
                   out: i64,
                   inp: i64,
                   read: i64,
                   write: i64| UsageRecord {
            cred_id: Some(cred),
            cred_label: if cred == a { "a".into() } else { "b".into() },
            model: Some(model.into()),
            status,
            has_usage: true,
            input_tokens: Some(inp),
            output_tokens: Some(out),
            cache_creation_tokens: Some(write),
            cache_read_tokens: Some(read),
            ttft_ms: Some(ttft),
            total_ms: Some(total),
            ..Default::default()
        };
        // 最近半小时：opus 五条成功（TTFT 100..500，吞吐 1000 token / 1s），一条失败（不进延迟）。
        for (i, ttft) in [100i64, 200, 300, 400, 500].iter().enumerate() {
            store
                .insert_usage_log_at(
                    &rec(a, "claude-opus-5", 200, *ttft, ttft + 1000, 1000, 100, 60, 20),
                    Some(last - 100 - i as i64),
                )
                .await
                .unwrap();
        }
        store
            .insert_usage_log_at(
                &rec(a, "claude-opus-5", 500, 9000, 9000, 0, 100, 0, 0),
                Some(last),
            )
            .await
            .unwrap();
        // 三小时前：sonnet 在 b 上一条慢的，没有缓存。
        store
            .insert_usage_log_at(
                &rec(b, "claude-sonnet-5", 200, 4000, 4000, 0, 100, 0, 0),
                Some(now - 3 * 3600),
            )
            .await
            .unwrap();

        // 分位数：p50 = 300、p95 = 500（nearest-rank：ceil(5×0.95)=5）、平均 300；吞吐 1000 tok/s。
        let recent = store.ttft_summary(now - 3600).await.unwrap();
        assert_eq!((recent.count, recent.avg_ms), (5, 300), "条数与平均是精确累加的");
        assert_close(recent.p50_ms, 300);
        assert_close(recent.p95_ms, 500);
        assert!(
            (recent.tokens_per_sec.unwrap() - 1000.0).abs() < 1e-6,
            "{:?}",
            recent.tokens_per_sec
        );
        // 整窗口（含三小时前那条）：六条，p50 取第 3 个 = 300，p95 取第 6 个 = 4000。
        let all = store.ttft_summary(now - 6 * 3600).await.unwrap();
        assert_eq!(all.count, 6);
        assert_close(all.p50_ms, 300);
        assert_close(all.p95_ms, 4000);
        assert_eq!(all.ts, now - 6 * 3600);
        // 逐小时桶：两个桶，慢的那条在自己的桶里；桶起点按偏移对齐。
        let hourly = store.ttft_series(now - 6 * 3600, 3600, 0).await.unwrap();
        assert_eq!(hourly.len(), 2, "{hourly:?}");
        assert_eq!(hourly[0].count, 1);
        assert_close(hourly[0].p95_ms, 4000);
        assert_eq!(hourly[0].tokens_per_sec, None, "没有输出 token 就没有吞吐");
        assert_eq!(hourly[0].ts % 3600, 0);
        let tz = 8 * 3600;
        let shifted = store.ttft_series(now - 6 * 3600, 86400, tz).await.unwrap();
        assert!(
            shifted.iter().all(|b| (b.ts + tz) % 86400 == 0),
            "日桶边界落在本地零点: {shifted:?}"
        );
        // 一次扫描出三样，与分开算的一致。
        let report = store.ttft_report(now - 6 * 3600, 3600, 0).await.unwrap();
        assert_eq!(report.points.len(), 2);
        assert_eq!((report.summary.count, report.recent.count), (6, 5));
        assert_close(report.summary.p95_ms, 4000);
        assert_close(report.recent.p50_ms, 300);
        // 一个空集：全零、吞吐 None。
        let none = store.ttft_summary(now + 10).await.unwrap();
        assert_eq!((none.count, none.p50_ms), (0, 0));
        assert_eq!(none.tokens_per_sec, None);

        // 缓存：近一小时六条——五条各 180、一条 100 → 1000；命中 300、写入 100。
        let cache = store.cache_summary(now - 3600).await.unwrap();
        assert_eq!(
            (cache.input_tokens, cache.cached_tokens, cache.written_tokens),
            (1000, 300, 100)
        );
        assert_eq!(cache.ts, now - 3600);
        let cache_all = store.cache_summary(now - 6 * 3600).await.unwrap();
        assert_eq!(cache_all.input_tokens, 1100);
        let cache_hourly = store.cache_series(now - 6 * 3600, 3600, 0).await.unwrap();
        assert_eq!(cache_hourly.len(), 2);
        assert_eq!(cache_hourly[1].written_tokens, 100);
        let cache_none = store.cache_summary(now + 10).await.unwrap();
        assert_eq!(cache_none.input_tokens, 0);
        let cache_report = store.cache_report(now - 6 * 3600, 3600, 0).await.unwrap();
        assert_eq!(cache_report.points.len(), 2);
        assert_eq!(cache_report.summary.input_tokens, 1100, "合计是各桶之和");
        assert_eq!(cache_report.recent.cached_tokens, 300);

        // 拆分：按模型请求数降序（opus 6 条在前），延迟只算成功的；按账号带 label，limit 生效。
        let by_model = store.usage_breakdown(now - 6 * 3600, BreakdownBy::Model, 10).await.unwrap();
        assert_eq!(by_model.len(), 2);
        assert_eq!(by_model[0].key, "claude-opus-5");
        assert_eq!(by_model[0].requests, 6);
        assert_eq!(by_model[0].latency.count, 5);
        assert_close(by_model[0].latency.p50_ms, 300);
        assert_eq!(by_model[0].cache.cached_tokens, 300);
        assert_eq!(by_model[1].key, "claude-sonnet-5");
        assert_close(by_model[1].latency.p95_ms, 4000);
        // 省钱：opus-5 输入 $5/MTok，命中 300 省 0.9×5×300e-6 = 0.00135，写入 100（5m 档）多付
        // 0.25×5×100e-6 = 0.000125 → 0.001225；sonnet 那组没缓存是 0；按模型拆没有套餐。
        assert!(
            (by_model[0].cache_saved_usd - 0.001225).abs() < 1e-9,
            "{}",
            by_model[0].cache_saved_usd
        );
        assert_eq!(by_model[1].cache_saved_usd, 0.0);
        assert_eq!(by_model[0].tier, None);
        let _ = store.set_tier(a, Some("Max 5x")).await;
        let by_account =
            store.usage_breakdown(now - 6 * 3600, BreakdownBy::Account, 10).await.unwrap();
        assert_eq!(by_account[0].key, a.to_string());
        assert_eq!(by_account[0].label, "a");
        assert_eq!(by_account[1].label, "b");
        assert_eq!(by_account[1].tier, None);
        assert_eq!(
            store.usage_breakdown(now - 6 * 3600, BreakdownBy::Account, 1).await.unwrap().len(),
            1
        );
        // 删掉的号：流水还在，label 退回流水自带的账号名。
        assert!(store.delete(b).await.unwrap());
        let deleted =
            store.usage_breakdown(now - 6 * 3600, BreakdownBy::Account, 10).await.unwrap();
        assert!(deleted.iter().any(|r| r.key == b.to_string() && r.label == "b"), "{deleted:?}");
        // 流水里也没名字（极老的行）才退成 #id。
        let ghost = b + 100;
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_label: String::new(),
                    ..rec(ghost, "claude-sonnet-5", 200, 10, 20, 1, 1, 0, 0)
                },
                Some(now - 100),
            )
            .await
            .unwrap();
        let orphan = store.usage_breakdown(now - 3600, BreakdownBy::Account, 10).await.unwrap();
        assert!(orphan.iter().any(|r| r.label == format!("#{ghost}")), "{orphan:?}");

        // 本地拒绝按原因分类：`rejected_locally:<kind>` 归到 kind，裸的归 other，按条数降序。
        let reject = |tag: &str| UsageRecord {
            status: 429,
            forensics: Forensics { rewrites: Some(tag.into()), ..Default::default() },
            ..Default::default()
        };
        for tag in [
            "rejected_locally:device-limit",
            "rejected_locally:device-limit",
            "rejected_locally:session-limit",
            "rejected_locally",
        ] {
            store.insert_usage_log_at(&reject(tag), Some(now - 60)).await.unwrap();
        }
        store
            .insert_usage_log_at(&reject("rejected_locally:device-limit"), Some(now - 2 * 3600))
            .await
            .unwrap();
        let rejections = store.local_rejections(now - 3600).await.unwrap();
        assert_eq!(
            rejections,
            vec![
                ("device-limit".to_string(), 2),
                ("other".to_string(), 1),
                ("session-limit".to_string(), 1)
            ]
        );
        // 流水按模型与起点筛：拆分表点进来看明细走的就是这两个条件。
        let logs = store
            .query_usage_logs(UsageLogQuery {
                model: Some("claude-sonnet-5".into()),
                since: Some(now - 6 * 3600),
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !logs.is_empty() && logs.iter().all(|l| l.model.as_deref() == Some("claude-sonnet-5"))
        );
        let stats = store
            .usage_log_stats(UsageLogQuery {
                model: Some("claude-opus-5".into()),
                since: Some(now - 3600),
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(stats.total, 6);
    }
}
