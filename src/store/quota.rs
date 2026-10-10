//! 额度快照：从账本取每个号最新的额度窗口，并按窗口聚合流水。

/// 上游报告的**一个**额度窗口。窗口名原样保留（`5h`/`7d`/`7d_oi`/`overage` …）。
///
/// 存在的理由：快照原先只有 5h/7d 两组写死的列，而上游的窗口种类是它说了算的——实测里
/// 真正被拒的常常是超额池 `7d_oi`（见 `crate::proxy::rate_limit_scope` 记录的那次 fable-5
/// 429）。它不落库，后台就只能看到「5h/7d 都没满」，却解释不了这个号为什么在烧钱或被拒，
/// 前端只能把状态挂成一个永远摘不掉的「超额待确认」。
///
/// 以 JSON 数组整体存进 `credential_stats.windows`，而不是拆成一张表：快照永远是「最新一份、
/// 整体覆盖」，没有按窗口查询或聚合的需求，一张表换来的只是删号时多四处级联清理。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuotaWindow {
    /// 窗口名，取自 `anthropic-ratelimit-unified-<窗口>-*` 的中段。
    pub name: String,
    /// `…-status`（`allowed`/`allowed_warning`/`rejected`/`rate_limited`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// `…-utilization`，0~1（超额池可能 > 1）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    /// `…-reset`，Unix 秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<i64>,
}

/// 单个凭证最新一次的额度快照（用于凭证卡片展示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuotaSnapshot {
    /// 该快照对应的请求时间（Unix 秒）。
    pub ts: i64,
    pub unified_status: Option<String>,
    pub rl_5h_utilization: Option<f64>,
    pub rl_5h_reset: Option<i64>,
    pub rl_7d_utilization: Option<f64>,
    pub rl_7d_reset: Option<i64>,
    pub rl_representative: Option<String>,
    /// 最近一次带限流头的响应是否动用了 **usage credits**：套餐额度满了但上游照样 200，
    /// 烧的是按量计费的钱。卡片靠它把「满了在烧钱的号」和健康号区分开。
    pub overage_in_use: Option<bool>,
    /// 当前 5h / 7d 窗口内该凭证已用的等价费用（USD）。窗口起点由对应 reset 反推。
    pub cost_5h: Option<f64>,
    pub cost_7d: Option<f64>,
    /// 当前 5h / 7d 窗口内经该凭证转发的请求数。口径与窗口费用完全一致。
    pub requests_5h: Option<i64>,
    pub requests_7d: Option<i64>,
    /// 当前 5h / 7d 窗口内该凭证用掉的**总 token**。窗口与上面两项完全一致，只是换了个量纲。
    ///
    /// 口径按官方 `usage` 对象的四项相加：`input_tokens` + `output_tokens` +
    /// `cache_creation_input_tokens` + `cache_read_input_tokens`。官方这四项互不重叠——缓存命中
    /// 的那部分**不**再计进 `input_tokens`——所以直接相加就是这个窗口真实吞掉的 token 量。
    ///
    /// **不加权**：计价那边给缓存写 ×1.25、缓存读 ×0.1（见 [`crate::pricing`]），但那是**钱**的
    /// 口径；token 数一旦跟着加权，就和上游用量页上的数字对不上了。于是「token 很多、花费很少」
    /// 是常态（缓存读通常占大头），两个数放在一起看才有意义。
    pub tokens_5h: Option<i64>,
    pub tokens_7d: Option<i64>,
    /// 上游本次报告的**全部**窗口（含上面那两个，也含 `7d_oi` 这类没有专用列的）。
    ///
    /// 5h/7d 的专用列没有被它取代，两者并存是有意的：只有这两个窗口有配套的窗口内费用与
    /// 请求数（要靠 `reset` 反推窗口起点去聚合流水），而这里的窗口只有上游给的三个字段。
    /// 前端拿它补齐「专用列覆盖不到的那些窗口」，见 admin-ui 的 quotaRiskMeta。
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
}

/// 5 小时窗口秒数。
pub(super) const WINDOW_5H_SECS: i64 = 5 * 3600;

/// [`CredentialStore::latest_quotas_cached`] 的缓存时长。
pub(super) const QUOTA_LIST_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// 7 天窗口秒数。
pub(super) const WINDOW_7D_SECS: i64 = 7 * 24 * 3600;

use std::collections::HashMap;

use super::CredentialStore;
use anyhow::Result;
use sqlx::Row;

/// 额度快照 + 窗口统计的那条 SQL，见 [`CredentialStore::quota_snapshots`]。`$1` / `$2` 是 5h / 7d
/// 窗口时长，`$3` 为 NULL 时算全部号，否则只算那一个。
///
/// 与 SQLite 版的差异：标量 `MIN(a, b)` 换成 `LEAST`；token 合计是 `SUM(bigint)`，PG 回的是
/// `NUMERIC`，显式压回 `BIGINT`；`GROUP BY s.cred_id` 能带出 `s.*` 的其它列，是因为
/// `cred_id` 是 `credential_stats` 的主键（函数依赖）。
const QUOTA_SNAPSHOTS_SQL: &str = "SELECT s.cred_id, s.snapshot_ts, s.unified_status,
            s.rl_5h_utilization, s.rl_5h_reset,
            s.rl_7d_utilization, s.rl_7d_reset, s.rl_representative, s.overage_in_use,
            s.windows,
            CASE WHEN s.rl_5h_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_5h_reset - $1 THEN u.cost_usd END), 0)
            END,
            CASE WHEN s.rl_7d_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_7d_reset - $2 THEN u.cost_usd END), 0)
            END,
            CASE WHEN s.rl_5h_reset IS NULL THEN NULL ELSE
                COUNT(*) FILTER (WHERE u.ts >= s.rl_5h_reset - $1)
            END,
            CASE WHEN s.rl_7d_reset IS NULL THEN NULL ELSE
                COUNT(*) FILTER (WHERE u.ts >= s.rl_7d_reset - $2)
            END,
            -- 窗口内的总 token（口径见 QuotaSnapshot::tokens_5h）。四项逐个 COALESCE 成 0
            -- 再相加：没嗅探到 usage 的那些行（4xx/429）各列都是 NULL，而 NULL + x 是
            -- NULL，会把整条流水的 token 抹掉。
            -- 缓存写取合计列，它为空时退回 5m/1h 两档之和——同 crate::pricing 的兜底。
            CASE WHEN s.rl_5h_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_5h_reset - $1
                    THEN COALESCE(u.input_tokens, 0) + COALESCE(u.output_tokens, 0)
                       + COALESCE(u.cache_creation_tokens,
                                  COALESCE(u.cache_5m_tokens, 0)
                                + COALESCE(u.cache_1h_tokens, 0))
                       + COALESCE(u.cache_read_tokens, 0)
                END), 0)::BIGINT
            END,
            CASE WHEN s.rl_7d_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_7d_reset - $2
                    THEN COALESCE(u.input_tokens, 0) + COALESCE(u.output_tokens, 0)
                       + COALESCE(u.cache_creation_tokens,
                                  COALESCE(u.cache_5m_tokens, 0)
                                + COALESCE(u.cache_1h_tokens, 0))
                       + COALESCE(u.cache_read_tokens, 0)
                END), 0)::BIGINT
            END
       FROM credential_stats s
       LEFT JOIN usage_logs u
              ON u.cred_id = s.cred_id
             -- 只连**可能落进某个窗口**的流水。没有这个下界，覆盖索引
             -- idx_usage_logs_cred_usage 只能按 cred_id 定位，然后把该账号保留期内的
             -- 全部流水逐行走一遍、靠上面的 CASE 过滤——而窗口最长才 7 天。
             -- 账号列表每次刷新都要跑一遍这条 SQL。下界引用外层的 s，能压成
             -- (cred_id = ? AND ts >= ?) 的范围扫描；上面读的 u.* 列都在那条索引里。
             --
             -- 取两个窗口起点里更早的那个。COALESCE 的第二个参数是给「只有一个窗口
             -- 有 reset」准备的：两个都没有时退化为 0（无下界），此时两个 CASE 本来就
             -- 恒为 NULL，多连的行不影响结果。
             AND u.ts >= LEAST(
                   COALESCE(s.rl_5h_reset - $1, s.rl_7d_reset - $2, 0),
                   COALESCE(s.rl_7d_reset - $2, s.rl_5h_reset - $1, 0))
      WHERE s.snapshot_ts IS NOT NULL
        AND ($3::BIGINT IS NULL OR s.cred_id = $3)
      GROUP BY s.cred_id";

impl CredentialStore {
    /// 每个凭证「最新一条带限流信息」的额度快照（cred_id → 快照），
    /// 并附带当前 5h / 7d 窗口内的累计费用与请求数。
    #[cfg(test)]
    pub async fn latest_quotas(&self) -> Result<HashMap<i64, QuotaSnapshot>> {
        self.quota_snapshots(None).await
    }

    /// 每个凭证的额度快照（口径同 [`Self::latest_quota`]），但结果在进程内缓存 [`QUOTA_LIST_CACHE_TTL`]：账号列表
    /// 每个打开的页面每 10 秒刷一次，而这条 SQL 要把每个号 7 天窗口内的流水逐行加一遍，
    /// 流水多时几百毫秒。窗口统计晚几秒无关紧要；要最新值的（单号详情、阈值判定）不走这里。
    pub async fn latest_quotas_cached(&self) -> Result<HashMap<i64, QuotaSnapshot>> {
        if let Some((at, cached)) = self.quota_cache.lock().as_ref()
            && at.elapsed() < QUOTA_LIST_CACHE_TTL
        {
            return Ok(cached.clone());
        }
        let fresh = self.quota_snapshots(None).await?;
        *self.quota_cache.lock() = Some((std::time::Instant::now(), fresh.clone()));
        Ok(fresh)
    }

    /// 单个凭证的额度快照；口径与 [`Self::latest_quotas`] 完全一致（同一条 SQL）。
    pub async fn latest_quota(&self, cred_id: i64) -> Result<Option<QuotaSnapshot>> {
        Ok(self.quota_snapshots(Some(cred_id)).await?.remove(&cred_id))
    }

    /// 额度快照 + 窗口费用/请求数，一条 SQL 出全部结果。`only` 为 `Some(id)` 时只算该凭证。
    ///
    /// 快照直接读账本（credential_stats，写日志时同事务落好），不从 usage_logs 里扫「最新
    /// 一条带限流信息的行」。窗口统计（起点 = 快照的 reset 反推一个窗口时长）仍从流水条件
    /// 聚合：窗口最长 7 天多一点，流水的保留期覆盖它绰绰有余。
    async fn quota_snapshots(&self, only: Option<i64>) -> Result<HashMap<i64, QuotaSnapshot>> {
        // LEFT JOIN：快照在账本里长存，而窗口内的流水可能已被裁剪清空（此时窗口统计为 0，
        // 语义正确——窗口比保留期短，裁掉的必然是窗口外的行；真正空窗口就该是 0）。
        let rows = sqlx::query(QUOTA_SNAPSHOTS_SQL)
            .bind(WINDOW_5H_SECS)
            .bind(WINDOW_7D_SECS)
            .bind(only)
            .fetch_all(&self.pool)
            .await?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let q = QuotaSnapshot {
                ts: r.try_get(1)?,
                unified_status: r.try_get(2)?,
                rl_5h_utilization: r.try_get(3)?,
                rl_5h_reset: r.try_get(4)?,
                rl_7d_utilization: r.try_get(5)?,
                rl_7d_reset: r.try_get(6)?,
                rl_representative: r.try_get(7)?,
                overage_in_use: r.try_get::<Option<i64>, _>(8)?.map(|v| v != 0),
                // 列为 NULL 或存坏了都只当没有窗口，不让一条脏 JSON 把整张账号列表打成 500。
                windows: r
                    .try_get::<Option<String>, _>(9)?
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default(),
                cost_5h: r.try_get(10)?,
                cost_7d: r.try_get(11)?,
                requests_5h: r.try_get(12)?,
                requests_7d: r.try_get(13)?,
                tokens_5h: r.try_get(14)?,
                tokens_7d: r.try_get(15)?,
            };
            out.insert(r.try_get::<i64, _>(0)?, q);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::settings::store_with_local;
    use super::super::{QuotaWindow, UsageRecord};
    use super::*;

    /// overage-in-use 标记随快照落账：带限流头的响应写入即更新，后续不带头的响应不得抹掉，
    /// 下一条带头的响应按新值覆盖。这是「额度满但不 429（usage credits 在放行）」唯一的外显信号。
    #[sqlx::test]
    async fn overage_marker_lands_in_snapshot(pool: PgPool) {
        let (store, ids) = store_with_local(pool, &["a"]).await;
        let a = ids[0];

        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_5h_utilization: Some(1.02),
                    rl_5h_reset: Some(9_000),
                    rl_overage_in_use: Some(true),
                    ..Default::default()
                },
                Some(1_000),
            )
            .await
            .unwrap();
        assert_eq!(store.latest_quota(a).await.unwrap().unwrap().overage_in_use, Some(true));

        // 不带限流头的响应（CDN 拦截页之类）不动快照。
        store
            .insert_usage_log_at(
                &UsageRecord { cred_id: Some(a), cost_usd: Some(1.0), ..Default::default() },
                Some(2_000),
            )
            .await
            .unwrap();
        let q = store.latest_quota(a).await.unwrap().unwrap();
        assert_eq!(q.overage_in_use, Some(true), "无头响应不得抹掉标记");
        assert_eq!(q.ts, 1_000);

        // 额度恢复后上游不再报 overage → 按新值覆盖。
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_5h_utilization: Some(0.3),
                    rl_5h_reset: Some(20_000),
                    rl_overage_in_use: Some(false),
                    ..Default::default()
                },
                Some(3_000),
            )
            .await
            .unwrap();
        assert_eq!(store.latest_quota(a).await.unwrap().unwrap().overage_in_use, Some(false));
    }

    fn win(name: &str, util: f64, reset: i64, status: &str) -> QuotaWindow {
        QuotaWindow {
            name: name.into(),
            status: Some(status.into()),
            utilization: Some(util),
            reset: Some(reset),
        }
    }

    /// 全窗口快照原样落库、原样读回——含 `7d_oi` 这类**没有专用列**的窗口。
    ///
    /// 这一列存在的全部意义就是它：5h/7d 两组写死的列覆盖不到超额池，而实测里真正被拒的
    /// 正是它，缺了它后台就只能看到「两个窗口都没满」却解释不了这个号为什么在烧钱。
    #[sqlx::test]
    async fn snapshot_keeps_windows_without_dedicated_columns(pool: PgPool) {
        let (store, ids) = store_with_local(pool, &["a"]).await;
        let a = ids[0];

        // 形态取自 proxy::rate_limit_scope 记录的那次真实 fable-5 429：基础窗口都很空，
        // 满掉的只有超额池。
        let windows = vec![
            win("5h", 0.20, 9_000, "allowed"),
            win("7d", 0.70, 90_000, "allowed"),
            win("7d_oi", 1.02, 400_000, "rejected"),
        ];
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_5h_utilization: Some(0.20),
                    rl_5h_reset: Some(9_000),
                    rl_7d_utilization: Some(0.70),
                    rl_7d_reset: Some(90_000),
                    rl_representative: Some("seven_day_overage_included".into()),
                    rl_overage_in_use: Some(true),
                    windows: windows.clone(),
                    ..Default::default()
                },
                Some(1_000),
            )
            .await
            .unwrap();

        let q = store.latest_quota(a).await.unwrap().unwrap();
        assert_eq!(q.windows, windows, "全窗口快照应原样读回");
        // 专用列不受影响：窗口内费用/请求数仍靠它们反推窗口起点。
        assert_eq!(q.rl_5h_utilization, Some(0.20));
        assert_eq!(q.rl_7d_reset, Some(90_000));
        // 批量口径与单条口径是同一条 SQL，不能只有一边带窗口。
        assert_eq!(store.latest_quotas().await.unwrap().get(&a).unwrap().windows, windows);
    }

    /// 窗口内费用 / 请求数 / token 的聚合口径（PG 版新增：旧测试分在别的名单里，这里补一条
    /// 直接覆盖 `SUM(bigint)::BIGINT`、`COUNT(*) FILTER` 与 `LEAST` 下界的翻译）。
    #[sqlx::test]
    async fn window_aggregates_count_cost_and_tokens(pool: PgPool) {
        let (store, ids) = store_with_local(pool, &["a"]).await;
        let a = ids[0];
        // 快照：5h reset 在 50_000（窗口起点 32_000），没有 7d reset。
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_5h_utilization: Some(0.5),
                    rl_5h_reset: Some(50_000),
                    cost_usd: Some(2.0),
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    cache_read_tokens: Some(100),
                    ..Default::default()
                },
                Some(40_000),
            )
            .await
            .unwrap();
        // 窗口外的一条不计。
        store
            .insert_usage_log_at(
                &UsageRecord { cred_id: Some(a), cost_usd: Some(9.0), ..Default::default() },
                Some(10_000),
            )
            .await
            .unwrap();
        let q = store.latest_quota(a).await.unwrap().unwrap();
        assert_eq!(q.cost_5h, Some(2.0));
        assert_eq!(q.requests_5h, Some(1));
        assert_eq!(q.tokens_5h, Some(115));
        assert_eq!(q.cost_7d, None, "无 7d reset 时不应给出 0");
        assert_eq!(q.requests_7d, None);
        assert_eq!(q.tokens_7d, None);
    }

    /// 老库补出来的 `windows` 是 NULL，读回时退化成空列表（前端即按「只有 5h/7d」渲染），
    /// 而不是把整张账号列表打成 500。存进脏 JSON 同理。
    #[sqlx::test]
    async fn legacy_and_corrupt_windows_degrade_to_empty(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a: i64 = sqlx::query_scalar(
            "INSERT INTO credentials (label, access_token, refresh_token, expires_at) \
             VALUES ('a', 'ta', 'ra', 0) RETURNING id",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();

        // 模拟老库：快照行有 5h/7d，windows 列为 NULL。
        sqlx::query(
            "INSERT INTO credential_stats
                     (cred_id, snapshot_ts, rl_5h_utilization, rl_5h_reset, windows)
                 VALUES ($1, 1000, 0.4, 9000, NULL)",
        )
        .bind(a)
        .execute(&store.pool)
        .await
        .unwrap();
        let q = store.latest_quota(a).await.unwrap().unwrap();
        assert!(q.windows.is_empty(), "老库的 NULL 应读成空列表");
        assert_eq!(q.rl_5h_utilization, Some(0.4), "专用列照常可用");

        sqlx::query("UPDATE credential_stats SET windows = '{oops'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            store.latest_quota(a).await.unwrap().unwrap().windows.is_empty(),
            "脏 JSON 不得报错"
        );
    }
}
