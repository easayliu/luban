//! 封号事件与冻结流水的 PG 版，对应 `store::bans`。
//!
//! [`BanContext`] / [`BanEvent`] 与冻结窗口常量直接用 `store::bans` 的。

use std::collections::HashMap;

use anyhow::Result;
use sqlx::postgres::PgArguments;
use sqlx::{Arguments, Row};

use super::super::{
    BanContext, BanEvent, ERROR_MESSAGE_MAX, FREEZE_WINDOW_SECS, USAGE_LOG_COLS, UsageLog,
    head_chars, redact_proxy,
};
use super::PgStore;
use super::session_events::log_removed;
use super::usage::usage_log_from_row;

impl PgStore {
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
    /// 串行化写事务里做，且一上来就给账号行上 `FOR UPDATE`：它与写流水时的 `FOR KEY SHARE`
    /// 互斥（见 [`Self::insert_usage_log_at`]），于是在途的流水要么在冻结之前提交、被这里冻进去，
    /// 要么等这里提交后才写、看得到这条事件并自己补进冻结表，一条都漏不掉。
    pub async fn record_ban(&self, id: i64, ctx: &BanContext) -> Result<bool> {
        let mut tx = self.begin_write().await?;
        // (封号时刻, label, tier, org_type, proxy, created_at)
        type Account = (i64, String, Option<String>, Option<String>, Option<String>, i64);
        let account: Option<Account> = sqlx::query_as(
            "SELECT unixepoch(), label, tier, org_type, proxy, created_at
                   FROM credentials WHERE id = $1 FOR UPDATE",
        )
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
        let Some((ts, label, tier, org_type, proxy, created_at)) = account else {
            tx.commit().await?;
            return Ok(false);
        };
        sqlx::query(
            "UPDATE credentials SET disabled = 1, ban_reason = $2, resume_at = NULL,
                    updated_at = unixepoch()
              WHERE id = $1",
        )
        .bind(id)
        .bind(&ctx.reason)
        .execute(&mut *tx)
        .await?;
        // 账本侧快照：终身费用、最近使用，以及封前最后一次限流快照（额度是不是早就满了、
        // 在不在烧 credits）。
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
        let since = ts - FREEZE_WINDOW_SECS;
        // 设备数分两侧：device_id 是来访客户端自报的，device_id_out 是实际发给 Anthropic 的
        // （伪装开着时是派生值）。上游看到的是后者——「一个号在上游眼里有几台设备」看它。
        let (requests_7d, devices_7d, devices_out_7d): (i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COUNT(DISTINCT device_id), COUNT(DISTINCT device_id_out)
               FROM usage_logs WHERE cred_id = $1 AND ts >= $2",
        )
        .bind(id)
        .bind(since)
        .fetch_one(&mut *tx)
        .await?;
        let mut lists = Vec::with_capacity(4);
        for col in ["model", "ua", "proxy", "device_id_out"] {
            let rows: Vec<(String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {col}, COUNT(*) AS n FROM usage_logs
                  WHERE cred_id = $1 AND ts >= $2 AND {col} IS NOT NULL
                  GROUP BY {col} ORDER BY n DESC LIMIT 50"
            )))
            .bind(id)
            .bind(since)
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
        .bind(ts)
        .bind(id)
        .bind(label)
        .bind(ctx.source)
        .bind(&ctx.reason)
        .bind(ctx.status.map(i64::from))
        .bind(&ctx.error_type)
        .bind(ctx.error_message.as_deref().map(|m| head_chars(m, ERROR_MESSAGE_MAX * 4)))
        .bind(&ctx.request_id)
        .bind(&ctx.upstream_request_id)
        .bind(tier)
        .bind(org_type)
        .bind(proxy.as_deref().map(redact_proxy))
        .bind(created_at)
        .bind(lifetime_requests)
        .bind(lifetime_cost)
        .bind(last_used_at)
        .bind(requests_7d)
        .bind(devices_7d)
        .bind(models_7d)
        .bind(uas_7d)
        .bind(proxies_7d)
        .bind(last_unified)
        .bind(last_overage)
        .bind(devices_out_7d)
        .bind(device_ids_out_7d)
        .fetch_one(&mut *tx)
        .await?;
        let frozen = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO usage_logs_frozen (ban_event_id, src_id, {USAGE_LOG_COLS})
             SELECT $1, id, {USAGE_LOG_COLS} FROM usage_logs
              WHERE cred_id = $2 AND ts >= $3 ORDER BY id"
        )))
        .bind(ban_id)
        .bind(id)
        .bind(since)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query("UPDATE ban_events SET frozen_rows = $2 WHERE id = $1")
            .bind(ban_id)
            .bind(frozen as i64)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
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
        let logs = rows.iter().map(usage_log_from_row).collect::<Result<Vec<_>>>()?;
        // 只读事务，提交与回滚等价；显式结束，别等 drop 时静默回滚吞掉错误。
        tx.commit().await?;
        Ok((total, logs))
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{FREEZE_TAIL_SECS, Forensics, UsageRecord};
    use super::super::usage::tests::db_now;
    use super::*;

    /// 封号事件与冻结流水是取证材料：落地时快照账号侧读数、冻结封前流水；封后 10 分钟内
    /// 到达的流水（触发那一发）也要补进去；删号、解封、裁剪都不能碰它们。
    #[sqlx::test]
    async fn record_ban_freezes_history_and_survives_deletion(pool: sqlx::PgPool) {
        let store = PgStore::for_test(pool).await;
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

        let events = store.list_ban_events(None, 10).await.unwrap();
        assert_eq!(events.len(), 1);
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

    /// 封号与在途流水的先后：流水事务已经给账号行上了 `FOR KEY SHARE`、还没提交时，
    /// `record_ban` 要等它提交再冻结，冻得到这一条（PG 的事务真并发，两边各看各的快照的话
    /// 这一条会两头落空：冻结时看不到它，它写入时又看不到封号事件）。
    #[sqlx::test]
    async fn record_ban_waits_for_an_in_flight_usage_log(pool: sqlx::PgPool) {
        let store = std::sync::Arc::new(PgStore::for_test(pool).await);
        let a = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).await.unwrap().id;
        // 手工模拟一条在途的流水写入：上锁、插流水，先不提交。
        let mut tx = store.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM credentials WHERE id = $1 FOR KEY SHARE")
            .bind(a)
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
}
