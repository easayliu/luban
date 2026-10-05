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
    fn empty(ts: i64) -> Self {
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

/// 延迟原始行的查询：WHERE 与 `idx_usage_logs_latency` 的部分谓词逐字相同，规划器才会
/// 选它；测试里用 EXPLAIN QUERY PLAN 盯着这一点。
pub(super) const LATENCY_ROWS_SQL: &str = "SELECT ts, ttft_ms, total_ms, output_tokens
       FROM usage_logs
      WHERE ts >= ?1 AND ttft_ms IS NOT NULL AND status = 200";

/// 一条成功请求在延迟统计里要用的几个数，见 [`CredentialStore::ttft_series`]。
#[derive(Debug, Clone, Copy)]
struct LatencyRow {
    ts: i64,
    ttft_ms: i64,
    total_ms: Option<i64>,
    output_tokens: Option<i64>,
}

/// 一条请求里缓存省下的钱：命中与写入都按原价算一遍减去实际计价。命中省 0.9 倍输入价，
/// 5 分钟档写入多付 0.25 倍、1 小时档多付 1 倍；模型认不出价目时记 0。
fn cache_saved_usd(
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

/// nearest-rank 分位：`sorted` 已升序、非空，`p` 在 (0, 1]。
fn percentile(sorted: &[i64], p: f64) -> i64 {
    let rank = ((sorted.len() as f64) * p).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// 把一组成功请求汇总成一个 [`TtftBucket`]；空集返回全零、吞吐 `None`。
fn summarize_latency(ts: i64, rows: &[LatencyRow]) -> TtftBucket {
    if rows.is_empty() {
        return TtftBucket { ts, avg_ms: 0, p50_ms: 0, p95_ms: 0, count: 0, tokens_per_sec: None };
    }
    let mut sorted: Vec<i64> = rows.iter().map(|r| r.ttft_ms).collect();
    sorted.sort_unstable();
    let sum: i64 = sorted.iter().sum();
    let (mut out_tokens, mut gen_ms) = (0i64, 0i64);
    for r in rows {
        if let (Some(total), Some(out)) = (r.total_ms, r.output_tokens)
            && total > r.ttft_ms
            && out > 0
        {
            out_tokens += out;
            gen_ms += total - r.ttft_ms;
        }
    }
    TtftBucket {
        ts,
        avg_ms: sum / sorted.len() as i64,
        p50_ms: percentile(&sorted, 0.5),
        p95_ms: percentile(&sorted, 0.95),
        count: sorted.len() as i64,
        tokens_per_sec: (gen_ms > 0).then(|| out_tokens as f64 * 1000.0 / gen_ms as f64),
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
    fn add(&mut self, row: &CredentialStatsBucket) {
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
    fn add(&mut self, row: &CredentialStatsBucket) {
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

impl CredentialStore {
    /// 每个凭证最近 60 秒的请求数，即当前 RPM（cred_id → 条数）。窗口内没有请求的凭证
    /// **不出现**在结果里，调用方按 0 处理。
    ///
    /// 口径与 `requests_5h`/`requests_7d` 完全一致——数的是 `usage_logs` 的流水条数，
    /// 也就是真正发给上游的请求，失败的（4xx/5xx）同样计入，只是窗口固定为 60 秒。
    ///
    /// **刻意不复用 [`BareRateWindow`] 那个内存计数器**：它只数无 `metadata.user_id` 的
    /// 裸请求（带设备身份的一条都不进），且重启即清零，拿来当 RPM 会系统性地偏小。
    /// 而 60 秒的流水靠 `idx_usage_logs_ts` 只扫一小段范围，比那把锁贵不了多少。
    pub fn recent_rpm(&self) -> Result<HashMap<i64, i64>> {
        let conn = self.read_conn();
        // 时间下界用 SQLite 的时钟，与写入侧（insert_usage_log_at）同源：两边若各取各的
        // 时钟，机器时间稍有偏差就会把刚写进去的那几条数丢或多数。
        let mut stmt = conn.prepare(
            "SELECT cred_id, COUNT(*) FROM usage_logs
              WHERE ts >= unixepoch() - ?1 AND cred_id IS NOT NULL
              GROUP BY cred_id",
        )?;
        let rows =
            stmt.query_map([RPM_WINDOW_SECS], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, n) = row?;
            out.insert(cid, n);
        }
        Ok(out)
    }

    /// **全局 RPM**：最近 60 秒经 luban 转发的请求总数。
    ///
    /// 口径与 [`Self::recent_rpm`] 逐条对齐（同一张表、同一个窗口、同样只数落到某个账号头上
    /// 的那些），所以它恒等于各账号 RPM 之和——两个数摆在同一屏上，对不上会比看不到更让人
    /// 犯疑。代价是没选到号就失败的请求（全员限流、无可用凭证）不计入：它们压根没发出去。
    pub fn total_rpm(&self) -> Result<i64> {
        let conn = self.read_conn();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM usage_logs WHERE ts >= unixepoch() - ?1 AND cred_id IS NOT NULL",
            [RPM_WINDOW_SECS],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// 单个凭证当前的 RPM；口径同 [`Self::recent_rpm`]，无请求时为 0。
    pub fn recent_rpm_of(&self, cred_id: i64) -> Result<i64> {
        let conn = self.read_conn();
        // 这条走 idx_usage_logs_cred_usage 的 (cred_id, ts) 前缀，直接定位到该号最近 60 秒那一小段。
        let n = conn.query_row(
            "SELECT COUNT(*) FROM usage_logs WHERE cred_id = ?1 AND ts >= unixepoch() - ?2",
            params![cred_id, RPM_WINDOW_SECS],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// 每个凭证最近一次被使用（有转发记录）的时间（cred_id → Unix 秒）。读账本，
    /// 不扫流水——流水会被裁剪，账本才是终身口径（下同，cost_by_cred / cost_of 亦然）。
    pub fn last_used(&self) -> Result<HashMap<i64, i64>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT cred_id, last_used_at FROM credential_stats WHERE last_used_at IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, ts) = row?;
            out.insert(cid, ts);
        }
        Ok(out)
    }

    /// 单个凭证最近一次被使用的时间；无记录时为 `None`。口径同 [`Self::last_used`]。
    pub fn last_used_at(&self, cred_id: i64) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        // 账本行可能还不存在（该凭证从未有过流水），optional 后拍平。
        let ts = conn
            .query_row(
                "SELECT last_used_at FROM credential_stats WHERE cred_id = ?1",
                [cred_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(ts.flatten())
    }

    /// 每个凭证累计的等价 API 费用（cred_id → USD 合计）。
    pub fn cost_by_cred(&self) -> Result<HashMap<i64, f64>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare("SELECT cred_id, cost_total_usd FROM credential_stats")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, sum) = row?;
            out.insert(cid, sum);
        }
        Ok(out)
    }

    /// 单个凭证累计的等价 API 费用（USD）；无记录时为 0。口径同 [`Self::cost_by_cred`]。
    pub fn cost_of(&self, cred_id: i64) -> Result<f64> {
        let conn = self.conn.lock();
        let sum = conn.query_row(
            "SELECT COALESCE((SELECT cost_total_usd FROM credential_stats WHERE cred_id = ?1), 0)",
            [cred_id],
            |r| r.get(0),
        )?;
        Ok(sum)
    }

    /// 缓存命中率趋势的桶。回三个原始数（输入、命中、写入），比率由前端算——一个 300 token
    /// 的小时里的「命中 0%」与 17K 前缀那种小时里的「命中 94%」是两件事，光看比率判断不了。
    ///
    /// `bucket_secs` 是桶宽；`tz_offset_secs` 是本地时区相对 UTC 的偏移，按天分桶时桶边界
    /// 落在**本地**零点上——前端按本地日期铺格子，后端不按同一套边界切，日桶就会跨两天。
    /// `input_tokens` 是全部输入 token（含缓存命中与缓存写入）。
    pub fn cache_series(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<Vec<CacheBucket>> {
        let bucket_secs = bucket_secs.max(1);
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT ((ts + ?3) / ?2) * ?2 - ?3 AS bucket,
                    SUM(
                        COALESCE(input_tokens, 0)
                        + COALESCE(cache_creation_tokens,
                                   COALESCE(cache_5m_tokens, 0) + COALESCE(cache_1h_tokens, 0))
                        + COALESCE(cache_read_tokens, 0)
                    ),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(COALESCE(cache_creation_tokens,
                                          COALESCE(cache_5m_tokens, 0) + COALESCE(cache_1h_tokens, 0))), 0)
               FROM usage_logs
              WHERE ts >= ?1
              GROUP BY bucket
              HAVING SUM(
                        COALESCE(input_tokens, 0)
                        + COALESCE(cache_creation_tokens,
                                   COALESCE(cache_5m_tokens, 0) + COALESCE(cache_1h_tokens, 0))
                        + COALESCE(cache_read_tokens, 0)
                     ) > 0
              ORDER BY bucket",
        )?;
        let rows = stmt.query_map(params![since, bucket_secs, tz_offset_secs], |r| {
            Ok(CacheBucket {
                ts: r.get(0)?,
                input_tokens: r.get(1)?,
                cached_tokens: r.get(2)?,
                written_tokens: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 趋势接口一次要的三样：各桶、整窗口合计、近 60 分钟合计。整窗口合计直接把各桶加起来
    /// （桶是窗口的划分，不必再扫一遍），近 1 小时另走一次 1 小时的小范围扫描。
    pub fn cache_report(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<CacheReport> {
        let points = self.cache_series(since, bucket_secs, tz_offset_secs)?;
        let summary = points.iter().fold(CacheBucket::empty(since), |mut acc, b| {
            acc.input_tokens += b.input_tokens;
            acc.cached_tokens += b.cached_tokens;
            acc.written_tokens += b.written_tokens;
            acc
        });
        let now: i64 = self.read_conn().query_row("SELECT unixepoch()", [], |r| r.get(0))?;
        let recent = self.cache_summary(now - 3600)?;
        Ok(CacheReport { points, summary, recent })
    }

    /// `since` 起到现在的缓存三段 token 合计（一个桶），`ts` 是 `since`。给「近 1 小时」与
    /// 整个窗口的汇总用。
    pub fn cache_summary(&self, since: i64) -> Result<CacheBucket> {
        // 桶宽取一个远大于窗口的数，所有行落进同一个桶；偏移取 since 让桶起点等于 since。
        let mut rows = self.cache_series(since, 1 << 40, -since)?;
        Ok(rows
            .pop()
            .map(|b| CacheBucket { ts: since, ..b })
            .unwrap_or_else(|| CacheBucket::empty(since)))
    }

    /// `since` 起的成功请求（status = 200 且记了 TTFT）的延迟原始行，连同 SQLite 此刻的时钟
    /// （近 1 小时的窗口按它算，与写入侧同源）。WHERE 与 `idx_usage_logs_latency` 的部分谓词
    /// 逐字对应，整条查询在索引里走完、不回表；不排序——分桶用 BTreeMap，分位数各桶自己排。
    fn latency_rows(&self, since: i64) -> Result<(i64, Vec<LatencyRow>)> {
        let conn = self.read_conn();
        let now: i64 = conn.query_row("SELECT unixepoch()", [], |r| r.get(0))?;
        let mut stmt = conn.prepare(LATENCY_ROWS_SQL)?;
        let rows = stmt.query_map([since], |r| {
            Ok(LatencyRow {
                ts: r.get(0)?,
                ttft_ms: r.get(1)?,
                total_ms: r.get(2)?,
                output_tokens: r.get(3)?,
            })
        })?;
        Ok((now, rows.collect::<rusqlite::Result<Vec<_>>>()?))
    }

    /// 趋势接口一次要的三样：各桶、整窗口、近 60 分钟——同一次扫描分三路汇总，不扫三遍。
    pub fn ttft_report(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<TtftReport> {
        let bucket_secs = bucket_secs.max(1);
        let (now, rows) = self.latency_rows(since)?;
        let mut buckets: std::collections::BTreeMap<i64, Vec<LatencyRow>> = Default::default();
        let mut recent_rows = Vec::new();
        for r in &rows {
            let bucket =
                ((r.ts + tz_offset_secs).div_euclid(bucket_secs)) * bucket_secs - tz_offset_secs;
            buckets.entry(bucket).or_default().push(*r);
            if r.ts >= now - 3600 {
                recent_rows.push(*r);
            }
        }
        Ok(TtftReport {
            points: buckets.into_iter().map(|(ts, rows)| summarize_latency(ts, &rows)).collect(),
            summary: summarize_latency(since, &rows),
            recent: summarize_latency(now - 3600, &recent_rows),
        })
    }

    /// TTFT（首字时延）趋势的桶：每桶平均、p50、p95、请求数与输出吞吐。分位数没法从更细
    /// 的桶合并出来，所以桶宽与时区偏移由调用方按前端要画的格子给（同 [`Self::cache_series`]），
    /// 在 Rust 里对每桶的原始值排序取分位——保留期量级也就几十万个整数，没有压力。
    #[cfg(test)]
    pub fn ttft_series(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<Vec<TtftBucket>> {
        Ok(self.ttft_report(since, bucket_secs, tz_offset_secs)?.points)
    }

    /// `since` 起到现在的延迟汇总（一个桶），`ts` 是 `since`。线上走 [`Self::ttft_report`]
    /// 一次拿齐，这个只给测试核对。
    #[cfg(test)]
    pub fn ttft_summary(&self, since: i64) -> Result<TtftBucket> {
        let (_, rows) = self.latency_rows(since)?;
        Ok(summarize_latency(since, &rows))
    }

    /// `since` 起按模型或按账号拆开的用量：每组的请求数、延迟分位与吞吐、缓存三段 token，
    /// 按请求数降序，最多 `limit` 行。一次把窗口内的原始行拉回来在 Rust 里聚合——分位数在
    /// SQL 里算不了，而这张表只在打开趋势对话框时查一次。
    pub fn usage_breakdown(
        &self,
        since: i64,
        by: BreakdownBy,
        limit: usize,
    ) -> Result<Vec<BreakdownRow>> {
        struct Group {
            label: String,
            tier: Option<String>,
            requests: i64,
            latency: Vec<LatencyRow>,
            cache: CacheBucket,
            saved_usd: f64,
        }
        let conn = self.read_conn();
        let key_expr = match by {
            BreakdownBy::Model => "COALESCE(u.model, '')",
            BreakdownBy::Account => "COALESCE(CAST(u.cred_id AS TEXT), '')",
        };
        let label_expr = match by {
            BreakdownBy::Model => "COALESCE(u.model, '')",
            BreakdownBy::Account => {
                // 已删账号的流水留到保留期满（见 remove），账号表里没它了就退回流水自带的名字。
                "COALESCE(c.label, NULLIF(u.cred_label, ''), '#' || COALESCE(CAST(u.cred_id AS TEXT), '?'))"
            }
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT {key_expr}, {label_expr}, u.ts, u.status, u.ttft_ms, u.total_ms, u.output_tokens,
                    COALESCE(u.input_tokens, 0),
                    COALESCE(u.cache_read_tokens, 0),
                    u.cache_creation_tokens, u.cache_5m_tokens, u.cache_1h_tokens,
                    u.model, c.tier
               FROM usage_logs u
               LEFT JOIN credentials c ON c.id = u.cred_id
              WHERE u.ts >= ?1"
        ))?;
        let mut groups: std::collections::HashMap<String, Group> = Default::default();
        let mut rows = stmt.query([since])?;
        while let Some(r) = rows.next()? {
            let key: String = r.get(0)?;
            let label: String = r.get(1)?;
            let ts: i64 = r.get(2)?;
            let status: i64 = r.get(3)?;
            let ttft_ms: Option<i64> = r.get(4)?;
            let total_ms: Option<i64> = r.get(5)?;
            let output_tokens: Option<i64> = r.get(6)?;
            let plain: i64 = r.get(7)?;
            let cached: i64 = r.get(8)?;
            let creation: Option<i64> = r.get(9)?;
            let c5: Option<i64> = r.get(10)?;
            let c1: Option<i64> = r.get(11)?;
            let model: Option<String> = r.get(12)?;
            let tier: Option<String> = r.get(13)?;
            let written = creation.unwrap_or(c5.unwrap_or(0) + c1.unwrap_or(0));
            let g = groups.entry(key).or_insert_with(|| Group {
                label,
                tier: if by == BreakdownBy::Account { tier } else { None },
                requests: 0,
                latency: Vec::new(),
                cache: CacheBucket::empty(since),
                saved_usd: 0.0,
            });
            g.requests += 1;
            g.cache.input_tokens += plain + written + cached;
            g.cache.cached_tokens += cached;
            g.cache.written_tokens += written;
            g.saved_usd += cache_saved_usd(model.as_deref(), plain, cached, creation, c5, c1);
            if status == 200
                && let Some(ttft_ms) = ttft_ms
            {
                g.latency.push(LatencyRow { ts, ttft_ms, total_ms, output_tokens });
            }
        }
        let mut out: Vec<BreakdownRow> = groups
            .into_iter()
            .map(|(key, g)| BreakdownRow {
                key,
                label: g.label,
                tier: g.tier,
                requests: g.requests,
                cache_saved_usd: g.saved_usd,
                latency: summarize_latency(since, &g.latency),
                cache: g.cache,
            })
            .collect();
        out.sort_by(|a, b| b.requests.cmp(&a.requests).then_with(|| a.key.cmp(&b.key)));
        out.truncate(limit);
        Ok(out)
    }

    /// 单个账号 `since` 起的用量统计：按时间分桶（桶宽与时区偏移同 [`Self::ttft_report`]）、
    /// 整个窗口的合计，以及按模型 / 设备 / 来访客户端 / 状态码四个维度拆开的分组（各自按请求数
    /// 降序、最多 `group_limit` 组）。一次扫描把这几路都汇总出来——走 `idx_usage_logs_cred_usage` 的前缀
    /// 卡住账号与起点，量级是单号保留期内的流水，在 Rust 里聚合比拼五条 GROUP BY 省一半扫描。
    pub fn credential_stats(
        &self,
        cred_id: i64,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
        group_limit: usize,
    ) -> Result<CredentialStats> {
        let bucket_secs = bucket_secs.max(1);
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT ts, status, COALESCE(model, ''), COALESCE(device_id, ''), COALESCE(ua, ''),
                    COALESCE(input_tokens, 0), COALESCE(output_tokens, 0),
                    cache_creation_tokens, cache_5m_tokens, cache_1h_tokens,
                    COALESCE(cache_read_tokens, 0), COALESCE(cost_usd, 0),
                    COALESCE(rewrites LIKE 'rejected_locally%', 0)
               FROM usage_logs
              WHERE cred_id = ?1 AND ts >= ?2",
        )?;
        let mut buckets: std::collections::BTreeMap<i64, CredentialStatsBucket> =
            Default::default();
        let mut summary = CredentialStatsBucket { ts: since, ..Default::default() };
        let mut by_model: std::collections::HashMap<String, CredentialStatsGroup> =
            Default::default();
        let mut by_device: std::collections::HashMap<String, CredentialStatsGroup> =
            Default::default();
        let mut by_client: std::collections::HashMap<String, CredentialStatsGroup> =
            Default::default();
        let mut by_status: std::collections::HashMap<String, CredentialStatsGroup> =
            Default::default();
        let mut rows = stmt.query(params![cred_id, since])?;
        while let Some(r) = rows.next()? {
            let ts: i64 = r.get(0)?;
            let status: i64 = r.get(1)?;
            let model: String = r.get(2)?;
            let device: String = r.get(3)?;
            let ua: String = r.get(4)?;
            let creation: Option<i64> = r.get(7)?;
            let c5: Option<i64> = r.get(8)?;
            let c1: Option<i64> = r.get(9)?;
            let row = CredentialStatsBucket {
                ts,
                requests: 1,
                errors: i64::from(!(200..300).contains(&status)),
                rejected: r.get(12)?,
                input_tokens: r.get(5)?,
                output_tokens: r.get(6)?,
                // 同 usage_breakdown：老记录只有细分档没有合计列时，拿两档相加兜底。
                cache_write_tokens: creation.unwrap_or(c5.unwrap_or(0) + c1.unwrap_or(0)),
                cache_read_tokens: r.get(10)?,
                cost_usd: r.get(11)?,
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
        let ranked = |groups: std::collections::HashMap<String, CredentialStatsGroup>| {
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
    pub fn local_rejections(&self, since: i64) -> Result<Vec<(String, i64)>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT rewrites, COUNT(*) FROM usage_logs
              WHERE ts >= ?1 AND rewrites LIKE 'rejected_locally%'
              GROUP BY rewrites",
        )?;
        let mut counts: std::collections::HashMap<String, i64> = Default::default();
        for row in stmt.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (tag, n) = row?;
            let kind =
                tag.split_once(':').map(|(_, k)| k.to_string()).unwrap_or_else(|| "other".into());
            *counts.entry(kind).or_default() += n;
        }
        let mut out: Vec<(String, i64)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }
}
