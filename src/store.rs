//! 多凭证的 SQLite 持久化层（参照 kiro.rs 的做法）。
//!
//! 单连接 + `parking_lot::Mutex` 串行化；WAL + `synchronous=NORMAL`；STRICT 表 +
//! `CHECK`/`UNIQUE` 约束。token 轮换走单行 `UPDATE`，不重写整库。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::credentials::Credential;

/// 查询列顺序，与 [`row_to_cred`] 一一对应。
const COLS: &str = "id, label, tier, access_token, refresh_token, expires_at, priority, disabled, \
     created_at, updated_at, device_limit";

/// 凭证 SQLite 存储。
pub struct CredentialStore {
    conn: Mutex<Connection>,
}

/// 硬性设备上限触发：所有启用凭证的设备名额均已占满。
///
/// 通过 `anyhow` 向上传递，代理层 `downcast` 后映射为 HTTP 429。
#[derive(Debug)]
pub struct DeviceLimitReached;

impl std::fmt::Display for DeviceLimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "所有凭证的设备数均已达上限，暂无可用名额")
    }
}

impl std::error::Error for DeviceLimitReached {}

impl CredentialStore {
    /// 数据库文件路径。默认 `~/.luban/luban.db`；`LUBAN_HOME` 可覆盖基目录。
    pub fn db_path() -> Result<PathBuf> {
        let base = match std::env::var_os("LUBAN_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::home_dir()
                .context("无法定位用户主目录")?
                .join(".luban"),
        };
        Ok(base.join("luban.db"))
    }

    /// 在默认路径打开（或新建）凭证库并初始化 schema。
    pub fn open_default() -> Result<Self> {
        let path = Self::db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("打开凭证库失败: {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 插入一条新凭证，返回带 id 的完整记录。
    pub fn insert(
        &self,
        label: &str,
        tier: Option<&str>,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
    ) -> Result<Credential> {
        let conn = self.conn.lock();
        // 新凭证优先级默认排到末尾（现有最大 +1）。
        let next_priority: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(priority), -1) + 1 FROM credentials",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO credentials (label, tier, access_token, refresh_token, expires_at, priority)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![label, tier, access_token, refresh_token, expires_at as i64, next_priority],
        )
        .context("插入凭证失败（refresh_token 可能已存在）")?;
        let id = conn.last_insert_rowid();
        conn.query_row(
            &format!("SELECT {COLS} FROM credentials WHERE id = ?1"),
            [id],
            row_to_cred,
        )
        .context("读取新插入凭证失败")
    }

    /// 列出全部凭证，按 (priority, id) 升序。
    pub fn list(&self) -> Result<Vec<Credential>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare(&format!("SELECT {COLS} FROM credentials ORDER BY priority ASC, id ASC"))?;
        let rows = stmt.query_map([], row_to_cred)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 按 id 读取单条。
    pub fn get(&self, id: i64) -> Result<Option<Credential>> {
        let conn = self.conn.lock();
        conn.query_row(
            &format!("SELECT {COLS} FROM credentials WHERE id = ?1"),
            [id],
            row_to_cred,
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other.into()),
        })
    }

    /// 删除一条，返回是否确有删除。连带清除其设备绑定。
    pub fn delete(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
        let n = conn.execute("DELETE FROM credentials WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    /// 清空所有凭证，返回删除条数。
    pub fn clear(&self) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM credentials", [])?)
    }

    /// 设置停用状态。
    pub fn set_disabled(&self, id: i64, disabled: bool) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET disabled = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, disabled as i64],
        )
    }

    /// 设置优先级。
    pub fn set_priority(&self, id: i64, priority: i64) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET priority = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, priority],
        )
    }

    /// 设置设备数上限（`<= 0` 表示不限）。
    pub fn set_device_limit(&self, id: i64, limit: i64) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET device_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, limit],
        )
    }

    /// 单条凭证当前已绑定的设备数。
    pub fn device_count(&self, cred_id: i64) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM device_bindings WHERE cred_id = ?1",
            [cred_id],
            |r| r.get(0),
        )?)
    }

    /// 所有凭证的已绑定设备数（cred_id → count）。
    pub fn device_counts(&self) -> Result<HashMap<i64, i64>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT cred_id, COUNT(*) FROM device_bindings GROUP BY cred_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, n) = row?;
            out.insert(cid, n);
        }
        Ok(out)
    }

    /// 更新账号等级。
    pub fn set_tier(&self, id: i64, tier: Option<&str>) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET tier = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, tier],
        )
    }

    /// 重命名（设置显示名）。
    pub fn set_label(&self, id: i64, label: &str) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET label = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, label],
        )
    }

    /// 刷新后回写新的 token 三元组（单行 UPDATE）。
    pub fn update_tokens(
        &self,
        id: i64,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
    ) -> Result<bool> {
        self.update_one(
            "UPDATE credentials
                SET access_token = ?2, refresh_token = ?3, expires_at = ?4, updated_at = unixepoch()
              WHERE id = ?1",
            params![id, access_token, refresh_token, expires_at as i64],
        )
    }

    fn update_one(&self, sql: &str, p: impl rusqlite::Params) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(sql, p)?;
        Ok(n > 0)
    }

    /// 读取设置项；不存在返回 None。
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
    }

    /// 写入设置项（upsert）。
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    /// 设备绑定有效期（秒）；未设置或解析失败时用默认值。`<= 0` 表示永不过期。
    pub fn device_binding_ttl(&self) -> i64 {
        self.get_setting(DEVICE_BINDING_TTL)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_DEVICE_BINDING_TTL_SECS)
    }

    /// 删除设置项。
    pub fn delete_setting(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    }
}

/// 接入用 client api key 的 settings 键名。
pub const CLIENT_API_KEY: &str = "client_api_key";

/// 管理密码（sha256 hex）的 settings 键名。
pub const ADMIN_PASSWORD: &str = "admin_password_sha256";

/// 设备绑定有效期（秒）的 settings 键名；`<= 0` 表示永不过期。
pub const DEVICE_BINDING_TTL: &str = "device_binding_ttl_secs";

/// 设备绑定有效期默认值：1 小时。
pub const DEFAULT_DEVICE_BINDING_TTL_SECS: i64 = 3600;

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS credentials (
            id            INTEGER PRIMARY KEY,
            label         TEXT    NOT NULL DEFAULT '',
            tier          TEXT,
            access_token  TEXT    NOT NULL,
            refresh_token TEXT    NOT NULL,
            expires_at    INTEGER NOT NULL,
            priority      INTEGER NOT NULL DEFAULT 0,
            disabled      INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0,1)),
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;

        CREATE UNIQUE INDEX IF NOT EXISTS uq_credentials_refresh_token
            ON credentials(refresh_token);
        CREATE INDEX IF NOT EXISTS idx_credentials_priority
            ON credentials(priority, id);

        -- 键值设置表（如接入用的 client api key）。
        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        ) STRICT;

        -- 设备→凭证的粘性绑定：同一 device_id 始终命中同一凭证。
        CREATE TABLE IF NOT EXISTS device_bindings (
            device_id     TEXT    PRIMARY KEY,
            cred_id       INTEGER NOT NULL,
            request_count INTEGER NOT NULL DEFAULT 0,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            last_seen_at  INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_device_bindings_cred
            ON device_bindings(cred_id);",
    )
    .context("初始化凭证库 schema 失败")?;

    // 兼容旧库：新增列时若已存在会报 duplicate column，忽略即可（幂等）。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN tier TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE credentials ADD COLUMN device_limit INTEGER NOT NULL DEFAULT 0",
        [],
    );
    Ok(())
}

fn row_to_cred(row: &Row) -> rusqlite::Result<Credential> {
    Ok(Credential {
        id: row.get(0)?,
        label: row.get(1)?,
        tier: row.get(2)?,
        access_token: row.get(3)?,
        refresh_token: row.get(4)?,
        expires_at: row.get::<_, i64>(5)? as u64,
        priority: row.get(6)?,
        disabled: row.get::<_, i64>(7)? != 0,
        created_at: row.get::<_, i64>(8)? as u64,
        updated_at: row.get::<_, i64>(9)? as u64,
        device_limit: row.get(10)?,
    })
}

impl CredentialStore {
    /// 按 device_id 做粘性选择，返回选中的凭证（刷新在锁外由调用方处理）。
    ///
    /// 规则：
    /// 1. 已有绑定且该凭证仍启用 → 复用（更新 last_seen / request_count），已绑定设备不受限。
    /// 2. 绑定的凭证已停用或删除 → 清除陈旧绑定，作为新设备重新选择。
    /// 3. 新设备 → 在仍有名额的启用凭证中做负载均衡：选“当前设备数最少”者并绑定；
    ///    同数时按 (priority, id) 决定，保持确定性。
    /// 4. 所有启用凭证均达设备上限 → 硬性拒绝，返回 [`DeviceLimitReached`]（代理映射为 429）。
    ///
    /// `device_id` 为 `None`（请求未带 metadata）时无从绑定/计数：退化为负载均衡挑选，
    /// 不写绑定、也不受硬上限约束。
    /// `ttl_secs > 0` 时先清除超时未活跃的绑定（惰性过期）；`<= 0` 表示永不过期。
    /// 全部操作在单次持锁内完成，避免与其它写入竞态。
    pub fn select_for_device(&self, device_id: Option<&str>, ttl_secs: i64) -> Result<Credential> {
        let conn = self.conn.lock();

        // 惰性过期：清掉超过 TTL 未活跃的绑定，释放其占用的设备名额。
        if ttl_secs > 0 {
            conn.execute(
                "DELETE FROM device_bindings WHERE last_seen_at < unixepoch() - ?1",
                [ttl_secs],
            )?;
        }

        // 启用凭证，按 (priority, id) 升序。
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM credentials WHERE disabled = 0 ORDER BY priority ASC, id ASC"
        ))?;
        let creds: Vec<Credential> =
            stmt.query_map([], row_to_cred)?.collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        if creds.is_empty() {
            anyhow::bail!("没有可用凭证，请先登录");
        }

        // 1/2) 命中既有绑定。
        if let Some(did) = device_id {
            let bound: Option<i64> = conn
                .query_row(
                    "SELECT cred_id FROM device_bindings WHERE device_id = ?1",
                    [did],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(cid) = bound {
                if let Some(c) = creds.iter().find(|c| c.id == cid) {
                    conn.execute(
                        "UPDATE device_bindings
                            SET last_seen_at = unixepoch(), request_count = request_count + 1
                          WHERE device_id = ?1",
                        [did],
                    )?;
                    return Ok(c.clone());
                }
                // 绑定的凭证已停用/删除：清除后重新选择。
                conn.execute("DELETE FROM device_bindings WHERE device_id = ?1", [did])?;
            }
        }

        // 各凭证当前设备数。
        let mut counts: HashMap<i64, i64> = HashMap::new();
        {
            let mut cstmt =
                conn.prepare("SELECT cred_id, COUNT(*) FROM device_bindings GROUP BY cred_id")?;
            let rows = cstmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (cid, n) = row?;
                counts.insert(cid, n);
            }
        }

        // 当前设备数（惰性过期后已排除超时项）。
        let used = |c: &Credential| counts.get(&c.id).copied().unwrap_or(0);

        // 3/4) 负载均衡：在候选中选“当前设备数最少”者；同数按 (priority, id) 决定。
        let chosen = if device_id.is_some() {
            // 硬限制：仅在仍有名额者（limit<=0 不限，或 used<limit）中选；全满则拒绝。
            match creds
                .iter()
                .filter(|c| c.device_limit <= 0 || used(c) < c.device_limit)
                .min_by_key(|c| (used(c), c.priority, c.id))
            {
                Some(c) => c,
                None => return Err(DeviceLimitReached.into()),
            }
        } else {
            // 无 device_id：不占名额、不受限，纯负载均衡挑一个（creds 已保证非空）。
            creds
                .iter()
                .min_by_key(|c| (used(c), c.priority, c.id))
                .expect("启用凭证列表非空")
        };

        if let Some(did) = device_id {
            conn.execute(
                "INSERT INTO device_bindings (device_id, cred_id) VALUES (?1, ?2)
                 ON CONFLICT(device_id) DO UPDATE
                    SET cred_id = ?2, last_seen_at = unixepoch(), request_count = request_count + 1",
                params![did, chosen.id],
            )?;
        }
        Ok(chosen.clone())
    }
}

/// 代理转发使用：按 device_id 粘性选出凭证并返回 (access_token, 该凭证)（必要时刷新）。
///
/// 选择见 [`CredentialStore::select_for_device`]。若命中的凭证进入刷新窗口，
/// 则调用 OAuth 刷新并回写。注意刷新是异步 IO，不持有 DB 锁。
pub async fn valid_access_token_for_device(
    store: &CredentialStore,
    http: &reqwest::Client,
    device_id: Option<&str>,
) -> Result<(String, Credential)> {
    let ttl = store.device_binding_ttl();
    let cred = store.select_for_device(device_id, ttl)?;

    if cred.needs_refresh() {
        tracing::info!(id = cred.id, label = %cred.label, "凭证进入刷新窗口，刷新 token");
        let tokens = crate::oauth::refresh(http, &cred.refresh_token).await?;
        store.update_tokens(
            cred.id,
            &tokens.access_token,
            &tokens.refresh_token,
            tokens.expires_at,
        )?;
        Ok((tokens.access_token, cred))
    } else {
        let token = cred.access_token.clone();
        Ok((token, cred))
    }
}
