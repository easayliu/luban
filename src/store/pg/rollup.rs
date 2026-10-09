//! 用量预聚合（`usage_rollup`）的 PG 版，对应 `store::rollup`：延迟 / 缓存趋势与拆分表读这里，
//! 不再按时间扫流水。
//!
//! 时间桶、维度、直方图（[`LatencyHist`]）、单条贡献（[`RollupAgg::of`]）这些纯内存的部分
//! 直接用 `store::rollup` 的，这里只有碰库的三件事：写入时累加、按维度读格子、裁剪。
//!
//! 老库升级时的一次性回填（`backfill_rollup`）不移植：PG 库从空表起步。

use anyhow::Result;
use sqlx::{PgConnection, Row};

use super::super::{
    LatencyHist, ROLLUP_RETENTION_SECS, RollupAgg, RollupCell, RollupDim, RollupInput, UsageRecord,
    first_bucket, keys, rollup_bucket,
};
use super::PgStore;

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

impl PgStore {
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

    use super::super::super::{BreakdownBy, cache_saved_usd, series_grid};
    use super::super::usage::tests::{db_now, store_with};
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
        let count = async |store: &PgStore| -> i64 {
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
            "超过 90 天的那一桶裁掉，89 天前的还在（流水只留 8 天）"
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
