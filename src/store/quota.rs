//! 额度快照：从流水里取每个号最新的额度窗口。

use super::*;

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

/// 7 天窗口秒数。
pub(super) const WINDOW_7D_SECS: i64 = 7 * 24 * 3600;

/// 额度快照 + 窗口统计的那条 SQL，见 [`CredentialStore::quota_snapshots`]。拎成常量是为了让
/// 测试能对它跑 EXPLAIN QUERY PLAN，钉住「流水那一侧只走覆盖索引、不回表」。
pub(super) const QUOTA_SNAPSHOTS_SQL: &str = "SELECT s.cred_id, s.snapshot_ts, s.unified_status,
            s.rl_5h_utilization, s.rl_5h_reset,
            s.rl_7d_utilization, s.rl_7d_reset, s.rl_representative, s.overage_in_use,
            s.windows,
            CASE WHEN s.rl_5h_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_5h_reset - ?1 THEN u.cost_usd END), 0)
            END,
            CASE WHEN s.rl_7d_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_7d_reset - ?2 THEN u.cost_usd END), 0)
            END,
            CASE WHEN s.rl_5h_reset IS NULL THEN NULL ELSE
                SUM(CASE WHEN u.ts >= s.rl_5h_reset - ?1 THEN 1 ELSE 0 END)
            END,
            CASE WHEN s.rl_7d_reset IS NULL THEN NULL ELSE
                SUM(CASE WHEN u.ts >= s.rl_7d_reset - ?2 THEN 1 ELSE 0 END)
            END,
            -- 窗口内的总 token（口径见 QuotaSnapshot::tokens_5h）。四项逐个 COALESCE 成 0
            -- 再相加：没嗅探到 usage 的那些行（4xx/429）各列都是 NULL，而 NULL + x 在
            -- SQLite 里是 NULL，会把整条流水的 token 抹掉。
            -- 缓存写取合计列，它为空时退回 5m/1h 两档之和——同 crate::pricing 的兜底。
            CASE WHEN s.rl_5h_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_5h_reset - ?1
                    THEN COALESCE(u.input_tokens, 0) + COALESCE(u.output_tokens, 0)
                       + COALESCE(u.cache_creation_tokens,
                                  COALESCE(u.cache_5m_tokens, 0)
                                + COALESCE(u.cache_1h_tokens, 0))
                       + COALESCE(u.cache_read_tokens, 0)
                END), 0)
            END,
            CASE WHEN s.rl_7d_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_7d_reset - ?2
                    THEN COALESCE(u.input_tokens, 0) + COALESCE(u.output_tokens, 0)
                       + COALESCE(u.cache_creation_tokens,
                                  COALESCE(u.cache_5m_tokens, 0)
                                + COALESCE(u.cache_1h_tokens, 0))
                       + COALESCE(u.cache_read_tokens, 0)
                END), 0)
            END
       FROM credential_stats s
       LEFT JOIN usage_logs u
              ON u.cred_id = s.cred_id
             -- 只连**可能落进某个窗口**的流水。没有这个下界，索引
             -- idx_usage_logs_cred_usage 只能按 cred_id 定位，然后把该账号保留期内
             -- （保留期）的全部流水逐行走一遍、靠上面的 CASE 过滤——而窗口最长才 7 天。
             -- 账号列表每次刷新都要跑一遍这条 SQL。
             -- 下界引用外层的 s，故 SQLite 能把它压成 (cred_id=? AND ts>=?) 的范围扫描；
             -- 上面读的 u.* 列都在那条索引里，整段扫描不回表（见 init_schema 的注）。
             --
             -- 取两个窗口起点里更早的那个。COALESCE 的第二个参数是给「只有一个窗口
             -- 有 reset」准备的：min(NULL, x) 在 SQLite 里是 NULL，会把条件变成假、
             -- 一行都连不上，那就把窗口费用算成 0 了。两个都没有时退化为 0（无下界），
             -- 此时两个 CASE 本来就恒为 NULL，多连的行不影响结果。
             AND u.ts >= MIN(
                   COALESCE(s.rl_5h_reset - ?1, s.rl_7d_reset - ?2, 0),
                   COALESCE(s.rl_7d_reset - ?2, s.rl_5h_reset - ?1, 0))
      WHERE s.snapshot_ts IS NOT NULL
        AND (?3 IS NULL OR s.cred_id = ?3)
      GROUP BY s.cred_id";

impl CredentialStore {
    /// 每个凭证「最新一条带限流信息」的额度快照（cred_id → 快照），
    /// 并附带当前 5h / 7d 窗口内的累计费用与请求数。
    pub fn latest_quotas(&self) -> Result<HashMap<i64, QuotaSnapshot>> {
        self.quota_snapshots(None)
    }

    /// 单个凭证的额度快照；口径与 [`Self::latest_quotas`] 完全一致（同一条 SQL）。
    pub fn latest_quota(&self, cred_id: i64) -> Result<Option<QuotaSnapshot>> {
        Ok(self.quota_snapshots(Some(cred_id))?.remove(&cred_id))
    }

    /// 额度快照 + 窗口费用/请求数，一条 SQL 出全部结果。`only` 为 `Some(id)` 时只算该凭证。
    ///
    /// 快照直接读账本（credential_stats，写日志时同事务落好），不再从 usage_logs 里
    /// 扫「最新一条带限流信息的行」——那条 CTE 的过滤列不在索引里，表越大回表越多。
    /// 窗口统计（起点 = 快照的 reset 反推一个窗口时长）仍从流水条件聚合：窗口最长 7 天
    /// 多一点，流水的保留期（见 [`Self::prune_usage_logs`]）覆盖它绰绰有余。
    fn quota_snapshots(&self, only: Option<i64>) -> Result<HashMap<i64, QuotaSnapshot>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(QUOTA_SNAPSHOTS_SQL)?;
        // LEFT JOIN：快照在账本里长存，而窗口内的流水可能已被裁剪清空（此时窗口统计为 0，
        // 语义正确——窗口比保留期短，裁掉的必然是窗口外的行；真正空窗口就该是 0）。
        let rows = stmt.query_map(params![WINDOW_5H_SECS, WINDOW_7D_SECS, only], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                QuotaSnapshot {
                    ts: r.get(1)?,
                    unified_status: r.get(2)?,
                    rl_5h_utilization: r.get(3)?,
                    rl_5h_reset: r.get(4)?,
                    rl_7d_utilization: r.get(5)?,
                    rl_7d_reset: r.get(6)?,
                    rl_representative: r.get(7)?,
                    overage_in_use: r.get(8)?,
                    // 老库补出来的列是 NULL；真存坏了也只当没有窗口，不让一条脏 JSON
                    // 把整张账号列表打成 500。
                    windows: r
                        .get::<_, Option<String>>(9)?
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default(),
                    cost_5h: r.get(10)?,
                    cost_7d: r.get(11)?,
                    requests_5h: r.get(12)?,
                    requests_7d: r.get(13)?,
                    tokens_5h: r.get(14)?,
                    tokens_7d: r.get(15)?,
                },
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, q) = row?;
            out.insert(cid, q);
        }
        Ok(out)
    }
}
