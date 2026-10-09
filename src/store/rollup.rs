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
//! [`ROLLUP_RETENTION_SECS`]（90 天），比流水的 8 天长，裁剪随流水裁剪一起跑。

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

/// 往一格累加 `delta`（没有就新建）。`label` 只在新建时写：账号那一维记的是这一桶里第一条流水
/// 的账号名，给已删的号兜底显示。直方图读出来合并后整段写回——同一事务里，不会有人插队。
fn upsert(
    tx: &Connection,
    dim: RollupDim,
    bucket: i64,
    key: &str,
    label: &str,
    delta: &RollupAgg,
) -> Result<()> {
    let mut hist = delta.ttft.clone();
    if hist.count() > 0 {
        // 格子可能还不存在（None），也可能存在但之前没有一条带 TTFT（列是 NULL）。
        let old: Option<Vec<u8>> = tx
            .query_row(
                "SELECT ttft_hist FROM usage_rollup WHERE dim = ?1 AND bucket = ?2 AND key = ?3",
                params![dim.tag(), bucket, key],
                |r| r.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?
            .flatten();
        if let Some(old) = old.as_deref().and_then(LatencyHist::decode) {
            hist.merge(&old);
        }
    }
    let hist_blob = (hist.count() > 0).then(|| hist.encode());
    tx.execute(
        "INSERT INTO usage_rollup
             (dim, bucket, key, label, requests, input_tokens, cached_tokens, written_tokens,
              saved_usd, lat_count, ttft_sum, gen_tokens, gen_ms, ttft_hist)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT (dim, bucket, key) DO UPDATE SET
             requests = requests + excluded.requests,
             input_tokens = input_tokens + excluded.input_tokens,
             cached_tokens = cached_tokens + excluded.cached_tokens,
             written_tokens = written_tokens + excluded.written_tokens,
             saved_usd = saved_usd + excluded.saved_usd,
             lat_count = lat_count + excluded.lat_count,
             ttft_sum = ttft_sum + excluded.ttft_sum,
             gen_tokens = gen_tokens + excluded.gen_tokens,
             gen_ms = gen_ms + excluded.gen_ms,
             ttft_hist = COALESCE(excluded.ttft_hist, ttft_hist)",
        params![
            dim.tag(),
            bucket,
            key,
            label,
            delta.requests,
            delta.input_tokens,
            delta.cached_tokens,
            delta.written_tokens,
            delta.saved_usd,
            delta.lat_count,
            delta.ttft_sum,
            delta.gen_tokens,
            delta.gen_ms,
            hist_blob,
        ],
    )?;
    Ok(())
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

/// 写一条流水时顺手累加三个维度，与流水同一事务，见 `insert_usage_log_at`。
pub(super) fn rollup_record(tx: &Connection, ts: i64, rec: &UsageRecord) -> Result<()> {
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
    for (dim, key) in keys(rec.model.as_deref(), rec.cred_id) {
        let label = if dim == RollupDim::Cred { rec.cred_label.as_str() } else { "" };
        upsert(tx, dim, bucket, &key, label, &delta)?;
    }
    Ok(())
}

/// 老库升级：汇总表是空的而流水不是，就从现存流水一次性回填（同一事务，要么全有要么全无）。
/// 流水保留期内的行按写入顺序走一遍，账号名取每桶第一条，与写入时累加的口径一致。
///
/// **检查与回填在同一个 `BEGIN IMMEDIATE` 事务里**：一开始就拿写锁，两个进程同时首次升级时
/// 后到的那个等前一个提交，再看汇总已经不空，直接跳过。检查若在事务外（或用读锁起步的默认
/// 事务），两边都会看到「空」、各自回填一遍，历史数据就被累加两次。
pub(super) fn backfill_rollup(conn: &Connection) -> Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let has_rollup: bool =
        tx.query_row("SELECT EXISTS (SELECT 1 FROM usage_rollup)", [], |r| r.get(0))?;
    let has_logs: bool =
        tx.query_row("SELECT EXISTS (SELECT 1 FROM usage_logs)", [], |r| r.get(0))?;
    if has_rollup || !has_logs {
        // 没写任何东西，事务随 drop 回滚即可。
        return Ok(());
    }
    let started = std::time::Instant::now();
    let mut cells: HashMap<(RollupDim, i64, String), (String, RollupAgg)> = HashMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT ts, cred_id, cred_label, model, status, ttft_ms, total_ms, output_tokens,
                    input_tokens, cache_read_tokens, cache_creation_tokens, cache_5m_tokens,
                    cache_1h_tokens
               FROM usage_logs ORDER BY id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let ts: i64 = r.get(0)?;
            let cred_id: Option<i64> = r.get(1)?;
            let cred_label: String = r.get(2)?;
            let model: Option<String> = r.get(3)?;
            let delta = RollupAgg::of(RollupInput {
                model: model.as_deref(),
                status: r.get(4)?,
                ttft_ms: r.get(5)?,
                total_ms: r.get(6)?,
                output_tokens: r.get(7)?,
                input_tokens: r.get(8)?,
                cache_read_tokens: r.get(9)?,
                cache_creation_tokens: r.get(10)?,
                cache_5m_tokens: r.get(11)?,
                cache_1h_tokens: r.get(12)?,
            });
            let bucket = rollup_bucket(ts);
            for (dim, key) in keys(model.as_deref(), cred_id) {
                let label = if dim == RollupDim::Cred { cred_label.clone() } else { String::new() };
                cells
                    .entry((dim, bucket, key))
                    .or_insert_with(|| (label, RollupAgg::default()))
                    .1
                    .add(&delta);
            }
        }
    }
    for ((dim, bucket, key), (label, agg)) in &cells {
        upsert(&tx, *dim, *bucket, key, label, agg)?;
    }
    tx.commit()?;
    tracing::info!(
        cells = cells.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "backfilled the usage rollup from existing usage logs"
    );
    Ok(())
}

/// 读出来的一格：桶起点、键、新建时记下的名字、汇总。
pub(super) struct RollupCell {
    pub(super) bucket: i64,
    pub(super) key: String,
    pub(super) label: String,
    pub(super) agg: RollupAgg,
}

impl CredentialStore {
    /// 某个维度上窗口 `since` 起的全部格子，按桶起点升序。窗口按 [`first_bucket`] 对齐。
    pub(super) fn rollup_cells(&self, dim: RollupDim, since: i64) -> Result<Vec<RollupCell>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT bucket, key, label, requests, input_tokens, cached_tokens, written_tokens,
                    saved_usd, lat_count, ttft_sum, gen_tokens, gen_ms, ttft_hist
               FROM usage_rollup
              WHERE dim = ?1 AND bucket >= ?2
              ORDER BY bucket",
        )?;
        let rows = stmt.query_map(params![dim.tag(), first_bucket(since)], |r| {
            let hist: Option<Vec<u8>> = r.get(12)?;
            Ok(RollupCell {
                bucket: r.get(0)?,
                key: r.get(1)?,
                label: r.get(2)?,
                agg: RollupAgg {
                    requests: r.get(3)?,
                    input_tokens: r.get(4)?,
                    cached_tokens: r.get(5)?,
                    written_tokens: r.get(6)?,
                    saved_usd: r.get(7)?,
                    lat_count: r.get(8)?,
                    ttft_sum: r.get(9)?,
                    gen_tokens: r.get(10)?,
                    gen_ms: r.get(11)?,
                    ttft: hist.as_deref().and_then(LatencyHist::decode).unwrap_or_default(),
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 裁掉超过 [`ROLLUP_RETENTION_SECS`] 的汇总。表小（每 15 分钟几行到几十行），一条 DELETE。
    pub(super) fn prune_rollup(&self) -> Result<usize> {
        Ok(self.conn.lock().execute(
            "DELETE FROM usage_rollup WHERE bucket < unixepoch() - ?1",
            [ROLLUP_RETENTION_SECS],
        )?)
    }
}
