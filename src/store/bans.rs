//! 封号事件与冻结流水。

/// 封号时冻结的流水回看窗口：7 天。上游的额度窗口最长 7 天，判封的依据不太可能更久远；
/// 再长冻结表就会比流水表还大。
pub const FREEZE_WINDOW_SECS: i64 = 7 * 24 * 3600;

/// 冻结流水的保留期（按所属封号事件的时刻算）：过了就删。事件本身（含当场快照的那组读数与
/// `frozen_rows` 条数）不删，删的只是逐条流水——它每行一千多字节，一个高频号封一次就是几十 MB，
/// 不设上限会一直涨。半年足够事后对照。
pub const FROZEN_LOG_RETENTION_SECS: i64 = 180 * 24 * 3600;

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

/// 停号之后、取证落地之前的一次封号（`ban_pending` 表的 `payload`，JSON），见
/// [`CredentialStore::record_ban`]。
///
/// 除了 [`BanContext`] 那几项（已经清洗、截断过），停号那一刻的账号侧读数也在这里：补做可能
/// 晚了很久，号可能已被改过甚至删掉（删号连账本一起删），事后读不到当时的值。逐条流水的那几项
/// 统计不在这里——流水不随删号删，补做时按封号时刻的窗口去算。
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingBan {
    ts: i64,
    reason: String,
    source: String,
    status: Option<u16>,
    error_type: Option<String>,
    error_message: Option<String>,
    request_id: Option<String>,
    upstream_request_id: Option<String>,
    label: String,
    tier: Option<String>,
    org_type: Option<String>,
    /// 已打码（见 [`redact_proxy`]）。
    proxy: Option<String>,
    account_created_at: i64,
    lifetime_requests: i64,
    lifetime_cost_usd: f64,
    last_used_at: Option<i64>,
    last_unified_status: Option<String>,
    last_overage_in_use: Option<i64>,
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

use std::collections::HashMap;

use anyhow::Result;
use sqlx::postgres::PgArguments;
use sqlx::{Arguments, Row};

use super::credential::SUBSCRIPTION_PAUSE_SQL;
use super::session_events::log_removed;
use super::usage::usage_log_from_row;
use super::{CredentialStore, nul_free};
use super::{ERROR_MESSAGE_MAX, USAGE_LOG_COLS, UsageLog, head_chars, redact_proxy};

impl CredentialStore {
    /// 自动停用并**落一条封号事件**（[`BanContext`]），同时把该号最近
    /// [`FREEZE_WINDOW_SECS`] 的流水冻结进 `usage_logs_frozen`。
    ///
    /// 事件与冻结流水都是取证材料：解封不清、删号不删、裁剪不碰（对比 `credentials.ban_reason`
    /// 会在重新启用时被清空、`usage_logs` 只留保留期内的）。
    ///
    /// 事件里除了上游给的那几句，还**当场快照**一组账号侧读数（等级、组织类型、代理、账龄、
    /// 终身请求数与费用、封前 7 天的请求数/设备数/模型/客户端）：这些在事后从别处凑不齐——
    /// 账号可能已被删、流水可能已被裁，而它们正是拿被封的号和活着的号对照时最先要看的列。
    ///
    /// 凭证不存在时返回 `false`（此时也不落事件——没有主体）。
    ///
    /// 号已经封着时（`disabled` 且没有恢复时刻、带着非订阅暂停的原因）只清绑定，不再落事件与
    /// 冻结，返回 `true`。
    ///
    /// 分两步：停号、清绑定、记一条取证待办（`ban_pending` 表，含停号那一刻的账号侧读数）在
    /// 串行化写事务里做完就提交；事件与冻结另开普通事务补（[`Self::finish_ban_forensics`]），
    /// 不占全局写锁。补的那笔一上来拿这个号的冻结锁（advisory 排它锁，键见
    /// [`super::freeze_lock_key`]）：它与写流水时的共享锁互斥（见 [`Self::insert_usage_log_at`]），
    /// 于是在途的流水要么在冻结之前提交、被这里冻进去，要么等这里提交后才写、看得到这条事件并
    /// 自己补进冻结表，一条都漏不掉——号被删掉之后也一样。
    ///
    /// 停号与取证互不连累：取证这一步出错（或请求被取消、进程退出）只记日志，停号已经生效，
    /// 待办留在表里，下一次对这个号的 `record_ban` 或后台的
    /// [`Self::finish_pending_ban_forensics`] 接着补。每次封号各记一条待办，号被重新启用后
    /// 再被封也不会盖掉上一次没补完的。
    pub async fn record_ban(&self, id: i64, ctx: &BanContext) -> Result<bool> {
        // 原因与报错可能回显客户端塞进来的串，见 [`nul_free`]。
        let reason = nul_free(&ctx.reason);
        let mut tx = self.begin_write().await?;
        // (封号时刻, label, tier, org_type, proxy, created_at, 已经封着)
        type Account = (i64, String, Option<String>, Option<String>, Option<String>, i64, bool);
        let account: Option<Account> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT unixepoch(), label, tier, org_type, proxy, created_at,
                    COALESCE(disabled = 1 AND resume_at IS NULL AND ban_reason IS NOT NULL
                             AND NOT ({SUBSCRIPTION_PAUSE_SQL}), FALSE)
               FROM credentials WHERE id = $1 FOR UPDATE"
        )))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM device_bindings WHERE cred_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let mut args = PgArguments::default();
        args.add(id).map_err(anyhow::Error::from_boxed)?;
        log_removed(&mut tx, "unbound", Some("account_banned"), "cred_id = $1", args).await?;
        sqlx::query("DELETE FROM session_bindings WHERE cred_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let Some((ts, label, tier, org_type, proxy, created_at, already_banned)) = account else {
            tx.commit().await?;
            return Ok(false);
        };
        // 已经封着（同一个号上并发在飞的几发先后都吃到 401/403，或保活、连通性测试又撞上一次）：
        // 不再记事件、不再冻结。否则每一发都把这个号 7 天的流水整段再复制一份，冻结表永不清理。
        // 第一次封号之后才写的流水会自己补进那次的冻结（见 [`Self::insert_usage_log_at`]）。
        if already_banned {
            tracing::debug!(
                cred_id = id,
                "credential already banned, not recording another ban event"
            );
        } else {
            // 账本侧快照：终身费用、最近使用，以及封前最后一次限流快照（额度是不是早就满了、
            // 在不在烧 credits）。都是按号的点查，放在这里取：删号会连账本一起删。
            let (lifetime_cost, last_used_at, last_unified, last_overage): (
                f64,
                Option<i64>,
                Option<String>,
                Option<i64>,
            ) = sqlx::query_as(
                "SELECT cost_total_usd, last_used_at, unified_status, overage_in_use
                   FROM credential_stats WHERE cred_id = $1",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or((0.0, None, None, None));
            let lifetime_requests: i64 = sqlx::query_scalar(
                "SELECT COALESCE(SUM(request_count), 0)::BIGINT FROM device_costs WHERE cred_id = $1",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let text = |s: &Option<String>| s.as_deref().map(|s| nul_free(s).into_owned());
            let pending = PendingBan {
                ts,
                reason: reason.into_owned(),
                source: ctx.source.to_string(),
                status: ctx.status,
                error_type: text(&ctx.error_type),
                error_message: text(&ctx.error_message)
                    .map(|m| head_chars(&m, ERROR_MESSAGE_MAX * 4).to_string()),
                request_id: text(&ctx.request_id),
                upstream_request_id: text(&ctx.upstream_request_id),
                label,
                tier,
                org_type,
                proxy: proxy.as_deref().map(redact_proxy),
                account_created_at: created_at,
                lifetime_requests,
                lifetime_cost_usd: lifetime_cost,
                last_used_at,
                last_unified_status: last_unified,
                last_overage_in_use: last_overage,
            };
            sqlx::query(
                "UPDATE credentials SET disabled = 1, ban_reason = $2, resume_at = NULL,
                        updated_at = unixepoch()
                  WHERE id = $1",
            )
            .bind(id)
            .bind(&pending.reason)
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO ban_pending (cred_id, ban_ts, payload) VALUES ($1, $2, $3)")
                .bind(id)
                .bind(ts)
                .bind(serde_json::to_string(&pending)?)
                .execute(&mut *tx)
                .await?;
        }
        // 停号与清绑定到此为止，先提交、放掉全局写锁：下面的快照统计与冻结要聚合、复制这个号
        // 7 天的流水，量大时要几百毫秒，压在全局锁里全站选号都得等着。
        tx.commit().await?;
        // 这个号的待办（这一次的，加上之前没补完的）逐条补。停号已经生效，取证出错不往上报。
        let pending: Vec<i64> = match sqlx::query_scalar(
            "SELECT id FROM ban_pending WHERE cred_id = $1 ORDER BY id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(cred_id = id, error = %e, "failed to list pending ban forensics; left for the background retry");
                Vec::new()
            }
        };
        for pid in pending {
            if let Err(e) = self.finish_ban_forensics(pid).await {
                tracing::warn!(cred_id = id, pending_id = pid, error = %format!("{e:#}"), "failed to record ban forensics; left for the background retry");
            }
        }
        Ok(true)
    }

    /// 把 `ban_pending` 里的待办逐条补完，回补成了几条。后台定期调，见 [`Self::record_ban`]。
    /// 单条出错只记日志、接着补下一条。
    pub async fn finish_pending_ban_forensics(&self) -> Result<usize> {
        let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM ban_pending ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        let mut done = 0;
        for pid in ids {
            match self.finish_ban_forensics(pid).await {
                Ok(true) => done += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(pending_id = pid, error = %format!("{e:#}"), "failed to record ban forensics")
                }
            }
        }
        Ok(done)
    }

    /// 补一条取证待办：按它记的那次封号落事件、算封前 7 天的流水统计、冻结流水，落完删掉待办。
    /// 待办已经不在（别人刚补完）时什么都不做，回 `false`。
    async fn finish_ban_forensics(&self, pending_id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let Some(cred_id): Option<i64> =
            sqlx::query_scalar("SELECT cred_id FROM ban_pending WHERE id = $1")
                .bind(pending_id)
                .fetch_optional(&mut *tx)
                .await?
        else {
            tx.commit().await?;
            return Ok(false);
        };
        // 先拿这个号的冻结锁（排它），与写流水时的共享锁互斥，冻结的完整性见
        // [`Self::record_ban`]。advisory 锁不依赖账号行：号已被删掉也照样和删号时还在途的流水
        // 分出先后。
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(super::freeze_lock_key(cred_id))
            .execute(&mut *tx)
            .await?;
        // 再锁待办并重读：并发补同一条的两路只有先拿到锁的那路会落事件。
        let Some(payload): Option<String> =
            sqlx::query_scalar("SELECT payload FROM ban_pending WHERE id = $1 FOR UPDATE")
                .bind(pending_id)
                .fetch_optional(&mut *tx)
                .await?
        else {
            tx.commit().await?;
            return Ok(false);
        };
        let p: PendingBan = match serde_json::from_str(&payload) {
            Ok(p) => p,
            Err(e) => {
                // 补不了了，删掉，免得每次都卡在这条上。
                tracing::warn!(pending_id, cred_id, error = %e, "unreadable ban_pending payload, dropping it");
                sqlx::query("DELETE FROM ban_pending WHERE id = $1")
                    .bind(pending_id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(false);
            }
        };
        // 窗口按当初封号的时刻算：补做可能晚了很久（号期间被重新启用过），封号之后的流水不该
        // 算进这次。上界放宽 FREEZE_TAIL_SECS，与写流水时补进冻结表的口径一致。
        let since = p.ts - FREEZE_WINDOW_SECS;
        let until = p.ts + FREEZE_TAIL_SECS;
        // 设备数分两侧：device_id 是来访客户端自报的，device_id_out 是实际发给 Anthropic 的
        // （伪装开着时是派生值）。上游看到的是后者——「一个号在上游眼里有几台设备」看它。
        let (requests_7d, devices_7d, devices_out_7d): (i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COUNT(DISTINCT device_id), COUNT(DISTINCT device_id_out)
               FROM usage_logs WHERE cred_id = $1 AND ts >= $2 AND ts <= $3",
        )
        .bind(cred_id)
        .bind(since)
        .bind(until)
        .fetch_one(&mut *tx)
        .await?;
        let mut lists = Vec::with_capacity(4);
        for col in ["model", "ua", "proxy", "device_id_out"] {
            let rows: Vec<(String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {col}, COUNT(*) AS n FROM usage_logs
                  WHERE cred_id = $1 AND ts >= $2 AND ts <= $3 AND {col} IS NOT NULL
                  GROUP BY {col} ORDER BY n DESC LIMIT 50"
            )))
            .bind(cred_id)
            .bind(since)
            .bind(until)
            .fetch_all(&mut *tx)
            .await?;
            let list = rows
                .into_iter()
                .map(|(value, count)| serde_json::json!({ "value": value, "count": count }))
                .collect();
            lists.push(serde_json::Value::Array(list).to_string());
        }
        let [models_7d, uas_7d, proxies_7d, device_ids_out_7d]: [String; 4] =
            lists.try_into().expect("four columns");
        let ban_id: i64 = sqlx::query_scalar(
            "INSERT INTO ban_events
                (ts, cred_id, cred_label, source, reason, status, error_type, error_message,
                 request_id, upstream_request_id, tier, org_type, proxy, account_created_at,
                 lifetime_requests, lifetime_cost_usd, last_used_at, requests_7d, devices_7d,
                 models_7d, uas_7d, proxies_7d, last_unified_status, last_overage_in_use,
                 devices_out_7d, device_ids_out_7d)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                     $17, $18, $19, $20, $21, $22, $23, $24, $25, $26)
             RETURNING id",
        )
        .bind(p.ts)
        .bind(cred_id)
        .bind(&p.label)
        .bind(&p.source)
        .bind(&p.reason)
        .bind(p.status.map(i64::from))
        .bind(&p.error_type)
        .bind(&p.error_message)
        .bind(&p.request_id)
        .bind(&p.upstream_request_id)
        .bind(&p.tier)
        .bind(&p.org_type)
        .bind(&p.proxy)
        .bind(p.account_created_at)
        .bind(p.lifetime_requests)
        .bind(p.lifetime_cost_usd)
        .bind(p.last_used_at)
        .bind(requests_7d)
        .bind(devices_7d)
        .bind(models_7d)
        .bind(uas_7d)
        .bind(proxies_7d)
        .bind(&p.last_unified_status)
        .bind(p.last_overage_in_use)
        .bind(devices_out_7d)
        .bind(device_ids_out_7d)
        .fetch_one(&mut *tx)
        .await?;
        let frozen = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO usage_logs_frozen (ban_event_id, src_id, {USAGE_LOG_COLS})
             SELECT $1, id, {USAGE_LOG_COLS} FROM usage_logs
              WHERE cred_id = $2 AND ts >= $3 AND ts <= $4 ORDER BY id"
        )))
        .bind(ban_id)
        .bind(cred_id)
        .bind(since)
        .bind(until)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query("UPDATE ban_events SET frozen_rows = $2 WHERE id = $1")
            .bind(ban_id)
            .bind(frozen as i64)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM ban_pending WHERE id = $1")
            .bind(pending_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// 删掉过了 [`FROZEN_LOG_RETENTION_SECS`] 的冻结流水，回删除条数。分批短事务，同
    /// [`Self::prune_usage_logs`]。
    pub async fn prune_frozen_usage_logs(&self) -> Result<usize> {
        const BATCH: i64 = 500;
        let mut total = 0;
        loop {
            let n = sqlx::query(
                "DELETE FROM usage_logs_frozen WHERE id IN (
                     SELECT f.id FROM usage_logs_frozen f
                       JOIN ban_events b ON b.id = f.ban_event_id
                      WHERE b.ts < unixepoch() - $1 LIMIT $2)",
            )
            .bind(FROZEN_LOG_RETENTION_SECS)
            .bind(BATCH)
            .execute(&self.pool)
            .await?
            .rows_affected() as usize;
            total += n;
            if (n as i64) < BATCH {
                return Ok(total);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// 封号事件列表（新的在前）。`cred_id` 为 `Some` 时只看那个号（含已删的号：事件按 id 存，
    /// 不随删号消失）。
    pub async fn list_ban_events(&self, cred_id: Option<i64>, limit: i64) -> Result<Vec<BanEvent>> {
        const COLS: &str = "id, ts, cred_id, cred_label, source, reason, status, error_type,
            error_message, request_id, upstream_request_id, tier, org_type, proxy,
            account_created_at, lifetime_requests, lifetime_cost_usd, last_used_at, requests_7d,
            devices_7d, models_7d, uas_7d, proxies_7d, last_unified_status, last_overage_in_use,
            frozen_rows, devices_out_7d, device_ids_out_7d";
        let rows = match cred_id {
            Some(c) => {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "SELECT {COLS} FROM ban_events WHERE cred_id = $1 ORDER BY id DESC LIMIT $2"
                )))
                .bind(c)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "SELECT {COLS} FROM ban_events ORDER BY id DESC LIMIT $1"
                )))
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.iter()
            .map(|r| {
                let json_list = |i: usize| -> Result<Vec<serde_json::Value>> {
                    let raw: Option<String> = r.try_get(i)?;
                    Ok(raw
                        .and_then(|t| serde_json::from_str::<Vec<serde_json::Value>>(&t).ok())
                        .unwrap_or_default())
                };
                Ok(BanEvent {
                    id: r.try_get(0)?,
                    ts: r.try_get(1)?,
                    cred_id: r.try_get(2)?,
                    cred_label: r.try_get(3)?,
                    source: r.try_get(4)?,
                    reason: r.try_get(5)?,
                    status: r.try_get::<Option<i64>, _>(6)?.map(|v| v as u16),
                    error_type: r.try_get(7)?,
                    error_message: r.try_get(8)?,
                    request_id: r.try_get(9)?,
                    upstream_request_id: r.try_get(10)?,
                    tier: r.try_get(11)?,
                    org_type: r.try_get(12)?,
                    proxy: r.try_get(13)?,
                    account_created_at: r.try_get(14)?,
                    lifetime_requests: r.try_get(15)?,
                    lifetime_cost_usd: r.try_get(16)?,
                    last_used_at: r.try_get(17)?,
                    requests_7d: r.try_get(18)?,
                    devices_7d: r.try_get(19)?,
                    models_7d: json_list(20)?,
                    uas_7d: json_list(21)?,
                    proxies_7d: json_list(22)?,
                    last_unified_status: r.try_get(23)?,
                    last_overage_in_use: r.try_get::<Option<i64>, _>(24)?.map(|v| v != 0),
                    frozen_rows: r.try_get(25)?,
                    devices_out_7d: r.try_get(26)?,
                    device_ids_out_7d: json_list(27)?,
                })
            })
            .collect()
    }

    /// 每个凭证被自动封停过几次（cred_id → 次数）；没封过的号不出现。
    pub async fn ban_counts(&self) -> Result<HashMap<i64, i64>> {
        let rows: Vec<(i64, i64)> =
            sqlx::query_as("SELECT cred_id, COUNT(*) FROM ban_events GROUP BY cred_id")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    /// 某封号事件冻结下来的**一页**流水（按时间正序，最早的在前，读起来是一条时间线），
    /// 连同该事件冻结的总条数一起给出。
    ///
    /// 一次封号常冻下上千行、几十 MB，整份吐给页面既慢又白读，所以这里只给一页。总条数与当页
    /// 在同一个 REPEATABLE READ 只读事务里取：封号后的补冻结（见 `insert_usage_log_at`）还会往
    /// 冻结表追加行，PG 默认的 READ COMMITTED 每条语句各看各的快照，总数与当页会差一条。
    /// `id` 是冻结表自己的主键；原流水的 id 不返回——它在原表里可能早已被裁掉。
    pub async fn frozen_usage_logs(
        &self,
        ban_event_id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<UsageLog>)> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM usage_logs_frozen WHERE ban_event_id = $1")
                .bind(ban_event_id)
                .fetch_one(&mut *tx)
                .await?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, {USAGE_LOG_COLS} FROM usage_logs_frozen
              WHERE ban_event_id = $1 ORDER BY ts, id LIMIT $2 OFFSET $3"
        )))
        .bind(ban_event_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *tx)
        .await?;
        let mut logs = rows.iter().map(usage_log_from_row).collect::<Result<Vec<_>>>()?;
        // 只读事务，提交与回滚等价；显式结束，别等 drop 时静默回滚吞掉错误。
        tx.commit().await?;
        self.fill_tool_names(&mut logs).await?;
        Ok((total, logs))
    }
}

#[cfg(test)]
mod tests {
    use super::super::usage::tests::db_now;
    use super::super::{FREEZE_TAIL_SECS, Forensics, Select, UsageRecord};
    use super::*;

    /// 封号事件与冻结流水是取证材料：落地时快照账号侧读数、冻结封前流水；封后 10 分钟内
    /// 到达的流水（触发那一发）也要补进去；删号、解封、裁剪都不能碰它们。
    #[sqlx::test]
    async fn record_ban_freezes_history_and_survives_deletion(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap();
        store.set_proxy(a.id, Some("socks5h://u:secret@exit1:1080")).await.unwrap();
        let now = db_now(&store).await;
        let mut rec = UsageRecord {
            cred_id: Some(a.id),
            cred_label: "a".into(),
            device_id: Some("dev1".into()),
            model: Some("claude-opus-5".into()),
            ua: Some("claude-cli/2.1.259".into()),
            status: 200,
            cost_usd: Some(0.5),
            forensics: Forensics {
                proxy: Some("socks5h://u:***@exit1:1080".into()),
                shape: Some("{\"keys\":[\"model\"]}".into()),
                device_id_out: Some("d230ce6e-out".into()),
                simulated: true,
                sim_reason: Some("not_cc_shaped".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        // 封前 3 天的两条 + 10 天前的一条（窗口外，不该被冻结）。
        store.insert_usage_log_at(&rec, Some(now - 3 * 86400)).await.unwrap();
        rec.device_id = Some("dev2".into());
        store.insert_usage_log_at(&rec, Some(now - 3 * 86400 + 5)).await.unwrap();
        store.insert_usage_log_at(&rec, Some(now - 10 * 86400)).await.unwrap();
        // 模拟原因随流水落库、读回。
        let back = store.list_usage_logs(1).await.unwrap().remove(0);
        assert!(back.forensics.simulated);
        assert_eq!(back.forensics.sim_reason.as_deref(), Some("not_cc_shaped"));

        let ctx = BanContext {
            reason: "[403] permission_error: account disabled".into(),
            source: "forward",
            status: Some(403),
            error_type: Some("permission_error".into()),
            error_message: Some("Your account has been disabled for policy violations".into()),
            request_id: Some("req_x".into()),
            upstream_request_id: Some("up_x".into()),
        };
        assert!(store.record_ban(a.id, &ctx).await.unwrap());
        assert!(!store.record_ban(9999, &ctx).await.unwrap(), "不存在的号不落事件");
        // 同一次封号又撞上一发（并发在飞的另一条、保活）：不再落事件、不再冻结一份。
        assert!(store.record_ban(a.id, &ctx).await.unwrap());

        let events = store.list_ban_events(None, 10).await.unwrap();
        assert_eq!(events.len(), 1, "已封着的号不重复落事件");
        let ev = &events[0];
        assert_eq!(ev.cred_id, a.id);
        assert_eq!(ev.source, "forward");
        assert_eq!(ev.status, Some(403));
        assert_eq!(ev.error_type.as_deref(), Some("permission_error"));
        assert_eq!(ev.request_id.as_deref(), Some("req_x"));
        assert_eq!(ev.proxy.as_deref(), Some("socks5h://u:***@exit1:1080"), "代理密码要打码");
        assert_eq!(ev.lifetime_requests, 3);
        assert!((ev.lifetime_cost_usd - 1.5).abs() < 1e-9);
        assert_eq!(ev.requests_7d, 2);
        assert_eq!(ev.devices_7d, 2);
        assert_eq!(ev.devices_out_7d, 1, "两台来访设备伪装成同一个出站 device_id");
        assert_eq!(ev.device_ids_out_7d[0]["value"], "d230ce6e-out");
        assert_eq!(ev.device_ids_out_7d[0]["count"], 2);
        assert_eq!(ev.models_7d[0]["value"], "claude-opus-5");
        assert_eq!(ev.models_7d[0]["count"], 2);
        assert_eq!(ev.frozen_rows, 2, "只冻结 7 天窗口内的流水");
        let frozen = store.frozen_usage_logs(ev.id, 100, 0).await.unwrap().1;
        assert_eq!(frozen.len(), 2);
        assert_eq!(frozen[0].forensics.shape.as_deref(), Some("{\"keys\":[\"model\"]}"));
        assert_eq!(frozen[0].forensics.proxy.as_deref(), Some("socks5h://u:***@exit1:1080"));
        assert_eq!(
            frozen[0].forensics.device_id_out.as_deref(),
            Some("d230ce6e-out"),
            "出站 device_id 要随流水一起落库并进冻结表"
        );
        assert_eq!(
            store.list_usage_logs(10).await.unwrap()[0].forensics.device_id_out.as_deref(),
            Some("d230ce6e-out")
        );

        // 封后到达的（触发那一发）也进冻结表。
        rec.status = 403;
        rec.forensics.error_type = Some("permission_error".into());
        store.insert_usage_log(&rec).await.unwrap();
        let frozen = store.frozen_usage_logs(ev.id, 100, 0).await.unwrap().1;
        assert_eq!(frozen.len(), 3);
        assert_eq!(frozen[2].status, 403);
        assert_eq!(frozen[2].forensics.error_type.as_deref(), Some("permission_error"));

        // 翻页：总条数报的是整份，页只给要的那几条，越界页空着（页面靠 total 算页数）。
        let (total, page) = store.frozen_usage_logs(ev.id, 2, 0).await.unwrap();
        assert_eq!((total, page.len()), (3, 2));
        assert_eq!(page[0].ts, frozen[0].ts);
        let (total, page) = store.frozen_usage_logs(ev.id, 2, 2).await.unwrap();
        assert_eq!((total, page.len()), (3, 1));
        assert_eq!(page[0].status, 403, "第二页接着同一条时间线，不从头再来");
        assert!(store.frozen_usage_logs(ev.id, 2, 10).await.unwrap().1.is_empty());

        // 封后太久的不算。
        store.insert_usage_log_at(&rec, Some(now + FREEZE_TAIL_SECS + 60)).await.unwrap();
        assert_eq!(store.frozen_usage_logs(ev.id, 100, 0).await.unwrap().1.len(), 3);

        // 解封不清事件；删号不删事件与冻结流水；裁剪不碰冻结表。
        store.set_disabled(a.id, false).await.unwrap();
        assert_eq!(store.ban_counts().await.unwrap()[&a.id], 1);
        assert!(store.delete(a.id).await.unwrap());
        assert_eq!(store.list_ban_events(Some(a.id), 10).await.unwrap().len(), 1);
        assert_eq!(store.frozen_usage_logs(ev.id, 100, 0).await.unwrap().1.len(), 3);
        store.prune_usage_logs().await.unwrap();
        assert_eq!(store.frozen_usage_logs(ev.id, 100, 0).await.unwrap().1.len(), 3);
    }

    /// 封号与在途流水的先后：流水事务已经拿了这个号的冻结锁（共享）、还没提交时，
    /// `record_ban` 要等它提交再冻结，冻得到这一条（PG 的事务真并发，两边各看各的快照的话
    /// 这一条会两头落空：冻结时看不到它，它写入时又看不到封号事件）。
    #[sqlx::test]
    async fn record_ban_waits_for_an_in_flight_usage_log(pool: sqlx::PgPool) {
        let store = std::sync::Arc::new(CredentialStore::for_test(pool).await);
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        // 手工模拟一条在途的流水写入：上锁、插流水，先不提交。
        let mut tx = store.pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
            .bind(super::super::freeze_lock_key(a))
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO usage_logs (cred_id, ts) VALUES ($1, unixepoch())")
            .bind(a)
            .execute(&mut *tx)
            .await
            .unwrap();
        let s = store.clone();
        let ban = tokio::spawn(async move {
            let ctx = BanContext { reason: "x".into(), source: "manual", ..Default::default() };
            s.record_ban(a, &ctx).await.unwrap()
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!ban.is_finished(), "封号要等在途的流水提交");
        tx.commit().await.unwrap();
        assert!(ban.await.unwrap());
        let ev = store.list_ban_events(Some(a), 1).await.unwrap().remove(0);
        assert_eq!(ev.frozen_rows, 1, "在途那一条要被冻进去");
    }

    /// 解封之后再被封是新的一次：照常落事件、冻结。限流暂停、人工停用中的号被封也照常记。
    #[sqlx::test]
    async fn record_ban_again_after_reenable_or_from_other_pauses(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        let ctx =
            BanContext { reason: "[403] disabled".into(), source: "forward", ..Default::default() };
        let events = async || store.list_ban_events(Some(a), 10).await.unwrap().len();

        assert!(store.record_ban(a, &ctx).await.unwrap());
        assert_eq!(events().await, 1);
        store.set_disabled(a, false).await.unwrap();
        assert!(store.record_ban(a, &ctx).await.unwrap());
        assert_eq!(events().await, 2, "解封后再封是新的一次");

        store.set_disabled(a, false).await.unwrap();
        let resume_at = crate::credentials::now_secs() + 600;
        assert!(store.pause_for_rate_limit(a, "rate limited", resume_at).await.unwrap());
        assert!(store.record_ban(a, &ctx).await.unwrap());
        assert_eq!(events().await, 3, "限流暂停中被封照常记");

        store.set_disabled(a, false).await.unwrap();
        store.set_disabled(a, true).await.unwrap();
        assert!(store.record_ban(a, &ctx).await.unwrap());
        assert_eq!(events().await, 4, "人工停用中被封照常记");
    }

    /// 冻结流水过了保留期就删，事件本身留着；保留期内的不动。
    #[sqlx::test]
    async fn frozen_logs_are_pruned_after_retention(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let ctx =
            BanContext { reason: "[403] disabled".into(), source: "forward", ..Default::default() };
        let rec = |id: i64| UsageRecord {
            cred_id: Some(id),
            cred_label: "x".into(),
            status: 200,
            ..Default::default()
        };
        let mut ids = Vec::new();
        for label in ["old", "new"] {
            let id = store
                .insert(label, None, label, &format!("r-{label}"), u64::MAX, None, None, 1)
                .await
                .unwrap()
                .id;
            store.insert_usage_log_at(&rec(id), None).await.unwrap();
            assert!(store.record_ban(id, &ctx).await.unwrap());
            ids.push(id);
        }
        sqlx::query("UPDATE ban_events SET ts = ts - $2 WHERE cred_id = $1")
            .bind(ids[0])
            .bind(FROZEN_LOG_RETENTION_SECS + 60)
            .execute(&store.pool)
            .await
            .unwrap();

        assert_eq!(store.prune_frozen_usage_logs().await.unwrap(), 1);
        let events = store.list_ban_events(None, 10).await.unwrap();
        assert_eq!(events.len(), 2, "事件本身不删");
        for ev in &events {
            let left = store.frozen_usage_logs(ev.id, 100, 0).await.unwrap().1.len();
            assert_eq!(left, usize::from(ev.cred_id == ids[1]), "{ev:?}");
        }
    }

    /// 测试用：直接记一条取证待办（模拟停号已提交、取证没落成）。
    async fn insert_pending(
        store: &CredentialStore,
        cred_id: i64,
        ts: i64,
        reason: &str,
        request_id: &str,
    ) {
        let payload = serde_json::json!({
            "ts": ts, "reason": reason, "source": "forward", "status": 403,
            "error_type": null, "error_message": null, "request_id": request_id,
            "upstream_request_id": null, "label": "a", "tier": null, "org_type": null,
            "proxy": null, "account_created_at": 0, "lifetime_requests": 0,
            "lifetime_cost_usd": 0.0, "last_used_at": null, "last_unified_status": null,
            "last_overage_in_use": null,
        });
        sqlx::query("INSERT INTO ban_pending (cred_id, ban_ts, payload) VALUES ($1, $2, $3)")
            .bind(cred_id)
            .bind(ts)
            .bind(payload.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
    }

    /// 取证这一步卡住（这里用表锁模拟）、调用方超时取消：停号照样已经生效，选号选不到它；
    /// 待办留着，之后后台补做补上，前后两次封号的事件都在。
    #[sqlx::test]
    async fn stuck_forensics_do_not_block_the_ban(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool.clone()).await;
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        let earlier = db_now(&store).await - 3600;
        insert_pending(&store, a, earlier, "[403] earlier", "req_earlier").await;

        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE ban_events IN EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let ctx =
            BanContext { reason: "[403] now".into(), source: "forward", ..Default::default() };
        let ban = store.record_ban(a, &ctx);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(500), ban).await.is_err());
        assert!(store.get(a).await.unwrap().unwrap().disabled, "取证卡住也不挡停号");
        assert!(store.select_for_device(Select::default()).await.is_err(), "停掉的号不再被选中");
        blocker.commit().await.unwrap();

        assert_eq!(store.finish_pending_ban_forensics().await.unwrap(), 2);
        let mut evs = store.list_ban_events(Some(a), 10).await.unwrap();
        evs.sort_by_key(|e| e.id);
        let got: Vec<_> = evs.iter().map(|e| (e.reason.as_str(), e.ts == earlier)).collect();
        assert_eq!(got, vec![("[403] earlier", true), ("[403] now", false)]);
    }

    /// 停号已提交、取证没落成：下一次 record_ban 或后台补做用当初记下的上下文与时刻补上，
    /// 且只补一次。
    #[sqlx::test]
    async fn interrupted_ban_forensics_are_finished_later(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        let b = store.insert("b", None, "bt", "rb", u64::MAX, None, None, 1).await.unwrap().id;
        let rec = |id: i64| UsageRecord {
            cred_id: Some(id),
            cred_label: "x".into(),
            status: 200,
            ..Default::default()
        };
        store.insert_usage_log_at(&rec(a), None).await.unwrap();
        let ban_ts = db_now(&store).await - 30;
        for id in [a, b] {
            sqlx::query(
                "UPDATE credentials SET disabled = 1, ban_reason = '[403] original' WHERE id = $1",
            )
            .bind(id)
            .execute(&store.pool)
            .await
            .unwrap();
            insert_pending(&store, id, ban_ts, "[403] original", "req_orig").await;
        }

        // 又撞上一发：号已封着，不记新的，只把当初那条补上。
        let later =
            BanContext { reason: "[401] later".into(), source: "keepalive", ..Default::default() };
        assert!(store.record_ban(a, &later).await.unwrap());
        let evs = store.list_ban_events(Some(a), 10).await.unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!((evs[0].reason.as_str(), evs[0].source.as_str()), ("[403] original", "forward"));
        assert_eq!((evs[0].ts, evs[0].request_id.as_deref()), (ban_ts, Some("req_orig")));
        assert_eq!(evs[0].frozen_rows, 1);
        assert!(store.record_ban(a, &later).await.unwrap());
        assert_eq!(store.list_ban_events(Some(a), 10).await.unwrap().len(), 1);

        // 后台补做把剩下的 b 补上；号删了也照样能补。
        store.delete(b).await.unwrap();
        assert_eq!(store.finish_pending_ban_forensics().await.unwrap(), 1);
        assert_eq!(store.list_ban_events(Some(b), 10).await.unwrap().len(), 1);
        assert_eq!(store.finish_pending_ban_forensics().await.unwrap(), 0);
    }

    /// 上一次的待办没补完、号被重新启用后再次被封：两次各记各的，上一次的原因、请求 id、时刻
    /// 都是当初的，只冻结它自己窗口里的流水。
    #[sqlx::test]
    async fn reban_keeps_the_previous_pending_forensics(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        let now = db_now(&store).await;
        let first_ts = now - 3 * 3600;
        let rec = UsageRecord {
            cred_id: Some(a),
            cred_label: "a".into(),
            status: 200,
            ..Default::default()
        };
        store.insert_usage_log_at(&rec, Some(first_ts - 60)).await.unwrap(); // 第一次封号之前
        store.insert_usage_log_at(&rec, Some(now - 60)).await.unwrap(); // 重新启用之后
        insert_pending(&store, a, first_ts, "[403] first", "req_first").await;

        let second =
            BanContext { reason: "[401] second".into(), source: "keepalive", ..Default::default() };
        assert!(store.record_ban(a, &second).await.unwrap());
        let mut evs = store.list_ban_events(Some(a), 10).await.unwrap();
        evs.sort_by_key(|e| e.id);
        assert_eq!(evs.len(), 2);
        assert_eq!(
            (evs[0].reason.as_str(), evs[0].request_id.as_deref()),
            ("[403] first", Some("req_first"))
        );
        assert_eq!(evs[0].ts, first_ts);
        assert_eq!(evs[0].frozen_rows, 1, "只冻结第一次封号窗口里的那条");
        assert_eq!(evs[1].reason, "[401] second");
        assert_eq!(evs[1].frozen_rows, 2);
        assert_eq!(store.finish_pending_ban_forensics().await.unwrap(), 0);
    }

    /// 待办读不出来（损坏）：删掉，不挡这次封号与它的取证。
    #[sqlx::test]
    async fn unreadable_pending_does_not_block_a_ban(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        sqlx::query(
            "INSERT INTO ban_pending (cred_id, ban_ts, payload) VALUES ($1, 0, 'not json')",
        )
        .bind(a)
        .execute(&store.pool)
        .await
        .unwrap();
        let ctx = BanContext { reason: "[403] x".into(), source: "forward", ..Default::default() };
        assert!(store.record_ban(a, &ctx).await.unwrap());
        assert!(store.get(a).await.unwrap().unwrap().disabled);
        assert_eq!(store.list_ban_events(Some(a), 10).await.unwrap().len(), 1);
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ban_pending")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    /// 号删掉之后补取证，删号时还在途的流水照样要等它提交、冻得进去（没有账号行可锁，靠的是
    /// 按号的 advisory 冻结锁）。
    #[sqlx::test]
    async fn forensics_after_deletion_wait_for_in_flight_logs(pool: sqlx::PgPool) {
        let store = std::sync::Arc::new(CredentialStore::for_test(pool).await);
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        let now = db_now(&store).await;
        insert_pending(&store, a, now, "[403] x", "req").await;
        store.delete(a).await.unwrap();

        // 在途的流水：拿了共享冻结锁、插了流水，先不提交。
        let mut tx = store.pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
            .bind(super::super::freeze_lock_key(a))
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO usage_logs (cred_id, ts) VALUES ($1, unixepoch())")
            .bind(a)
            .execute(&mut *tx)
            .await
            .unwrap();
        let s = store.clone();
        let finish = tokio::spawn(async move { s.finish_pending_ban_forensics().await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!finish.is_finished(), "补取证要等在途的流水提交");
        tx.commit().await.unwrap();
        assert_eq!(finish.await.unwrap(), 1);
        let ev = store.list_ban_events(Some(a), 1).await.unwrap().remove(0);
        assert_eq!(ev.frozen_rows, 1, "在途那一条要被冻进去");
    }

    /// 封后到达的流水补进**时间上最近**的那次封号：补做的旧事件 id 更大、时刻却更早，不能抢走。
    #[sqlx::test]
    async fn tail_logs_join_the_latest_ban_by_time(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        let now = db_now(&store).await;
        // 新的那次先落（id 小），旧的那次后补（id 大），两次相隔不到 FREEZE_TAIL_SECS。
        for (ts, reason) in [(now - 60, "newer"), (now - 120, "older")] {
            sqlx::query("INSERT INTO ban_events (ts, cred_id, cred_label, source, reason) VALUES ($1, $2, 'a', 'forward', $3)")
                .bind(ts)
                .bind(a)
                .bind(reason)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        let rec = UsageRecord {
            cred_id: Some(a),
            cred_label: "a".into(),
            status: 200,
            ..Default::default()
        };
        store.insert_usage_log_at(&rec, Some(now)).await.unwrap();
        let reason: String = sqlx::query_scalar(
            "SELECT b.reason FROM usage_logs_frozen f JOIN ban_events b ON b.id = f.ban_event_id",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(reason, "newer");
    }

    /// 取证拖过了流水保留期（停机、一直出错）：裁剪留着待办要冻结的那段窗口，补出来的冻结完整；
    /// 窗口外、别的号的过期流水照常裁。
    #[sqlx::test]
    async fn pruning_keeps_logs_still_needed_by_pending_forensics(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        let b = store.insert("b", None, "bt", "rb", u64::MAX, None, None, 1).await.unwrap().id;
        let now = db_now(&store).await;
        // 两天前封的号 a，取证一直没落成；封前 6 天的那条流水已过 8 天保留期。
        let ban_ts = now - 2 * 86400 - 3600;
        let old = ban_ts - 6 * 86400;
        let rec = |id: i64| UsageRecord {
            cred_id: Some(id),
            cred_label: "x".into(),
            status: 200,
            ..Default::default()
        };
        store.insert_usage_log_at(&rec(a), Some(old)).await.unwrap(); // 待办窗口内：留
        store.insert_usage_log_at(&rec(a), Some(ban_ts - 8 * 86400)).await.unwrap(); // 窗口外：裁
        store.insert_usage_log_at(&rec(b), Some(old)).await.unwrap(); // 别的号：裁
        insert_pending(&store, a, ban_ts, "[403] x", "req").await;

        assert_eq!(store.prune_usage_logs().await.unwrap(), 2);
        assert_eq!(store.finish_pending_ban_forensics().await.unwrap(), 1);
        let ev = store.list_ban_events(Some(a), 1).await.unwrap().remove(0);
        assert_eq!(ev.frozen_rows, 1);
        // 补完之后不再受保护，下一轮照常裁。
        assert_eq!(store.prune_usage_logs().await.unwrap(), 1);
    }
}
