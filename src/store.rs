//! 多凭证的持久化层（PostgreSQL）。
//!
//! [`CredentialStore`] 的方法按主题分在各子模块里，每个子模块连同它用到的类型、常量与纯内存的
//! 辅助放在一起。表结构在仓库根目录的 `migrations/` 下，连库与迁移见 [`db`]。
//!
//! ## 写事务的串行化
//!
//! rusqlite 版只有一条写连接（`Mutex<Connection>`），所有写入天然串行，许多「先读、再判断、
//! 再写」的逻辑（选号时找空槽位、设备上限、按旧值压档……）都靠这一点才不会并发错乱。PG 连接
//! 池里的事务是真并发的，这类事务一律用 [`CredentialStore::begin_write`] 开：它在事务开头拿一把全局
//! advisory 锁，效果等同于以前那把写锁，提交或回滚时自动释放。只做一组原子写、不依赖读到的
//! 旧值的，用普通的 `self.pool.begin()` 即可。
//!
//! 进程内先排一把 [`tokio::sync::Mutex`]，排到了才去池里取连接、开事务：只靠 advisory 锁的话，
//! 排队的事务各自先占着一条连接卡在锁上，持锁的那笔一慢（删号级联），
//! 连接池就被排队者占满，转发路径上的鉴权与流水写入全都取不到连接。advisory 锁留着，
//! 兜同一个库被多个进程共用的情况。

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Postgres, Row, Transaction};

use crate::credentials::{Credential, PRIORITY_DEFAULT, PRIORITY_MAX, PRIORITY_MIN};

mod bans;
mod billing;
mod bindings;
mod credential;
pub mod db;
mod flags;
mod groups;
mod learned;
mod limits;
mod portable;
mod provision;
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

#[cfg(test)]
mod tests;

pub use bans::*;
pub use billing::{BillingDim, BillingFilter, BillingRow};
pub use bindings::*;
pub use credential::*;
pub use flags::*;
use groups::KeyAccessCache;
pub use groups::*;
pub use learned::*;
pub use limits::*;
pub use portable::*;
pub use provision::*;
pub use proxies::*;
pub use quota::*;
pub use refresh::*;
pub use rollup::series_grid;
use rollup::*;
pub use secret::init_key;
use secret::*;
pub use select::*;
use session_events::*;
pub use session_events::{SESSION_EVENT_RETENTION_SECS, SessionEvent};
pub use settings::*;
pub use stats::*;
pub use usage::*;
pub use users::*;

/// 查询列顺序，与 [`row_to_cred`] 一一对应。
const COLS: &str = "id, label, tier, access_token, refresh_token, expires_at, priority, disabled, \
     created_at, updated_at, device_limit, ban_reason, account_uuid, resume_at, org_type, proxy, \
     rpm_limit, rate_limit_tier, org_uuid, subscription_created_at, quota_pause_pct, \
     quota_pause_pct_7d, session_limit, org_name, seat_tier, subscription_status, \
     extra_usage_enabled, owner_id";

/// 凭证存储（PostgreSQL）。
pub struct CredentialStore {
    pub(crate) pool: PgPool,
    /// 每凭证一把刷新锁，串行化 token 刷新，见 [`valid_access_token_for_device`]。
    /// 上游刷新会**轮换 refresh_token**：并发刷新时后完成的那次会把已被作废的 token 写回库，
    /// 该凭证之后所有刷新都 `invalid_grant`，等于账号被自己废掉。
    refresh_locks: parking_lot::Mutex<HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// 裸请求的每凭证限流窗口（进程内），见 [`RateWindow`] 与 [`CredentialStore::bare_rate_limit`]。
    bare_rate: RateWindow,
    /// 每账号 RPM 的限流窗口（进程内，窗口固定 [`RPM_WINDOW_SECS`]），
    /// 见 [`CredentialStore::default_rpm_limit`]。
    ///
    /// 与 [`Self::bare_rate`] 用同一种计数器、但**各算各的**：那个只卡没有设备身份的流量，
    /// 这个卡该账号的全部转发。两者都配了的话一条裸请求要同时过两道窗口。
    rpm_rate: RateWindow,
    /// 每**设备** RPM 的限流窗口（进程内，窗口同 [`RPM_WINDOW_SECS`]），
    /// 见 [`CredentialStore::take_device_rpm_slot`]。
    ///
    /// 键是客户端自报的 `device_id`（不是伪装后那个：要限的是发请求的那台机器）。上面两个
    /// 窗口都按账号分桶，管的是「一个号别被打爆」；这个按设备分桶，管的是「一台机器别把
    /// 同账号下其他设备的额度挤没」——账号 RPM 打满时，安分的设备和刷疯了的那台一起被拒。
    device_rate: RateWindow<String>,
    /// 每**会话** RPM 的限流窗口（进程内，窗口同 [`RPM_WINDOW_SECS`]），
    /// 见 [`CredentialStore::take_session_rpm_slot`]。
    ///
    /// 键是客户端自报的会话 id（`X-Claude-Code-Session-Id` 头，或 `metadata.user_id` 里的
    /// session 段，两处官方逐字相同）。与 [`Self::device_rate`] 是**同一件事的两个粒度**：
    /// 一台机器上开三个 CC 窗口，真实并发是三份对话的并发，按设备一刀切会让它们互相挤额度；
    /// 按会话分桶才对得上负载的来源。
    ///
    /// 但它**替代不了**设备那道闸，两道要一起配：会话 id 轮换是免费的（`/clear`、开新窗口、
    /// 重启都换一个新的，立刻是个满血的桶），而设备 id 轮换要付代价（改绑凭证、连累 thinking
    /// 签名、吃 `device_limit` 名额）。故会话闸给的是贴合真实并发的细粒度节流，设备闸兜的是
    /// 「这台机器总量别失控」——后者的阈值该给到前者的几倍，见 [`SESSION_RPM_LIMIT`]。
    session_rate: RateWindow<String>,
    /// 被上游 429 过的凭证的冷却表（进程内），见 [`RateLimitCooldown`]。
    cooldown: RateLimitCooldown,
    /// `settings` 全表的内存镜像，见 [`CredentialStore::get_setting`]。
    ///
    /// **每条转发请求要读 8 项设置**（接入 key、设备身份校验、6 个转发形态开关、重试次数、
    /// 绑定 TTL、设备上限、裸请求限流两项），逐项查库就是每请求 8 次往返。设置项极少变动，
    /// 缓存住之后这些查询直接归零。
    ///
    /// 写路径只有 [`CredentialStore::set_setting`]/[`CredentialStore::delete_setting`] 两处，
    /// 都是先落库再更新缓存，故进程内不会漂移。**多进程共享同一个库时会读到陈旧值**——
    /// luban 是单进程本地代理，没有这个场景（同 [`RateWindow`] 的取舍）。
    settings: parking_lot::RwLock<HashMap<String, String>>,
    /// 交给后台任务、还没落库的写入笔数，见 [`CredentialStore::spawn_write`]。
    pending_writes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// 串行化写事务的进程内那把锁，见模块文档与 [`CredentialStore::begin_write`]。
    write_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// [`CredentialStore::spawn_write`] 同时在跑的笔数上限，见 [`BACKGROUND_WRITE_SLOTS`]。
    background_slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// 账号列表用的额度快照缓存（算出的时刻, 结果），见 [`CredentialStore::latest_quotas_cached`]。
    quota_cache: Mutex<Option<(Instant, HashMap<i64, QuotaSnapshot>)>>,
    /// 接入 Key 认身份的缓存，见 [`KeyAccessCache`]。
    key_cache: std::sync::Arc<Mutex<KeyAccessCache>>,
    /// 已经落进 `tool_sets` 的工具集 sha，写流水时省掉重复的 upsert，见 `usage::split_tool_names`。
    known_tool_sets: Mutex<std::collections::HashSet<String>>,
}

/// 后台写入（流水、学到的规则）同时占用的连接数上限。
///
/// 一波请求同时结束时，每条都交出一笔流水写入；不设上限的话它们一起去池里取连接，把
/// [`db`] 里那 16 条占满，选号、鉴权这些转发路径上的查询取不到连接，5 秒后报错——流水自己
/// 也在取连接超时后整笔丢掉。在进程内排队就不会超时，也给前台留出连接。
const BACKGROUND_WRITE_SLOTS: usize = 6;

/// [`CredentialStore::begin_write`] 开的事务：连同进程内写锁一起持有，提交或丢弃时一并释放。
///
/// 解引用到 [`sqlx::PgConnection`]，用法同 `Transaction`：`&mut *tx` 当执行器，`&mut tx` 传给
/// 收 `&mut PgConnection` 的辅助函数。
pub(crate) struct WriteTx {
    tx: Transaction<'static, Postgres>,
    // 字段按声明顺序析构：事务先回滚、归还连接，再放锁。
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl WriteTx {
    pub(crate) async fn commit(self) -> Result<()> {
        Ok(self.tx.commit().await?)
    }
}

impl std::ops::Deref for WriteTx {
    type Target = sqlx::PgConnection;
    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

impl std::ops::DerefMut for WriteTx {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tx
    }
}

/// 一笔在途的后台写入，见 [`CredentialStore::spawn_write`]/[`CredentialStore::run_tracked`]。
/// 建时计数加一、析构时减一：任务 panic 也不会把计数卡住，害关停白等到超时。
struct PendingWrite(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl PendingWrite {
    fn new(counter: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(counter.clone())
    }
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// [`CredentialStore::begin_write`] 拿的 advisory 锁的键（任意常量，ASCII "lubanwr\0"）。
const WRITE_LOCK_KEY: i64 = 0x6c75_6261_6e77_7200;

impl CredentialStore {
    /// 在已迁移好的库上打开存储：补齐恒存在的行（admin、默认分组、两项全局默认上限），
    /// 核对密钥，读入设置缓存。
    ///
    /// 调用前要先 [`init_key`]（测试除外，测试用固定密钥）。
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
            pending_writes: Default::default(),
            write_lock: Default::default(),
            background_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(
                BACKGROUND_WRITE_SLOTS,
            )),
            quota_cache: Mutex::new(None),
            key_cache: Default::default(),
            known_tool_sets: Mutex::default(),
        })
    }

    /// 测试用：在 `#[sqlx::test]` 给的临时库上打开。
    #[cfg(test)]
    pub(crate) async fn for_test(pool: PgPool) -> Self {
        Self::open(pool).await.expect("failed to open the test store")
    }

    /// 开一个**串行化的**写事务，见模块文档。
    pub(crate) async fn begin_write(&self) -> Result<WriteTx> {
        let guard = self.write_lock.clone().lock_owned().await;
        let tx = begin_locked(&self.pool).await?;
        Ok(WriteTx { tx, _guard: guard })
    }

    /// 取该凭证的刷新锁（不存在则创建），见 [`CredentialStore::refresh_lock`]。
    pub(crate) fn refresh_lock(&self, cred_id: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks.lock().entry(cred_id).or_default().clone()
    }

    /// 把一笔落库交给后台任务。给没法 await 的调用方用（`Drop` 里写流水、记学到的规则）。
    ///
    /// 在途的笔数记在这个 store 上，关停时 [`Self::drain_writes`] 等它们落完——`tokio::spawn`
    /// 出去的任务在运行时退出时直接被丢弃，不等的话最后几秒的流水与账单就没了。拿不到运行时
    /// 句柄（单元测试里直接 drop）就写不了，记一条警告。
    pub fn spawn_write(
        &self,
        what: &'static str,
        write: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(what, "no tokio runtime; dropping the write");
            return;
        };
        let pending = PendingWrite::new(&self.pending_writes);
        let slots = self.background_slots.clone();
        handle.spawn(async move {
            let _pending = pending;
            // 信号量不会被关闭，`acquire_owned` 不会失败。
            let _slot = slots.acquire_owned().await;
            if let Err(e) = write.await {
                tracing::warn!(what, error = %format!("{e:#}"), "background store write failed");
            }
        });
    }

    /// 跑一笔**不受调用方取消影响**的写入：放进独立任务、等它的结果。
    ///
    /// 转发路径上的状态写入（封号、限流停号、套餐不含的模型……）直接 await 的话，客户端一断开
    /// handler 的 future 就被丢掉，写到一半的事务跟着回滚：号没停到池外，下一条请求又选中它、
    /// 再吃一发同样的拒绝。调用方被取消时任务照样跑完，也算进 [`Self::drain_writes`] 要等的笔数。
    pub async fn detached<T, Fut>(
        self: &std::sync::Arc<Self>,
        write: impl FnOnce(std::sync::Arc<Self>) -> Fut,
    ) -> Result<T>
    where
        Fut: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        self.run_tracked(write(self.clone())).await
    }

    /// 把 `fut` 放进独立任务跑完并等它的结果，期间算进 [`Self::drain_writes`] 要等的笔数：
    /// 调用方被取消时任务照样跑完，进程正常关停时也会等它落库。
    pub(crate) async fn run_tracked<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let pending = PendingWrite::new(&self.pending_writes);
        let task = tokio::spawn(async move {
            let _pending = pending;
            fut.await
        });
        task.await.context("store write task failed")?
    }

    /// 等 [`Self::spawn_write`] 交出去的写入落库，最多等 `timeout`。关停时调；测试里读流水前也调。
    pub async fn drain_writes(&self, timeout: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let n = self.pending_writes.load(std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(pending = n, "gave up waiting for background store writes");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// 执行一条写语句，回是否影响了行。
    async fn update_one(
        &self,
        q: sqlx::query::Query<'_, Postgres, sqlx::postgres::PgArguments>,
    ) -> Result<bool> {
        Ok(q.execute(&self.pool).await?.rows_affected() > 0)
    }
}

/// 去掉串里的 NUL（`\0`）。PG 的 TEXT 不收 NUL（SQLite 收），整条语句会直接报错：客户端在
/// `metadata.user_id` 里塞一个 `\u0000`，选号就 500、这条请求的流水与账单整笔丢掉。能被
/// 外面塞进来的串（设备 / 会话标识、模型名，以及上游回显它们的报错）落库或拿去查询前过一道。
/// 不含 NUL 时原样借用，不拷贝。
pub(super) fn nul_free(s: &str) -> Cow<'_, str> {
    if s.contains('\0') { Cow::Owned(s.replace('\0', "")) } else { Cow::Borrowed(s) }
}

/// [`nul_free`] 的原地版。
pub(super) fn strip_nul(s: &mut String) {
    s.retain(|c| c != '\0');
}

/// 开事务并拿全局 advisory 锁（只防多进程；进程内的排队在 [`CredentialStore::begin_write`]）。
async fn begin_locked(pool: &PgPool) -> Result<Transaction<'static, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(WRITE_LOCK_KEY).execute(&mut *tx).await?;
    Ok(tx)
}

/// 补齐恒存在的行。在串行化事务里做：几个进程同时启动时不会各插一份。
async fn seed(pool: &PgPool) -> Result<()> {
    let mut tx = begin_locked(pool).await?;
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
    .bind(DEFAULT_GROUP_NAME)
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

/// 解密 [`row_to_sealed_cred`] 读出来的两个 token（明文的原样不动，见 [`open`]）。
pub(super) fn open_tokens(cred: &mut Credential) -> Result<()> {
    cred.access_token =
        open(&cred.access_token).context("failed to decrypt a stored token (access_token)")?;
    cred.refresh_token =
        open(&cred.refresh_token).context("failed to decrypt a stored token (refresh_token)")?;
    Ok(())
}

/// 把一行 `SELECT {COLS}`（列清单就是 [`COLS`]）读成 [`Credential`]，token 列顺手解密。
pub(super) fn row_to_cred(row: &PgRow) -> Result<Credential> {
    let mut cred = row_to_sealed_cred(row)?;
    open_tokens(&mut cred)?;
    Ok(cred)
}

/// 同 [`row_to_cred`]，但两个 token 列**原样留着密文**，不解密。选号要把全部候选号读出来，
/// 却只用得到选中那一个的 token：先这样读，选定之后再 [`open_tokens`]。
pub(super) fn row_to_sealed_cred(row: &PgRow) -> Result<Credential> {
    Ok(Credential {
        id: row.try_get(0)?,
        label: row.try_get(1)?,
        tier: row.try_get(2)?,
        access_token: row.try_get(3)?,
        refresh_token: row.try_get(4)?,
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
