//! 用量预聚合（`usage_rollup`）：延迟 / 缓存趋势与拆分表读这里，不再按时间扫流水。
//!
//! 业界看板的通行做法：原始明细只留近期，看板读写入时就汇总好的时间桶；分位数存可合并的
//! 分布草图，查询时把窗口内各桶的草图合起来再取分位（存每桶的 p95 再平均是公认的反模式）。
//! 这里照此落地：
//!
//! - **时间桶 15 分钟**（[`ROLLUP_BUCKET_SECS`]）：现实里所有时区偏移都是 15 分钟的整数倍，
//!   按本地零点切日桶不会错位；界面的小时桶、日桶都能由它拼出来。
//! - **三个维度各存一份**（`dim`）：`all`（只有时间，给两张趋势图）、`model`、`cred`（给拆分表）。
//!   拆分表按哪一维看就只读哪一维，7 天窗口读的是几百到几万行小记录，与流量无关。
//! - **延迟分布用 OpenTelemetry 指数直方图**（[`LatencyHist`]，scale 5），相对误差上界约 1.08%；
//!   平均、吞吐、请求数、token、省下的钱都是精确累加的。
//! - **窗口对齐到桶**：只算起点不早于 `since`（向上取整到 15 分钟）的桶——窗口不会越过 `since`
//!   取到更早的数，最新的数一定在；代价是窗口最早那一段至多少算 15 分钟。
//!
//! 每条流水在写入的**同一事务**里累加三行（[`rollup_record`]），与账本同一套路；保留
//! [`ROLLUP_RETENTION_SECS`]（90 天），比流水的 30 天长，裁剪随流水裁剪一起跑。

use super::*;

/// 汇总的时间桶宽（秒）。
pub(super) const ROLLUP_BUCKET_SECS: i64 = 15 * 60;

/// 汇总的保留期（秒）：90 天。
pub(super) const ROLLUP_RETENTION_SECS: i64 = 90 * 24 * 3600;

/// OpenTelemetry 指数直方图的 scale：桶的底数是 `2^(2^-SCALE)`。5 即底数约 1.0219，
/// 估计值的相对误差上界 `(b-1)/(b+1)` 约 1.08%。
const HIST_SCALE: i32 = 5;

/// 汇总的维度：`all` 只按时间，`model` / `cred` 再按模型 / 账号拆。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum RollupDim {
    All,
    Model,
    Cred,
}

impl RollupDim {
    pub(super) fn tag(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Model => "model",
            Self::Cred => "cred",
        }
    }
}

/// 稀疏的指数直方图：桶下标 → 计数，按 OpenTelemetry 的约定，下标 `i` 收 `(b^i, b^(i+1)]`。
///
/// 只记正数：TTFT 以毫秒计，0 ms 不会真出现，万一有按 1 ms 记（落在 `(b^-1, 1]` 那一桶）。
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct LatencyHist {
    buckets: std::collections::BTreeMap<i32, u64>,
}

impl LatencyHist {
    fn base() -> f64 {
        2f64.powf(2f64.powi(-HIST_SCALE))
    }

    /// 值 `v` 落在哪个桶：`ceil(log2(v) · 2^scale) - 1`。恰好落在桶边界上的 2 的整数次幂，
    /// 浮点的 log2 是精确的，结果也就精确。
    pub(super) fn index(v: i64) -> i32 {
        let v = v.max(1) as f64;
        ((v.log2() * 2f64.powi(HIST_SCALE)).ceil() as i32) - 1
    }

    /// 桶 `i` 的代表值：`2·b^(i+1)/(b+1)`，到桶两端的相对误差相等（DDSketch 的取法）。
    fn estimate(i: i32) -> i64 {
        let b = Self::base();
        (2.0 * b.powi(i + 1) / (b + 1.0)).round() as i64
    }

    pub(super) fn record(&mut self, v: i64) {
        *self.buckets.entry(Self::index(v)).or_default() += 1;
    }

    pub(super) fn merge(&mut self, other: &LatencyHist) {
        for (&i, &n) in &other.buckets {
            *self.buckets.entry(i).or_default() += n;
        }
    }

    pub(super) fn count(&self) -> u64 {
        self.buckets.values().sum()
    }

    /// nearest-rank 分位（与按原始值算时同一口径）：第 `ceil(n·p)` 个值所在桶的代表值。
    /// 空直方图为 0。
    pub(super) fn percentile(&self, p: f64) -> i64 {
        let n = self.count();
        if n == 0 {
            return 0;
        }
        let rank = ((n as f64 * p).ceil() as u64).clamp(1, n);
        let mut seen = 0;
        for (&i, &c) in &self.buckets {
            seen += c;
            if seen >= rank {
                return Self::estimate(i);
            }
        }
        unreachable!("rank 不超过总数")
    }

    /// 编码：条目数，再逐条「与上一个下标的差（zigzag）、计数」，全用 LEB128 变长整数。
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.buckets.len() * 3);
        put_varint(&mut out, self.buckets.len() as u64);
        let mut prev = 0i64;
        for (&i, &n) in &self.buckets {
            let delta = i64::from(i) - prev;
            put_varint(&mut out, ((delta << 1) ^ (delta >> 63)) as u64);
            put_varint(&mut out, n);
            prev = i64::from(i);
        }
        out
    }

    /// 解码；格式不对返回 `None`（调用方当成空直方图，不让一行坏数据拖垮整张表）。
    pub(super) fn decode(mut bytes: &[u8]) -> Option<Self> {
        let len = take_varint(&mut bytes)?;
        let mut buckets = std::collections::BTreeMap::new();
        let mut prev = 0i64;
        for _ in 0..len {
            let z = take_varint(&mut bytes)?;
            let delta = ((z >> 1) as i64) ^ -((z & 1) as i64);
            prev += delta;
            buckets.insert(i32::try_from(prev).ok()?, take_varint(&mut bytes)?);
        }
        bytes.is_empty().then_some(Self { buckets })
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn take_varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let (&b, rest) = bytes.split_first()?;
        *bytes = rest;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// 一格汇总：一批流水可累加的全部口径。各项的算法与拆分前逐行累加时逐字相同。
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct RollupAgg {
    /// 全部请求数（含失败与本地拒绝）。
    pub(super) requests: i64,
    /// 全部输入 token（裸输入 + 缓存写入 + 缓存命中），见 [`CacheBucket::input_tokens`]。
    pub(super) input_tokens: i64,
    pub(super) cached_tokens: i64,
    pub(super) written_tokens: i64,
    /// 缓存省下的钱，见 [`BreakdownRow::cache_saved_usd`]。
    pub(super) saved_usd: f64,
    /// 参与延迟统计的成功请求数（status = 200 且记了 TTFT）与它们的 TTFT 之和（算平均）。
    pub(super) lat_count: i64,
    pub(super) ttft_sum: i64,
    /// 吞吐的分子分母：`Σ output_tokens` 与 `Σ (total_ms − ttft_ms)`，口径见
    /// [`TtftBucket::tokens_per_sec`]。
    pub(super) gen_tokens: i64,
    pub(super) gen_ms: i64,
    pub(super) ttft: LatencyHist,
}

/// 一条流水在汇总里要用的那几列。
#[derive(Debug, Clone, Copy)]
pub(super) struct RollupInput<'a> {
    pub(super) model: Option<&'a str>,
    pub(super) status: i64,
    pub(super) ttft_ms: Option<i64>,
    pub(super) total_ms: Option<i64>,
    pub(super) output_tokens: Option<i64>,
    pub(super) input_tokens: Option<i64>,
    pub(super) cache_read_tokens: Option<i64>,
    pub(super) cache_creation_tokens: Option<i64>,
    pub(super) cache_5m_tokens: Option<i64>,
    pub(super) cache_1h_tokens: Option<i64>,
}

impl RollupAgg {
    /// 一条流水的贡献。
    pub(super) fn of(row: RollupInput<'_>) -> Self {
        let plain = row.input_tokens.unwrap_or(0);
        let cached = row.cache_read_tokens.unwrap_or(0);
        let (creation, c5, c1) =
            (row.cache_creation_tokens, row.cache_5m_tokens, row.cache_1h_tokens);
        // 老记录只有细分档没有合计列时，拿两档相加兜底。
        let written = creation.unwrap_or(c5.unwrap_or(0) + c1.unwrap_or(0));
        let mut agg = Self {
            requests: 1,
            input_tokens: plain + written + cached,
            cached_tokens: cached,
            written_tokens: written,
            saved_usd: cache_saved_usd(row.model, plain, cached, creation, c5, c1),
            ..Default::default()
        };
        if row.status == 200
            && let Some(ttft) = row.ttft_ms
        {
            agg.lat_count = 1;
            agg.ttft_sum = ttft;
            agg.ttft.record(ttft);
            if let (Some(total), Some(out)) = (row.total_ms, row.output_tokens)
                && total > ttft
                && out > 0
            {
                agg.gen_tokens = out;
                agg.gen_ms = total - ttft;
            }
        }
        agg
    }

    pub(super) fn add(&mut self, other: &RollupAgg) {
        self.requests += other.requests;
        self.input_tokens += other.input_tokens;
        self.cached_tokens += other.cached_tokens;
        self.written_tokens += other.written_tokens;
        self.saved_usd += other.saved_usd;
        self.lat_count += other.lat_count;
        self.ttft_sum += other.ttft_sum;
        self.gen_tokens += other.gen_tokens;
        self.gen_ms += other.gen_ms;
        self.ttft.merge(&other.ttft);
    }

    /// 延迟口径的一个桶，`ts` 由调用方给（桶起点或窗口起点）。空集全零、吞吐 `None`。
    pub(super) fn latency(&self, ts: i64) -> TtftBucket {
        if self.lat_count == 0 {
            return TtftBucket {
                ts,
                avg_ms: 0,
                p50_ms: 0,
                p95_ms: 0,
                count: 0,
                tokens_per_sec: None,
            };
        }
        TtftBucket {
            ts,
            avg_ms: self.ttft_sum / self.lat_count,
            p50_ms: self.ttft.percentile(0.5),
            p95_ms: self.ttft.percentile(0.95),
            count: self.lat_count,
            tokens_per_sec: (self.gen_ms > 0)
                .then(|| self.gen_tokens as f64 * 1000.0 / self.gen_ms as f64),
        }
    }

    /// 缓存口径的一个桶。
    pub(super) fn cache(&self, ts: i64) -> CacheBucket {
        CacheBucket {
            ts,
            input_tokens: self.input_tokens,
            cached_tokens: self.cached_tokens,
            written_tokens: self.written_tokens,
        }
    }
}

/// 时刻所在汇总桶的起点。
pub(super) fn rollup_bucket(ts: i64) -> i64 {
    ts.div_euclid(ROLLUP_BUCKET_SECS) * ROLLUP_BUCKET_SECS
}

/// 窗口 `since` 起要读的第一个桶：向上取整，见模块文档里「窗口对齐到桶」。
pub(super) fn first_bucket(since: i64) -> i64 {
    -((-since).div_euclid(ROLLUP_BUCKET_SECS)) * ROLLUP_BUCKET_SECS
}

/// 把展示桶宽规整成汇总桶宽的整数倍（至少一个汇总桶），时区偏移规整到 15 分钟的整数倍：
/// 汇总桶是最小粒度，比它细的桶、不对齐的偏移都拼不出来。现实中的时区偏移本来就是 15 分钟的
/// 整数倍，界面用的小时桶、日桶也都是。
pub(super) fn display_grid(bucket_secs: i64, tz_offset_secs: i64) -> (i64, i64) {
    let bucket =
        (bucket_secs.max(1) + ROLLUP_BUCKET_SECS - 1) / ROLLUP_BUCKET_SECS * ROLLUP_BUCKET_SECS;
    let tz =
        (tz_offset_secs as f64 / ROLLUP_BUCKET_SECS as f64).round() as i64 * ROLLUP_BUCKET_SECS;
    (bucket, tz)
}

/// 趋势接口实际用的网格：窗口起点（向上对齐到汇总桶，见 [`first_bucket`]）、规整后的桶宽与
/// 时区偏移（见 [`display_grid`]）。接口层拿同一组值去查、也填进响应，声明的粒度与实际一致。
pub fn series_grid(since: i64, bucket_secs: i64, tz_offset_secs: i64) -> (i64, i64, i64) {
    let (bucket, tz) = display_grid(bucket_secs, tz_offset_secs);
    (first_bucket(since), bucket, tz)
}

/// 一个汇总桶落在哪个展示桶：按本地时区切，口径同拆分前的 `((ts + tz) / bucket) * bucket - tz`。
pub(super) fn display_bucket(rollup_bucket: i64, bucket_secs: i64, tz_offset_secs: i64) -> i64 {
    (rollup_bucket + tz_offset_secs).div_euclid(bucket_secs) * bucket_secs - tz_offset_secs
}

/// 三个维度上的键：`model` 维是模型名（没有为空串），`cred` 维是账号 id 的十进制串（没选到号
/// 的本地拒绝为空串），与拆分表的 `key` 同口径。
pub(super) fn keys(model: Option<&str>, cred_id: Option<i64>) -> [(RollupDim, String); 3] {
    [
        (RollupDim::All, String::new()),
        (RollupDim::Model, model.unwrap_or_default().to_string()),
        (RollupDim::Cred, cred_id.map(|id| id.to_string()).unwrap_or_default()),
    ]
}

/// 读出来的一格：桶起点、键、新建时记下的名字、汇总。
pub(super) struct RollupCell {
    pub(super) bucket: i64,
    pub(super) key: String,
    pub(super) label: String,
    pub(super) agg: RollupAgg,
}

use anyhow::Result;
use sqlx::{PgConnection, Row};

use super::CredentialStore;
use super::UsageRecord;

/// 写一条流水时顺手累加三个维度，与流水同一事务，见 `insert_usage_log_at`。
///
/// 这条在每条转发请求后都跑，三格一条语句累加完（`unnest` 拼三行）。直方图是自定义编码，
/// 库里合并不了，得读出来在 Rust 里合：
///
/// - 累加那条 `INSERT … ON CONFLICT DO UPDATE` 不碰 `ttft_hist`，新建的格子它是 NULL，已有的
///   原样留着，`RETURNING` 回的就是**合并前**的旧直方图；
/// - 这条语句同时给三格上了行锁，一直持到事务提交。别的事务往同一格累加要排在后面，读到的
///   一定是本事务写回之后的直方图——不会两边各读一份旧的、后写的把先写的盖掉（SQLite 版靠
///   全局写锁保证这一点，PG 的事务是真并发的）；
/// - 这一条没有 TTFT（失败、本地拒绝）时直方图不变，一条语句就完了；有的话再一条 UPDATE
///   把合并后的三份写回。
pub(super) async fn rollup_record(
    conn: &mut PgConnection,
    ts: i64,
    rec: &UsageRecord,
) -> Result<()> {
    let delta = RollupAgg::of(RollupInput {
        model: rec.model.as_deref(),
        status: i64::from(rec.status),
        ttft_ms: rec.ttft_ms,
        total_ms: rec.total_ms,
        output_tokens: rec.output_tokens,
        input_tokens: rec.input_tokens,
        cache_read_tokens: rec.cache_read_tokens,
        cache_creation_tokens: rec.cache_creation_tokens,
        cache_5m_tokens: rec.cache_5m_tokens,
        cache_1h_tokens: rec.cache_1h_tokens,
    });
    let bucket = rollup_bucket(ts);
    let mut dims = Vec::with_capacity(3);
    let mut cell_keys = Vec::with_capacity(3);
    let mut labels = Vec::with_capacity(3);
    for (dim, key) in keys(rec.model.as_deref(), rec.cred_id) {
        // label 只在新建时写：账号那一维记这一桶里第一条流水的账号名，给已删的号兜底显示。
        labels.push(if dim == RollupDim::Cred { rec.cred_label.clone() } else { String::new() });
        dims.push(dim.tag());
        cell_keys.push(key);
    }
    let old: Vec<(String, Option<Vec<u8>>)> = sqlx::query_as(
        "INSERT INTO usage_rollup AS r
             (dim, bucket, key, label, requests, input_tokens, cached_tokens, written_tokens,
              saved_usd, lat_count, ttft_sum, gen_tokens, gen_ms)
         SELECT t.dim, $1, t.key, t.label, $5, $6, $7, $8, $9, $10, $11, $12, $13
           FROM unnest($2::TEXT[], $3::TEXT[], $4::TEXT[]) WITH ORDINALITY AS t(dim, key, label, ord)
          ORDER BY t.ord
         ON CONFLICT (dim, bucket, key) DO UPDATE SET
             requests = r.requests + excluded.requests,
             input_tokens = r.input_tokens + excluded.input_tokens,
             cached_tokens = r.cached_tokens + excluded.cached_tokens,
             written_tokens = r.written_tokens + excluded.written_tokens,
             saved_usd = r.saved_usd + excluded.saved_usd,
             lat_count = r.lat_count + excluded.lat_count,
             ttft_sum = r.ttft_sum + excluded.ttft_sum,
             gen_tokens = r.gen_tokens + excluded.gen_tokens,
             gen_ms = r.gen_ms + excluded.gen_ms
         RETURNING dim, ttft_hist",
    )
    .bind(bucket)
    .bind(&dims)
    .bind(&cell_keys)
    .bind(&labels)
    .bind(delta.requests)
    .bind(delta.input_tokens)
    .bind(delta.cached_tokens)
    .bind(delta.written_tokens)
    .bind(delta.saved_usd)
    .bind(delta.lat_count)
    .bind(delta.ttft_sum)
    .bind(delta.gen_tokens)
    .bind(delta.gen_ms)
    .fetch_all(&mut *conn)
    .await?;
    if delta.ttft.count() == 0 {
        return Ok(());
    }
    let mut new_dims = Vec::with_capacity(3);
    let mut new_keys = Vec::with_capacity(3);
    let mut hists = Vec::with_capacity(3);
    for (dim, old_hist) in old {
        let Some(i) = dims.iter().position(|d| *d == dim) else { continue };
        let mut hist = delta.ttft.clone();
        // 格子可能是刚建的（NULL），也可能存在但之前没有一条带 TTFT（同样是 NULL）；
        // 坏数据当成空直方图，不让一行坏数据拖垮写入。
        if let Some(old) = old_hist.as_deref().and_then(LatencyHist::decode) {
            hist.merge(&old);
        }
        new_dims.push(dims[i]);
        new_keys.push(cell_keys[i].clone());
        hists.push(hist.encode());
    }
    sqlx::query(
        "UPDATE usage_rollup r SET ttft_hist = t.hist
           FROM unnest($2::TEXT[], $3::TEXT[], $4::BYTEA[]) AS t(dim, key, hist)
          WHERE r.dim = t.dim AND r.bucket = $1 AND r.key = t.key",
    )
    .bind(bucket)
    .bind(&new_dims)
    .bind(&new_keys)
    .bind(&hists)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

impl CredentialStore {
    /// 某个维度上窗口 `since` 起的全部格子，按桶起点升序。窗口按 [`first_bucket`] 对齐。
    pub(super) async fn rollup_cells(&self, dim: RollupDim, since: i64) -> Result<Vec<RollupCell>> {
        let rows = sqlx::query(
            "SELECT bucket, key, label, requests, input_tokens, cached_tokens, written_tokens,
                    saved_usd, lat_count, ttft_sum, gen_tokens, gen_ms, ttft_hist
               FROM usage_rollup
              WHERE dim = $1 AND bucket >= $2
              ORDER BY bucket",
        )
        .bind(dim.tag())
        .bind(first_bucket(since))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let hist: Option<Vec<u8>> = r.try_get(12)?;
                Ok(RollupCell {
                    bucket: r.try_get(0)?,
                    key: r.try_get(1)?,
                    label: r.try_get(2)?,
                    agg: RollupAgg {
                        requests: r.try_get(3)?,
                        input_tokens: r.try_get(4)?,
                        cached_tokens: r.try_get(5)?,
                        written_tokens: r.try_get(6)?,
                        saved_usd: r.try_get(7)?,
                        lat_count: r.try_get(8)?,
                        ttft_sum: r.try_get(9)?,
                        gen_tokens: r.try_get(10)?,
                        gen_ms: r.try_get(11)?,
                        ttft: hist.as_deref().and_then(LatencyHist::decode).unwrap_or_default(),
                    },
                })
            })
            .collect()
    }

    /// 裁掉超过 [`ROLLUP_RETENTION_SECS`] 的汇总。表小（每 15 分钟几行到几十行），一条 DELETE。
    pub(super) async fn prune_rollup(&self) -> Result<usize> {
        let n = sqlx::query("DELETE FROM usage_rollup WHERE bucket < unixepoch() - $1")
            .bind(ROLLUP_RETENTION_SECS)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::usage::tests::{db_now, store_with};
    use super::super::{BreakdownBy, cache_saved_usd, series_grid};
    use super::*;

    /// 分位数来自指数直方图（OTel scale 5）：与 nearest-rank 精确值的相对误差在 1.1% 以内。
    fn assert_close(est: i64, exact: i64) {
        let tol = (exact as f64 * 0.011).max(1.0);
        assert!((est - exact).abs() as f64 <= tol, "估计 {est} 偏离精确值 {exact} 超过 1.1%");
    }

    /// 一个确定性的伪随机序列（xorshift），测试数据可复现。
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// 桶下标按 OpenTelemetry 指数直方图的约定：下标 i 收 (b^i, b^(i+1)]，b = 2^(2^-5)。
    /// 2 的整数次幂恰好落在桶的上边界上。
    #[test]
    fn latency_hist_follows_otel_exponential_buckets() {
        assert_eq!(LatencyHist::index(1), -1, "1 = b^0 是 (b^-1, b^0] 的上边界");
        assert_eq!(LatencyHist::index(2), 31);
        assert_eq!(LatencyHist::index(1024), 10 * 32 - 1);
        assert_eq!(LatencyHist::index(3), 50, "log2(3)·32 = 50.7 → ceil 51 → 50");
        assert_eq!(LatencyHist::index(0), -1, "0 ms 按 1 ms 记");
        let mut h = LatencyHist::default();
        for v in [1, 3, 3, 250, 1024, 60_000] {
            h.record(v);
        }
        let bytes = h.encode();
        assert_eq!(LatencyHist::decode(&bytes), Some(h.clone()), "编码往返");
        assert_eq!(LatencyHist::decode(&bytes[..bytes.len() - 1]), None, "截断的数据不认");
        assert_eq!(LatencyHist::decode(&[]), None);
        assert_eq!(LatencyHist::default().percentile(0.5), 0, "空直方图");
    }

    /// 分位数估计与 nearest-rank 精确值的相对误差不超过 (b-1)/(b+1) ≈ 1.08%（再加 1 ms 取整）。
    #[test]
    fn latency_hist_percentiles_stay_within_relative_error() {
        let mut seed = 0x9e37_79b9_7f4a_7c15;
        let mut values: Vec<i64> = (0..20_000)
            .map(|_| {
                // 对数均匀地铺在 50 ms ～ 120 s：TTFT 实际落的范围。
                let u = (xorshift(&mut seed) % 1_000_000) as f64 / 1_000_000.0;
                (50.0 * (120_000.0f64 / 50.0).powf(u)).round() as i64
            })
            .collect();
        let mut h = LatencyHist::default();
        for &v in &values {
            h.record(v);
        }
        values.sort_unstable();
        for p in [0.5, 0.95, 0.99] {
            let rank = ((values.len() as f64 * p).ceil() as usize).clamp(1, values.len());
            let exact = values[rank - 1];
            let est = h.percentile(p);
            let rel = (est - exact).abs() as f64 / exact as f64;
            assert!(
                rel <= 0.0109 || (est - exact).abs() <= 1,
                "p{p}: 估计 {est}，精确 {exact}，相对误差 {rel}"
            );
        }
    }

    /// 随机写一批流水，汇总读出来的拆分表与趋势合计要与从原始行精确算出来的一致：可累加的各项
    /// 逐一相等，分位数在 1.1% 以内。
    #[sqlx::test]
    async fn rollup_matches_aggregates_computed_from_raw_logs(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a", "b", "c"]).await;
        let now = db_now(&store).await;
        // 窗口起点对齐到 15 分钟，免得边缘那一桶的取舍影响比对。
        let since = first_bucket(now - 6 * 3600);
        let models = ["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"];
        let mut seed = 42u64;
        let mut raw = Vec::new();
        for _ in 0..3000 {
            let r = xorshift(&mut seed);
            let cred = ids[(r % 3) as usize];
            let model = models[(r / 3 % 3) as usize];
            let status: u16 = if r.is_multiple_of(10) { 529 } else { 200 };
            let ttft = 100 + (xorshift(&mut seed) % 20_000) as i64;
            let rec = UsageRecord {
                cred_id: Some(cred),
                cred_label: format!("label-{cred}"),
                model: Some(model.into()),
                status,
                has_usage: true,
                input_tokens: Some((r % 5000) as i64),
                output_tokens: Some((r % 3000) as i64),
                cache_creation_tokens: (!r.is_multiple_of(7)).then_some((r % 900) as i64),
                cache_5m_tokens: Some((r % 400) as i64),
                cache_1h_tokens: Some((r % 300) as i64),
                cache_read_tokens: Some((r % 20_000) as i64),
                ttft_ms: (!r.is_multiple_of(13)).then_some(ttft),
                total_ms: Some(ttft + (r % 30_000) as i64),
                ..Default::default()
            };
            let ts = since + (xorshift(&mut seed) % (now - since).max(1) as u64) as i64;
            store.insert_usage_log_at(&rec, Some(ts)).await.unwrap();
            raw.push(rec);
        }

        // 从原始行精确算：按模型分组的请求数、token、省下的钱、延迟平均 / 吞吐 / 分位。
        struct Exact {
            requests: i64,
            input: i64,
            cached: i64,
            written: i64,
            saved: f64,
            ttft: Vec<i64>,
            gen_tokens: i64,
            gen_ms: i64,
        }
        let mut exact: HashMap<String, Exact> = HashMap::new();
        for rec in &raw {
            let e = exact.entry(rec.model.clone().unwrap()).or_insert(Exact {
                requests: 0,
                input: 0,
                cached: 0,
                written: 0,
                saved: 0.0,
                ttft: vec![],
                gen_tokens: 0,
                gen_ms: 0,
            });
            let plain = rec.input_tokens.unwrap_or(0);
            let cached = rec.cache_read_tokens.unwrap_or(0);
            let written = rec
                .cache_creation_tokens
                .unwrap_or(rec.cache_5m_tokens.unwrap_or(0) + rec.cache_1h_tokens.unwrap_or(0));
            e.requests += 1;
            e.input += plain + written + cached;
            e.cached += cached;
            e.written += written;
            e.saved += cache_saved_usd(
                rec.model.as_deref(),
                plain,
                cached,
                rec.cache_creation_tokens,
                rec.cache_5m_tokens,
                rec.cache_1h_tokens,
            );
            if rec.status == 200
                && let Some(t) = rec.ttft_ms
            {
                e.ttft.push(t);
                if let (Some(total), Some(out)) = (rec.total_ms, rec.output_tokens)
                    && total > t
                    && out > 0
                {
                    e.gen_tokens += out;
                    e.gen_ms += total - t;
                }
            }
        }
        let nearest = |sorted: &[i64], p: f64| {
            sorted[((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1]
        };
        let rows = store.usage_breakdown(since, BreakdownBy::Model, 10).await.unwrap();
        assert_eq!(rows.len(), 3);
        for row in &rows {
            let e = exact.get_mut(&row.key).unwrap();
            e.ttft.sort_unstable();
            assert_eq!(row.requests, e.requests);
            assert_eq!(
                (row.cache.input_tokens, row.cache.cached_tokens, row.cache.written_tokens),
                (e.input, e.cached, e.written)
            );
            assert!(
                (row.cache_saved_usd - e.saved).abs() < 1e-9,
                "{} vs {}",
                row.cache_saved_usd,
                e.saved
            );
            assert_eq!(row.latency.count, e.ttft.len() as i64);
            assert_eq!(row.latency.avg_ms, e.ttft.iter().sum::<i64>() / e.ttft.len() as i64);
            let tps = e.gen_tokens as f64 * 1000.0 / e.gen_ms as f64;
            assert!((row.latency.tokens_per_sec.unwrap() - tps).abs() < 1e-9);
            assert_close(row.latency.p50_ms, nearest(&e.ttft, 0.5));
            assert_close(row.latency.p95_ms, nearest(&e.ttft, 0.95));
        }
        // 按账号拆与整窗口合计：请求数加起来等于总数，缓存合计与按模型的一致。
        let by_cred = store.usage_breakdown(since, BreakdownBy::Account, 10).await.unwrap();
        assert_eq!(by_cred.iter().map(|r| r.requests).sum::<i64>(), 3000);
        let mut labels: Vec<&str> = by_cred.iter().map(|r| r.label.as_str()).collect();
        labels.sort_unstable();
        assert_eq!(labels, ["a", "b", "c"], "号还在，名字取账号表的");
        let cache = store.cache_report(since, 3600, 0).await.unwrap();
        assert_eq!(cache.summary.input_tokens, exact.values().map(|e| e.input).sum::<i64>());
        let ttft = store.ttft_report(since, 3600, 0).await.unwrap();
        assert_eq!(ttft.summary.count, exact.values().map(|e| e.ttft.len() as i64).sum::<i64>());
        assert_eq!(
            ttft.points.iter().map(|b| b.count).sum::<i64>(),
            ttft.summary.count,
            "各桶之和等于合计"
        );
    }

    /// 并发写同一格：直方图是读出来在 Rust 里合并再写回的，两个事务同时累加同一格不能
    /// 互相覆盖（SQLite 版靠全局写锁天然串行，PG 版靠累加语句给格子上的行锁）。
    #[sqlx::test]
    async fn concurrent_writes_to_one_cell_keep_every_histogram_sample(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let store = std::sync::Arc::new(store);
        let now = db_now(&store).await;
        let ts = first_bucket(now - 3600);
        let mut tasks = Vec::new();
        for i in 0..40 {
            let store = store.clone();
            let cred = ids[0];
            tasks.push(tokio::spawn(async move {
                let rec = UsageRecord {
                    cred_id: Some(cred),
                    cred_label: "a".into(),
                    model: Some("claude-opus-5".into()),
                    status: 200,
                    ttft_ms: Some(100 + i),
                    ..Default::default()
                };
                store.insert_usage_log_at(&rec, Some(ts)).await.unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        for dim in [RollupDim::All, RollupDim::Model, RollupDim::Cred] {
            let cells = store.rollup_cells(dim, ts).await.unwrap();
            assert_eq!(cells.len(), 1);
            assert_eq!(cells[0].agg.requests, 40);
            assert_eq!(cells[0].agg.lat_count, 40);
            assert_eq!(cells[0].agg.ttft.count(), 40, "{dim:?} 维的直方图丢了样本");
        }
    }

    /// 汇总保留 90 天，随流水裁剪一起裁；清空库时一起清掉。
    #[sqlx::test]
    async fn rollup_is_pruned_after_ninety_days_and_cleared_with_the_store(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let now = db_now(&store).await;
        let rec = UsageRecord { cred_id: Some(ids[0]), status: 200, ..Default::default() };
        store.insert_usage_log_at(&rec, Some(now - 91 * 86400)).await.unwrap();
        store.insert_usage_log_at(&rec, Some(now - 89 * 86400)).await.unwrap();
        store.insert_usage_log_at(&rec, Some(now - 100)).await.unwrap();
        let count = async |store: &CredentialStore| -> i64 {
            sqlx::query_scalar("SELECT COUNT(*) FROM usage_rollup WHERE dim = 'all'")
                .fetch_one(&store.pool)
                .await
                .unwrap()
        };
        assert_eq!(count(&store).await, 3);
        store.prune_usage_logs().await.unwrap();
        assert_eq!(
            count(&store).await,
            2,
            "超过 90 天的那一桶裁掉，89 天前的还在（流水只留 30 天）"
        );
        store.clear().await.unwrap();
        assert_eq!(count(&store).await, 0);
    }

    /// 趋势接口的网格：桶宽向上取整到 15 分钟的整数倍、偏移对齐到 15 分钟、窗口起点向上对齐。
    #[test]
    fn series_grid_reports_the_granularity_actually_used() {
        assert_eq!(series_grid(0, 60, 0), (0, 900, 0), "比汇总桶细的桶宽拼不出来，取一个汇总桶");
        assert_eq!(series_grid(0, 1000, 0).1, 1800, "不是整倍数的向上取整");
        assert_eq!(series_grid(0, 3600, 0).1, 3600);
        assert_eq!(series_grid(0, 86400, 0).1, 86400);
        assert_eq!(series_grid(0, 86400, 5 * 3600 + 1800).2, 19800, "UTC+5:30 本身就对齐");
        assert_eq!(series_grid(0, 86400, 1000).2, 900, "不对齐的偏移取最近的 15 分钟");
        assert_eq!(series_grid(901, 3600, 0).0, 1800, "窗口起点向上对齐");
        assert_eq!(series_grid(900, 3600, 0).0, 900);
    }
}
