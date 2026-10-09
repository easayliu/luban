//! PostgreSQL 版的存储层（移植中）。
//!
//! 与 rusqlite 版的 [`CredentialStore`] 并存：各模块按 `src/store/<模块>.rs` 一一对应地移植到
//! `src/store/pg/<模块>.rs`，方法签名不变，只是改成 `async`。类型、常量与纯内存的辅助
//! （限流窗口、rollup 编解码等）直接复用 `store` 里现成的。全部移植完再删掉 rusqlite 版，
//! 把 [`PgStore`] 改名回 `CredentialStore`。
//!
//! ## 写事务的串行化
//!
//! rusqlite 版只有一条写连接（`Mutex<Connection>`），所有写入天然串行，许多「先读、再判断、
//! 再写」的逻辑（选号时找空槽位、设备上限、按旧值压档……）都靠这一点才不会并发错乱。PG 连接
//! 池里的事务是真并发的，这类事务一律用 [`PgStore::begin_write`] 开：它在事务开头拿一把全局
//! advisory 锁，效果等同于以前那把写锁，提交或回滚时自动释放。只做一组原子写、不依赖读到的
//! 旧值的，用普通的 `self.pool.begin()` 即可。

// 移植期间新旧两套并存，新代码还没有调用方。
#![allow(dead_code)]

use std::collections::HashMap;

use anyhow::{Context, Result};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Postgres, Row, Transaction};

use super::{
    Credential, DEFAULT_DEVICE_LIMIT, DEFAULT_DEVICE_LIMIT_VALUE, DEFAULT_SESSION_LIMIT,
    DEFAULT_SESSION_LIMIT_VALUE, RateLimitCooldown, RateWindow,
};

mod bans;
mod billing;
mod bindings;
mod credential;
#[cfg(test)]
mod cross_tests;
mod flags;
mod groups;
mod learned;
mod limits;
mod portable;
mod proxies;
mod quota;
mod refresh;
mod rollup;
mod secret;
mod select;
mod session_events;
mod settings;
mod stats;
mod usage;
mod users;

/// 凭证存储（PostgreSQL）。进程内状态（设置缓存、各限流窗口、冷却表、刷新锁）与 rusqlite 版
/// 一模一样，含义见 [`CredentialStore`] 的同名字段。
pub struct PgStore {
    pub(crate) pool: PgPool,
    refresh_locks: parking_lot::Mutex<HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    bare_rate: RateWindow,
    rpm_rate: RateWindow,
    device_rate: RateWindow<String>,
    session_rate: RateWindow<String>,
    cooldown: RateLimitCooldown,
    settings: parking_lot::RwLock<HashMap<String, String>>,
}

/// [`PgStore::begin_write`] 拿的 advisory 锁的键（任意常量，ASCII "lubanwr\0"）。
const WRITE_LOCK_KEY: i64 = 0x6c75_6261_6e77_7200;

impl PgStore {
    /// 在已迁移好的库上打开存储：补齐恒存在的行（admin、默认分组、两项全局默认上限），
    /// 核对密钥，读入设置缓存。
    ///
    /// 调用前要先 [`super::init_key`]（测试除外，测试用固定密钥）。
    pub async fn open(pool: PgPool) -> Result<Self> {
        seed(&pool).await?;
        let settings = load_settings(&pool).await?;
        Ok(Self {
            pool,
            refresh_locks: parking_lot::Mutex::new(HashMap::new()),
            bare_rate: RateWindow::default(),
            rpm_rate: RateWindow::default(),
            device_rate: RateWindow::default(),
            session_rate: RateWindow::default(),
            cooldown: RateLimitCooldown::default(),
            settings: parking_lot::RwLock::new(settings),
        })
    }

    /// 测试用：在 `#[sqlx::test]` 给的临时库上打开。
    #[cfg(test)]
    pub(crate) async fn for_test(pool: PgPool) -> Self {
        Self::open(pool).await.expect("failed to open the test store")
    }

    /// 开一个**串行化的**写事务，见模块文档。
    pub(crate) async fn begin_write(&self) -> Result<Transaction<'static, Postgres>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(WRITE_LOCK_KEY)
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    /// 取该凭证的刷新锁（不存在则创建），见 [`CredentialStore::refresh_lock`]。
    pub(crate) fn refresh_lock(&self, cred_id: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks.lock().entry(cred_id).or_default().clone()
    }

    /// 执行一条写语句，回是否影响了行。
    async fn update_one(
        &self,
        q: sqlx::query::Query<'_, Postgres, sqlx::postgres::PgArguments>,
    ) -> Result<bool> {
        Ok(q.execute(&self.pool).await?.rows_affected() > 0)
    }
}

/// 补齐恒存在的行。在串行化事务里做：几个进程同时启动时不会各插一份。
async fn seed(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(WRITE_LOCK_KEY).execute(&mut *tx).await?;
    // admin 行恒存在（还没设密码时 password_hash 为空），号与出口代理的 owner 才总能落到
    // 一个确定的人身上。
    sqlx::query(
        "INSERT INTO users (username, role) SELECT 'admin', 'admin' \
          WHERE NOT EXISTS (SELECT 1 FROM users WHERE role = 'admin')",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO pool_groups (name, is_default) SELECT $1, 1 \
          WHERE NOT EXISTS (SELECT 1 FROM pool_groups WHERE is_default = 1)",
    )
    .bind(super::DEFAULT_GROUP_NAME)
    .execute(&mut *tx)
    .await?;
    // 全局默认上限落一行，让它在控制台里看得见、改得动；判定不变（设置缺失时本来就回落到
    // 同一个默认值）。显式写入的值（含 0 = 不限）不动。
    for (key, value) in [
        (DEFAULT_DEVICE_LIMIT, DEFAULT_DEVICE_LIMIT_VALUE),
        (DEFAULT_SESSION_LIMIT, DEFAULT_SESSION_LIMIT_VALUE),
    ] {
        sqlx::query("INSERT INTO settings (key, value) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(key)
            .bind(value.to_string())
            .execute(&mut *tx)
            .await?;
    }
    secret::verify_secret_key(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}

/// `settings` 全表读进内存，见 [`CredentialStore`] 的 `settings` 字段。
pub(super) async fn load_settings(pool: &PgPool) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM settings").fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

/// 把一行 `SELECT {COLS}`（列清单就是 [`super::COLS`]）读成 [`Credential`]，token 列顺手解密。
pub(super) fn row_to_cred(row: &PgRow) -> Result<Credential> {
    let open = |i: usize| -> Result<String> {
        super::open(&row.try_get::<String, _>(i)?)
            .with_context(|| format!("failed to decrypt a stored token (column {i})"))
    };
    Ok(Credential {
        id: row.try_get(0)?,
        label: row.try_get(1)?,
        tier: row.try_get(2)?,
        access_token: open(3)?,
        refresh_token: open(4)?,
        expires_at: row.try_get::<i64, _>(5)? as u64,
        priority: row.try_get(6)?,
        disabled: row.try_get::<i64, _>(7)? != 0,
        created_at: row.try_get::<i64, _>(8)? as u64,
        updated_at: row.try_get::<i64, _>(9)? as u64,
        device_limit: row.try_get(10)?,
        ban_reason: row.try_get(11)?,
        account_uuid: row.try_get(12)?,
        resume_at: row.try_get::<Option<i64>, _>(13)?.map(|t| t as u64),
        org_type: row.try_get(14)?,
        proxy: row.try_get(15)?,
        rpm_limit: row.try_get(16)?,
        rate_limit_tier: row.try_get(17)?,
        org_uuid: row.try_get(18)?,
        subscription_created_at: row.try_get(19)?,
        quota_pause_pct: row.try_get(20)?,
        quota_pause_pct_7d: row.try_get(21)?,
        session_limit: row.try_get(22)?,
        org_name: row.try_get(23)?,
        seat_tier: row.try_get(24)?,
        subscription_status: row.try_get(25)?,
        extra_usage_enabled: row.try_get::<Option<i64>, _>(26)?.map(|v| v != 0),
        owner_id: row.try_get(27)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 打开时补齐 admin、默认分组与两项默认上限；重复打开不会多插。
    #[sqlx::test]
    async fn open_seeds_required_rows_once(pool: PgPool) {
        PgStore::for_test(pool.clone()).await;
        let store = PgStore::for_test(pool).await;
        let admins: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin'")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(admins, 1);
        let defaults: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pool_groups WHERE is_default = 1")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(defaults, 1);
        assert_eq!(
            store.settings.read().get(DEFAULT_DEVICE_LIMIT).map(String::as_str),
            Some(DEFAULT_DEVICE_LIMIT_VALUE.to_string().as_str())
        );
    }

    /// 串行化写事务：第二个事务要等第一个提交后才拿得到锁。
    #[sqlx::test]
    async fn write_transactions_are_serialized(pool: PgPool) {
        let store = std::sync::Arc::new(PgStore::for_test(pool).await);
        let tx = store.begin_write().await.unwrap();
        let s = store.clone();
        let waiter = tokio::spawn(async move { s.begin_write().await.map(|_| ()) });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!waiter.is_finished());
        tx.commit().await.unwrap();
        waiter.await.unwrap().unwrap();
    }
}
