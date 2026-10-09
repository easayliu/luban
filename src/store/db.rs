//! PostgreSQL 连接池与建表。
//!
//! 表结构在仓库根目录的 `migrations/` 下，按文件名顺序执行，由 [`MIGRATOR`] 编进二进制。
//! 改表结构就加一个新的 `NNNN_*.sql`，**已发布的文件不能再改**：sqlx 记着每个文件的校验和，
//! 改过的文件在已经跑过它的库上会直接报错、拒绝启动。

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;

/// 连库地址的环境变量。
pub const DATABASE_URL_ENV: &str = "LUBAN_DATABASE_URL";

/// `migrations/` 下的全部迁移。
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// 连接池上限。转发路径每条请求落一次流水、选号要读几次，后台聚合查询另占几条；
/// PostgreSQL 默认 max_connections 是 100，留足余量给运维连接与其它进程。
const MAX_CONNECTIONS: u32 = 16;

/// 取连接的等待上限：库挂了或连接全被占着时，请求在这里报错，而不是无限挂起。
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// 读 [`DATABASE_URL_ENV`]，连上库并把表结构迁到最新。
pub async fn connect_from_env() -> Result<PgPool> {
    let url = std::env::var(DATABASE_URL_ENV).with_context(|| {
        format!("{DATABASE_URL_ENV} is not set (e.g. postgres://user:password@localhost/luban)")
    })?;
    connect(&url).await
}

/// 连上 `url` 指向的库并把表结构迁到最新。
///
/// 多个进程同时启动时，sqlx 的迁移会先拿 advisory lock，同一时刻只有一个在执行迁移。
pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .connect(url)
        .await
        .context("failed to connect to PostgreSQL")?;
    MIGRATOR.run(&pool).await.context("failed to migrate the database schema")?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    /// 迁移在全新库上能跑通，两个默认值触发器生效：不带 owner 插入的号挂到 admin 名下，
    /// 并落进默认分组。
    #[sqlx::test]
    async fn schema_defaults_owner_and_group(pool: PgPool) {
        let admin: i64 = sqlx::query_scalar(
            "INSERT INTO users (username, role) VALUES ('admin', 'admin') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let group: i64 = sqlx::query_scalar(
            "INSERT INTO pool_groups (name, is_default) VALUES ('Default', 1) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let (cred, owner): (i64, Option<i64>) = sqlx::query_as(
            "INSERT INTO credentials (access_token, refresh_token, expires_at) \
             VALUES ('a', 'r', 0) RETURNING id, owner_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(owner, Some(admin));
        let groups: Vec<i64> =
            sqlx::query_scalar("SELECT group_id FROM credential_groups WHERE cred_id = $1")
                .bind(cred)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(groups, vec![group]);
    }

    /// 用户名、分组名不区分大小写唯一（SQLite 时代是 COLLATE NOCASE）。
    #[sqlx::test]
    async fn names_are_case_insensitive_unique(pool: PgPool) {
        sqlx::query("INSERT INTO users (username, role) VALUES ('Alice', 'user')")
            .execute(&pool)
            .await
            .unwrap();
        let dup = sqlx::query("INSERT INTO users (username, role) VALUES ('alice', 'user')")
            .execute(&pool)
            .await;
        assert!(dup.is_err());
        sqlx::query("INSERT INTO pool_groups (name) VALUES ('Team')").execute(&pool).await.unwrap();
        let dup =
            sqlx::query("INSERT INTO pool_groups (name) VALUES ('TEAM')").execute(&pool).await;
        assert!(dup.is_err());
    }
}
