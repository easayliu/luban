//! 多凭证的 SQLite 持久化层（参照 kiro.rs 的做法）。
//!
//! 单连接 + `parking_lot::Mutex` 串行化；WAL + `synchronous=NORMAL`；STRICT 表 +
//! `CHECK`/`UNIQUE` 约束。token 轮换走单行 `UPDATE`，不重写整库。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use crate::credentials::{
    Credential, PRIORITY_DEFAULT, PRIORITY_MAX, PRIORITY_MIN, priority_tiers_by_rank,
};

mod bans;
mod billing;
mod bindings;
mod credential;
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
mod schema;
mod secret;
mod select;
mod session_events;
mod settings;
mod stats;
mod usage;
mod users;

pub use bans::*;
use billing::*;
pub use billing::{BillingDim, BillingFilter, BillingRow};
pub use bindings::*;
pub use credential::*;
pub use flags::*;
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
use schema::*;
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

/// 凭证 SQLite 存储。
pub struct CredentialStore {
    /// 后台统计专用的**只读**连接池（同一个库文件，WAL 下读不挡写），见 [`Self::read_conn`]。
    ///
    /// 控制台的聚合查询（额度快照、趋势、流水翻页等）要扫几十万行流水，一次几十到几百毫秒。
    /// 它们以前和转发路径共用 `conn`，持锁期间所有选号、落流水都排在后面，而等锁的正是
    /// tokio 工作线程——整个运行时的 SSE 会跟着一起停。拆成两把锁后转发路径不再等它们。
    ///
    /// 不止一条：概览页同时在拉账号列表（30s）、实时指标（10s）、24h/7d 两组趋势，只有一条
    /// 只读连接时它们互相排队，账号列表要等前面那几条 7 天扫描跑完才轮到。WAL 下多条读连接
    /// 各读各的快照、真正并行，条数见 [`READER_POOL_SIZE`]。
    ///
    /// 内存库（测试）开不出第二条连接去读同一份数据，此时为空，读退回 `conn`。
    ///
    /// **必须声明在 `conn` 之前**：字段按声明顺序析构，它们得先关。主连接关闭时若只读连接还
    /// 开着，主连接就不是最后一条，SQLite 会跳过关库时的 checkpoint；只读连接自己又做不了
    /// checkpoint，于是优雅退出后 `-wal` 留在磁盘上、最近的写入没回写进 `.db`——只拷
    /// `luban.db` 做备份或迁移的人会漏掉这一段。
    readers: Vec<Mutex<Connection>>,
    /// 只读连接全忙时下一个去排队的下标，轮着排，别都挤在第一条上。
    next_reader: std::sync::atomic::AtomicUsize,
    conn: Mutex<Connection>,
    /// 每凭证一把刷新锁，串行化 token 刷新，见 [`valid_access_token_for_device`]。
    /// 上游刷新会**轮换 refresh_token**：并发刷新时后完成的那次会把已被作废的 token 写回库，
    /// 该凭证之后所有刷新都 `invalid_grant`，等于账号被自己废掉。
    refresh_locks: Mutex<HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
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
    /// 绑定 TTL、设备上限、裸请求限流两项），逐项走 SQL 就是每请求 8 次查询，且全部串行在
    /// 上面那把全局 `conn` 锁上——转发路径的落库、后台的列表查询都得排在它们后面。设置项
    /// 极少变动，缓存住之后这些查询直接归零。
    ///
    /// 写路径只有 [`CredentialStore::set_setting`]/[`CredentialStore::delete_setting`] 两处，
    /// 都是先落库再更新缓存，故进程内不会漂移。**多进程共享同一个库时会读到陈旧值**——
    /// luban 是单进程本地代理，没有这个场景（同 [`RateWindow`] 的取舍）。
    settings: parking_lot::RwLock<HashMap<String, String>>,
}

impl CredentialStore {
    /// 数据库文件路径。默认 `~/.luban/luban.db`；`LUBAN_HOME` 可覆盖基目录。
    pub fn db_path() -> Result<PathBuf> {
        let base = match std::env::var_os("LUBAN_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::home_dir()
                .context("could not determine the user home directory")?
                .join(".luban"),
        };
        Ok(base.join("luban.db"))
    }

    /// 在默认路径打开（或新建）凭证库并初始化 schema。
    pub fn open_default() -> Result<Self> {
        Self::open_at(&Self::db_path()?)
    }

    /// 在指定路径打开（或新建）凭证库并初始化 schema，另开一条后台统计用的只读连接。
    fn open_at(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open credential database: {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // 有了独立的只读连接，自动 checkpoint 可能撞上它的读事务，WAL 那一刻重置不了、只能
        // 接着往后长（改动前读写同锁串行，没有这个情况）。WAL 本身不会自己缩，这里给个上限：
        // 下次重置时截回 64MB，一阵连续的慢查询过后磁盘占用能降回来。
        conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
        // 密钥要在建表迁移之前就位：迁移会把存量明文 token 加密（见 `secret`）。
        init_key(path.parent().unwrap_or(std::path::Path::new(".")))?;
        init_schema(&conn)?;
        let mut store = Self::with_conn(conn);
        // schema 已由主连接建好，只读连接不做迁移。开不出来不影响服务：少几条就少几条并行，
        // 一条都没有时后台读退回主连接，只是回到拆分之前的性能。
        for _ in 0..READER_POOL_SIZE {
            match open_reader(path) {
                Ok(reader) => store.readers.push(Mutex::new(reader)),
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        opened = store.readers.len(),
                        "failed to open a read-only database connection"
                    );
                    break;
                }
            }
        }
        if store.readers.is_empty() {
            tracing::warn!("no read-only database connection; admin queries share the main one");
        }
        Ok(store)
    }

    /// 内存库（**仅测试**）：schema 已初始化，进程退出即消失。
    ///
    /// 给 crate 内其它模块的测试用（`with_conn`/`init_schema` 都是本模块私有的）；
    /// store 自己的测试直接用 `with_conn`。
    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        init_schema(&conn)?;
        Ok(Self::with_conn(conn))
    }

    /// 由已初始化的连接构造（`open_default` 与测试共用）。
    fn with_conn(conn: Connection) -> Self {
        // 设置表整张读进内存，见 `settings` 字段的说明。读失败（表还不存在等）就从空表起步，
        // 所有取值退回各自的默认值——绝不能因为读设置失败而让整个服务起不来。
        let settings = load_settings(&conn).unwrap_or_default();
        Self {
            conn: Mutex::new(conn),
            readers: Vec::new(),
            next_reader: std::sync::atomic::AtomicUsize::new(0),
            refresh_locks: Mutex::new(HashMap::new()),
            bare_rate: RateWindow::default(),
            rpm_rate: RateWindow::default(),
            device_rate: RateWindow::default(),
            session_rate: RateWindow::default(),
            cooldown: RateLimitCooldown::default(),
            settings: parking_lot::RwLock::new(settings),
        }
    }

    /// 后台统计用的只读连接（没有就退回主连接）。
    ///
    /// **只给纯读、且只由管理接口调用的方法用**：连接以只读方式打开，写语句会直接报错；
    /// 转发路径也不该用它——它可能正被一条几百毫秒的聚合查询占着。调用方（web 的 handler）
    /// 要放在 `spawn_blocking` 里跑，等这把锁同样不能占 tokio 工作线程。
    ///
    /// 先挑一条空闲的；全忙才排队，排哪条轮着来。
    fn read_conn(&self) -> parking_lot::MutexGuard<'_, Connection> {
        if self.readers.is_empty() {
            return self.conn.lock();
        }
        if let Some(guard) = self.readers.iter().find_map(|r| r.try_lock()) {
            return guard;
        }
        let i = self.next_reader.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.readers[i % self.readers.len()].lock()
    }

    /// 取该凭证的刷新锁（不存在则创建）。
    pub(crate) fn refresh_lock(&self, cred_id: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks.lock().entry(cred_id).or_default().clone()
    }

    fn update_one(&self, sql: &str, p: impl rusqlite::Params) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(sql, p)?;
        Ok(n > 0)
    }
}

/// 后台统计只读连接的条数，见 [`CredentialStore::readers`]。控制台同一时刻在跑的重查询也就
/// 三四条（账号列表、实时指标、两组趋势）；再多只是多占几份页缓存和文件句柄。
const READER_POOL_SIZE: usize = 3;

/// 打开后台统计用的只读连接，见 [`CredentialStore::readers`]。
///
/// `SQLITE_OPEN_READ_ONLY`：误把写语句挂到这条连接上会当场报错，而不是悄悄绕开主连接的
/// 串行化。WAL 模式由主连接设在库文件上，这里不用（也不能）再设；busy_timeout 与主连接
/// 一致，兜住 checkpoint 等极短的排他窗口。
fn open_reader(path: &std::path::Path) -> Result<Connection> {
    use rusqlite::OpenFlags;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open read-only connection: {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

fn row_to_cred(row: &Row) -> rusqlite::Result<Credential> {
    Ok(Credential {
        id: row.get(0)?,
        label: row.get(1)?,
        tier: row.get(2)?,
        access_token: open_column(row.get(3)?, 3)?,
        refresh_token: open_column(row.get(4)?, 4)?,
        expires_at: row.get::<_, i64>(5)? as u64,
        priority: row.get(6)?,
        disabled: row.get::<_, i64>(7)? != 0,
        created_at: row.get::<_, i64>(8)? as u64,
        updated_at: row.get::<_, i64>(9)? as u64,
        device_limit: row.get(10)?,
        ban_reason: row.get(11)?,
        account_uuid: row.get(12)?,
        resume_at: row.get::<_, Option<i64>>(13)?.map(|t| t as u64),
        org_type: row.get(14)?,
        proxy: row.get(15)?,
        rpm_limit: row.get(16)?,
        rate_limit_tier: row.get(17)?,
        org_uuid: row.get(18)?,
        subscription_created_at: row.get(19)?,
        quota_pause_pct: row.get(20)?,
        quota_pause_pct_7d: row.get(21)?,
        session_limit: row.get(22)?,
        org_name: row.get(23)?,
        seat_tier: row.get(24)?,
        subscription_status: row.get(25)?,
        extra_usage_enabled: row.get::<_, Option<i64>>(26)?.map(|v| v != 0),
        owner_id: row.get(27)?,
    })
}

#[cfg(test)]
mod tests;
