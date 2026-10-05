//! 会话绑定的历史事件（`session_binding_events`）：新建、接手槽位、被接手、休眠后恢复、活跃中
//! 换槽位、改绑、解绑、过期清理。只在绑定**状态变化**时记，每轮续用不记——正常对话一般只有建绑定那一条。
//!
//! 事件跟着状态变化写在同一把锁里（选号持着 `conn`，解绑与清理同样），保留
//! [`SESSION_EVENT_RETENTION_SECS`]，由 [`CredentialStore::prune_expired_bindings`] 一并清。

use super::*;

/// 事件保留 7 天。
pub const SESSION_EVENT_RETENTION_SECS: i64 = 7 * 86_400;

/// 一次查询最多返回的事件条数（按时间倒序取最近的）。
const EVENTS_LIMIT: i64 = 100;

/// 一条待写入的事件。字段语义见建表处的注释（`schema`）。
#[derive(Default)]
pub(super) struct NewEvent<'a> {
    pub key: &'a str,
    pub event: &'static str,
    pub cred_id: Option<i64>,
    pub prev_cred_id: Option<i64>,
    pub slot: Option<i64>,
    pub prev_slot: Option<i64>,
    pub other_key: Option<&'a str>,
    pub idle_secs: Option<i64>,
    pub reason: Option<&'static str>,
}

pub(super) fn log_event(conn: &Connection, e: NewEvent<'_>) -> Result<()> {
    conn.execute(
        "INSERT INTO session_binding_events \
            (session_key, event, cred_id, prev_cred_id, slot, prev_slot, other_key, idle_secs, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            e.key,
            e.event,
            e.cred_id,
            e.prev_cred_id,
            e.slot,
            e.prev_slot,
            e.other_key,
            e.idle_secs,
            e.reason
        ],
    )?;
    Ok(())
}

/// 删绑定行**之前**调用：把 `WHERE` 命中的每一行记成一条 `event`（`unbound` / `expired`），
/// 方式或原因记在 `reason`。`filter` 是 `session_bindings` 上的条件，只由代码里的常量传入，
/// 参数按 `?1`、`?2`…… 跟在后面。
pub(super) fn log_removed(
    conn: &Connection,
    event: &'static str,
    reason: Option<&'static str>,
    filter: &str,
    args: impl rusqlite::Params,
) -> Result<usize> {
    // `reason` 拼成字面量而不是参数：参数位已经留给 `filter`，而它只会是代码里的常量。
    let reason = reason.map_or("NULL".to_string(), |r| format!("'{r}'"));
    Ok(conn.execute(
        &format!(
            "INSERT INTO session_binding_events \
                (session_key, event, cred_id, slot, idle_secs, reason) \
             SELECT session_key, '{event}', cred_id, slot, unixepoch() - last_seen_at, {reason} \
               FROM session_bindings WHERE {filter}"
        ),
        args,
    )?)
}

/// 新绑定（或休眠后回来、换了槽位的绑定）拿到 `cred_id` 上的槽位 `slot` 之后调用：这个槽位若
/// 原本属于某条**休眠**的绑定（[`super::select`] 里的 `free_session_slot` 把休眠绑定的槽位当
/// 空位），记一对事件——新键上 `slot_taken`、前任键上 `evicted`，都带前任的闲置时长——并把
/// 前任标成已丢槽位（`slot_lost`）。
///
/// **按占用轮次去重**：一个槽位可能先后被几条对话占过又休眠，它们的行都还记着这个槽位号；
/// 已经被接手过的那些 `slot_lost = 1`，不会再被认成前任，重新拿到槽位时才清零（选号写绑定行
/// 时）。不靠事件表判——同秒的时间戳分不清先后，事件过了 7 天还会被清掉。仍占着的若不止一条
/// （标记上线前的存量行），事件记最近活跃的那条，其余一并标掉。`ttl_secs <= 0` 时绑定永不
/// 休眠、槽位不会被接手，直接跳过。
pub(super) fn note_slot_takeover(
    conn: &Connection,
    cred_id: i64,
    slot: i64,
    new_key: &str,
    ttl_secs: i64,
) -> Result<()> {
    if slot < 0 || ttl_secs <= 0 {
        return Ok(());
    }
    let holder = "cred_id = ?1 AND slot = ?2 AND session_key <> ?3 AND slot_lost = 0 \
                  AND last_seen_at < unixepoch() - ?4";
    let prev: Option<(String, i64)> = conn
        .query_row(
            &format!(
                "SELECT session_key, unixepoch() - last_seen_at FROM session_bindings \
                  WHERE {holder} ORDER BY last_seen_at DESC LIMIT 1"
            ),
            params![cred_id, slot, new_key, ttl_secs],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((old_key, idle)) = prev else { return Ok(()) };
    conn.execute(
        &format!("UPDATE session_bindings SET slot_lost = 1 WHERE {holder}"),
        params![cred_id, slot, new_key, ttl_secs],
    )?;
    let at = |key, event, other_key| NewEvent {
        key,
        event,
        cred_id: Some(cred_id),
        slot: Some(slot),
        other_key: Some(other_key),
        idle_secs: Some(idle),
        ..Default::default()
    };
    log_event(conn, at(new_key, "slot_taken", &old_key))?;
    log_event(conn, at(&old_key, "evicted", new_key))
}

/// 一条历史事件（后台会话详情与槽位历史展示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionEvent {
    pub id: i64,
    /// 发生时间（Unix 秒）。
    pub ts: i64,
    pub session_key: String,
    /// `bound` / `slot_taken` / `evicted` / `resumed` / `reslotted` / `rebound` / `unbound` /
    /// `expired`。
    pub event: String,
    /// 当前（或新落）的账号与它的名称；账号已删时名称为 `None`。
    pub cred_id: Option<i64>,
    pub cred_label: Option<String>,
    /// 改绑前的账号（`rebound`），休眠后恢复时同 `cred_id`。
    pub prev_cred_id: Option<i64>,
    pub prev_cred_label: Option<String>,
    pub slot: Option<i64>,
    /// 改绑前 / 恢复前的槽位。
    pub prev_slot: Option<i64>,
    /// `slot_taken` 的前任会话键、`evicted` 的接手者会话键。
    pub other_key: Option<String>,
    /// 闲置时长（秒）：接手、被接手、解绑、过期时那条绑定已经多久没有请求。
    pub idle_secs: Option<i64>,
    /// `rebound` 的原因，`unbound` 的方式。
    pub reason: Option<String>,
}

impl CredentialStore {
    /// 一条会话的历史事件，最近的在前。跨账号：改绑之后的事件记在新账号上，同样列出。
    pub fn session_events(&self, session_key: &str) -> Result<Vec<SessionEvent>> {
        self.query_events("e.session_key = ?1", [session_key])
    }

    /// 某账号某槽位（也就是某个上游会话 id）的历史：在这个槽位上发生的事件，以及从这个槽位
    /// 离开的改绑与换槽位，最近的在前。回答「这个会话 id 先后被哪些对话用过」。
    pub fn slot_events(&self, cred_id: i64, slot: i64) -> Result<Vec<SessionEvent>> {
        self.query_events(
            "(e.cred_id = ?1 AND e.slot = ?2) OR (e.prev_cred_id = ?1 AND e.prev_slot = ?2)",
            params![cred_id, slot],
        )
    }

    fn query_events(&self, filter: &str, args: impl rusqlite::Params) -> Result<Vec<SessionEvent>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT e.id, e.ts, e.session_key, e.event, e.cred_id, c.label, e.prev_cred_id, \
                    p.label, e.slot, e.prev_slot, e.other_key, e.idle_secs, e.reason \
               FROM session_binding_events e \
               LEFT JOIN credentials c ON c.id = e.cred_id \
               LEFT JOIN credentials p ON p.id = e.prev_cred_id \
              WHERE {filter} ORDER BY e.id DESC LIMIT {EVENTS_LIMIT}"
        ))?;
        let rows = stmt
            .query_map(args, |r| {
                Ok(SessionEvent {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    session_key: r.get(2)?,
                    event: r.get(3)?,
                    cred_id: r.get(4)?,
                    cred_label: r.get(5)?,
                    prev_cred_id: r.get(6)?,
                    prev_cred_label: r.get(7)?,
                    slot: r.get(8)?,
                    prev_slot: r.get(9)?,
                    other_key: r.get(10)?,
                    idle_secs: r.get(11)?,
                    reason: r.get(12)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
}
