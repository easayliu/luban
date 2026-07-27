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
     created_at, updated_at, device_limit, ban_reason, account_uuid";

/// 凭证 SQLite 存储。
pub struct CredentialStore {
    conn: Mutex<Connection>,
    /// 每凭证一把刷新锁，串行化 token 刷新，见 [`valid_access_token_for_device`]。
    /// 上游刷新会**轮换 refresh_token**：并发刷新时后完成的那次会把已被作废的 token 写回库，
    /// 该凭证之后所有刷新都 `invalid_grant`，等于账号被自己废掉。
    refresh_locks: Mutex<HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
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
        Ok(Self::with_conn(conn))
    }

    /// 由已初始化的连接构造（`open_default` 与测试共用）。
    fn with_conn(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
            refresh_locks: Mutex::new(HashMap::new()),
        }
    }

    /// 取该凭证的刷新锁（不存在则创建）。
    fn refresh_lock(&self, cred_id: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks.lock().entry(cred_id).or_default().clone()
    }

    /// 插入一条新凭证，返回带 id 的完整记录。
    pub fn insert(
        &self,
        label: &str,
        tier: Option<&str>,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
        account_uuid: Option<&str>,
    ) -> Result<Credential> {
        let conn = self.conn.lock();
        // 新凭证一律落在默认档 P0：同档内按设备数负载均衡，新账号立刻参与分摊。
        // 需要瀑布式（榨干一个再用下一个）时，手动/批量把账号调到不同优先级即可。
        conn.execute(
            "INSERT INTO credentials
                 (label, tier, access_token, refresh_token, expires_at, account_uuid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![label, tier, access_token, refresh_token, expires_at as i64, account_uuid],
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

    /// 删除一条，返回是否确有删除。连带清除其设备绑定与历史用量日志。
    ///
    /// 用量日志一并清掉：账号已不存在，其历史记录既无处归属（后台按 cred_id 关联展示
    /// 费用/额度/最近使用），留着只会在请求日志里堆积无主行、并让费用统计包含已删账号。
    /// 三张表在同一事务内删除，避免中途失败留下半清理状态。
    pub fn delete(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM usage_logs WHERE cred_id = ?1", [id])?;
        tx.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
        let n = tx.execute("DELETE FROM credentials WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// 清空所有凭证，返回删除条数。连带清空设备绑定与全部用量日志（口径同
    /// [`Self::delete`]：账号没了，历史用量不再保留）。
    pub fn clear(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM usage_logs", [])?;
        tx.execute("DELETE FROM device_bindings", [])?;
        let n = tx.execute("DELETE FROM credentials", [])?;
        tx.commit()?;
        Ok(n)
    }

    /// 设置停用状态（管理员手动开关）。
    ///
    /// 停用时立即清空其设备绑定，让已绑定设备的下一次请求马上改选其它凭证，
    /// 而不必等绑定 TTL 惰性过期；重新启用时清除 `ban_reason`（若之前是被自动停用）。
    pub fn set_disabled(&self, id: i64, disabled: bool) -> Result<bool> {
        let conn = self.conn.lock();
        if disabled {
            conn.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
            Ok(conn.execute(
                "UPDATE credentials SET disabled = 1, updated_at = unixepoch() WHERE id = ?1",
                [id],
            )? > 0)
        } else {
            Ok(conn.execute(
                "UPDATE credentials SET disabled = 0, ban_reason = NULL, updated_at = unixepoch() \
                 WHERE id = ?1",
                [id],
            )? > 0)
        }
    }

    /// 自动检测到上游账号级错误（如封号）时调用：停用凭证并记录原因，
    /// 同时清空其设备绑定，使下一次请求立即改选其它凭证。
    ///
    /// 与 [`Self::set_disabled`] 的区别在于会写入 `ban_reason`，供后台 UI 区分
    /// 「管理员手动停用」与「上游自动判定停用」。
    pub fn mark_banned(&self, id: i64, reason: &str) -> Result<bool> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
        Ok(conn.execute(
            "UPDATE credentials SET disabled = 1, ban_reason = ?2, updated_at = unixepoch() \
             WHERE id = ?1",
            params![id, reason],
        )? > 0)
    }

    /// 设置优先级。
    pub fn set_priority(&self, id: i64, priority: i64) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET priority = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, priority],
        )
    }

    /// 批量设置优先级：把 `ids` 里的账号统一改到 `priority`，返回实际更新的条数。
    /// 单事务内完成，避免中途失败留下一半新一半旧的调度档位。`ids` 为空时直接返回 0。
    pub fn set_priorities(&self, ids: &[i64], priority: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET priority = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, priority])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量删除：连带清掉这些账号的用量日志与设备绑定（口径同 [`Self::delete`]），
    /// 返回实际删除的条数。单事务内完成，避免删到一半留下无主的日志/绑定。
    pub fn delete_many(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut logs = tx.prepare("DELETE FROM usage_logs WHERE cred_id = ?1")?;
            let mut binds = tx.prepare("DELETE FROM device_bindings WHERE cred_id = ?1")?;
            let mut cred = tx.prepare("DELETE FROM credentials WHERE id = ?1")?;
            for id in ids {
                logs.execute([id])?;
                binds.execute([id])?;
                n += cred.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量启停：语义与 [`Self::set_disabled`] 一致（停用时清设备绑定使其立即改选其它
    /// 凭证；启用时清 `ban_reason`），返回实际更新的条数。单事务内完成。
    pub fn set_disabled_many(&self, ids: &[i64], disabled: bool) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            if disabled {
                let mut binds = tx.prepare("DELETE FROM device_bindings WHERE cred_id = ?1")?;
                let mut stmt = tx.prepare(
                    "UPDATE credentials SET disabled = 1, updated_at = unixepoch() WHERE id = ?1",
                )?;
                for id in ids {
                    binds.execute([id])?;
                    n += stmt.execute([id])?;
                }
            } else {
                let mut stmt = tx.prepare(
                    "UPDATE credentials SET disabled = 0, ban_reason = NULL, \
                     updated_at = unixepoch() WHERE id = ?1",
                )?;
                for id in ids {
                    n += stmt.execute([id])?;
                }
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量设置设备数上限（三态语义同 [`Self::set_device_limit`]），返回实际更新的条数。
    pub fn set_device_limits(&self, ids: &[i64], limit: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET device_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, limit])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 设置该账号的设备数上限。三态：`> 0` 本账号独立上限；`0` 跟随全局默认
    /// （见 [`DEFAULT_DEVICE_LIMIT`]）；`< 0` 本账号明确不限（不受全局默认约束）。
    pub fn set_device_limit(&self, id: i64, limit: i64) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET device_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, limit],
        )
    }

    /// 全局默认设备数上限：`<= 0` 表示默认不限。未设置或解析失败时按 0（不限）。
    pub fn default_device_limit(&self) -> i64 {
        self.get_setting(DEFAULT_DEVICE_LIMIT)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0)
    }

    /// 单条凭证当前**有效**绑定的设备数：已排除超过 TTL 未活跃的绑定（与选路时的惰性
    /// 过期口径一致），故后台显示会随时间自然回落，不必等下一次请求触发 sweep。
    /// TTL `<= 0`（永不过期）时按全量计。
    pub fn device_count(&self, cred_id: i64) -> Result<i64> {
        let ttl = self.device_binding_ttl();
        let conn = self.conn.lock();
        let n = if ttl > 0 {
            conn.query_row(
                "SELECT COUNT(*) FROM device_bindings \
                 WHERE cred_id = ?1 AND last_seen_at >= unixepoch() - ?2",
                params![cred_id, ttl],
                |r| r.get(0),
            )?
        } else {
            conn.query_row(
                "SELECT COUNT(*) FROM device_bindings WHERE cred_id = ?1",
                [cred_id],
                |r| r.get(0),
            )?
        };
        Ok(n)
    }

    /// 单条凭证当前**有效**绑定的设备明细，按最近活跃倒序。
    ///
    /// 过滤口径与 [`Self::device_count`] 完全一致（同一个 TTL），否则后台会出现「设备数写着
    /// 2、展开却列出 5 条」这种自相矛盾的展示。
    pub fn list_devices(&self, cred_id: i64) -> Result<Vec<DeviceBinding>> {
        let ttl = self.device_binding_ttl();
        let conn = self.conn.lock();
        let ttl_clause = if ttl > 0 { "AND last_seen_at >= unixepoch() - ?2" } else { "" };
        let sql = format!(
            "SELECT device_id, request_count, created_at, last_seen_at FROM device_bindings \
             WHERE cred_id = ?1 {ttl_clause} ORDER BY last_seen_at DESC, device_id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &Row| {
            Ok(DeviceBinding {
                device_id: r.get(0)?,
                request_count: r.get(1)?,
                created_at: r.get(2)?,
                last_seen_at: r.get(3)?,
            })
        };
        let rows: Vec<DeviceBinding> = if ttl > 0 {
            stmt.query_map(params![cred_id, ttl], map_row)?.collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map([cred_id], map_row)?.collect::<rusqlite::Result<_>>()?
        };
        Ok(rows)
    }

    /// 所有凭证当前**有效**绑定的设备数（cred_id → count）；口径同 [`Self::device_count`]，
    /// 排除超过 TTL 未活跃的绑定。TTL `<= 0` 时按全量计。
    pub fn device_counts(&self) -> Result<HashMap<i64, i64>> {
        let ttl = self.device_binding_ttl();
        let conn = self.conn.lock();
        let where_clause = if ttl > 0 { "WHERE last_seen_at >= unixepoch() - ?1" } else { "" };
        let sql =
            format!("SELECT cred_id, COUNT(*) FROM device_bindings {where_clause} GROUP BY cred_id");
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &Row| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?));
        let rows = if ttl > 0 { stmt.query_map([ttl], map_row)? } else { stmt.query_map([], map_row)? };
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

    /// 回填账号 UUID（旧库凭证登录时未存、刷新 token 时补上）。仅在非空时覆盖。
    pub fn set_account_uuid(&self, id: i64, account_uuid: &str) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET account_uuid = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, account_uuid],
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

    /// 是否对转发请求做身份伪装：改写 `metadata.user_id` 的 account_uuid/device_id，
    /// 并给 `x-anthropic-billing-header` 补 `cch`（订阅模式独有字段）。
    /// 未设置时默认开启；仅 `"0"`/`"false"`（忽略大小写与首尾空白）视为关闭——关掉即完全
    /// 不动这两处，用于排查「是否因伪装本身被识别」。
    pub fn spoof_identity_enabled(&self) -> bool {
        match self.get_setting(SPOOF_IDENTITY_ENABLED).ok().flatten() {
            Some(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false"),
            None => true,
        }
    }

    /// 是否要求请求携带有效设备身份（`metadata.user_id`）；未设置时默认要求（保持严格）。
    /// 仅 `"0"`/`"false"`（忽略大小写与首尾空白）视为关闭。
    pub fn require_device_id(&self) -> bool {
        match self.get_setting(REQUIRE_DEVICE_ID).ok().flatten() {
            Some(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false"),
            None => true,
        }
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

/// 是否启用身份伪装的 settings 键名；`"0"`/`"false"` 关闭，缺省或其它值视为开启。
pub const SPOOF_IDENTITY_ENABLED: &str = "spoof_identity_enabled";

/// 是否要求请求携带有效设备身份的 settings 键名；`"0"`/`"false"` 关闭（放行裸请求），
/// 缺省或其它值视为要求（无有效 `metadata.user_id` 的请求直接 403）。
pub const REQUIRE_DEVICE_ID: &str = "require_device_id";

/// 全局默认设备数上限的 settings 键名；`<= 0` 表示默认不限。
/// 账号自身 `device_limit == 0`（默认值）时套用它，无需逐个账号配置。
pub const DEFAULT_DEVICE_LIMIT: &str = "default_device_limit";

/// 账号实际生效的设备数上限：返回 `0` 表示不限。
///
/// `cred_limit` 三态——`> 0` 账号独立上限（覆盖全局）；`0` 跟随全局默认 `default_limit`；
/// `< 0` 账号明确不限（即便全局有默认值也不限）。旧库所有账号都是 0，全局默认亦为 0
/// （不限），故行为与加入本机制前一致。
pub fn effective_device_limit(cred_limit: i64, default_limit: i64) -> i64 {
    match cred_limit {
        n if n > 0 => n,
        0 => default_limit.max(0),
        _ => 0,
    }
}

/// 待写入的一条用量日志（代理层组装后交给 [`CredentialStore::insert_usage_log`]）。
#[derive(Debug, Default)]
pub struct UsageRecord {
    pub cred_id: Option<i64>,
    pub cred_label: String,
    /// 完整 device_id（供本地分析；对外展示可自行截断）。
    pub device_id: Option<String>,
    pub model: Option<String>,
    pub path: String,
    pub status: u16,
    /// 是否从响应中解析到用量。
    pub has_usage: bool,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    /// 缓存写细分：5 分钟 / 1 小时档。
    pub cache_5m_tokens: Option<i64>,
    pub cache_1h_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub unified_status: Option<String>,
    pub rl_5h_status: Option<String>,
    pub rl_5h_reset: Option<i64>,
    pub rl_5h_utilization: Option<f64>,
    pub rl_7d_status: Option<String>,
    pub rl_7d_reset: Option<i64>,
    pub rl_7d_utilization: Option<f64>,
    pub rl_representative: Option<String>,
    pub ratelimit_raw: Option<String>,
    /// 等价 API 费用（USD）。
    pub cost_usd: Option<f64>,
}

/// 一条落库后的用量日志（读取用）。
#[derive(Debug, serde::Serialize)]
pub struct UsageLog {
    pub id: i64,
    pub ts: i64,
    pub cred_id: Option<i64>,
    pub cred_label: String,
    pub device_id: Option<String>,
    pub model: Option<String>,
    pub path: String,
    pub status: u16,
    pub has_usage: bool,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_5m_tokens: Option<i64>,
    pub cache_1h_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub unified_status: Option<String>,
    pub rl_5h_status: Option<String>,
    pub rl_5h_reset: Option<i64>,
    pub rl_5h_utilization: Option<f64>,
    pub rl_7d_status: Option<String>,
    pub rl_7d_reset: Option<i64>,
    pub rl_7d_utilization: Option<f64>,
    pub rl_representative: Option<String>,
    pub ratelimit_raw: Option<String>,
    pub cost_usd: Option<f64>,
}

/// 单个凭证最新一次的额度快照（用于凭证卡片展示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuotaSnapshot {
    /// 该快照对应的请求时间（Unix 秒）。
    pub ts: i64,
    pub unified_status: Option<String>,
    pub rl_5h_utilization: Option<f64>,
    pub rl_5h_reset: Option<i64>,
    pub rl_7d_utilization: Option<f64>,
    pub rl_7d_reset: Option<i64>,
    pub rl_representative: Option<String>,
    /// 当前 5h / 7d 窗口内该凭证已用的等价费用（USD）。窗口起点由对应 reset 反推。
    pub cost_5h: Option<f64>,
    pub cost_7d: Option<f64>,
}

/// 一条设备绑定明细（凭证卡片展开「已绑定设备」时展示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceBinding {
    /// 客户端 `metadata.user_id` 里的原始 device_id（非伪装后的那个）。
    pub device_id: String,
    /// 该设备经此凭证转发过的累计请求数。
    pub request_count: i64,
    /// 首次绑定到该凭证的时间（Unix 秒）。
    pub created_at: i64,
    /// 最近一次活跃时间（Unix 秒）；TTL 就是按它算的。
    pub last_seen_at: i64,
}

/// 5 小时窗口秒数。
const WINDOW_5H_SECS: i64 = 5 * 3600;
/// 7 天窗口秒数。
const WINDOW_7D_SECS: i64 = 7 * 24 * 3600;

impl CredentialStore {
    /// 每个凭证「最新一条带限流信息」的额度快照（cred_id → 快照），
    /// 并附带当前 5h / 7d 窗口内的累计费用。
    ///
    /// 借助 SQLite 的特性：`MAX(ts)` 存在时，同 SELECT 里的裸列取自该最大行。
    pub fn latest_quotas(&self) -> Result<HashMap<i64, QuotaSnapshot>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT cred_id, MAX(ts), unified_status,
                    rl_5h_utilization, rl_5h_reset, rl_7d_utilization, rl_7d_reset, rl_representative
               FROM usage_logs
              WHERE cred_id IS NOT NULL
                AND (rl_5h_utilization IS NOT NULL OR rl_7d_utilization IS NOT NULL)
              GROUP BY cred_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                QuotaSnapshot {
                    ts: r.get(1)?,
                    unified_status: r.get(2)?,
                    rl_5h_utilization: r.get(3)?,
                    rl_5h_reset: r.get(4)?,
                    rl_7d_utilization: r.get(5)?,
                    rl_7d_reset: r.get(6)?,
                    rl_representative: r.get(7)?,
                    cost_5h: None,
                    cost_7d: None,
                },
            ))
        })?;
        let mut list: Vec<(i64, QuotaSnapshot)> = Vec::new();
        for row in rows {
            list.push(row?);
        }
        drop(stmt);

        // 逐凭证累加各窗口起点(reset - 窗口时长)以来的费用。
        let mut cost_stmt = conn.prepare(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_logs
              WHERE cred_id = ?1 AND ts >= ?2",
        )?;
        let mut out = HashMap::new();
        for (cid, mut q) in list {
            if let Some(reset) = q.rl_5h_reset {
                q.cost_5h =
                    Some(cost_stmt.query_row(params![cid, reset - WINDOW_5H_SECS], |r| r.get(0))?);
            }
            if let Some(reset) = q.rl_7d_reset {
                q.cost_7d =
                    Some(cost_stmt.query_row(params![cid, reset - WINDOW_7D_SECS], |r| r.get(0))?);
            }
            out.insert(cid, q);
        }
        Ok(out)
    }

    /// 每个凭证最近一次被使用（有转发记录）的时间（cred_id → Unix 秒）。
    pub fn last_used(&self) -> Result<HashMap<i64, i64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT cred_id, MAX(ts) FROM usage_logs
              WHERE cred_id IS NOT NULL GROUP BY cred_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, ts) = row?;
            out.insert(cid, ts);
        }
        Ok(out)
    }

    /// 每个凭证累计的等价 API 费用（cred_id → USD 合计）。
    pub fn cost_by_cred(&self) -> Result<HashMap<i64, f64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT cred_id, COALESCE(SUM(cost_usd), 0) FROM usage_logs
              WHERE cred_id IS NOT NULL GROUP BY cred_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, sum) = row?;
            out.insert(cid, sum);
        }
        Ok(out)
    }

    /// 写入一条用量日志。
    pub fn insert_usage_log(&self, rec: &UsageRecord) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO usage_logs
                (cred_id, cred_label, device_id, model, path, status, has_usage,
                 input_tokens, output_tokens, cache_creation_tokens, cache_5m_tokens,
                 cache_1h_tokens, cache_read_tokens, ttft_ms, total_ms,
                 unified_status, rl_5h_status, rl_5h_reset, rl_5h_utilization,
                 rl_7d_status, rl_7d_reset, rl_7d_utilization, rl_representative, ratelimit_raw,
                 cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)",
            params![
                rec.cred_id,
                rec.cred_label,
                rec.device_id,
                rec.model,
                rec.path,
                rec.status as i64,
                rec.has_usage as i64,
                rec.input_tokens,
                rec.output_tokens,
                rec.cache_creation_tokens,
                rec.cache_5m_tokens,
                rec.cache_1h_tokens,
                rec.cache_read_tokens,
                rec.ttft_ms,
                rec.total_ms,
                rec.unified_status,
                rec.rl_5h_status,
                rec.rl_5h_reset,
                rec.rl_5h_utilization,
                rec.rl_7d_status,
                rec.rl_7d_reset,
                rec.rl_7d_utilization,
                rec.rl_representative,
                rec.ratelimit_raw,
                rec.cost_usd,
            ],
        )?;
        Ok(())
    }

    /// 最近的用量日志，按时间倒序，最多 `limit` 条。
    pub fn list_usage_logs(&self, limit: i64) -> Result<Vec<UsageLog>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ts, cred_id, cred_label, device_id, model, path, status, has_usage,
                    input_tokens, output_tokens, cache_creation_tokens, cache_5m_tokens,
                    cache_1h_tokens, cache_read_tokens, ttft_ms, total_ms,
                    unified_status, rl_5h_status, rl_5h_reset, rl_5h_utilization,
                    rl_7d_status, rl_7d_reset, rl_7d_utilization, rl_representative, ratelimit_raw,
                    cost_usd
               FROM usage_logs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(UsageLog {
                id: r.get(0)?,
                ts: r.get(1)?,
                cred_id: r.get(2)?,
                cred_label: r.get(3)?,
                device_id: r.get(4)?,
                model: r.get(5)?,
                path: r.get(6)?,
                status: r.get::<_, i64>(7)? as u16,
                has_usage: r.get::<_, i64>(8)? != 0,
                input_tokens: r.get(9)?,
                output_tokens: r.get(10)?,
                cache_creation_tokens: r.get(11)?,
                cache_5m_tokens: r.get(12)?,
                cache_1h_tokens: r.get(13)?,
                cache_read_tokens: r.get(14)?,
                ttft_ms: r.get(15)?,
                total_ms: r.get(16)?,
                unified_status: r.get(17)?,
                rl_5h_status: r.get(18)?,
                rl_5h_reset: r.get(19)?,
                rl_5h_utilization: r.get(20)?,
                rl_7d_status: r.get(21)?,
                rl_7d_reset: r.get(22)?,
                rl_7d_utilization: r.get(23)?,
                rl_representative: r.get(24)?,
                ratelimit_raw: r.get(25)?,
                cost_usd: r.get(26)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS credentials (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
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
            ON device_bindings(cred_id);

        -- 每次转发的用量日志：从上游响应里嗅探到的 token 用量（若响应带了 usage）。
        CREATE TABLE IF NOT EXISTS usage_logs (
            id             INTEGER PRIMARY KEY,
            ts             INTEGER NOT NULL DEFAULT (unixepoch()),
            cred_id        INTEGER,
            cred_label     TEXT    NOT NULL DEFAULT '',
            device_id      TEXT,
            model          TEXT,
            path           TEXT    NOT NULL DEFAULT '',
            status         INTEGER NOT NULL DEFAULT 0,
            -- 是否从响应中解析到用量（1/0）；未解析到时下面各 token 列为空。
            has_usage      INTEGER NOT NULL DEFAULT 0 CHECK (has_usage IN (0,1)),
            input_tokens          INTEGER,
            output_tokens         INTEGER,
            cache_creation_tokens INTEGER,
            cache_5m_tokens       INTEGER,
            cache_1h_tokens       INTEGER,
            cache_read_tokens     INTEGER,
            ttft_ms        INTEGER,
            total_ms       INTEGER,
            -- 订阅账号限流（anthropic-ratelimit-unified-*）：状态/额度重置时刻/使用率。
            unified_status     TEXT,
            rl_5h_status       TEXT,
            rl_5h_reset        INTEGER,
            rl_5h_utilization  REAL,
            rl_7d_status       TEXT,
            rl_7d_reset        INTEGER,
            rl_7d_utilization  REAL,
            rl_representative  TEXT,
            -- 原始限流头（兜底：字段变化时仍可回看）。
            ratelimit_raw      TEXT,
            -- 按官方定价估算的等价 API 费用（USD）；模型未知时为空。
            cost_usd           REAL
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_usage_logs_ts   ON usage_logs(ts);
        CREATE INDEX IF NOT EXISTS idx_usage_logs_cred ON usage_logs(cred_id);",
    )
    .context("初始化凭证库 schema 失败")?;

    // 兼容旧 usage_logs：逐列幂等新增（已存在则忽略 duplicate column）。
    for col in [
        "unified_status TEXT",
        "rl_5h_status TEXT",
        "rl_5h_reset INTEGER",
        "rl_5h_utilization REAL",
        "rl_7d_status TEXT",
        "rl_7d_reset INTEGER",
        "rl_7d_utilization REAL",
        "rl_representative TEXT",
        "ratelimit_raw TEXT",
        "cost_usd REAL",
        "cache_5m_tokens INTEGER",
        "cache_1h_tokens INTEGER",
    ] {
        let _ = conn.execute(&format!("ALTER TABLE usage_logs ADD COLUMN {col}"), []);
    }

    // 兼容旧库：新增列时若已存在会报 duplicate column，忽略即可（幂等）。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN tier TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE credentials ADD COLUMN device_limit INTEGER NOT NULL DEFAULT 0",
        [],
    );
    // 自动检测到的上游账号级错误原因（如封号）；NULL 表示未被自动停用，
    // 与管理员手动停用（disabled=1 且本字段为空）区分开。见 `mark_banned`。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN ban_reason TEXT", []);
    // 账号 UUID（profile.account.uuid）；转发身份伪装用。旧库为空，刷新 token 时回填。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN account_uuid TEXT", []);

    // 迁移：credentials.id 改为 AUTOINCREMENT。旧表（无 AUTOINCREMENT）删掉最大 id 的行后
    // 会回收复用该 id，令新账号错误继承被删账号的历史用量（usage_logs 按 cred_id 关联、
    // 删号时不清理）。此处须在上面所有 ADD COLUMN 之后执行，确保重建时列已齐全。
    migrate_credentials_autoincrement(conn)?;

    // 清理旧库遗留的无主历史数据（此前删号只清 device_bindings，用量日志留了下来）。
    purge_orphan_rows(conn)?;
    Ok(())
}

/// 清扫 cred_id 已指向不存在账号的行（用量日志 + 设备绑定）。
///
/// 旧版删号不清 `usage_logs`，被删账号的历史记录会一直留在库里：后台请求日志里显示为
/// 无主行、费用/额度统计也仍会按 cred_id 聚合到它们。开机时做一次清扫补上这段历史欠账；
/// 删号路径（[`CredentialStore::delete`]）已同步清理，故对新库是 no-op。
///
/// `cred_id IS NULL` 的日志（尚未选到凭证就失败的请求）不属于任何账号，保留。
fn purge_orphan_rows(conn: &Connection) -> Result<()> {
    let logs = conn
        .execute(
            "DELETE FROM usage_logs
              WHERE cred_id IS NOT NULL AND cred_id NOT IN (SELECT id FROM credentials)",
            [],
        )
        .context("清理无主用量日志失败")?;
    let binds = conn
        .execute(
            "DELETE FROM device_bindings WHERE cred_id NOT IN (SELECT id FROM credentials)",
            [],
        )
        .context("清理无主设备绑定失败")?;
    if logs > 0 || binds > 0 {
        tracing::info!(usage_logs = logs, device_bindings = binds, "已清理被删账号遗留的历史数据");
    }
    Ok(())
}

/// 若 `credentials` 仍是非 AUTOINCREMENT 的旧表，则原地重建为 AUTOINCREMENT 主键。
/// 幂等：DDL 已含 AUTOINCREMENT 时直接返回。保留所有既有行与其 id——AUTOINCREMENT 会据
/// 当前 `MAX(id)` 播种 `sqlite_sequence`，此后新 id 严格递增、永不回收，杜绝历史错配。
fn migrate_credentials_autoincrement(conn: &Connection) -> Result<()> {
    let ddl: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'credentials'",
        [],
        |r| r.get(0),
    )?;
    if ddl.contains("AUTOINCREMENT") {
        return Ok(());
    }
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE credentials_new (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            label         TEXT    NOT NULL DEFAULT '',
            tier          TEXT,
            access_token  TEXT    NOT NULL,
            refresh_token TEXT    NOT NULL,
            expires_at    INTEGER NOT NULL,
            priority      INTEGER NOT NULL DEFAULT 0,
            disabled      INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0,1)),
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            device_limit  INTEGER NOT NULL DEFAULT 0,
            ban_reason    TEXT,
            account_uuid  TEXT
         ) STRICT;
         INSERT INTO credentials_new
             (id, label, tier, access_token, refresh_token, expires_at, priority,
              disabled, created_at, updated_at, device_limit, ban_reason, account_uuid)
         SELECT id, label, tier, access_token, refresh_token, expires_at, priority,
              disabled, created_at, updated_at, device_limit, ban_reason, account_uuid
         FROM credentials;
         DROP TABLE credentials;
         ALTER TABLE credentials_new RENAME TO credentials;
         CREATE UNIQUE INDEX IF NOT EXISTS uq_credentials_refresh_token
             ON credentials(refresh_token);
         CREATE INDEX IF NOT EXISTS idx_credentials_priority
             ON credentials(priority, id);
         COMMIT;",
    )
    .context("迁移 credentials 为 AUTOINCREMENT 失败")?;
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
        ban_reason: row.get(11)?,
        account_uuid: row.get(12)?,
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
        // 全局默认上限须在取锁前读（内部自己会取锁，parking_lot 不可重入）。
        let default_limit = self.default_device_limit();
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
        // 生效上限：账号未单独配置（device_limit == 0）时套用全局默认。
        let limit_of = |c: &Credential| effective_device_limit(c.device_limit, default_limit);

        // 3/4) 优先级分档调度：优先级为主键（数值小者优先），同一档内再按设备数
        //      负载均衡，最后 id 兜底。低优先级档仅在高优先级档全部占满/不可用后才触及。
        let chosen = if device_id.is_some() {
            // 硬限制：仅在仍有名额者（生效上限 <=0 不限，或 used<上限）中选；
            // 当前优先级档全满时其成员被过滤掉，min 自然溢出到下一档；全部满则拒绝。
            match creds
                .iter()
                .filter(|c| limit_of(c) <= 0 || used(c) < limit_of(c))
                .min_by_key(|c| (c.priority, used(c), c.id))
            {
                Some(c) => c,
                None => return Err(DeviceLimitReached.into()),
            }
        } else {
            // 无 device_id：不占名额、不受限，按优先级档 + 档内负载均衡挑一个（creds 已保证非空）。
            creds
                .iter()
                .min_by_key(|c| (c.priority, used(c), c.id))
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

/// 刷新失败后最多改选几个凭证。
///
/// 每失败一轮就停用一个凭证（可用池严格变小），循环必然收敛；这个上限只是防御性兜底，
/// 免得停用没生效时打成死循环。也顺带给单次请求的耗时封了顶——每一轮都是一次上游往返。
const MAX_REFRESH_FAILOVER: usize = 5;

/// 一次「拿到该凭证可用 access_token」的尝试结果。可重试的错误（网络抖动、5xx、限流）
/// 走 `Err` 直接冒泡，不在这里表达。
enum TokenAttempt {
    /// 拿到可用 access_token。
    Ready(String),
    /// 该凭证的 refresh_token 已被上游永久作废，重试没有意义——外层会停用它并改选其它号。
    /// 携带写入 `ban_reason` 的原因。
    Revoked(String),
}

/// 代理转发使用：按 device_id 粘性选出凭证并返回 (access_token, 该凭证)（必要时刷新）。
///
/// 选择见 [`CredentialStore::select_for_device`]。若命中的凭证进入刷新窗口，
/// 则调用 OAuth 刷新并回写。注意刷新是异步 IO，不持有 DB 锁。
///
/// **刷新失败要自动换号**：`select_for_device` 在返回前就写好了设备绑定，之后才轮到刷新。
/// 若刷新失败直接把错误抛出去，这个设备就被钉死在坏号上——绑定还在，下一次请求照样选中它，
/// 永远 503 直到人工介入。故这里在「refresh_token 已被作废」时停用该凭证
/// （[`CredentialStore::mark_banned`] 会连带清掉它的设备绑定），再重选一个号继续。
/// 网络抖动/5xx 这类可重试错误**不**停用，原样抛出，让客户端重试时还落回同一个号。
pub async fn valid_access_token_for_device(
    store: &CredentialStore,
    http: &reqwest::Client,
    device_id: Option<&str>,
) -> Result<(String, Credential)> {
    select_with_refresh_failover(store, device_id, |cred| {
        Box::pin(async move { ensure_fresh_token(store, http, &cred).await })
    })
    .await
}

/// [`select_with_refresh_failover`] 注入的「取一次 token」返回的 future。
///
/// 写成显式 boxed future 而不是 `impl AsyncFn`：后者的 `CallRefFuture` 带高阶生命周期，
/// 会让捕获了 `&CredentialStore`/`&reqwest::Client` 的闭包推不出 `Send`
/// （报 `implementation of Send is not general enough`），而这条链最终要塞进 axum handler。
/// 固定成单个 `'a` 就没有这个问题；代价是每轮一次 Box 分配，紧挨着一次上游往返，可忽略。
type AttemptFut<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<TokenAttempt>> + Send + 'a>>;

/// [`valid_access_token_for_device`] 的重选循环本体。把「取 token」这一步抽成参数注入，
/// 是为了让换号逻辑本身能脱离网络被测到——这段逻辑此前不存在（刷新失败直接抛错），
/// 设备会被钉死在坏号上，属于只在生产才暴露的那类 bug，必须有回归测试盯着。
///
/// `attempt` 收 `Credential` 而非 `&Credential`：按值传就不会让返回的 future 借用参数，
/// `AttemptFut<'a>` 里那个 `'a` 才能是固定的。
async fn select_with_refresh_failover<'a>(
    store: &CredentialStore,
    device_id: Option<&str>,
    attempt: impl Fn(Credential) -> AttemptFut<'a>,
) -> Result<(String, Credential)> {
    let ttl = store.device_binding_ttl();

    for round in 0..MAX_REFRESH_FAILOVER {
        // 每轮都重新选：上一轮停用的那个已被排除，且它的设备绑定已清，这里才会换到新号。
        let cred = store.select_for_device(device_id, ttl)?;
        match attempt(cred.clone()).await? {
            TokenAttempt::Ready(token) => return Ok((token, cred)),
            TokenAttempt::Revoked(reason) => {
                tracing::warn!(
                    cred = format!("#{} {}", cred.id, cred.label),
                    round,
                    reason = %reason,
                    "refresh_token 已被上游作废，停用该凭证并改选其它账号"
                );
                // 停用没生效就必须中止：否则下一轮还会选中同一个号，白转满 MAX_REFRESH_FAILOVER 圈。
                if !store.mark_banned(cred.id, &reason)? {
                    anyhow::bail!("凭证 #{} 刷新失败且停用未生效：{reason}", cred.id);
                }
            }
        }
    }

    anyhow::bail!("连续 {MAX_REFRESH_FAILOVER} 个凭证刷新失败，暂无可用账号")
}

/// 取该凭证的可用 access_token，未进入刷新窗口就直接复用，否则刷新并回写。
///
/// 刷新走该凭证的专属锁 + 双重检查：上游刷新会轮换 refresh_token，并发刷新中后完成的那次
/// 会把已作废的 token 写回库，导致该凭证之后所有刷新都 `invalid_grant`（账号被自己废掉）。
/// 拿到锁后重新读库，若他人已刷好则直接复用，不再多打一次刷新。
async fn ensure_fresh_token(
    store: &CredentialStore,
    http: &reqwest::Client,
    cred: &Credential,
) -> Result<TokenAttempt> {
    if !cred.needs_refresh() {
        return Ok(TokenAttempt::Ready(cred.access_token.clone()));
    }

    let lock = store.refresh_lock(cred.id);
    let _guard = lock.lock().await;
    // 双重检查：等锁期间可能已被其它请求刷新过。
    let cred = store.get(cred.id)?.unwrap_or_else(|| cred.clone());
    if !cred.needs_refresh() {
        tracing::debug!(id = cred.id, "等锁期间该凭证已被刷新，复用新 token");
        return Ok(TokenAttempt::Ready(cred.access_token));
    }

    tracing::info!(id = cred.id, label = %cred.label, "凭证进入刷新窗口，刷新 token");
    let err = match crate::oauth::refresh(http, &cred.refresh_token).await {
        Ok(tokens) => {
            store.update_tokens(
                cred.id,
                &tokens.access_token,
                &tokens.refresh_token,
                tokens.expires_at,
            )?;
            return Ok(TokenAttempt::Ready(tokens.access_token));
        }
        Err(e) => e,
    };

    // 无论是否判定为永久失效，都把失败原文打出来：这个端点的失败响应形态我们没有实测样本，
    // 线上真出现一次就能据此收紧 `is_grant_revoked`。
    tracing::warn!(id = cred.id, label = %cred.label, error = %err, "刷新 token 失败");
    match err.downcast_ref::<crate::oauth::TokenEndpointError>() {
        Some(te) if te.is_grant_revoked() => Ok(TokenAttempt::Revoked(te.ban_reason())),
        // 网络抖动 / 5xx / 限流 / 非 invalid_grant 的 4xx：凭证本身可能是好的，不停用。
        _ => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧库（无 AUTOINCREMENT）经 init_schema 迁移后，删号腾出的 id 不再被复用。
    #[test]
    fn migrates_and_stops_id_reuse() {
        let conn = Connection::open_in_memory().unwrap();
        // 造一张旧表：非 AUTOINCREMENT，且只含早期列（模拟老库，后续列靠 ALTER 补）。
        conn.execute_batch(
            "CREATE TABLE credentials (
                id INTEGER PRIMARY KEY,
                label TEXT NOT NULL DEFAULT '',
                tier TEXT,
                access_token TEXT NOT NULL,
                refresh_token TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                priority INTEGER NOT NULL DEFAULT 0,
                disabled INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )
        .unwrap();
        for (id, tok) in [(1, "a"), (2, "b"), (3, "c")] {
            conn.execute(
                "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
                 VALUES (?1, ?2, ?3, 0)",
                params![id, tok, format!("r{tok}")],
            )
            .unwrap();
        }

        init_schema(&conn).unwrap();

        let ddl: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name = 'credentials'", [], |r| r.get(0))
            .unwrap();
        assert!(ddl.contains("AUTOINCREMENT"), "迁移后应为 AUTOINCREMENT");

        // 既有行与其 id 全部保留（迁移把 sqlite_sequence 播种为 MAX(id)=3）。
        let cnt: i64 = conn.query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get(0)).unwrap();
        assert_eq!(cnt, 3);

        // 迁移后删掉最大 id，新插入应得 4，而非复用被删的 3。
        conn.execute("DELETE FROM credentials WHERE id = 3", []).unwrap();
        conn.execute(
            "INSERT INTO credentials (access_token, refresh_token, expires_at) VALUES ('d','rd',0)",
            [],
        )
        .unwrap();
        assert_eq!(conn.last_insert_rowid(), 4, "AUTOINCREMENT 不应复用被删的 id=3");

        // 迁移后再次 init_schema 必须是无副作用的 no-op（RENAME 后 DDL 仍含 AUTOINCREMENT，
        // 不应二次重建而丢数据）。
        init_schema(&conn).unwrap();
        let after: i64 = conn.query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get(0)).unwrap();
        assert_eq!(after, 3, "二次 init_schema 不应改动数据");
    }

    /// 开机清扫：被删账号遗留的用量日志/设备绑定被清掉，在册账号与无主(NULL)日志保留。
    #[test]
    fn purges_history_of_deleted_credentials() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
             VALUES (1, 'a', 'ra', 0)",
            [],
        )
        .unwrap();
        // 账号 2 已被（旧版逻辑）删掉，但历史数据还在。
        for cid in ["1", "2", "NULL"] {
            conn.execute(&format!("INSERT INTO usage_logs (cred_id) VALUES ({cid})"), [])
                .unwrap();
        }
        for (did, cid) in [("d1", 1), ("d2", 2)] {
            conn.execute(
                "INSERT INTO device_bindings (device_id, cred_id) VALUES (?1, ?2)",
                params![did, cid],
            )
            .unwrap();
        }

        purge_orphan_rows(&conn).unwrap();

        let logs: Vec<Option<i64>> = conn
            .prepare("SELECT cred_id FROM usage_logs ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(logs, vec![Some(1), None], "只应清掉已删账号(2)的日志");
        let binds: Vec<i64> = conn
            .prepare("SELECT cred_id FROM device_bindings")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(binds, vec![1]);
    }

    /// 删号连带清掉该账号的用量日志与设备绑定，其它账号的历史不受影响。
    #[test]
    fn delete_cascades_usage_logs() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap();
        {
            let conn = store.conn.lock();
            for cid in [a.id, b.id] {
                conn.execute("INSERT INTO usage_logs (cred_id) VALUES (?1)", [cid]).unwrap();
            }
            conn.execute(
                "INSERT INTO device_bindings (device_id, cred_id) VALUES ('d1', ?1)",
                [a.id],
            )
            .unwrap();
        }

        assert!(store.delete(a.id).unwrap());

        let conn = store.conn.lock();
        let logs: Vec<i64> = conn
            .prepare("SELECT cred_id FROM usage_logs")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(logs, vec![b.id], "被删账号的用量日志应一并清掉");
        let binds: i64 =
            conn.query_row("SELECT COUNT(*) FROM device_bindings", [], |r| r.get(0)).unwrap();
        assert_eq!(binds, 0);
    }

    /// 设备上限三态：账号独立值覆盖全局，0 跟随全局，负值明确不限。
    #[test]
    fn effective_device_limit_tri_state() {
        assert_eq!(effective_device_limit(3, 5), 3, "账号独立上限覆盖全局");
        assert_eq!(effective_device_limit(0, 5), 5, "未配置则跟随全局默认");
        assert_eq!(effective_device_limit(0, 0), 0, "全局也不限时不限");
        assert_eq!(effective_device_limit(-1, 5), 0, "账号明确不限，忽略全局默认");
    }

    /// 新增账号一律落在 P0；批量改优先级把选中的账号统一调档、其余不动。
    #[test]
    fn insert_defaults_to_p0_and_batch_priority() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap();
        let c = store.insert("c", None, "tc", "rc", 0, None).unwrap();
        assert_eq!((a.priority, b.priority, c.priority), (0, 0, 0), "新账号都应是 P0");

        assert_eq!(store.set_priorities(&[a.id, c.id], 2).unwrap(), 2);
        let by_id: HashMap<i64, i64> =
            store.list().unwrap().into_iter().map(|x| (x.id, x.priority)).collect();
        assert_eq!(by_id[&a.id], 2);
        assert_eq!(by_id[&c.id], 2);
        assert_eq!(by_id[&b.id], 0, "未选中的账号不应被改动");
        assert_eq!(store.set_priorities(&[], 9).unwrap(), 0, "空列表为 no-op");
    }

    /// 批量启停 / 设备上限 / 删除：只作用于选中的 id，且各自保持单账号接口的语义。
    #[test]
    fn batch_ops_only_touch_selected() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap();
        let c = store.insert("c", None, "tc", "rc", 0, None).unwrap();
        // 给 a、b 各造一条设备绑定与用量日志，验证连带清理。
        {
            let conn = store.conn.lock();
            for (did, cid) in [("d1", a.id), ("d2", b.id)] {
                conn.execute(
                    "INSERT INTO device_bindings (device_id, cred_id) VALUES (?1, ?2)",
                    params![did, cid],
                )
                .unwrap();
                conn.execute("INSERT INTO usage_logs (cred_id) VALUES (?1)", [cid]).unwrap();
            }
        }

        // 批量停用 a、b：c 不受影响；停用会清掉被选中账号的设备绑定。
        assert_eq!(store.set_disabled_many(&[a.id, b.id], true).unwrap(), 2);
        let by_id = |s: &CredentialStore| -> HashMap<i64, Credential> {
            s.list().unwrap().into_iter().map(|x| (x.id, x)).collect()
        };
        let m = by_id(&store);
        assert!(m[&a.id].disabled && m[&b.id].disabled);
        assert!(!m[&c.id].disabled, "未选中的账号不应被停用");
        {
            let conn = store.conn.lock();
            let n: i64 =
                conn.query_row("SELECT COUNT(*) FROM device_bindings", [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "停用应清掉这两个账号的设备绑定");
        }

        // 批量启用要清 ban_reason（模拟先被自动封禁）。
        store.mark_banned(a.id, "banned").unwrap();
        assert!(by_id(&store)[&a.id].ban_reason.is_some());
        assert_eq!(store.set_disabled_many(&[a.id], false).unwrap(), 1);
        let m = by_id(&store);
        assert!(!m[&a.id].disabled && m[&a.id].ban_reason.is_none(), "启用应清除封禁原因");

        // 批量设备上限：负值由 web 层收敛，这里验证按传入值原样落库。
        assert_eq!(store.set_device_limits(&[a.id, c.id], 5).unwrap(), 2);
        let m = by_id(&store);
        assert_eq!((m[&a.id].device_limit, m[&c.id].device_limit), (5, 5));
        assert_eq!(m[&b.id].device_limit, 0, "未选中的账号不应被改动");

        // 批量删除：连带清用量日志，未选中的账号及其日志保留。
        assert_eq!(store.delete_many(&[a.id]).unwrap(), 1);
        let m = by_id(&store);
        assert!(!m.contains_key(&a.id) && m.contains_key(&b.id) && m.contains_key(&c.id));
        {
            let conn = store.conn.lock();
            let logs: Vec<i64> = conn
                .prepare("SELECT cred_id FROM usage_logs")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(logs, vec![b.id], "只应清掉被删账号的用量日志");
        }

        // 空列表一律 no-op，不误伤全表。
        assert_eq!(store.delete_many(&[]).unwrap(), 0);
        assert_eq!(store.set_disabled_many(&[], true).unwrap(), 0);
        assert_eq!(store.set_device_limits(&[], 9).unwrap(), 0);
        assert_eq!(store.list().unwrap().len(), 2, "空列表操作不应改动任何账号");
    }

    /// 刷新失败自动换号所依赖的那一步：停用坏号后，原本绑在它上面的设备必须能改选到别的号。
    ///
    /// `select_for_device` 会优先命中既有绑定，所以只是「不再选中被停用的号」还不够——
    /// `mark_banned` 必须把它的 device_bindings 一并清掉，否则设备被钉死在坏号上，
    /// [`valid_access_token_for_device`] 的重选循环会一直选回同一个，白转满上限。
    #[test]
    fn banned_credential_releases_its_devices() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap();

        // 先把设备粘到 a 上（a 是 id 更小的那个，同优先级下会被先选中）。
        let first = store.select_for_device(Some("dev-1"), 0).unwrap();
        assert_eq!(first.id, a.id);
        // 再选一次仍命中既有绑定，确认粘性生效——这正是坏号会把设备钉死的原因。
        assert_eq!(store.select_for_device(Some("dev-1"), 0).unwrap().id, a.id);

        // 模拟「a 的 refresh_token 被作废」后的停用。
        assert!(store.mark_banned(a.id, "[refresh 400] invalid_grant").unwrap());

        // 重选必须换到 b，而不是继续返回 a 或直接报错。
        let after = store.select_for_device(Some("dev-1"), 0).unwrap();
        assert_eq!(after.id, b.id, "停用坏号后设备应改选到其它账号");

        // a 确实被停用并记了原因。
        let a2 = store.get(a.id).unwrap().unwrap();
        assert!(a2.disabled);
        assert_eq!(a2.ban_reason.as_deref(), Some("[refresh 400] invalid_grant"));

        // 池子空了要报错，而不是把停用的号又选回来。
        assert!(store.mark_banned(b.id, "[refresh 400] invalid_grant").unwrap());
        assert!(store.select_for_device(Some("dev-1"), 0).is_err(), "无可用凭证时应报错");
    }

    /// 设备明细必须与设备数同口径：条数等于 `device_count`、只含本凭证的绑定、
    /// 超过 TTL 未活跃的不出现。否则后台会显示「设备 1/3，展开却列出 2 台」。
    #[test]
    fn list_devices_matches_device_count() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);

        // dev-1 粘到 a（同优先级下 id 小者先中），再来一次命中既有绑定、请求数 +1。
        assert_eq!(store.select_for_device(Some("dev-1"), 0).unwrap().id, a);
        assert_eq!(store.select_for_device(Some("dev-1"), 0).unwrap().id, a);
        // dev-2 是新设备：a 已有 1 台、b 还是 0 台，负载均衡会把它分给 b。
        assert_eq!(store.select_for_device(Some("dev-2"), 0).unwrap().id, b);

        let a_devs = store.list_devices(a).unwrap();
        assert_eq!(a_devs.len() as i64, store.device_count(a).unwrap(), "条数应等于设备数");
        assert_eq!(a_devs.len(), 1, "只应列出绑到 a 的设备");
        assert_eq!(a_devs[0].device_id, "dev-1");
        assert_eq!(a_devs[0].request_count, 1, "第二次命中既有绑定应计数");
        assert_eq!(store.list_devices(b).unwrap()[0].device_id, "dev-2");

        // 把 dev-1 的活跃时间推到 TTL 之外：明细与计数应同步把它排除。
        store.set_setting(DEVICE_BINDING_TTL, "60").unwrap();
        store
            .conn
            .lock()
            .execute(
                "UPDATE device_bindings SET last_seen_at = unixepoch() - 600 WHERE device_id = ?1",
                ["dev-1"],
            )
            .unwrap();
        assert_eq!(store.device_count(a).unwrap(), 0);
        assert!(store.list_devices(a).unwrap().is_empty(), "超时绑定不应出现在明细里");
    }

    fn store_with(labels: &[&str]) -> (CredentialStore, Vec<i64>) {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let ids = labels
            .iter()
            // refresh_token 有 UNIQUE 约束，按 label 取值保证互不相同。
            .map(|l| store.insert(l, None, &format!("tok-{l}"), &format!("refresh-{l}"), 0, None).unwrap().id)
            .collect();
        (store, ids)
    }

    const REVOKED: &str = "[refresh 400] invalid_grant";

    /// 刷新失败要自动换号：坏号被停用、设备改绑到下一个可用号，请求正常拿到 token。
    ///
    /// 这是本次修复的核心——此前刷新失败直接抛错，而设备绑定在选号时就已写库，
    /// 导致该设备永远选回同一个坏号、永远 503。
    #[tokio::test]
    async fn refresh_failure_fails_over_to_next_credential() {
        let (store, ids) = store_with(&["a", "b", "c"]);
        let tried = std::cell::RefCell::new(Vec::new());

        // a、b 的 refresh_token 已作废，c 正常。
        let (token, cred) = select_with_refresh_failover(&store, Some("dev-1"), |c| {
            tried.borrow_mut().push(c.id);
            Box::pin(async move {
                Ok(if c.label == "c" {
                    TokenAttempt::Ready("good-token".into())
                } else {
                    TokenAttempt::Revoked(REVOKED.into())
                })
            })
        })
        .await
        .unwrap();

        assert_eq!(token, "good-token");
        assert_eq!(cred.id, ids[2], "应换到第一个刷新得动的号");
        assert_eq!(*tried.borrow(), ids, "应按优先级依次试过 a、b、c");

        // a、b 被停用并记了原因；c 不受影响。
        for id in &ids[..2] {
            let c = store.get(*id).unwrap().unwrap();
            assert!(c.disabled, "作废的号应被停用");
            assert_eq!(c.ban_reason.as_deref(), Some(REVOKED));
        }
        assert!(!store.get(ids[2]).unwrap().unwrap().disabled);

        // 设备最终绑在 c 上，后续请求直接命中它，不再重走换号。
        assert_eq!(store.select_for_device(Some("dev-1"), 0).unwrap().id, ids[2]);
    }

    /// 可重试错误（网络抖动、5xx、限流）**不得**停用凭证——误停一个健康账号的代价，
    /// 远高于让客户端重试一次。
    #[tokio::test]
    async fn transient_refresh_error_does_not_disable() {
        let (store, ids) = store_with(&["a", "b"]);
        let calls = std::cell::Cell::new(0);

        let e = select_with_refresh_failover(&store, Some("dev-1"), |_| {
            calls.set(calls.get() + 1);
            Box::pin(async { anyhow::bail!("请求 token 端点失败: connection reset") })
        })
        .await
        .unwrap_err();

        assert!(e.to_string().contains("connection reset"), "应原样抛出底层错误: {e}");
        assert_eq!(calls.get(), 1, "可重试错误应立即返回，不该继续换号");
        for id in &ids {
            assert!(!store.get(*id).unwrap().unwrap().disabled, "可重试错误不得停用凭证");
        }
        // 绑定保留，客户端重试时仍落回同一个号。
        assert_eq!(store.select_for_device(Some("dev-1"), 0).unwrap().id, ids[0]);
    }

    /// 所有号的 refresh_token 都作废时要报错收场，不能死循环、也不能返回停用的号。
    #[tokio::test]
    async fn all_credentials_revoked_gives_up() {
        let (store, ids) = store_with(&["a", "b"]);
        let tried = std::cell::RefCell::new(Vec::new());

        let e = select_with_refresh_failover(&store, Some("dev-1"), |c| {
            tried.borrow_mut().push(c.id);
            Box::pin(async { Ok(TokenAttempt::Revoked(REVOKED.into())) })
        })
        .await
        .unwrap_err();

        // 号用完后是 select_for_device 先报「没有可用凭证」，而不是转满 MAX_REFRESH_FAILOVER 圈。
        assert!(e.to_string().contains("没有可用凭证"), "错误信息应指向根因: {e}");
        assert_eq!(*tried.borrow(), ids, "每个号都应被试过一次，且只试一次");
        assert!(store.list().unwrap().iter().all(|c| c.disabled));
    }

    /// 迁移是幂等的：对已是 AUTOINCREMENT 的库再次 init_schema 不改动、不报错。
    #[test]
    fn migration_is_idempotent_on_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap(); // 全新库：基表已带 AUTOINCREMENT。
        init_schema(&conn).unwrap(); // 再来一次应无副作用。
        let ddl: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name = 'credentials'", [], |r| r.get(0))
            .unwrap();
        assert!(ddl.contains("AUTOINCREMENT"));
    }
}
