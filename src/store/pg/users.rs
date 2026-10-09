//! 成员与登录会话（PG 版），对应 `store::users`。

use anyhow::Result;
use sqlx::PgConnection;

/// 这个人存在、且能当号主（访客不能）。给上号 / 转交号时校验 owner 用。
pub(super) async fn owner_exists(conn: &mut PgConnection, owner_id: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND role <> 'viewer')")
        .bind(owner_id)
        .fetch_one(conn)
        .await?)
}
