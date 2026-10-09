//! 设备 / 会话绑定的查询、解绑与过期清理（PG 版），对应 `store::bindings`。

use std::collections::HashMap;

use anyhow::Result;
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::super::{
    DeviceBinding, SESSION_EVENT_RETENTION_SECS, SessionBinding, effective_retention,
};
use super::PgStore;
use super::session_events::{args1, args2, delete_logged};

impl PgStore {
    /// 这台设备是否已有绑定记录（任一凭证、不论是否仍在 TTL 内）。有记录就是「见过的设备」；
    /// 绑定被保留期清掉后它会再次被当成没见过，那时它也确实很久没来了。供探针判定
    /// （`proxy::probe_signature`）区分「老设备的一条小请求」与「凭空冒出来的一次性会话」。
    /// 查询失败按「没见过」算。
    pub async fn device_is_known(&self, device_id: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM device_bindings WHERE device_id = $1)",
        )
        .bind(device_id)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(false)
    }

    /// 单条凭证当前**占名额**的设备数：已排除超过 TTL 未活跃的绑定（与选路时判上限的口径
    /// 一致），故后台显示会随时间自然回落，不必等下一次请求触发 sweep。
    /// TTL `<= 0`（永不过期）时按全量计。
    ///
    /// 数不到休眠中的软绑定是有意的：它们不占名额，只是还记着「这台设备上次用的是这个号」
    /// （见 [`Self::select_for_device`]），列进来会让「设备 x/y」这个名额口径失真。
    pub async fn device_count(&self, cred_id: i64) -> Result<i64> {
        self.active_count("device_bindings", cred_id, self.device_binding_ttl()).await
    }

    /// 单条凭证在 `table`（只由代码里的常量传入）里 TTL 内活跃的绑定数；TTL `<= 0` 按全量计。
    async fn active_count(&self, table: &'static str, cred_id: i64, ttl: i64) -> Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} \
              WHERE cred_id = $1 AND ($2 <= 0 OR last_seen_at >= unixepoch() - $2)"
        )))
        .bind(cred_id)
        .bind(ttl)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 单条凭证当前**有效**绑定的设备明细（含费用），按最近活跃倒序。
    ///
    /// **真实绑定**那部分的过滤口径与 [`Self::device_count`] 完全一致（同一个 TTL），否则后台
    /// 会出现「设备数写着 2、展开却列出 5 条」这种自相矛盾的展示。末尾追加的模拟伪设备
    /// （`simulated` 为真）**不在这个口径内**——它们不写绑定、不占名额，故 `device_count`
    /// 数不到它们。前端要显示「设备数」时只能数 `!simulated` 那些。
    ///
    /// 费用来自 `device_costs` 账本（写日志时同事务累加），与绑定表是两套账，刻意不合并：
    /// 绑定行会被解绑/停用/TTL 清掉并从零重新计数，账本则终身累计。同时给出跨账号合计，
    /// 便于识别换号仍在持续烧钱的同一台设备。
    pub async fn list_devices(&self, cred_id: i64) -> Result<Vec<DeviceBinding>> {
        let ttl = self.device_binding_ttl();
        // 真实绑定在前（按最近活跃倒序），模拟客户端的伪设备在后（按请求数倒序）：一条
        // `UNION ALL` 取齐，省一次往返。伪设备不写绑定（故第一段一条都查不到），但用量与
        // 费用照常落进 `device_costs`；不接 TTL——那是绑定的过期规则，这里没有绑定可过期。
        let rows = sqlx::query(
            "SELECT * FROM ( \
               SELECT 0 AS part, b.device_id, b.request_count, b.created_at, b.last_seen_at, \
                      COALESCE((SELECT dc.cost_usd FROM device_costs dc \
                                 WHERE dc.cred_id = b.cred_id AND dc.device_id = b.device_id), 0) \
                          AS cost, \
                      COALESCE((SELECT SUM(dc.cost_usd) FROM device_costs dc \
                                 WHERE dc.device_id = b.device_id), 0) AS cost_all \
                 FROM device_bindings b \
                WHERE b.cred_id = $1 AND ($2 <= 0 OR b.last_seen_at >= unixepoch() - $2) \
               UNION ALL \
               SELECT 1, dc.device_id, dc.request_count, NULL, NULL, dc.cost_usd, \
                      COALESCE((SELECT SUM(d2.cost_usd) FROM device_costs d2 \
                                 WHERE d2.device_id = dc.device_id), 0) \
                 FROM device_costs dc \
                WHERE dc.cred_id = $1 AND dc.device_id LIKE 'sim:%' \
             ) t \
             ORDER BY part, \
                      CASE WHEN part = 0 THEN last_seen_at END DESC, \
                      CASE WHEN part = 1 THEN request_count END DESC, \
                      device_id ASC",
        )
        .bind(cred_id)
        .bind(ttl)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(DeviceBinding {
                    device_id: r.try_get(1)?,
                    request_count: r.try_get(2)?,
                    created_at: r.try_get(3)?,
                    last_seen_at: r.try_get(4)?,
                    cost_usd: r.try_get(5)?,
                    cost_usd_all: r.try_get(6)?,
                    simulated: r.try_get::<i32, _>(0)? == 1,
                })
            })
            .collect()
    }

    /// 手动解除一条设备绑定，返回是否确有删除。
    ///
    /// 按 `(cred_id, device_id)` 双条件删除，而不是只按 `device_id`：后台拿到的设备列表可能
    /// 已经过期（设备刚被换到别的号上），只按 device_id 删会把它从**当前**所在账号上摘掉。
    ///
    /// 不受绑定 TTL 影响：TTL 外那些休眠的软绑定虽然不占名额，但还留着亲和性，解绑就是要把
    /// 这份记忆一并抹掉（下次来当新设备重新分号）。
    pub async fn unbind_device(&self, cred_id: i64, device_id: &str) -> Result<bool> {
        self.update_one(
            sqlx::query("DELETE FROM device_bindings WHERE cred_id = $1 AND device_id = $2")
                .bind(cred_id)
                .bind(device_id),
        )
        .await
    }

    /// 所有凭证当前**有效**绑定的设备数（cred_id → count）；口径同 [`Self::device_count`]，
    /// 排除超过 TTL 未活跃的绑定。TTL `<= 0` 时按全量计。
    pub async fn device_counts(&self) -> Result<HashMap<i64, i64>> {
        active_counts(
            &mut *self.pool.acquire().await?,
            "device_bindings",
            self.device_binding_ttl(),
        )
        .await
    }

    /// 单条凭证当前**占名额**的模拟会话数；口径同 [`Self::device_count`]（TTL 内活跃的绑定）。
    pub async fn session_count(&self, cred_id: i64) -> Result<i64> {
        self.active_count("session_bindings", cred_id, self.session_binding_ttl()).await
    }

    /// 所有凭证当前**有效**的模拟会话绑定数（cred_id → count）；口径同 [`Self::session_count`]。
    pub async fn session_counts(&self) -> Result<HashMap<i64, i64>> {
        active_counts(
            &mut *self.pool.acquire().await?,
            "session_bindings",
            self.session_binding_ttl(),
        )
        .await
    }

    /// 单条凭证当前**有效**的模拟会话明细，按最近活跃倒序；过滤口径与 [`Self::session_count`]
    /// 完全一致，否则后台会出现「会话数写着 2、展开却列出 5 条」。
    pub async fn list_sessions(&self, cred_id: i64) -> Result<Vec<SessionBinding>> {
        let ttl = self.session_binding_ttl();
        // 会话 id 要账号 uuid 才算得出；凭证不存在时按空 uuid 算（调用方已先判过 404）。
        // 一并在同一条查询里取（LEFT JOIN），省一次往返。
        let rows = sqlx::query(
            "SELECT s.session_key, s.slot, s.request_count, s.created_at, s.last_seen_at, \
                    s.last_model, s.device_id, c.account_uuid \
               FROM session_bindings s LEFT JOIN credentials c ON c.id = s.cred_id \
              WHERE s.cred_id = $1 AND ($2 <= 0 OR s.last_seen_at >= unixepoch() - $2) \
              ORDER BY s.last_seen_at DESC, s.session_key ASC",
        )
        .bind(cred_id)
        .bind(ttl)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| session_row(r, cred_id)).collect()
    }

    /// 这条会话键在该凭证上占的槽位；没绑在这个号上为 `None`。转发路径不再用它——槽位随
    /// 选号结果一起返回（[`Self::select_with_slot`]），这里只给测试核对绑定行。
    #[cfg(test)]
    pub async fn session_slot(&self, cred_id: i64, session_key: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT slot FROM session_bindings WHERE session_key = $1 AND cred_id = $2",
        )
        .bind(session_key)
        .bind(cred_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 一键清掉该凭证的**全部**模拟会话绑定（含休眠的软绑定），返回删掉的条数。会话比设备
    /// 多得多、又是 luban 自己派生的键，逐条解绑不现实；清掉只是腾名额、抹亲和性，下一条请求
    /// 照常重新选号。
    pub async fn unbind_all_sessions(&self, cred_id: i64) -> Result<usize> {
        // 串行化：不和选号交错（选号刚读到的绑定在这里被删，它随后的续用就落空了）。
        let mut tx = self.begin_write().await?;
        let n = delete_logged(&mut tx, "unbound", Some("clear"), "cred_id = $1", args1(cred_id)?)
            .await?;
        tx.commit().await?;
        Ok(n as usize)
    }

    /// 手动解除一条模拟会话绑定，返回是否确有删除。按 `(cred_id, session_key)` 双条件删，
    /// 理由同 [`Self::unbind_device`]。
    pub async fn unbind_session(&self, cred_id: i64, session_key: &str) -> Result<bool> {
        let mut tx = self.begin_write().await?;
        let n = delete_logged(
            &mut tx,
            "unbound",
            Some("manual"),
            "cred_id = $1 AND session_key = $2",
            args2(cred_id, session_key)?,
        )
        .await?;
        tx.commit().await?;
        Ok(n > 0)
    }

    /// 删掉「连保留期都过了」的设备绑定与会话绑定，返回两张表各删了多少行。后台定时跑
    /// （见 `web::run`），不在转发路径上。
    ///
    /// **清理时机不影响选号**：过了保留期、还没被删掉的行在 [`Self::select_with_slot`] 那侧
    /// 已被显式滤掉（`bound` 查询上那道保留期条件），故「还在表里」与「已经删了」对选号是同一个
    /// 结果，这里跑得早跑得晚都一样——所以也不必拿串行化写锁挡住选号。
    ///
    /// TTL 与保留期都取当前配置：两项都可以在后台改，改小之后下一次清理就按新值收。
    /// 任一项配成 `<= 0`（永不过期／永久保留）时那张表整张不动，见 [`effective_retention`]。
    pub async fn prune_expired_bindings(&self) -> Result<(usize, usize)> {
        let device =
            effective_retention(self.device_binding_ttl(), self.device_binding_retention());
        let session =
            effective_retention(self.session_binding_ttl(), self.session_binding_retention());
        let mut conn = self.pool.acquire().await?;
        let devices = match device {
            Some(secs) => {
                sqlx::query("DELETE FROM device_bindings WHERE last_seen_at < unixepoch() - $1")
                    .bind(secs)
                    .execute(&mut *conn)
                    .await?
                    .rows_affected()
            }
            None => 0,
        };
        // 记 expired 事件与删行是同一条语句，记下的正是删掉的那些行。
        let sessions = match session {
            Some(secs) => {
                delete_logged(
                    &mut conn,
                    "expired",
                    None,
                    "last_seen_at < unixepoch() - $1",
                    args1(secs)?,
                )
                .await?
            }
            None => 0,
        };
        // 会话历史事件保留 7 天，与绑定的保留期无关。
        sqlx::query("DELETE FROM session_binding_events WHERE ts < unixepoch() - $1")
            .bind(SESSION_EVENT_RETENTION_SECS)
            .execute(&mut *conn)
            .await?;
        Ok((devices as usize, sessions as usize))
    }
}

/// `list_sessions` 的一行（末列是号的 `account_uuid`）→ [`SessionBinding`]。
fn session_row(r: &PgRow, cred_id: i64) -> Result<SessionBinding> {
    let session_key: String = r.try_get(0)?;
    let slot: i64 = r.try_get(1)?;
    let account_uuid: Option<String> = r.try_get(7)?;
    // 真实客户端的会话键带来访会话 id 时（`lb:v2:sid:<id>`）取最后一段按账号钉住；钉不出来
    // （号没有 account_uuid）时上游收到的就是来访原值。按前缀指纹分的（`…:pfx:…`）来访
    // 根本没带会话 id，出站也就没有，留空。
    let session_id = if slot < 0 {
        match session_key.rsplit_once(':') {
            Some((head, sid)) if head.ends_with(":sid") => {
                crate::credentials::pinned_session_id(account_uuid.as_deref(), sid)
                    .unwrap_or_else(|| sid.to_string())
            }
            _ => String::new(),
        }
    } else {
        crate::credentials::sim_slot_session_id(account_uuid.as_deref(), cred_id, slot)
    };
    Ok(SessionBinding {
        session_key,
        slot,
        session_id,
        request_count: r.try_get(2)?,
        created_at: r.try_get(3)?,
        last_seen_at: r.try_get(4)?,
        last_model: r.try_get(5)?,
        device_id: r.try_get(6)?,
    })
}

/// 按凭证数「TTL 内活跃」绑定（`table` 是 `device_bindings` 或 `session_bindings`，只由代码里
/// 的常量传入）。TTL `<= 0` 时不过滤、按全量计。选号与后台列表共用，口径才一致。
pub(super) async fn active_counts(
    conn: &mut sqlx::PgConnection,
    table: &'static str,
    ttl_secs: i64,
) -> Result<HashMap<i64, i64>> {
    // 两种写法分开拼，不写成 `$1 <= 0 OR last_seen_at >= …`：带 OR 的条件在通用执行计划里
    // 走不了 `last_seen_at` 索引的范围扫描，而选号每条请求都要数一次，只该扫 TTL 内那一小段。
    let rows: Vec<(i64, i64)> = if ttl_secs > 0 {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT cred_id, COUNT(*) FROM {table} \
              WHERE last_seen_at >= unixepoch() - $1 GROUP BY cred_id"
        )))
        .bind(ttl_secs)
        .fetch_all(conn)
        .await?
    } else {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT cred_id, COUNT(*) FROM {table} GROUP BY cred_id"
        )))
        .fetch_all(conn)
        .await?
    };
    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::super::*;
    use super::super::PgStore;
    use super::super::select::tests::{
        age_binding, age_session_binding, scalar, soft, soft_store, store_with,
    };

    /// 走真实写入口落一条用量日志（只填与费用统计相关的字段）。
    async fn log_cost(store: &PgStore, cred_id: i64, device_id: &str, cost: Option<f64>) {
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(cred_id),
                device_id: Some(device_id.to_string()),
                path: "/v1/messages".to_string(),
                status: 200,
                has_usage: true,
                cost_usd: cost,
                ..Default::default()
            })
            .await
            .unwrap();
    }

    /// 模拟客户端（`sim:` 前缀）不写绑定，故此前在设备列表里完全看不到——用量与费用都在
    /// `device_costs` 里，只是没人读。现在把它们作为伪设备追加在真实设备之后。
    ///
    /// 同时钉住三件事：请求数**无条件**计（含没有 usage 的 4xx，否则限流排查时数字对不上）、
    /// 不占 `device_count` 名额、跨账号合计仍然正确。
    #[sqlx::test]
    async fn lists_simulated_devices_with_request_counts(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let log = async |cred: i64, dev: &str, cost: Option<f64>| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    cred_label: "x".into(),
                    device_id: Some(dev.into()),
                    cost_usd: cost,
                    ..Default::default()
                })
                .await
                .unwrap();
        };
        let sim = "sim:ff813c9166f0d2f3";
        log(a, sim, Some(0.01)).await;
        log(a, sim, Some(0.02)).await;
        // 模型认不出 → 无费用可计，但请求确实发生过，请求数照记。
        log(a, sim, None).await;
        // 同一个伪设备也可能落到别的账号上（换号重试／负载均衡）。
        log(b, sim, Some(0.05)).await;

        let devs = store.list_devices(a).await.unwrap();
        assert_eq!(devs.len(), 1, "伪设备该出现在列表里: {devs:?}");
        let d = &devs[0];
        assert!(d.simulated, "该标记成模拟客户端");
        assert_eq!(d.device_id, sim);
        assert_eq!(d.request_count, 3, "没有 usage 的那条也要计数");
        assert!((d.cost_usd - 0.03).abs() < 1e-9, "本账号费用: {}", d.cost_usd);
        assert!((d.cost_usd_all - 0.08).abs() < 1e-9, "跨账号合计: {}", d.cost_usd_all);
        assert_eq!(d.created_at, None, "没有绑定就没有绑定时刻");
        assert_eq!(d.last_seen_at, None);

        // 不占设备名额——那是 device_bindings 的口径，伪设备一行都不写。
        assert_eq!(store.device_count(a).await.unwrap(), 0, "伪设备不该计入设备数");

        // 真实设备与伪设备并存时，真实的排在前面且不被标记。
        store
            .select_for_device(Select { device_id: Some("real-1"), ..Default::default() })
            .await
            .unwrap();
        log(a, "real-1", Some(1.0)).await;
        let devs = store.list_devices(a).await.unwrap();
        assert_eq!(devs.len(), 2, "{devs:?}");
        assert!(!devs[0].simulated && devs[0].device_id == "real-1", "真实设备排前面: {devs:?}");
        assert!(devs[1].simulated, "伪设备排后面: {devs:?}");
        assert_eq!(store.device_count(a).await.unwrap(), 1, "只有真实设备占名额");
    }

    /// 设备明细必须与设备数同口径：条数等于 `device_count`、只含本凭证的绑定、
    /// 超过 TTL 未活跃的不出现。否则后台会显示「设备 1/3，展开却列出 2 台」。
    #[sqlx::test]
    async fn list_devices_matches_device_count(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);

        // dev-1 粘到 a（同优先级下 id 小者先中），再来一次命中既有绑定、请求数 +1。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            a
        );
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            a
        );
        // dev-2 是新设备：a 已有 1 台、b 还是 0 台，负载均衡会把它分给 b。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-2"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            b
        );

        let a_devs = store.list_devices(a).await.unwrap();
        assert_eq!(a_devs.len() as i64, store.device_count(a).await.unwrap(), "条数应等于设备数");
        assert_eq!(a_devs.len(), 1, "只应列出绑到 a 的设备");
        assert_eq!(a_devs[0].device_id, "dev-1");
        assert_eq!(a_devs[0].request_count, 1, "第二次命中既有绑定应计数");
        assert_eq!(store.list_devices(b).await.unwrap()[0].device_id, "dev-2");

        // 把 dev-1 的活跃时间推到 TTL 之外：明细与计数应同步把它排除。
        store.set_setting(DEVICE_BINDING_TTL, "60").await.unwrap();
        age_binding(&store, "dev-1", 600).await;
        assert_eq!(store.device_count(a).await.unwrap(), 0);
        assert!(store.list_devices(a).await.unwrap().is_empty(), "超时绑定不应出现在明细里");
    }

    /// 会话绑定的有效期与保留期是**单独**的一对设置：设备那对永不过期时，会话照样按自己的
    /// TTL 释放名额、按自己的保留期清行；计数与明细读的也是会话那对。
    #[sqlx::test]
    async fn session_bindings_expire_on_their_own_ttl(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];
        store.set_setting(DEVICE_BINDING_TTL, "0").await.unwrap();
        store.set_setting(SESSION_BINDING_TTL, "60").await.unwrap();
        store.set_setting(SESSION_BINDING_RETENTION, "600").await.unwrap();
        assert_eq!(store.session_binding_ttl(), 60);
        assert_eq!(store.session_binding_retention(), 600);
        fn sel(k: &str) -> Select<'_> {
            Select {
                session_key: Some(k),
                ttl_secs: 0,
                retention_secs: 0,
                session_ttl_secs: 60,
                session_retention_secs: 600,
                rate_limited: true,
                ..Default::default()
            }
        }
        assert_eq!(store.select_for_device(sel("s1")).await.unwrap().id, a);
        assert_eq!(store.session_count(a).await.unwrap(), 1);
        // 设备永不过期，会话 61 秒后不占名额；行还在（保留期 600 秒内），回来回原槽位。
        age_session_binding(&store, "s1", 61).await;
        assert_eq!(store.session_count(a).await.unwrap(), 0, "按会话自己的 TTL 释放");
        assert!(store.list_sessions(a).await.unwrap().is_empty());
        assert_eq!(store.session_slot(a, "s1").await.unwrap(), Some(0), "行还在");
        assert_eq!(store.select_for_device(sel("s2")).await.unwrap().id, a);
        assert_eq!(store.session_slot(a, "s2").await.unwrap(), Some(0), "释放了的槽位被复用");
        // 超过会话保留期：选号立刻当它不存在（行还在，由后台按自己的节奏删），s1 再来是新会话。
        age_session_binding(&store, "s1", 601).await;
        assert_eq!(store.select_for_device(sel("s3")).await.unwrap().id, a);
        assert_eq!(store.session_slot(a, "s1").await.unwrap(), Some(0), "行还在，后台还没跑");
        assert_eq!(store.prune_expired_bindings().await.unwrap(), (0, 1), "按会话自己的保留期清行");
        assert_eq!(store.session_slot(a, "s1").await.unwrap(), None);
        // 默认值：会话 30 分钟 / 1 天，与设备的 1 小时 / 7 天不同。
        for key in [SESSION_BINDING_TTL, SESSION_BINDING_RETENTION, DEVICE_BINDING_TTL] {
            store.delete_setting(key).await.unwrap();
        }
        let fresh = &store;
        assert_eq!(fresh.session_binding_ttl(), DEFAULT_SESSION_BINDING_TTL_SECS);
        assert_eq!(fresh.session_binding_retention(), DEFAULT_SESSION_BINDING_RETENTION_SECS);
        assert_ne!(fresh.session_binding_ttl(), fresh.device_binding_ttl());
    }

    /// 保留期到点后设备就是台新设备，回不去原号——**与行删没删无关**。
    ///
    /// 删行这件事挪去了后台（[`CredentialStore::prune_expired_bindings`]，见那里的记述），
    /// 故这条用例钉的是两件事：一、行还在表里的时候选号就已经不认它了（否则后台跑之前那段
    /// 时间里，设备会被送回一个本该忘掉的号）；二、后台真跑的时候那行会被删掉。
    #[sqlx::test]
    async fn binding_rows_are_forgotten_once_the_retention_window_passes(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);

        assert_eq!(store.select_for_device(soft("dev-1")).await.unwrap().id, a);
        age_binding(&store, "dev-1", 7200).await;
        // 让 a 上多一台活跃设备，好让下面的负载均衡有个明确去向。
        assert_eq!(store.select_for_device(soft("dev-2")).await.unwrap().id, a);

        let rows = async |store: &PgStore| -> i64 {
            scalar(store, "SELECT COUNT(*) FROM device_bindings WHERE device_id = 'dev-1'").await
        };
        assert_eq!(rows(&store).await, 1, "后台还没跑，行还在表里");
        assert_eq!(
            store.select_for_device(soft("dev-1")).await.unwrap().id,
            b,
            "行还在也不算数：过了保留期就按负载均衡走"
        );

        // 上面那一发已经把 dev-1 改绑到 b 并刷新了 last_seen_at，故这行现在是活的、不该被清。
        assert_eq!(store.prune_expired_bindings().await.unwrap(), (0, 0), "活着的绑定不动");
        age_binding(&store, "dev-1", 7200).await;
        assert_eq!(store.prune_expired_bindings().await.unwrap(), (1, 0), "过了保留期的由后台删掉");
        assert_eq!(rows(&store).await, 0);
    }

    /// 后台清理认的是**当前配置**：保留期配成「永久」（`<= 0`）时一行都不许删。
    #[sqlx::test]
    async fn pruning_bindings_respects_a_forever_retention(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a"]).await;
        assert_eq!(store.select_for_device(soft("dev-1")).await.unwrap().id, ids[0]);
        age_binding(&store, "dev-1", 7200).await;
        store.set_setting(DEVICE_BINDING_RETENTION, "0").await.unwrap();
        assert_eq!(store.prune_expired_bindings().await.unwrap(), (0, 0), "永久保留即一行不删");
    }

    /// 设备明细里的费用：本账号一列只算本账号花的，跨账号合计要把换号前的也算进去，
    /// 且不因解绑/重绑而归零（用量日志与绑定行是两套账）。
    #[sqlx::test]
    async fn list_devices_sums_cost_per_device(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            a
        );
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-2"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            b
        );

        // dev-1 在 a 上花了 0.5+0.25，换号后在 b 上又花了 1.0；dev-2 只在 b 上花了 0.125。
        log_cost(&store, a, "dev-1", Some(0.5)).await;
        log_cost(&store, a, "dev-1", Some(0.25)).await;
        log_cost(&store, b, "dev-1", Some(1.0)).await;
        log_cost(&store, b, "dev-2", Some(0.125)).await;
        // 模型未知的请求 cost_usd 为空，SUM 要能跳过而不是把整行算成 NULL。
        log_cost(&store, a, "dev-1", None).await;

        let d = &store.list_devices(a).await.unwrap()[0];
        assert_eq!(d.device_id, "dev-1");
        assert!((d.cost_usd - 0.75).abs() < 1e-9, "本账号只算 a 上的花费：{}", d.cost_usd);
        assert!((d.cost_usd_all - 1.75).abs() < 1e-9, "合计要含 b 上的：{}", d.cost_usd_all);

        // 没有任何用量日志的设备给 0，而不是 NULL 取值失败。
        assert_eq!(
            store
                .list_devices(b)
                .await
                .unwrap()
                .iter()
                .find(|x| x.device_id == "dev-2")
                .unwrap()
                .cost_usd,
            0.125
        );

        // 解绑再重绑：请求数从零重数，费用是历史累计，不受影响。
        assert!(store.unbind_device(a, "dev-1").await.unwrap());
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            a
        );
        let d = &store.list_devices(a).await.unwrap()[0];
        assert_eq!(d.request_count, 0, "重绑后是新的一条绑定");
        assert!((d.cost_usd - 0.75).abs() < 1e-9, "费用不该被解绑清掉");
    }

    /// 手动解绑：立刻腾出名额（计数与明细同步减一）、只动本凭证名下的那条绑定、
    /// 重复解绑返回 false（后台据此给 404，而不是静默成功）。
    #[sqlx::test]
    async fn unbind_device_frees_slot_and_is_scoped_to_credential(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);

        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            a
        );
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-2"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            b
        );

        // 拿 b 的 id 去解 dev-1（模拟后台列表已过期、设备其实绑在 a 上）：不能误伤 a 的绑定。
        assert!(!store.unbind_device(b, "dev-1").await.unwrap(), "跨凭证解绑应无效");
        assert_eq!(store.device_count(a).await.unwrap(), 1, "误删他号绑定会让名额凭空消失");

        assert!(store.unbind_device(a, "dev-1").await.unwrap());
        assert_eq!(store.device_count(a).await.unwrap(), 0, "解绑后名额应立刻释放");
        assert!(store.list_devices(a).await.unwrap().is_empty());
        assert_eq!(store.device_count(b).await.unwrap(), 1, "不应波及其它账号");

        // 已经没有这条绑定了：再解一次要报「没删到」。
        assert!(!store.unbind_device(a, "dev-1").await.unwrap());

        // 解绑不是拉黑：设备下次请求重新走选号，仍可能落回同一个账号。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            a
        );
        assert_eq!(store.device_count(a).await.unwrap(), 1);
    }
}
