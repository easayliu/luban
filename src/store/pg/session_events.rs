//! 会话绑定的历史事件（PG 版），对应 `store::session_events`。

use anyhow::Result;
use sqlx::PgConnection;
use sqlx::Row;
use sqlx::postgres::PgArguments;

use super::super::{NewEvent, SessionEvent};
use super::PgStore;

/// 删掉一批会话绑定**之前**调用：给 `session_bindings WHERE {filter}` 命中的每一行记一条
/// `event` 事件（带闲置时长与 `reason`）。`filter` 只会是代码里的常量，参数从 `$1` 起编号，
/// 由 `args` 依次给出。回记了几条。
pub(super) async fn log_removed(
    conn: &mut PgConnection,
    event: &'static str,
    reason: Option<&'static str>,
    filter: &str,
    args: PgArguments,
) -> Result<u64> {
    // `reason` 拼成字面量而不是参数：参数位已经留给 `filter`，而它只会是代码里的常量。
    let reason = reason.map_or("NULL".to_string(), |r| format!("'{r}'"));
    let sql = format!(
        "INSERT INTO session_binding_events \
            (session_key, event, cred_id, slot, idle_secs, reason) \
         SELECT session_key, '{event}', cred_id, slot, unixepoch() - last_seen_at, {reason} \
           FROM session_bindings WHERE {filter}"
    );
    Ok(sqlx::query_with(sqlx::AssertSqlSafe(sql), args).execute(conn).await?.rows_affected())
}

/// 删掉 `session_bindings WHERE {filter}` 命中的行，并给每一行记一条 `event` 事件（带闲置
/// 时长与 `reason`），回删了几行。效果同先 [`log_removed`] 再 `DELETE`，但是**一条语句**
/// （可写 CTE）：记下的正是删掉的那几行，两步之间不会有别的事务插进来续上某一行、让它记了
/// 事件却没被删。`filter` 只会是代码里的常量，参数从 `$1` 起编号，由 `args` 依次给出。
pub(super) async fn delete_logged(
    conn: &mut PgConnection,
    event: &'static str,
    reason: Option<&'static str>,
    filter: &str,
    args: PgArguments,
) -> Result<u64> {
    let reason = reason.map_or("NULL".to_string(), |r| format!("'{r}'"));
    let sql = format!(
        "WITH gone AS ( \
             DELETE FROM session_bindings WHERE {filter} \
             RETURNING session_key, cred_id, slot, last_seen_at) \
         INSERT INTO session_binding_events \
            (session_key, event, cred_id, slot, idle_secs, reason) \
         SELECT session_key, '{event}', cred_id, slot, unixepoch() - last_seen_at, {reason} \
           FROM gone"
    );
    Ok(sqlx::query_with(sqlx::AssertSqlSafe(sql), args).execute(conn).await?.rows_affected())
}

/// 给 [`log_removed`] 拼一个参数的 `PgArguments`（对应 `filter` 里的 `$1`）。
pub(super) fn args1<'t, A>(a: A) -> Result<PgArguments>
where
    A: sqlx::Encode<'t, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    let mut args = PgArguments::default();
    sqlx::Arguments::add(&mut args, a).map_err(anyhow::Error::from_boxed)?;
    Ok(args)
}

/// 同 [`args1`]，两个参数（`$1`、`$2`）。
pub(super) fn args2<'t, A, B>(a: A, b: B) -> Result<PgArguments>
where
    A: sqlx::Encode<'t, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
    B: sqlx::Encode<'t, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    let mut args = args1(a)?;
    sqlx::Arguments::add(&mut args, b).map_err(anyhow::Error::from_boxed)?;
    Ok(args)
}

/// 一次查询最多返回的事件条数（按时间倒序取最近的），同 `store::session_events`。
const EVENTS_LIMIT: i64 = 100;

/// 写一条事件。字段语义见建表处的注释（`migrations/0001_init.sql`）。
pub(super) async fn log_event(conn: &mut PgConnection, e: NewEvent<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO session_binding_events \
            (session_key, event, cred_id, prev_cred_id, slot, prev_slot, other_key, idle_secs, reason) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(e.key)
    .bind(e.event)
    .bind(e.cred_id)
    .bind(e.prev_cred_id)
    .bind(e.slot)
    .bind(e.prev_slot)
    .bind(e.other_key)
    .bind(e.idle_secs)
    .bind(e.reason)
    .execute(conn)
    .await?;
    Ok(())
}

/// 新绑定（或休眠后回来、换了槽位的绑定）拿到 `cred_id` 上的槽位 `slot` 之后调用：这个槽位若
/// 原本属于某条**休眠**的绑定（选号里的 `free_session_slot` 把休眠绑定的槽位当空位），记一对
/// 事件——新键上 `slot_taken`、前任键上 `evicted`，都带前任的闲置时长——并把前任标成已丢槽位
/// （`slot_lost`）。
///
/// **按占用轮次去重**：一个槽位可能先后被几条对话占过又休眠，它们的行都还记着这个槽位号；
/// 已经被接手过的那些 `slot_lost = 1`，不会再被认成前任，重新拿到槽位时才清零（选号写绑定行
/// 时）。不靠事件表判——同秒的时间戳分不清先后，事件过了 7 天还会被清掉。仍占着的若不止一条
/// （标记上线前的存量行），事件记最近活跃的那条，其余一并标掉。`ttl_secs <= 0` 时绑定永不
/// 休眠、槽位不会被接手，直接跳过。
///
/// 调用方须在选号的串行化写事务（[`PgStore::begin_write`]）里调用：先查前任、再标记，是读后写。
pub(super) async fn note_slot_takeover(
    conn: &mut PgConnection,
    cred_id: i64,
    slot: i64,
    new_key: &str,
    ttl_secs: i64,
) -> Result<()> {
    if slot < 0 || ttl_secs <= 0 {
        return Ok(());
    }
    // 一条语句做完「找前任 + 把仍占着的都标掉」：UPDATE 的 RETURNING 只能拿到每行改之前的
    // 闲置时长（`last_seen_at` 没改），挑闲置最短（最近活跃）的那条记事件，同旧版。
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "UPDATE session_bindings SET slot_lost = 1 \
          WHERE cred_id = $1 AND slot = $2 AND session_key <> $3 AND slot_lost = 0 \
            AND last_seen_at < unixepoch() - $4 \
         RETURNING session_key, unixepoch() - last_seen_at",
    )
    .bind(cred_id)
    .bind(slot)
    .bind(new_key)
    .bind(ttl_secs)
    .fetch_all(&mut *conn)
    .await?;
    // 同闲置时长时按键名取最小的，保证确定性（旧版 `ORDER BY last_seen_at DESC LIMIT 1` 在
    // 平局时取哪条由 SQLite 决定）。
    let Some((old_key, idle)) =
        rows.into_iter().min_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)))
    else {
        return Ok(());
    };
    let at = |key, event, other_key| NewEvent {
        key,
        event,
        cred_id: Some(cred_id),
        slot: Some(slot),
        other_key: Some(other_key),
        idle_secs: Some(idle),
        ..Default::default()
    };
    log_event(&mut *conn, at(new_key, "slot_taken", &old_key)).await?;
    log_event(&mut *conn, at(&old_key, "evicted", new_key)).await
}

impl PgStore {
    /// 一条会话的历史事件，最近的在前。跨账号：改绑之后的事件记在新账号上，同样列出。
    pub async fn session_events(&self, session_key: &str) -> Result<Vec<SessionEvent>> {
        self.query_events("e.session_key = $1", args1(session_key)?).await
    }

    /// 某账号某槽位（也就是某个上游会话 id）的历史：在这个槽位上发生的事件，以及从这个槽位
    /// 离开的改绑与换槽位，最近的在前。回答「这个会话 id 先后被哪些对话用过」。
    pub async fn slot_events(&self, cred_id: i64, slot: i64) -> Result<Vec<SessionEvent>> {
        self.query_events(
            "(e.cred_id = $1 AND e.slot = $2) OR (e.prev_cred_id = $1 AND e.prev_slot = $2)",
            args2(cred_id, slot)?,
        )
        .await
    }

    async fn query_events(&self, filter: &str, args: PgArguments) -> Result<Vec<SessionEvent>> {
        let sql = format!(
            "SELECT e.id, e.ts, e.session_key, e.event, e.cred_id, c.label, e.prev_cred_id, \
                    p.label, e.slot, e.prev_slot, e.other_key, e.idle_secs, e.reason \
               FROM session_binding_events e \
               LEFT JOIN credentials c ON c.id = e.cred_id \
               LEFT JOIN credentials p ON p.id = e.prev_cred_id \
              WHERE {filter} ORDER BY e.id DESC LIMIT {EVENTS_LIMIT}"
        );
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|r| {
                Ok(SessionEvent {
                    id: r.try_get(0)?,
                    ts: r.try_get(1)?,
                    session_key: r.try_get(2)?,
                    event: r.try_get(3)?,
                    cred_id: r.try_get(4)?,
                    cred_label: r.try_get(5)?,
                    prev_cred_id: r.try_get(6)?,
                    prev_cred_label: r.try_get(7)?,
                    slot: r.try_get(8)?,
                    prev_slot: r.try_get(9)?,
                    other_key: r.try_get(10)?,
                    idle_secs: r.try_get(11)?,
                    reason: r.try_get(12)?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::super::*;
    use super::super::select::tests::{age_session_binding, soft_session, soft_store};

    /// 同一槽位上已被接手过的旧绑定（`slot_lost = 1`）不再被认成前任：当前持有者 B 解绑、槽位
    /// 分给 C 时，C 只记 `bound`，不能被记成「接手 A」。（rusqlite 版
    /// `upgrade_initializes_slot_owners` 里不涉及补列迁移的那一半：这里直接按迁移后的状态造行。）
    #[sqlx::test]
    async fn slot_takeover_skips_bindings_that_already_lost_the_slot(pool: PgPool) {
        const KA: &str = "lb:v2:sid:A";
        const KB: &str = "lb:v2:sid:B";
        const KC: &str = "lb:v2:sid:C";
        const KP: &str = "lb:v2:sid:P";
        let (store, ids) = soft_store(pool, &["a"]).await;
        let a = ids[0];
        for (key, idle, slot, lost) in [(KA, 7200, 0, 1), (KB, 600, 0, 0), (KP, 9000, -1, 0)] {
            sqlx::query(
                "INSERT INTO session_bindings (session_key, cred_id, slot, slot_lost, last_seen_at) \
                 VALUES ($1, $2, $3, $4, unixepoch() - $5)",
            )
            .bind(key)
            .bind(a)
            .bind(slot)
            .bind(lost)
            .bind(idle)
            .execute(&store.pool)
            .await
            .unwrap();
        }
        assert!(store.unbind_session(a, KB).await.unwrap());
        assert_eq!(store.select_with_slot(soft_session(KC)).await.unwrap().1, Some(0));
        let events: Vec<String> =
            store.session_events(KC).await.unwrap().into_iter().map(|e| e.event).collect();
        assert_eq!(events, ["bound"], "C 不该被记成接手 A");
        assert!(store.session_events(KA).await.unwrap().is_empty());
    }

    /// 会话绑定的历史事件：新建、接手休眠绑定的槽位（新键 `slot_taken`、前任 `evicted`）、休眠后
    /// 恢复（换了槽位）、上游失败换号改绑、手动解绑、停用账号、过期清理，以及槽位历史与 7 天清理。
    #[sqlx::test]
    async fn session_binding_events_follow_state_changes(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let kinds = async |key: &str| -> Vec<String> {
            store.session_events(key).await.unwrap().into_iter().rev().map(|e| e.event).collect()
        };

        // 新建；同键续用不再记。
        store.select_for_device(soft_session("s1")).await.unwrap();
        store.select_for_device(soft_session("s1")).await.unwrap();
        assert_eq!(kinds("s1").await, ["bound"]);
        let e = &store.session_events("s1").await.unwrap()[0];
        assert_eq!((e.cred_id, e.slot, e.cred_label.as_deref()), (Some(a), Some(0), Some("a")));

        // s1 休眠，s2 接手槽位 0；s1 回来换到槽位 1。
        age_session_binding(&store, "s1", 600).await;
        store.select_for_device(soft_session("s2")).await.unwrap();
        assert_eq!(kinds("s2").await, ["bound", "slot_taken"]);
        let taken = &store.session_events("s2").await.unwrap()[0];
        assert_eq!(taken.other_key.as_deref(), Some("s1"));
        assert!(taken.idle_secs.is_some_and(|s| s >= 600), "{taken:?}");
        store.select_for_device(soft_session("s1")).await.unwrap();
        assert_eq!(kinds("s1").await, ["bound", "evicted", "resumed"]);
        let resumed = &store.session_events("s1").await.unwrap()[0];
        assert_eq!((resumed.prev_slot, resumed.slot), (Some(0), Some(1)));
        // 槽位 0 的历史：s1 建、s2 建并接手、s1 被接手、s1 从这里离开。
        let slot0: Vec<(String, String)> = store
            .slot_events(a, 0)
            .await
            .unwrap()
            .into_iter()
            .rev()
            .map(|e| (e.session_key, e.event))
            .collect();
        assert_eq!(
            slot0,
            [
                ("s1".into(), "bound".into()),
                ("s2".into(), "bound".into()),
                ("s2".into(), "slot_taken".into()),
                ("s1".into(), "evicted".into()),
                ("s1".into(), "resumed".into()),
            ]
        );

        // 上游失败换号：原号在本轮已试过，改绑到 b，带原号、原槽位与原因。
        let tried = [a];
        let sel = Select { exclude: &tried, ..soft_session("s2") };
        assert_eq!(store.select_for_device(sel).await.unwrap().id, b);
        let rebound = &store.session_events("s2").await.unwrap()[0];
        assert_eq!(rebound.event, "rebound");
        assert_eq!(
            (rebound.prev_cred_id, rebound.prev_slot, rebound.cred_id, rebound.reason.as_deref()),
            (Some(a), Some(0), Some(b), Some("retried"))
        );

        // 手动解绑与停用账号各记一条解绑，带方式。
        assert!(store.unbind_session(b, "s2").await.unwrap());
        assert_eq!(store.session_events("s2").await.unwrap()[0].reason.as_deref(), Some("manual"));
        store.set_disabled(a, true).await.unwrap();
        let e = &store.session_events("s1").await.unwrap()[0];
        assert_eq!((e.event.as_str(), e.reason.as_deref()), ("unbound", Some("account_disabled")));

        // 过期清理记 expired；7 天前的事件被清掉。
        store.select_for_device(soft_session("s3")).await.unwrap();
        age_session_binding(&store, "s3", 7200).await;
        sqlx::query(
            "UPDATE session_binding_events SET ts = ts - 8 * 86400 WHERE session_key = 's1'",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        store.prune_expired_bindings().await.unwrap();
        assert_eq!(kinds("s3").await, ["bound", "expired"]);
        assert!(store.session_events("s1").await.unwrap().is_empty(), "超过 7 天的事件清掉");
    }

    /// 历史事件的几处边界：同一秒里槽位来回被接手（按占用轮次去重，不靠事件时间戳）、活跃中在
    /// 沿用来访 ID 与派生槽位之间切换（`reslotted`）、自动封停连带清掉的绑定记解绑。
    #[sqlx::test]
    async fn session_binding_events_edge_cases(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let count = async |key: &str, event: &str| {
            store.session_events(key).await.unwrap().iter().filter(|e| e.event == event).count()
        };

        // x 占槽位 0 后休眠，y 接手；y 休眠，x 回来又拿回 0（接手 y）；x 再休眠，z 接手。全在同一秒，
        // x 两次被接手都要记上，前任也不能认错。
        store.select_for_device(soft_session("x")).await.unwrap();
        age_session_binding(&store, "x", 600).await;
        store.select_for_device(soft_session("y")).await.unwrap();
        age_session_binding(&store, "y", 600).await;
        assert_eq!(store.select_with_slot(soft_session("x")).await.unwrap().1, Some(0));
        age_session_binding(&store, "x", 600).await;
        store.select_for_device(soft_session("z")).await.unwrap();
        assert_eq!(
            count("x", "evicted").await,
            2,
            "{:?}",
            store.session_events("x").await.unwrap()
        );
        assert_eq!(count("y", "evicted").await, 1);
        let z = &store.session_events("z").await.unwrap()[0];
        assert_eq!((z.event.as_str(), z.other_key.as_deref()), ("slot_taken", Some("x")));

        // 活跃中从派生槽位切到沿用来访 ID，再切回来：各记一条换槽位，带原槽位（p 落在空着的 b 上，
        // 槽位 0）。
        store.select_for_device(soft_session("p")).await.unwrap();
        let pass = Select { passthrough_session: true, ..soft_session("p") };
        assert_eq!(store.select_with_slot(pass).await.unwrap().1, None);
        let e = &store.session_events("p").await.unwrap()[0];
        assert_eq!((e.event.as_str(), e.prev_slot, e.slot), ("reslotted", Some(0), Some(-1)));
        store.select_for_device(soft_session("p")).await.unwrap();
        assert_eq!(count("p", "reslotted").await, 2);
        // 续用不换槽位不记。
        store.select_for_device(soft_session("p")).await.unwrap();
        assert_eq!(count("p", "reslotted").await, 2);

        // 自动封停：连带清掉的绑定记解绑。
        let bound_to_b = Select { exclude: &[a], ..soft_session("q") };
        assert_eq!(store.select_for_device(bound_to_b).await.unwrap().id, b);
        assert!(
            store
                .record_ban(b, &BanContext { reason: "x".into(), ..Default::default() })
                .await
                .unwrap()
        );
        let e = &store.session_events("q").await.unwrap()[0];
        assert_eq!((e.event.as_str(), e.reason.as_deref()), ("unbound", Some("account_banned")));
    }
}
