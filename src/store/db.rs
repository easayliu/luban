//! PostgreSQL 连接池与建表。
//!
//! 表结构在仓库根目录的 `migrations/` 下，按文件名顺序执行，由 [`MIGRATOR`] 编进二进制。
//! 改表结构就加一个新的 `NNNN_*.sql`，**已发布的文件不能再改**：sqlx 记着每个文件的校验和，
//! 改过的文件在已经跑过它的库上会直接报错、拒绝启动。

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// `migrations/` 下的全部迁移。
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// 连接池上限。转发路径每条请求落一次流水、选号要读几次，后台聚合查询另占几条；
/// PostgreSQL 默认 max_connections 是 100，留足余量给运维连接与其它进程。
const MAX_CONNECTIONS: u32 = 16;

/// 取连接的等待上限：库挂了或连接全被占着时，请求在这里报错，而不是无限挂起。
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// 数据目录：存密钥文件（见 [`super::init_key`]）。默认 `~/.luban`；`LUBAN_HOME` 可覆盖。
pub fn data_dir() -> Result<std::path::PathBuf> {
    match std::env::var_os("LUBAN_HOME") {
        Some(dir) => Ok(std::path::PathBuf::from(dir)),
        None => Ok(dirs::home_dir()
            .context("could not determine the user home directory")?
            .join(".luban")),
    }
}

/// 打开存储：载入密钥、连 `url` 指向的库并迁到最新、补齐恒存在的行。
pub async fn open(url: &str) -> Result<super::CredentialStore> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create directory: {}", dir.display()))?;
    super::init_key(&dir)?;
    let store = super::CredentialStore::open(connect(url).await?).await?;
    warn_on_legacy_sqlite(&dir, &store).await;
    Ok(store)
}

/// 数据目录里还躺着 SQLite 时代的 `luban.db`、而库里一个号都没有：多半是刚升级、旧数据还没
/// 搬。库不会自动迁过来，提示一下怎么搬。
async fn warn_on_legacy_sqlite(dir: &std::path::Path, store: &super::CredentialStore) {
    let legacy = dir.join("luban.db");
    if !legacy.exists() {
        return;
    }
    if matches!(store.list().await, Ok(list) if list.is_empty()) {
        tracing::warn!(
            path = %legacy.display(),
            "found a SQLite database from an older luban, but PostgreSQL has no accounts yet; \
             it is not migrated automatically: run the previous luban version, export from the \
             console (Settings > Export), then import the file here"
        );
    }
}

/// 连库地址里的主机与库名（不含账号密码），给日志与 `status` 用。查询串整段去掉：密码也能
/// 写在 `?password=` 里。
pub fn describe_url(url: &str) -> &str {
    let url = url.split_once('?').map_or(url, |(base, _)| base);
    match url.rsplit_once('@') {
        Some((_, host)) => host,
        None => url.split("://").nth(1).unwrap_or(url),
    }
}

/// 连上 `url` 指向的库并把表结构迁到最新。
///
/// 多个进程同时启动时，sqlx 的迁移会先拿 advisory lock，同一时刻只有一个在执行迁移。
pub async fn connect(url: &str) -> Result<PgPool> {
    // NOTICE 不要：sqlx 会按 info 打出来，每次启动都有一句迁移表「already exists, skipping」。
    let options = url
        .parse::<PgConnectOptions>()
        .context("invalid PostgreSQL connection URL")?
        .options([("client_min_messages", "warning")]);
    let pool = PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await
        .context("failed to connect to PostgreSQL")?;
    migrate(&pool).await?;
    Ok(pool)
}

/// 把表结构迁到最新，并在日志里写清这次跑了哪几条：一条都没跑也记一行当前版本，升级后一眼
/// 看得出迁移到底执行了没有。
async fn migrate(pool: &PgPool) -> Result<()> {
    let before = applied_versions(pool).await?;
    MIGRATOR.run(pool).await.context("failed to migrate the database schema")?;
    let pending: Vec<_> = MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration() && !before.contains(&m.version))
        .collect();
    for m in &pending {
        tracing::info!(version = m.version, description = %m.description, "applied database migration");
    }
    let latest = MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0);
    if pending.is_empty() {
        tracing::info!(version = latest, "database schema is up to date, no migrations to apply");
    } else {
        tracing::info!(applied = pending.len(), version = latest, "database schema migrated");
    }
    Ok(())
}

/// 库里已经跑成功的迁移版本。全新的库还没有 sqlx 的记录表，算作一条都没跑。
async fn applied_versions(pool: &PgPool) -> Result<std::collections::HashSet<i64>> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(pool)
        .await?;
    if !exists {
        return Ok(Default::default());
    }
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
            .fetch_all(pool)
            .await?;
    Ok(versions.into_iter().collect())
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
