//! 用量流水的 PG 版，对应 `store::usage`：落库、翻页查询与裁剪。
//!
//! [`UsageRecord`] / [`UsageLog`] / [`UsageLogQuery`] 等类型、列清单 [`USAGE_LOG_COLS`] 与
//! 保留期常量直接用 `store::usage` 的。

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::{Arguments, Row};

use super::super::{
    ERROR_MESSAGE_MAX, FREEZE_TAIL_SECS, Forensics, RESPONSE_EXCERPT_MAX, USAGE_LOG_COLS,
    USAGE_LOG_RETENTION_SECS, UsageLog, UsageLogQuery, UsageLogStats, UsageRecord, head_chars,
};
use super::PgStore;
use super::billing::billing_record;
use super::rollup::rollup_record;

/// 往动态拼的参数表里追加一个值。
fn push<'q, T>(args: &mut PgArguments, v: T) -> Result<usize>
where
    T: 'q + sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    args.add(v).map_err(anyhow::Error::from_boxed)?;
    Ok(args.len())
}

/// 把 [`UsageLogQuery`] 的筛选条件拼成 `WHERE …`（可能为空串）与对应的绑定参数，口径同
/// `UsageLogQuery::where_clause`。
///
/// **按条件动态拼而不是写 `($1 IS NULL OR col = $1)`**：那种写法规划器得为「参数可能是 NULL」
/// 留后路，按号翻页与按请求 id 查就用不上各自的索引了。统计与取页共用这一份，「共 N 条」与
/// 翻得到的记录永远是同一个集合。
pub(super) fn where_clause(q: &UsageLogQuery) -> Result<(String, PgArguments)> {
    let mut clauses: Vec<String> = Vec::new();
    let mut args = PgArguments::default();
    if let Some(c) = q.cred_id {
        let n = push(&mut args, c)?;
        clauses.push(format!("cred_id = ${n}"));
    }
    if let Some(o) = q.owner_id {
        let n = push(&mut args, o)?;
        clauses.push(format!("cred_id IN (SELECT id FROM credentials WHERE owner_id = ${n})"));
    }
    if let Some(u) = q.until_id {
        let n = push(&mut args, u)?;
        clauses.push(format!("id <= ${n}"));
    }
    if let Some(r) = q.request_id.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        let n = push(&mut args, r.to_string())?;
        clauses.push(format!("request_id = ${n}"));
    }
    if let Some(m) = q.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        let n = push(&mut args, m.to_string())?;
        clauses.push(format!("model = ${n}"));
    }
    if let Some(s) = q.since {
        let n = push(&mut args, s)?;
        clauses.push(format!("ts >= ${n}"));
    }
    if let Some(k) = q.session_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        let n = push(&mut args, k.to_string())?;
        clauses.push(format!("session_key = ${n}"));
    }
    // 出站与来访任一命中。两个 OR 分支各有自己的部分索引，PG 走 BitmapOr，不会退成整表扫。
    if let Some(sid) = q.session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let n = push(&mut args, sid.to_string())?;
        clauses.push(format!("(session_id = ${n} OR session_id_in = ${n})"));
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    Ok((sql, args))
}

/// 流水统计（条数、花费合计、最大 id）的 SQL，见 [`PgStore::usage_log_stats`]。
pub(super) fn usage_log_stats_sql(where_sql: &str) -> String {
    format!("SELECT COUNT(*), COALESCE(SUM(cost_usd), 0), MAX(id) FROM usage_logs{where_sql}")
}

/// 流水取页的 SQL，见 [`PgStore::query_usage_logs`]。`n` 是参数个数，最后两个是
/// LIMIT / OFFSET。
///
/// **分两步**：子查询只按筛选取出这一页的 id，外层再按 id 读整行。子查询只碰 id，各条筛选
/// 索引都能覆盖它；OFFSET 跳过的那些行只在索引里跳，不回表读整行。
pub(super) fn usage_log_page_sql(where_sql: &str, n: usize) -> String {
    format!(
        "SELECT id, {USAGE_LOG_COLS}
           FROM usage_logs
          WHERE id IN (SELECT id FROM usage_logs{where_sql}
                        ORDER BY id DESC LIMIT ${} OFFSET ${})
          ORDER BY id DESC",
        n - 1,
        n
    )
}

/// 写一条流水并顺手补冻结的那条 SQL，见 [`PgStore::insert_usage_log_at`]。
///
/// 写流水与「刚封的号补进冻结表」合成一条：CTE 里插流水、`RETURNING` 整行，外层按这一行的
/// 号与时刻找封后 [`FREEZE_TAIL_SECS`] 内最近的那条封号事件，有就把这一行照搬进冻结表。
static INSERT_USAGE_LOG_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "WITH ins AS (
             INSERT INTO usage_logs
                (ts, cred_id, cred_label, device_id, model, path, status, has_usage,
                 input_tokens, output_tokens, cache_creation_tokens, cache_5m_tokens,
                 cache_1h_tokens, cache_read_tokens, ttft_ms, total_ms,
                 unified_status, rl_5h_status, rl_5h_reset, rl_5h_utilization,
                 rl_7d_status, rl_7d_reset, rl_7d_utilization, rl_representative,
                 rl_overage_in_use, ratelimit_raw, cost_usd, ua, ua_out, sse_aggregated,
                 request_id, upstream_request_id,
                 proxy, simulated, shape, session_id, error_type, error_message, third_party,
                 rewrites, device_id_out, response_excerpt, sim_reason, session_key,
                 session_id_in)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                     $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29,
                     $30, $31, $32, $33, $34, $35, $36, $37, $38, $39, $40, $41, $42, $43, $44,
                     $45)
             RETURNING id, {USAGE_LOG_COLS})
         INSERT INTO usage_logs_frozen (ban_event_id, src_id, {USAGE_LOG_COLS})
         SELECT b.id, ins.id, {USAGE_LOG_COLS}
           FROM ins
           JOIN LATERAL (SELECT id FROM ban_events
                          WHERE cred_id = ins.cred_id AND ts >= ins.ts - $46
                          ORDER BY id DESC LIMIT 1) b ON TRUE"
    )
});

/// 落账（`credential_stats` 与 `device_costs`）的那条 SQL：不带额度快照的。设备账本在 CTE 里，
/// `$4`（device_id）为 NULL 时不记。
const LEDGER_SQL: &str = "WITH dev AS (
         INSERT INTO device_costs AS d (device_id, cred_id, cost_usd, request_count)
         SELECT $4, $1, COALESCE($3, 0), 1 WHERE $4::TEXT IS NOT NULL
         ON CONFLICT (device_id, cred_id) DO UPDATE
                SET cost_usd = d.cost_usd + excluded.cost_usd,
                    request_count = d.request_count + 1)
     INSERT INTO credential_stats AS s (cred_id, last_used_at, cost_total_usd)
     VALUES ($1, $2, COALESCE($3, 0))
     ON CONFLICT (cred_id) DO UPDATE SET
         last_used_at   = excluded.last_used_at,
         cost_total_usd = s.cost_total_usd + excluded.cost_total_usd";

/// 同上，带额度快照（`$5` 起），整份覆盖账本里的快照列。
const LEDGER_SNAPSHOT_SQL: &str = "WITH dev AS (
         INSERT INTO device_costs AS d (device_id, cred_id, cost_usd, request_count)
         SELECT $4, $1, COALESCE($3, 0), 1 WHERE $4::TEXT IS NOT NULL
         ON CONFLICT (device_id, cred_id) DO UPDATE
                SET cost_usd = d.cost_usd + excluded.cost_usd,
                    request_count = d.request_count + 1)
     INSERT INTO credential_stats AS s
         (cred_id, last_used_at, cost_total_usd, snapshot_ts, unified_status,
          rl_5h_utilization, rl_5h_reset, rl_7d_utilization, rl_7d_reset, rl_representative,
          overage_in_use, windows)
     VALUES ($1, $2, COALESCE($3, 0), $2, $5, $6, $7, $8, $9, $10, $11, $12)
     ON CONFLICT (cred_id) DO UPDATE SET
         last_used_at      = excluded.last_used_at,
         cost_total_usd    = s.cost_total_usd + excluded.cost_total_usd,
         snapshot_ts       = excluded.snapshot_ts,
         unified_status    = excluded.unified_status,
         rl_5h_utilization = excluded.rl_5h_utilization,
         rl_5h_reset       = excluded.rl_5h_reset,
         rl_7d_utilization = excluded.rl_7d_utilization,
         rl_7d_reset       = excluded.rl_7d_reset,
         rl_representative = excluded.rl_representative,
         overage_in_use    = excluded.overage_in_use,
         windows           = excluded.windows";

impl PgStore {
    /// 最近 `days` 天流水里出现过的模型（去重），附最后一次出现的时刻，按时刻倒序。
    /// 连通性测试自己打的那些（`device_id = 'probe'`）不算——它们是人挑的，不是客户端在用的。
    ///
    /// 不按 ts 扫窗口再 GROUP BY：那样要把窗口内的全部流水逐行回表读 model / device_id。改成在
    /// `idx_usage_logs_model_ts` 上跳着走（递归 CTE 模拟 loose index scan）：先逐个取下一个
    /// 不同的模型名，再对每个模型从最新一条往回找第一条非 probe 的——模型就十来个。
    pub async fn recent_models(&self, days: i64) -> Result<Vec<(String, i64)>> {
        Ok(sqlx::query_as(
            "WITH RECURSIVE m(model) AS (
                 SELECT MIN(model) FROM usage_logs WHERE model > ''
                 UNION ALL
                 SELECT (SELECT MIN(model) FROM usage_logs WHERE model > m.model)
                   FROM m WHERE m.model IS NOT NULL
             )
             SELECT model, last_ts FROM (
                 SELECT model,
                        (SELECT MAX(ts) FROM usage_logs u
                          WHERE u.model = m.model
                            AND u.ts >= unixepoch() - $1 * 86400
                            AND (u.device_id IS NULL OR u.device_id <> 'probe')) AS last_ts
                   FROM m WHERE model IS NOT NULL
             ) s
             WHERE last_ts IS NOT NULL
             ORDER BY last_ts DESC",
        )
        .bind(days)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 写入一条用量日志。
    pub async fn insert_usage_log(&self, rec: &UsageRecord) -> Result<()> {
        self.insert_usage_log_at(rec, None).await
    }

    /// 写入一条用量日志，并在**同一事务**里把账本（credential_stats / device_costs）、预聚合
    /// （usage_rollup）与费用汇总（billing_hourly）记上。
    ///
    /// 账本承接三个终身口径：最近使用、累计费用、最新额度快照。流水只保留近期（见
    /// [`Self::prune_usage_logs`]），这些口径若继续从流水聚合，裁剪一跑数字就会跟着变小；写时
    /// 落账之后，读路径不再依赖流水的历史深度。同一事务保证几边不漂移。
    ///
    /// `ts` 为 `None` 时取库的时钟（`unixepoch()`，即事务开始的时刻）；拆出这个参数是给测试
    /// 用的——窗口/裁剪相关的用例需要指定「这条流水发生在何时」。
    ///
    /// 每条转发请求后都跑：普通事务（不拿全局写锁），语句能合的都合了。并发正确性靠行锁：
    ///
    /// - 第一步给这个号的账号行上 `FOR KEY SHARE`，顺手读出号主（费用汇总的号主就定在这一刻）。
    ///   它和别的流水写入、改号的普通 UPDATE 都不冲突，只和 [`Self::record_ban`] 的
    ///   `FOR UPDATE` 互斥：封号事件落地与这条流水的写入一定分得出先后——先封的，这里后面的
    ///   语句看得到那条事件、把这一行补进冻结表；先写的，封号那边等它提交后再冻结、冻得到它。
    ///   SQLite 版靠全局写锁串行化天然如此，PG 版不加这一步，两边各看各的快照就会漏掉
    ///   触发封号的那一发；
    /// - 账本、预聚合、费用汇总都是 `INSERT … ON CONFLICT DO UPDATE` 原地累加，并发写同一行
    ///   由行锁排队，不会丢；预聚合的直方图读改写见 [`rollup_record`]。
    pub(super) async fn insert_usage_log_at(
        &self,
        rec: &UsageRecord,
        ts: Option<i64>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let (ts, owner) = if ts.is_some() && rec.cred_id.is_none() {
            (ts.unwrap_or_default(), None)
        } else {
            let (now, owner): (i64, Option<i64>) = sqlx::query_as(
                "SELECT unixepoch(),
                        (SELECT owner_id FROM credentials WHERE id = $1 FOR KEY SHARE)",
            )
            .bind(rec.cred_id)
            .fetch_one(&mut *tx)
            .await?;
            (ts.unwrap_or(now), owner)
        };
        // 刚封的号：封号事件落地时冻结的是**当时已有**的流水，而触发封号的那一发（以及同时
        // 在途的几发）要等响应流结束才落库，冻结时还不存在。故封后 FREEZE_TAIL_SECS 内到达
        // 的这个号的流水，写入时顺手补进冻结表——不然最要紧的那一条恰好缺席。
        sqlx::query(sqlx::AssertSqlSafe(INSERT_USAGE_LOG_SQL.as_str()))
            .bind(ts)
            .bind(rec.cred_id)
            .bind(&rec.cred_label)
            .bind(&rec.device_id)
            .bind(&rec.model)
            .bind(&rec.path)
            .bind(rec.status as i64)
            .bind(rec.has_usage as i64)
            .bind(rec.input_tokens)
            .bind(rec.output_tokens)
            .bind(rec.cache_creation_tokens)
            .bind(rec.cache_5m_tokens)
            .bind(rec.cache_1h_tokens)
            .bind(rec.cache_read_tokens)
            .bind(rec.ttft_ms)
            .bind(rec.total_ms)
            .bind(&rec.unified_status)
            .bind(&rec.rl_5h_status)
            .bind(rec.rl_5h_reset)
            .bind(rec.rl_5h_utilization)
            .bind(&rec.rl_7d_status)
            .bind(rec.rl_7d_reset)
            .bind(rec.rl_7d_utilization)
            .bind(&rec.rl_representative)
            .bind(rec.rl_overage_in_use.map(i64::from))
            .bind(&rec.ratelimit_raw)
            .bind(rec.cost_usd)
            .bind(&rec.ua)
            .bind(&rec.ua_out)
            .bind(rec.sse_aggregated as i64)
            .bind(&rec.request_id)
            .bind(&rec.upstream_request_id)
            .bind(&rec.forensics.proxy)
            .bind(rec.forensics.simulated as i64)
            .bind(&rec.forensics.shape)
            .bind(&rec.forensics.session_id)
            .bind(&rec.forensics.error_type)
            .bind(rec.forensics.error_message.as_deref().map(|m| head_chars(m, ERROR_MESSAGE_MAX)))
            .bind(rec.forensics.third_party as i64)
            .bind(&rec.forensics.rewrites)
            .bind(&rec.forensics.device_id_out)
            .bind(
                rec.forensics
                    .response_excerpt
                    .as_deref()
                    .map(|m| head_chars(m, RESPONSE_EXCERPT_MAX)),
            )
            .bind(&rec.forensics.sim_reason)
            .bind(&rec.forensics.session_key)
            .bind(&rec.forensics.session_id_in)
            .bind(FREEZE_TAIL_SECS)
            .execute(&mut *tx)
            .await?;
        // 预聚合：延迟 / 缓存趋势与拆分表读它，不再按时间扫流水，见 `rollup` 模块。
        rollup_record(&mut tx, ts, rec).await?;
        // 费用汇总：号主、分组、接入 Key 在这一刻定死，见 `billing` 模块。
        billing_record(&mut tx, ts, rec, owner).await?;
        // 落账。cred_id 为空的流水（还没选到凭证就失败的请求）无处归属，只记日志不记账。
        if let Some(cid) = rec.cred_id {
            // 快照只在响应带**窗口级**限流信息时覆盖，口径同旧版「最新一条带限流信息的行」
            // ——更晚的普通响应不能把快照抹掉。
            //
            // 判据里的 `!rec.windows.is_empty()` 不是冗余：一个只上报 `7d_oi` 之类窗口的账号
            // 若只认 5h/7d 两个专用字段就**永远写不进快照**。窗口种类是上游说了算的。
            //
            // 仍然不认「只有 unified_status / overage_in_use、一个窗口都没有」的响应：
            // 那种覆盖会把已有的窗口列一并抹成空，拿一条信息更少的快照换掉信息更多的。
            //
            // 设备账本只要认得出设备就记一笔：**请求数无条件 +1**，费用取不到时按 0 计——
            // 4xx/429 这些没有 usage 的请求同样是这台设备打出去的，排查限流恰恰要看这些。
            let snapshot = rec.rl_5h_utilization.is_some()
                || rec.rl_7d_utilization.is_some()
                || !rec.windows.is_empty();
            let q = sqlx::query(if snapshot { LEDGER_SNAPSHOT_SQL } else { LEDGER_SQL })
                .bind(cid)
                .bind(ts)
                .bind(rec.cost_usd)
                .bind(&rec.device_id);
            let q = if snapshot {
                // 序列化失败在这里不可达（定长结构），真失败也只是少存这一列，不该把整条
                // 用量日志连坐掉。
                q.bind(&rec.unified_status)
                    .bind(rec.rl_5h_utilization)
                    .bind(rec.rl_5h_reset)
                    .bind(rec.rl_7d_utilization)
                    .bind(rec.rl_7d_reset)
                    .bind(&rec.rl_representative)
                    .bind(rec.rl_overage_in_use.map(i64::from))
                    .bind(serde_json::to_string(&rec.windows).ok())
            } else {
                q
            };
            q.execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 裁掉超过保留期（[`USAGE_LOG_RETENTION_SECS`]）的用量日志流水，返回删除条数。汇总另有
    /// 保留期（90 天，比流水长），随这里一起裁。
    ///
    /// 流水裁剪不影响任何终身口径——最近使用/累计费用/最新快照都在账本里（写时落账）；还要读
    /// 流水的只剩两处：5h/7d 窗口统计（最多回看 7 天多）和请求日志页（只翻近期），8 天都覆盖
    /// 得住。
    ///
    /// 分批删、批间歇一下：日志表可能积了几百万行，一条大 DELETE 是一个长事务，产生的 WAL
    /// 与死元组一次性压上来，复制与 autovacuum 都跟着抖；分批让在线写入平稳穿插。
    pub async fn prune_usage_logs(&self) -> Result<usize> {
        const BATCH: i64 = 500;
        const PAUSE: Duration = Duration::from_millis(50);
        let mut total = 0;
        loop {
            let n = sqlx::query(
                "DELETE FROM usage_logs WHERE id IN (
                     SELECT id FROM usage_logs WHERE ts < unixepoch() - $1 LIMIT $2)",
            )
            .bind(USAGE_LOG_RETENTION_SECS)
            .bind(BATCH)
            .execute(&self.pool)
            .await?
            .rows_affected() as usize;
            total += n;
            if (n as i64) < BATCH {
                break;
            }
            tokio::time::sleep(PAUSE).await;
        }
        self.prune_rollup().await?;
        Ok(total)
    }

    /// 最近的用量日志，按时间倒序，最多 `limit` 条。测试用；线上那两条路径都带筛选，
    /// 直接走 [`Self::query_usage_logs`]。
    #[cfg(test)]
    pub async fn list_usage_logs(&self, limit: i64) -> Result<Vec<UsageLog>> {
        self.query_usage_logs(UsageLogQuery { limit, ..Default::default() }).await
    }

    /// 同一批筛选条件下的条数、花费合计与最大 id。见 [`UsageLogStats`]。
    ///
    /// `q` 里的 `limit`/`offset` **不参与**——统计的是整个集合，不是当前这一页。
    pub async fn usage_log_stats(&self, q: UsageLogQuery) -> Result<UsageLogStats> {
        let (where_sql, args) = where_clause(&q)?;
        let row = sqlx::query_with(sqlx::AssertSqlSafe(usage_log_stats_sql(&where_sql)), args)
            .fetch_one(&self.pool)
            .await?;
        Ok(UsageLogStats {
            total: row.try_get(0)?,
            cost_usd: row.try_get(1)?,
            max_id: row.try_get(2)?,
        })
    }

    /// 按条件查用量流水，恒按 `id` 倒序。见 [`UsageLogQuery`] 与 [`usage_log_page_sql`]。
    pub async fn query_usage_logs(&self, q: UsageLogQuery) -> Result<Vec<UsageLog>> {
        let (where_sql, mut args) = where_clause(&q)?;
        push(&mut args, q.limit)?;
        let n = push(&mut args, q.offset)?;
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(usage_log_page_sql(&where_sql, n)), args)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(usage_log_from_row).collect()
    }
}

/// 按 [`USAGE_LOG_COLS`] 的顺序把一行读成 [`UsageLog`]（0 号列是主键）。
pub(super) fn usage_log_from_row(r: &PgRow) -> Result<UsageLog> {
    Ok(UsageLog {
        id: r.try_get(0)?,
        ts: r.try_get(1)?,
        cred_id: r.try_get(2)?,
        cred_label: r.try_get(3)?,
        device_id: r.try_get(4)?,
        model: r.try_get(5)?,
        path: r.try_get(6)?,
        status: r.try_get::<i64, _>(7)? as u16,
        has_usage: r.try_get::<i64, _>(8)? != 0,
        input_tokens: r.try_get(9)?,
        output_tokens: r.try_get(10)?,
        cache_creation_tokens: r.try_get(11)?,
        cache_5m_tokens: r.try_get(12)?,
        cache_1h_tokens: r.try_get(13)?,
        cache_read_tokens: r.try_get(14)?,
        ttft_ms: r.try_get(15)?,
        total_ms: r.try_get(16)?,
        unified_status: r.try_get(17)?,
        rl_5h_status: r.try_get(18)?,
        rl_5h_reset: r.try_get(19)?,
        rl_5h_utilization: r.try_get(20)?,
        rl_7d_status: r.try_get(21)?,
        rl_7d_reset: r.try_get(22)?,
        rl_7d_utilization: r.try_get(23)?,
        rl_representative: r.try_get(24)?,
        ratelimit_raw: r.try_get(25)?,
        cost_usd: r.try_get(26)?,
        rl_overage_in_use: r.try_get::<Option<i64>, _>(27)?.map(|v| v != 0),
        ua: r.try_get(28)?,
        ua_out: r.try_get(29)?,
        sse_aggregated: r.try_get::<i64, _>(30)? != 0,
        request_id: r.try_get(31)?,
        upstream_request_id: r.try_get(32)?,
        forensics: Forensics {
            proxy: r.try_get(33)?,
            simulated: r.try_get::<i64, _>(34)? != 0,
            shape: r.try_get(35)?,
            session_id: r.try_get(36)?,
            error_type: r.try_get(37)?,
            error_message: r.try_get(38)?,
            third_party: r.try_get::<i64, _>(39)? != 0,
            rewrites: r.try_get(40)?,
            device_id_out: r.try_get(41)?,
            response_excerpt: r.try_get(42)?,
            sim_reason: r.try_get(43)?,
            session_key: r.try_get(44)?,
            session_id_in: r.try_get(45)?,
        },
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// 建一个测试库和若干个号（号主 admin），回号的 id。
    pub(in crate::store::pg) async fn store_with(
        pool: sqlx::PgPool,
        labels: &[&str],
    ) -> (PgStore, Vec<i64>) {
        let store = PgStore::for_test(pool).await;
        let mut ids = Vec::new();
        for l in labels {
            // refresh_token 有唯一约束，按 label 取值保证互不相同。
            let c = store
                .insert(l, None, &format!("tok-{l}"), &format!("refresh-{l}"), 0, None, None, 1)
                .await
                .unwrap();
            ids.push(c.id);
        }
        (store, ids)
    }

    /// 库的当前时刻。
    pub(in crate::store::pg) async fn db_now(store: &PgStore) -> i64 {
        sqlx::query_scalar("SELECT unixepoch()").fetch_one(&store.pool).await.unwrap()
    }

    /// 写一条带用量与（可选）限流头的流水，走真实的写入路径（流水 + 账本同一事务）。
    /// 每条记 10 个 token（输入/输出/缓存写/缓存读 各 1 + 3 + 2 + 4）。
    pub(in crate::store::pg) async fn log_row(
        store: &PgStore,
        cred_id: i64,
        ts: i64,
        cost: f64,
        r5: Option<i64>,
        r7: Option<i64>,
    ) {
        let rec = UsageRecord {
            cred_id: Some(cred_id),
            cost_usd: Some(cost),
            has_usage: true,
            input_tokens: Some(1),
            output_tokens: Some(3),
            cache_creation_tokens: Some(2),
            cache_read_tokens: Some(4),
            rl_5h_utilization: r5.map(|_| 0.5),
            rl_5h_reset: r5,
            rl_7d_utilization: r7.map(|_| 0.25),
            rl_7d_reset: r7,
            ..Default::default()
        };
        store.insert_usage_log_at(&rec, Some(ts)).await.unwrap();
    }

    #[test]
    fn redact_proxy_hides_only_the_password() {
        use super::super::super::redact_proxy;
        assert_eq!(redact_proxy("socks5h://u:p@h:1"), "socks5h://u:***@h:1");
        assert_eq!(redact_proxy("http://h:8080"), "http://h:8080");
        assert_eq!(redact_proxy("http://u@h:8080"), "http://u@h:8080");
        assert_eq!(redact_proxy("http://u:p:q@h"), "http://u:***@h");
    }

    /// 裁剪只动流水，不动账本：累计费用/最近使用/额度快照在裁剪后原样保留。
    #[sqlx::test]
    async fn prune_keeps_ledger(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];

        // 一条早已过保留期的旧流水（带限流头，会写快照）+ 一条刚发生的新流水（无头）。
        let old_ts = 1_000;
        log_row(&store, a, old_ts, 2.0, Some(old_ts + 100), Some(old_ts + 100)).await;
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(a),
                cost_usd: Some(1.0),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(store.prune_usage_logs().await.unwrap(), 1, "只裁过保留期的旧流水");
        assert_eq!(store.list_usage_logs(10).await.unwrap().len(), 1, "新流水应保留");
        assert_eq!(store.cost_of(a).await.unwrap(), 3.0, "累计费用是账本口径，不随裁剪变小");
        assert!(store.last_used_at(a).await.unwrap().is_some());
        let q = store.latest_quota(a).await.unwrap().expect("快照在账本里长存");
        assert_eq!(q.ts, old_ts, "快照仍是最后一次带限流头的那条");
        // 窗口统计只看还留着的流水：新流水 ts 在窗口起点之后，计入。
        assert_eq!(q.cost_5h, Some(1.0));
        assert_eq!(q.requests_5h, Some(1));
    }

    /// 「非流转流」标记要能落库并原样读回；没写这一列的行（这里用裸 INSERT 造一条）取列默认
    /// 值 0，读回 false 而不是读取失败。
    ///
    /// SQLite 版测的是老库补列的升级路径，PG 版没有升级路径，只保留「默认 false」这一半。
    #[sqlx::test]
    async fn sse_aggregated_round_trips_and_defaults_to_false(pool: sqlx::PgPool) {
        let store = PgStore::for_test(pool).await;
        sqlx::query("INSERT INTO usage_logs (cred_label) VALUES ('old')")
            .execute(&store.pool)
            .await
            .unwrap();
        let cred = store.insert("a", None, "t", "r", 0, None, None, 1).await.unwrap().id;

        for aggregated in [true, false] {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    sse_aggregated: aggregated,
                    ..Default::default()
                })
                .await
                .unwrap();
        }

        let logs = store
            .query_usage_logs(UsageLogQuery { limit: 10, ..Default::default() })
            .await
            .unwrap();
        // 倒序：最新写入的（false）在前，然后是 true，最后是裸插入的那条。
        assert_eq!(
            logs.iter().map(|l| l.sse_aggregated).collect::<Vec<_>>(),
            vec![false, true, false],
            "标记要原样读回，且没写这一列的记录退化成 false 而不是读取失败"
        );
    }

    /// 请求明细的筛选与分页：按账号只出该账号的记录，页码不重叠，且**锚点之后新写入的记录
    /// 不得挤动已在翻的页**——这正是页码翻页要带 `until_id` 的理由。
    #[sqlx::test]
    async fn usage_logs_filter_by_credential_and_paginate(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let log = async |cred: i64, cost: f64| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    cost_usd: Some(cost),
                    ..Default::default()
                })
                .await
                .unwrap()
        };
        // a 四条、b 一条，交替写入，确保筛选不是靠「恰好连续」蒙对的。
        for (cred, cost) in [(a, 1.0), (a, 2.0), (b, 100.0), (a, 4.0), (a, 8.0)] {
            log(cred, cost).await;
        }

        let all = store
            .query_usage_logs(UsageLogQuery { limit: 10, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(all.len(), 5, "不筛时是全部");

        // 统计与记录同一套条件：a 的四条、花费合计 15，最大 id 即锚点。
        let only_a = UsageLogQuery { cred_id: Some(a), ..Default::default() };
        let stats = store.usage_log_stats(only_a.clone()).await.unwrap();
        assert_eq!(stats.total, 4, "b 的那条不该计入");
        assert_eq!(stats.cost_usd, 15.0);
        let anchor = stats.max_id.expect("有记录就有锚点");

        let page = async |n: i64| {
            store
                .query_usage_logs(UsageLogQuery {
                    cred_id: Some(a),
                    until_id: Some(anchor),
                    offset: n * 3,
                    limit: 3,
                    request_id: None,
                    model: None,
                    since: None,
                    session_key: None,
                    session_id: None,
                    owner_id: None,
                })
                .await
                .unwrap()
        };
        let first = page(0).await;
        assert_eq!(first.len(), 3);
        assert!(first.iter().all(|l| l.cred_id == Some(a)), "b 的那条不该出现");
        assert!(first.windows(2).all(|w| w[0].id > w[1].id), "按 id 倒序");

        let second = page(1).await;
        assert_eq!(second.len(), 1, "a 共 4 条，第二页只剩 1 条");
        assert!(second[0].id < first[2].id, "第二页不得与第一页重叠");
        assert!(page(2).await.is_empty(), "翻到底为空");

        // 翻页途中来了新请求：锚点之下的两页一字不变，锚点之上的统计才会长。
        let ids = |logs: &[UsageLog]| logs.iter().map(|l| l.id).collect::<Vec<_>>();
        log(a, 16.0).await;
        assert_eq!(ids(&page(0).await), ids(&first), "新记录不得把第一页往后挤");
        assert_eq!(ids(&page(1).await), ids(&second));
        let pinned =
            store.usage_log_stats(UsageLogQuery { until_id: Some(anchor), ..only_a.clone() }).await;
        assert_eq!(pinned.unwrap().total, 4, "钉在锚点上的统计不动");
        assert_eq!(store.usage_log_stats(only_a).await.unwrap().total, 5, "不带锚点才看得到新记录");
    }

    /// 请求 id 落库、可精确查。
    ///
    /// SQLite 版还用 EXPLAIN QUERY PLAN 钉住了「按号翻页走 (cred_id, id)、按请求 id 走
    /// request_id 索引、按模型走 (model, ts) 覆盖索引、按号统计走覆盖索引」；PG 的计划随统计
    /// 信息变，空表上一律顺序扫，那几条断言不移植（索引本身在 schema 里都建了）。
    #[sqlx::test]
    async fn usage_logs_are_searchable_by_request_id_using_indexes(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let log = async |cred: i64, rid: &str, up: Option<&str>| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    request_id: Some(rid.into()),
                    upstream_request_id: up.map(Into::into),
                    ..Default::default()
                })
                .await
                .unwrap()
        };
        log(a, "lb-1", Some("req_up_1")).await;
        log(a, "lb-2", None).await;
        log(b, "lb-3", Some("req_up_3")).await;

        let by = |rid: &str| UsageLogQuery {
            request_id: Some(rid.into()),
            limit: 10,
            ..Default::default()
        };
        let hit = store.query_usage_logs(by("lb-3")).await.unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].cred_id, Some(b));
        assert_eq!(hit[0].upstream_request_id.as_deref(), Some("req_up_3"));
        assert_eq!(
            store.usage_log_stats(by("lb-3")).await.unwrap().total,
            1,
            "统计与取页同一套条件"
        );
        assert!(store.query_usage_logs(by("nope")).await.unwrap().is_empty());
        // 空白视同不筛。
        assert_eq!(store.query_usage_logs(by("   ")).await.unwrap().len(), 3);
        // 与按号筛叠加。
        let both = UsageLogQuery {
            cred_id: Some(a),
            request_id: Some("lb-3".into()),
            limit: 10,
            ..Default::default()
        };
        assert!(store.query_usage_logs(both).await.unwrap().is_empty(), "lb-3 是 b 的");
        // 按模型 + 起点（拆分表点进来）带不带锚点都能跑通。
        for until_id in [None, Some(100)] {
            let q = UsageLogQuery {
                model: Some("claude-opus-5".into()),
                since: Some(0),
                until_id,
                limit: 10,
                ..Default::default()
            };
            assert_eq!(store.usage_log_stats(q.clone()).await.unwrap().total, 0);
            assert!(store.query_usage_logs(q).await.unwrap().is_empty());
        }
    }

    /// 流水按**模拟会话键**筛：键落进流水、与按号筛可叠、空白视同不筛。（SQLite 版另用
    /// EXPLAIN QUERY PLAN 钉住走部分索引，PG 版不移植那一条。）
    #[sqlx::test]
    async fn usage_logs_filter_by_session_key_using_a_partial_index(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let log = async |cred: i64, key: Option<&str>| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    forensics: Forensics { session_key: key.map(Into::into), ..Default::default() },
                    ..Default::default()
                })
                .await
                .unwrap()
        };
        let one = "lb:v2:sid:7fe47444-c834-44e0-b568-d61e07daa35e";
        let two = "lb:v2:pfx:3f9a1c7e5b2d4680a1b2c3d4e5f60718";
        log(a, Some(one)).await;
        log(a, Some(one)).await;
        log(a, Some(two)).await;
        log(b, Some(one)).await;
        log(a, None).await; // 带设备身份的那类：这一列为空

        let by = |key: &str| UsageLogQuery {
            session_key: Some(key.into()),
            limit: 10,
            ..Default::default()
        };
        let hit = store.query_usage_logs(by(one)).await.unwrap();
        assert_eq!(hit.len(), 3, "两个号上的同键请求都算");
        assert!(
            hit.iter().all(|l| l.forensics.session_key.as_deref() == Some(one)),
            "键随流水落库"
        );
        assert_eq!(store.usage_log_stats(by(one)).await.unwrap().total, 3, "统计与取页同一套条件");
        assert_eq!(store.query_usage_logs(by(two)).await.unwrap().len(), 1);
        assert!(store.query_usage_logs(by("lb:v2:pfx:nope")).await.unwrap().is_empty());
        assert_eq!(store.query_usage_logs(by("  ")).await.unwrap().len(), 5, "空白视同不筛");
        // 与按号筛叠加——会话行点进来带的正是这两项。
        let scoped = UsageLogQuery {
            cred_id: Some(a),
            session_key: Some(one.into()),
            limit: 10,
            ..Default::default()
        };
        assert_eq!(store.query_usage_logs(scoped.clone()).await.unwrap().len(), 2, "b 上那条不算");
        assert_eq!(store.usage_log_stats(scoped).await.unwrap().total, 2);
    }

    /// 按**会话 id** 查：出站与来访两侧任一命中。（SQLite 版另用 EXPLAIN QUERY PLAN 钉住
    /// MULTI-INDEX OR，PG 版不移植那一条。）
    #[sqlx::test]
    async fn usage_logs_look_up_a_session_id_on_either_side(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];
        let log = async |out: Option<&str>, inn: Option<&str>| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(a),
                    forensics: Forensics {
                        session_id: out.map(Into::into),
                        session_id_in: inn.map(Into::into),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .await
                .unwrap()
        };
        let client = "11111111-2222-4333-8444-555555555555";
        let upstream = "7fe47444-c834-44e0-b568-d61e07daa35e";
        log(Some(upstream), Some(client)).await; // 走模拟：两侧不同
        log(Some(upstream), Some(client)).await;
        log(Some(client), Some(client)).await; // 没改身份：两侧同一个
        log(None, Some(client)).await; // 本地拒绝：只有来访那侧
        log(Some("99999999-9999-4999-8999-999999999999"), None).await; // luban 自己发的

        let by = |sid: &str| UsageLogQuery {
            session_id: Some(sid.into()),
            limit: 10,
            ..Default::default()
        };
        assert_eq!(
            store.query_usage_logs(by(client)).await.unwrap().len(),
            4,
            "下游拿自己那个 uuid 来查：被改过身份的两条、没改的一条、本地拒绝的一条"
        );
        assert_eq!(
            store.query_usage_logs(by(upstream)).await.unwrap().len(),
            2,
            "从上游侧回查：只有出站是它的那两条"
        );
        assert_eq!(
            store.usage_log_stats(by(client)).await.unwrap().total,
            4,
            "统计与取页同一套条件"
        );
        assert!(
            store
                .query_usage_logs(by("00000000-0000-4000-8000-000000000000"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.query_usage_logs(by("  ")).await.unwrap().len(), 5, "空白视同不筛");
    }

    /// 「近 N 天出现过的模型」：空 / NULL 模型名不算，只被连通性测试打过的不算，窗口外的不算；
    /// 最新几条是 probe 的，退回它之前那条客户端流水的时刻。按时刻倒序。
    #[sqlx::test]
    async fn recent_models_skips_probe_empty_and_stale(pool: sqlx::PgPool) {
        let store = PgStore::for_test(pool).await;
        let now = chrono::Utc::now().timestamp();
        let log = async |model: Option<&str>, device: Option<&str>, ts: i64| {
            store
                .insert_usage_log_at(
                    &UsageRecord {
                        model: model.map(Into::into),
                        device_id: device.map(Into::into),
                        ..Default::default()
                    },
                    Some(ts),
                )
                .await
                .unwrap();
        };
        log(Some("opus"), Some("d1"), now - 300).await;
        // 最新那几条是连通性测试打的：退回它之前那条客户端流水的时刻。
        log(Some("opus"), Some("probe"), now - 10).await;
        log(Some("sonnet"), None, now - 100).await;
        log(Some("probe-only"), Some("probe"), now - 50).await;
        log(Some("stale"), Some("d1"), now - 8 * 86400).await;
        log(Some(""), Some("d1"), now - 20).await;
        log(None, Some("d1"), now - 20).await;

        assert_eq!(
            store.recent_models(7).await.unwrap(),
            vec![("sonnet".to_string(), now - 100), ("opus".to_string(), now - 300)]
        );
    }

    /// 后台的各条只读报表在一个有流水、有封号事件的库上都跑得通（SQL 在 PG 上合法、聚合列的
    /// 类型解码得了）。对应 SQLite 版 `reader_sees_committed_writes_and_rejects_writes` 里
    /// 「每个读方法跑一遍」的那一半；只读连接、WAL 那一半 PG 版没有对应物。
    #[sqlx::test]
    async fn admin_reports_run_on_a_populated_store(pool: sqlx::PgPool) {
        use super::super::super::{BanContext, BreakdownBy};
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(a),
                cred_label: "a".into(),
                status: 200,
                ttft_ms: Some(100),
                total_ms: Some(300),
                output_tokens: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        let ctx = BanContext { reason: "banned".into(), source: "manual", ..Default::default() };
        assert!(store.record_ban(a, &ctx).await.unwrap());
        let since = 0;
        store.recent_rpm().await.unwrap();
        store.recent_rpm_of(a).await.unwrap();
        store.total_rpm().await.unwrap();
        store.last_used().await.unwrap();
        store.cost_by_cred().await.unwrap();
        store.cache_report(since, 3600, 0).await.unwrap();
        store.ttft_report(since, 3600, 0).await.unwrap();
        store.usage_breakdown(since, BreakdownBy::Model, 12).await.unwrap();
        store.usage_breakdown(since, BreakdownBy::Account, 12).await.unwrap();
        store.credential_stats(a, since, 3600, 0, 20).await.unwrap();
        store.local_rejections(since).await.unwrap();
        store.recent_models(7).await.unwrap();
        let q = UsageLogQuery { limit: 10, ..Default::default() };
        assert_eq!(store.usage_log_stats(q.clone()).await.unwrap().total, 1);
        assert_eq!(store.query_usage_logs(q).await.unwrap().len(), 1);
        let ev = store.list_ban_events(None, 10).await.unwrap().remove(0);
        assert_eq!(store.ban_counts().await.unwrap().get(&a).copied(), Some(1));
        assert_eq!(store.frozen_usage_logs(ev.id, 10, 0).await.unwrap().0, 1);
    }
}
