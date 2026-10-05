//! 封号事件与冻结流水。

use super::*;

impl CredentialStore {
    /// 自动停用并**落一条封号事件**（[`BanContext`]），同时把该号最近
    /// [`FREEZE_WINDOW_SECS`] 的流水冻结进 `usage_logs_frozen`。
    ///
    /// 事件与冻结流水都是取证材料：解封不清、删号不删、裁剪不碰（对比 `credentials.ban_reason`
    /// 会在重新启用时被清空、`usage_logs` 会随删号级联删除并只留保留期内的）。
    ///
    /// 事件里除了上游给的那几句，还**当场快照**一组账号侧读数（等级、组织类型、代理、账龄、
    /// 终身请求数与费用、封前 7 天的请求数/设备数/模型/客户端）：这些在事后从别处凑不齐——
    /// 账号可能已被删、流水可能已被裁，而它们正是拿被封的号和活着的号对照时最先要看的列。
    ///
    /// 凭证不存在时返回 `false`（此时也不落事件——没有主体）。
    pub fn record_ban(&self, id: i64, ctx: &BanContext) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let ts: i64 = tx.query_row("SELECT unixepoch()", [], |r| r.get(0))?;
        tx.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
        tx.execute("DELETE FROM session_bindings WHERE cred_id = ?1", [id])?;
        let updated = tx.execute(
            "UPDATE credentials SET disabled = 1, ban_reason = ?2, resume_at = NULL, \
                    updated_at = unixepoch() \
             WHERE id = ?1",
            params![id, ctx.reason],
        )? > 0;
        if !updated {
            tx.commit()?;
            return Ok(false);
        }
        // 账号侧快照。
        let (label, tier, org_type, proxy, created_at): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
        ) = tx.query_row(
            "SELECT label, tier, org_type, proxy, created_at FROM credentials WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let (lifetime_cost, last_used_at): (f64, Option<i64>) = tx
            .query_row(
                "SELECT cost_total_usd, last_used_at FROM credential_stats WHERE cred_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((0.0, None));
        let lifetime_requests: i64 = tx.query_row(
            "SELECT COALESCE(SUM(request_count), 0) FROM device_costs WHERE cred_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        let since = ts - FREEZE_WINDOW_SECS;
        // 设备数分两侧：device_id 是来访客户端自报的，device_id_out 是实际发给 Anthropic 的
        // （伪装开着时是派生值）。上游看到的是后者——「一个号在上游眼里有几台设备」看它。
        let (requests_7d, devices_7d, devices_out_7d): (i64, i64, i64) = tx.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT device_id), COUNT(DISTINCT device_id_out)
               FROM usage_logs WHERE cred_id = ?1 AND ts >= ?2",
            params![id, since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let distinct = |col: &str| -> Result<String> {
            let mut st = tx.prepare(&format!(
                "SELECT {col}, COUNT(*) AS n FROM usage_logs
                  WHERE cred_id = ?1 AND ts >= ?2 AND {col} IS NOT NULL
                  GROUP BY {col} ORDER BY n DESC LIMIT 50"
            ))?;
            let rows = st.query_map(params![id, since], |r| {
                Ok(serde_json::json!({ "value": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? }))
            })?;
            Ok(serde_json::Value::Array(rows.collect::<rusqlite::Result<Vec<_>>>()?).to_string())
        };
        let models_7d = distinct("model")?;
        let uas_7d = distinct("ua")?;
        let proxies_7d = distinct("proxy")?;
        let device_ids_out_7d = distinct("device_id_out")?;
        // 封前最后一条流水的限流快照：额度是不是早就满了、在不在烧 credits。
        let (last_unified, last_overage): (Option<String>, Option<i64>) = tx
            .query_row(
                "SELECT unified_status, overage_in_use FROM credential_stats WHERE cred_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None));
        tx.execute(
            "INSERT INTO ban_events
                (ts, cred_id, cred_label, source, reason, status, error_type, error_message,
                 request_id, upstream_request_id, tier, org_type, proxy, account_created_at,
                 lifetime_requests, lifetime_cost_usd, last_used_at, requests_7d, devices_7d,
                 models_7d, uas_7d, proxies_7d, last_unified_status, last_overage_in_use,
                 devices_out_7d, device_ids_out_7d)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
            params![
                ts,
                id,
                label,
                ctx.source,
                ctx.reason,
                ctx.status.map(i64::from),
                ctx.error_type,
                ctx.error_message.as_deref().map(|m| head_chars(m, ERROR_MESSAGE_MAX * 4)),
                ctx.request_id,
                ctx.upstream_request_id,
                tier,
                org_type,
                proxy.as_deref().map(redact_proxy),
                created_at,
                lifetime_requests,
                lifetime_cost,
                last_used_at,
                requests_7d,
                devices_7d,
                models_7d,
                uas_7d,
                proxies_7d,
                last_unified,
                last_overage,
                devices_out_7d,
                device_ids_out_7d,
            ],
        )?;
        let ban_id = tx.last_insert_rowid();
        let frozen = tx.execute(
            &format!(
                "INSERT INTO usage_logs_frozen (ban_event_id, src_id, {USAGE_LOG_COLS})
                 SELECT ?1, id, {USAGE_LOG_COLS} FROM usage_logs
                  WHERE cred_id = ?2 AND ts >= ?3 ORDER BY id"
            ),
            params![ban_id, id, since],
        )?;
        tx.execute(
            "UPDATE ban_events SET frozen_rows = ?2 WHERE id = ?1",
            params![ban_id, frozen as i64],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// 封号事件列表（新的在前）。`cred_id` 为 `Some` 时只看那个号（含已删的号：事件按 id 存，
    /// 不随删号消失）。
    pub fn list_ban_events(&self, cred_id: Option<i64>, limit: i64) -> Result<Vec<BanEvent>> {
        let conn = self.read_conn();
        let (where_sql, params): (&str, Vec<rusqlite::types::Value>) = match cred_id {
            Some(c) => (" WHERE cred_id = ?1", vec![c.into(), limit.into()]),
            None => ("", vec![limit.into()]),
        };
        let n = params.len();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, ts, cred_id, cred_label, source, reason, status, error_type, error_message,
                    request_id, upstream_request_id, tier, org_type, proxy, account_created_at,
                    lifetime_requests, lifetime_cost_usd, last_used_at, requests_7d, devices_7d,
                    models_7d, uas_7d, proxies_7d, last_unified_status, last_overage_in_use,
                    frozen_rows, devices_out_7d, device_ids_out_7d
               FROM ban_events{where_sql} ORDER BY id DESC LIMIT ?{n}"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
            let json_list = |i: usize| -> rusqlite::Result<Vec<serde_json::Value>> {
                let raw: Option<String> = r.get(i)?;
                Ok(raw
                    .and_then(|t| serde_json::from_str::<Vec<serde_json::Value>>(&t).ok())
                    .unwrap_or_default())
            };
            Ok(BanEvent {
                id: r.get(0)?,
                ts: r.get(1)?,
                cred_id: r.get(2)?,
                cred_label: r.get(3)?,
                source: r.get(4)?,
                reason: r.get(5)?,
                status: r.get::<_, Option<i64>>(6)?.map(|v| v as u16),
                error_type: r.get(7)?,
                error_message: r.get(8)?,
                request_id: r.get(9)?,
                upstream_request_id: r.get(10)?,
                tier: r.get(11)?,
                org_type: r.get(12)?,
                proxy: r.get(13)?,
                account_created_at: r.get(14)?,
                lifetime_requests: r.get(15)?,
                lifetime_cost_usd: r.get(16)?,
                last_used_at: r.get(17)?,
                requests_7d: r.get(18)?,
                devices_7d: r.get(19)?,
                models_7d: json_list(20)?,
                uas_7d: json_list(21)?,
                proxies_7d: json_list(22)?,
                last_unified_status: r.get(23)?,
                last_overage_in_use: r.get::<_, Option<i64>>(24)?.map(|v| v != 0),
                frozen_rows: r.get(25)?,
                devices_out_7d: r.get::<_, Option<i64>>(26)?.unwrap_or(0),
                device_ids_out_7d: json_list(27)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 每个凭证被自动封停过几次（cred_id → 次数）；没封过的号不出现。
    pub fn ban_counts(&self) -> Result<HashMap<i64, i64>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare("SELECT cred_id, COUNT(*) FROM ban_events GROUP BY cred_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// 某封号事件冻结下来的**一页**流水（按时间正序，最早的在前，读起来是一条时间线），
    /// 连同该事件冻结的总条数一起给出。
    ///
    /// 一次封号常冻下上千行、几十 MB（每行还带形态摘要 JSON），整份吐给页面既慢又白读，
    /// 所以这里只给一页。总条数与当页在同一个读事务里取：封号后的补冻结（见
    /// `insert_usage_log_at`）还会往冻结表追加行，两条语句若各看各的快照，总数与当页会差一条。
    /// `id` 是冻结表自己的主键；原流水的 id 不返回——它在原表里可能早已被裁掉。
    pub fn frozen_usage_logs(
        &self,
        ban_event_id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<UsageLog>)> {
        let conn = self.read_conn();
        let tx = conn.unchecked_transaction()?;
        let total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM usage_logs_frozen WHERE ban_event_id = ?1",
            [ban_event_id],
            |r| r.get(0),
        )?;
        let mut stmt = tx.prepare(&format!(
            "SELECT id, {USAGE_LOG_COLS} FROM usage_logs_frozen
              WHERE ban_event_id = ?1 ORDER BY ts, id LIMIT ?2 OFFSET ?3"
        ))?;
        let logs = stmt
            .query_map([ban_event_id, limit, offset], usage_log_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        // 只读事务，提交与回滚等价；显式结束，别等 drop 时静默回滚吞掉错误。
        tx.commit()?;
        Ok((total, logs))
    }
}

/// 封号时冻结的流水回看窗口：7 天。上游的额度窗口最长 7 天，判封的依据不太可能更久远；
/// 再长冻结表就会比流水表还大。
pub const FREEZE_WINDOW_SECS: i64 = 7 * 24 * 3600;

/// 封号事件之后多长时间内到达的流水也补进冻结表，见 `insert_usage_log_at`。触发那一发的
/// 响应流通常几十秒内结束，10 分钟够覆盖并发在途的所有请求。
pub const FREEZE_TAIL_SECS: i64 = 600;

/// 一次自动停用的上下文，见 [`CredentialStore::record_ban`]。
#[derive(Debug, Default, Clone)]
pub struct BanContext {
    /// 写进 `credentials.ban_reason` 的一句话（200 字符内）。
    pub reason: String,
    /// 触发来源：`forward`（转发 4xx）、`forward_401`（转发 401 换号）、`probe`（连通性
    /// 测试）、`keepalive`（保活端点 401/403）、`refresh`（刷新 token 被作废）、`proxy`
    /// （代理建不出来）、`manual`（其它，目前只有测试用）。前端
    /// `ban-events-dialog.tsx` 的 `sourceLabel` 与这份表一一对应。
    pub source: &'static str,
    /// 上游 HTTP 状态码（有的话）。
    pub status: Option<u16>,
    /// 上游 `error.type` 与**完整** `error.message`（reason 是截断过的）。
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    /// 触发那条请求的 luban 请求 id 与上游 `request-id`——拿它能在冻结流水里精确找到那一发。
    pub request_id: Option<String>,
    pub upstream_request_id: Option<String>,
}

/// 一条封号事件（读取用），见 [`CredentialStore::record_ban`]。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BanEvent {
    pub id: i64,
    pub ts: i64,
    pub cred_id: i64,
    pub cred_label: String,
    pub source: String,
    pub reason: String,
    pub status: Option<u16>,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub request_id: Option<String>,
    pub upstream_request_id: Option<String>,
    pub tier: Option<String>,
    pub org_type: Option<String>,
    /// 封号当时该号配的代理（密码已打码）。
    pub proxy: Option<String>,
    pub account_created_at: i64,
    pub lifetime_requests: i64,
    pub lifetime_cost_usd: f64,
    pub last_used_at: Option<i64>,
    /// 封前 7 天的请求数、去重设备数，以及模型 / 来访 UA / 代理的分布（`{value, count}`，按次数降序）。
    pub requests_7d: i64,
    pub devices_7d: i64,
    pub models_7d: Vec<serde_json::Value>,
    pub uas_7d: Vec<serde_json::Value>,
    pub proxies_7d: Vec<serde_json::Value>,
    /// 封前账本里最后一次限流快照的 unified_status / overage_in_use。
    pub last_unified_status: Option<String>,
    pub last_overage_in_use: Option<bool>,
    /// 冻结进 `usage_logs_frozen` 的流水条数（含封后补进去的）。
    pub frozen_rows: i64,
    /// 封前 7 天**发给 Anthropic** 的去重设备数与分布（`device_id_out`，伪装开着时是派生值）。
    /// 与 `devices_7d`（来访自报）分开：上游眼里这个号有几台设备，看的是这一对。
    pub devices_out_7d: i64,
    pub device_ids_out_7d: Vec<serde_json::Value>,
}
