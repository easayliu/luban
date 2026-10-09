//! 会话绑定的历史事件（PG 版），对应 `store::session_events`。

use anyhow::Result;
use sqlx::PgConnection;
use sqlx::postgres::PgArguments;

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
