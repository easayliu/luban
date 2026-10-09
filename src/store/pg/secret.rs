//! 密钥核对（PG 版），对应 `store::secret` 里碰库的那两步。加解密本身（`seal` / `open` /
//! `token_fingerprint`）是纯函数，直接用 `store::secret` 的。
//!
//! SQLite 版另有「清空闲页里的旧明文」（`scrub_freed_pages`）：那是 SQLite 库文件特有的
//! 问题，PG 版没有对应物，不移植。

use anyhow::Result;
use sqlx::PgConnection;

use super::super::{SECRET_CHECK_KEY, SECRET_CHECK_PLAINTEXT, mismatch, open, seal};

/// 核对密钥：库里已有校验值就解开比对，并试解每一把接入 Key 的密文；解不开就拒绝启动——
/// 绝不能带着一把错的密钥跑起来，把每个号都当成 token 失效去停用。还没有校验值（新库）
/// 就用当前密钥写一份。
pub(super) async fn verify_secret_key(conn: &mut PgConnection) -> Result<()> {
    let check: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
        .bind(SECRET_CHECK_KEY)
        .fetch_optional(&mut *conn)
        .await?;
    match check {
        Some(check) => {
            if open(&check).ok().as_deref() != Some(SECRET_CHECK_PLAINTEXT) {
                return Err(mismatch("key check"));
            }
        }
        None => {
            sqlx::query("INSERT INTO settings (key, value) VALUES ($1, $2)")
                .bind(SECRET_CHECK_KEY)
                .bind(seal(SECRET_CHECK_PLAINTEXT))
                .execute(&mut *conn)
                .await?;
        }
    }
    let keys: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, key_sealed FROM api_keys").fetch_all(&mut *conn).await?;
    for (id, sealed) in keys {
        if open(&sealed).is_err() {
            return Err(mismatch(&format!("access key #{id}")));
        }
    }
    let creds: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, access_token, refresh_token FROM credentials")
            .fetch_all(&mut *conn)
            .await?;
    for (id, access, refresh) in creds {
        if open(&access).is_err() || open(&refresh).is_err() {
            return Err(mismatch(&format!("credential #{id}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::PgStore;
    use super::*;

    /// 新库打开时写下校验值；校验值被换成别的密钥加密的东西时拒绝打开。
    #[sqlx::test]
    async fn rejects_a_mismatched_key_check(pool: PgPool) {
        PgStore::for_test(pool.clone()).await;
        let stored: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
            .bind(SECRET_CHECK_KEY)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(open(&stored).unwrap(), SECRET_CHECK_PLAINTEXT);
        sqlx::query(
            "UPDATE settings SET value = 'enc1:AAAAAAAAAAAAAAAAAAAAAAAAAAAA' WHERE key = $1",
        )
        .bind(SECRET_CHECK_KEY)
        .execute(&pool)
        .await
        .unwrap();
        assert!(PgStore::open(pool).await.is_err());
    }
}
