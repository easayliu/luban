//! 控制台账号体系：admin / 访客 / 代理 / 用户四种角色，以及登录会话。
//!
//! 层级最多三层：admin 下挂代理和用户，代理下只挂用户。admin 与访客各唯一（部分唯一索引兜底）。
//! 号（`credentials.owner_id`）与出口代理（`proxies.owner_id`）都挂在上号的人名下；代理看不到
//! 下属用户的号，只能管下属的登录账号。
//!
//! 「停用」按**生效**口径算：自己停用、或上级停用，都等于停用——登录不了、名下的号不接流量。
//! 停代理就是把这一支整个停掉，不必逐个去停下属用户。

use super::*;

/// 会话有效期：登录后 7 天；还剩不到 [`SESSION_RENEW_BELOW_SECS`] 时随使用顺延。
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 3600;

/// 会话剩余有效期低于这个值才顺延：每个请求都写一次会话表没有必要，一天顺延一次足够。
pub(super) const SESSION_RENEW_BELOW_SECS: i64 = SESSION_TTL_SECS - 24 * 3600;

/// 旧版管理 / 访客密码（无盐 sha256）迁移进 `users.password_hash` 时加的前缀，用来跟
/// argon2 的 PHC 串（`$argon2id$…`）分开。登录校验通过后即重算成 argon2 覆盖掉。
pub const LEGACY_SHA256_PREFIX: &str = "sha256:";

/// 控制台角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    /// 唯一的管理员：全站可见可改，管理接入 Key 与全局设置，可开代理和用户。
    Admin,
    /// 唯一的只读访客：全站只读，给需要看号池的人员。
    Viewer,
    /// 代理：管自己的号，可开挂在自己名下的用户。
    Agent,
    /// 用户：只管自己的号。
    User,
}

impl UserRole {
    pub fn as_str(self) -> &'static str {
        match self {
            UserRole::Admin => "admin",
            UserRole::Viewer => "viewer",
            UserRole::Agent => "agent",
            UserRole::User => "user",
        }
    }

    pub(super) fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "admin" => Ok(UserRole::Admin),
            "viewer" => Ok(UserRole::Viewer),
            "agent" => Ok(UserRole::Agent),
            "user" => Ok(UserRole::User),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown user role: {other}").into(),
            )),
        }
    }
}

/// 一个控制台账号。
#[derive(Debug, Clone, serde::Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: UserRole,
    pub parent_id: Option<i64>,
    /// 自己这一行的停用标记（不含上级）。
    pub disabled: bool,
    /// 上级被停用（自己因此连带停用）。生效停用 = `disabled || parent_disabled`。
    pub parent_disabled: bool,
    /// 是否设过密码。admin 在首次初始化前为 false。
    pub password_set: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

impl User {
    /// 生效停用：自己或上级停用。
    pub fn effectively_disabled(&self) -> bool {
        self.disabled || self.parent_disabled
    }
}

/// 用户列表里的一行：账号本身，加上上级名称与名下计数。
#[derive(Debug, Clone, serde::Serialize)]
pub struct UserListItem {
    #[serde(flatten)]
    pub user: User,
    pub parent_username: Option<String>,
    /// 名下的号数。只给 admin 看（代理看不到下属的号，连个数也不给）。
    pub credential_count: Option<i64>,
    /// 名下的下属用户数（只有代理有）。
    pub child_count: i64,
}

/// 建号或转移时指定的上级不成立：不存在（比如正好被删了），或角色不对（代理只能挂在 admin
/// 名下，用户只能挂在 admin 或代理名下）。handler 据此回 400。
#[derive(Debug)]
pub struct InvalidParent;

impl std::fmt::Display for InvalidParent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the parent account does not exist or cannot have this kind of account")
    }
}

impl std::error::Error for InvalidParent {}

/// 在已持有的连接上核对上级：代理只能挂在 admin 名下，用户可以挂在 admin 或代理名下。
fn parent_accepts(conn: &Connection, parent_id: i64, role: UserRole) -> Result<bool> {
    let parent_role: Option<String> = conn
        .query_row("SELECT role FROM users WHERE id = ?1", [parent_id], |r| r.get(0))
        .optional()?;
    Ok(matches!(
        (role, parent_role.as_deref()),
        (UserRole::Agent, Some("admin")) | (UserRole::User, Some("admin" | "agent"))
    ))
}

/// 删除用户被拒的原因。
#[derive(Debug, PartialEq, Eq)]
pub enum DeleteUserError {
    NotFound,
    /// admin / 访客不走这条路删。
    Protected,
    /// 代理名下还有用户：先转走或删掉。
    HasChildren(i64),
    /// 名下还有号：先删掉。
    HasCredentials(i64),
}

/// 带上级停用标记取用户的列（`u` 是用户本表，`p` 是 LEFT JOIN 的上级）。
pub(super) const USER_COLS: &str = "u.id, u.username, u.role, u.parent_id, u.disabled, \
     COALESCE(p.disabled, 0), u.password_hash <> '', u.created_at, u.updated_at";
pub(super) const USER_FROM: &str = "users u LEFT JOIN users p ON p.id = u.parent_id";

fn row_to_user(row: &Row) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        role: UserRole::parse(&row.get::<_, String>(2)?)?,
        parent_id: row.get(3)?,
        disabled: row.get::<_, i64>(4)? != 0,
        parent_disabled: row.get::<_, i64>(5)? != 0,
        password_set: row.get(6)?,
        created_at: row.get::<_, i64>(7)? as u64,
        updated_at: row.get::<_, i64>(8)? as u64,
    })
}

/// 建表与迁移，由 `init_schema` 调用。幂等，每次启动都跑。
///
/// - admin 行**恒存在**（还没设密码时 `password_hash` 为空），号与出口代理的 owner 才总能落到
///   一个确定的人身上；
/// - 旧版存在 settings 里的管理 / 访客密码（无盐 sha256）搬进 `users`，加
///   [`LEGACY_SHA256_PREFIX`] 前缀，登录时校验通过再换成 argon2；搬完删掉 settings 里那几个键；
/// - 存量的号与出口代理全部挂到 admin 名下；出口代理的唯一约束从「地址」改成「人 + 地址」，
///   不同的人可以各存一条同样的地址。
pub(super) fn migrate_users(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS users (
             id            INTEGER PRIMARY KEY AUTOINCREMENT,
             username      TEXT    NOT NULL COLLATE NOCASE UNIQUE,
             password_hash TEXT    NOT NULL DEFAULT '',
             role          TEXT    NOT NULL CHECK (role IN ('admin','viewer','agent','user')),
             parent_id     INTEGER,
             disabled      INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0,1)),
             created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
             updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
         ) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS uq_users_singleton
             ON users(role) WHERE role IN ('admin', 'viewer');
         CREATE INDEX IF NOT EXISTS idx_users_parent ON users(parent_id);
         CREATE TABLE IF NOT EXISTS sessions (
             token_hash TEXT    PRIMARY KEY,
             user_id    INTEGER NOT NULL,
             created_at INTEGER NOT NULL DEFAULT (unixepoch()),
             expires_at INTEGER NOT NULL
         ) STRICT, WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);",
    )
    .context("failed to create the users tables")?;

    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let legacy = |key: &str| -> Result<Option<String>> {
        Ok(tx
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .filter(|v| !v.is_empty())
            .map(|v| format!("{LEGACY_SHA256_PREFIX}{v}")))
    };
    let admin_hash = legacy(ADMIN_PASSWORD)?.unwrap_or_default();
    let viewer_hash = legacy(VIEWER_PASSWORD)?;
    let has_admin: bool =
        tx.query_row("SELECT EXISTS (SELECT 1 FROM users WHERE role = 'admin')", [], |r| r.get(0))?;
    if !has_admin {
        tx.execute(
            "INSERT INTO users (username, password_hash, role) VALUES ('admin', ?1, 'admin')",
            [&admin_hash],
        )?;
    }
    if let Some(h) = viewer_hash {
        tx.execute(
            "INSERT OR IGNORE INTO users (username, password_hash, role) VALUES ('viewer', ?1, 'viewer')",
            [&h],
        )?;
    }
    for key in CONSOLE_AUTH_KEYS {
        tx.execute("DELETE FROM settings WHERE key = ?1", [key])?;
    }
    tx.commit()?;

    // 会话签发时的密码指纹（见 `crate::auth::password_tag`）：每次认会话都与当前指纹比对，
    // 密码改了、环境变量接管的密码换了或撤了，旧会话随即失效。补列前签发的会话指纹为空，
    // 一律失效，重新登录即可。
    let _ = conn.execute("ALTER TABLE sessions ADD COLUMN pw_tag TEXT NOT NULL DEFAULT ''", []);
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN owner_id INTEGER", []);
    let _ = conn.execute("ALTER TABLE proxies ADD COLUMN owner_id INTEGER", []);
    conn.execute_batch(
        "UPDATE credentials SET owner_id = (SELECT id FROM users WHERE role = 'admin')
          WHERE owner_id IS NULL;
         UPDATE proxies SET owner_id = (SELECT id FROM users WHERE role = 'admin')
          WHERE owner_id IS NULL;
         CREATE INDEX IF NOT EXISTS idx_credentials_owner ON credentials(owner_id);
         -- 不带 owner 插入的号（迁移文件导入、手工 SQL）默认挂到 admin 名下：调度只认号主
         -- 存在的号（OWNER_ACTIVE），留成 NULL 就永远调度不到了。
         CREATE TRIGGER IF NOT EXISTS trg_credentials_default_owner
             AFTER INSERT ON credentials WHEN NEW.owner_id IS NULL
         BEGIN
             UPDATE credentials SET owner_id = (SELECT id FROM users WHERE role = 'admin')
              WHERE id = NEW.id;
         END;
         DROP INDEX IF EXISTS uq_proxies_url;
         CREATE UNIQUE INDEX IF NOT EXISTS uq_proxies_owner_url ON proxies(owner_id, url);",
    )
    .context("failed to assign owners to credentials and proxies")?;
    Ok(())
}

/// 数据可见范围：admin / 访客看全部，代理和用户只看自己名下的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    Owner(i64),
}

impl Scope {
    /// 绑进 SQL 的 `?N IS NULL OR owner_id = ?N` 用：全部为 None。
    pub fn owner(self) -> Option<i64> {
        match self {
            Scope::All => None,
            Scope::Owner(id) => Some(id),
        }
    }
}

/// 「谁能管这个账号」：admin 管全部代理和用户（`:manager` 为 NULL），代理只管自己名下的用户
/// （`:manager` 为代理 id）。拼在写语句的 WHERE 里，权限核对与写入是同一条 SQL，中间不留被
/// 改掉的窗口——比如权限检查之后、等密码哈希算完之前，用户被转到了别的代理名下。
const MANAGED_BY: &str =
    "role IN ('agent', 'user') AND (:manager IS NULL OR (role = 'user' AND parent_id = :manager))";

/// 认会话时取回的一行：账号、签发时的密码指纹、当前存的密码哈希、剩余有效期（秒）。
pub struct SessionRow {
    pub user: User,
    pub pw_tag: String,
    pub password_hash: String,
    pub remaining_secs: i64,
}

/// 选号时排除「号主生效停用」的号用的条件（拼在 `credentials` 的 WHERE 里）。
///
/// 写成「号主存在且没停用」而不是「号主没被停用」：后者对号主不存在的号（在途的上号请求在
/// 号主被删之后才落库）判成可用，一个无主的号就这么进了调度。
pub(super) const OWNER_ACTIVE: &str = "EXISTS (SELECT 1 FROM users ou \
       LEFT JOIN users op ON op.id = ou.parent_id \
      WHERE ou.id = credentials.owner_id AND ou.role <> 'viewer' \
        AND ou.disabled = 0 AND COALESCE(op.disabled, 0) = 0)";

/// 上号时号主已不存在（等换码 / 拉 profile 那几秒里账号被删了）。handler 据此回 400。
#[derive(Debug)]
pub struct OwnerGone;

impl std::fmt::Display for OwnerGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the account adding this credential no longer exists")
    }
}

impl std::error::Error for OwnerGone {}

/// 在已持有的连接上核对号主：存在、且是能有号的角色（访客不上号）。
pub(super) fn owner_exists(conn: &Connection, owner_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM users WHERE id = ?1 AND role <> 'viewer')",
        [owner_id],
        |r| r.get(0),
    )?)
}

impl CredentialStore {
    fn query_user(
        conn: &Connection,
        filter: &str,
        p: impl rusqlite::Params,
    ) -> Result<Option<User>> {
        Ok(conn
            .query_row(
                &format!("SELECT {USER_COLS} FROM {USER_FROM} WHERE {filter}"),
                p,
                row_to_user,
            )
            .optional()?)
    }

    /// admin 账号（恒存在，见 [`migrate_users`]）。
    pub fn admin_user(&self) -> Result<User> {
        let conn = self.conn.lock();
        Self::query_user(&conn, "u.role = 'admin'", [])?.context("the admin user row is missing")
    }

    /// 访客账号；没设过访客密码时为 None。
    pub fn viewer_user(&self) -> Result<Option<User>> {
        let conn = self.conn.lock();
        Self::query_user(&conn, "u.role = 'viewer'", [])
    }

    pub fn user_by_id(&self, id: i64) -> Result<Option<User>> {
        let conn = self.conn.lock();
        Self::query_user(&conn, "u.id = ?1", [id])
    }

    /// 按用户名取（不区分大小写）。
    pub fn user_by_username(&self, username: &str) -> Result<Option<User>> {
        let conn = self.conn.lock();
        Self::query_user(&conn, "u.username = ?1", [username])
    }

    /// 存着的密码哈希（argon2 PHC 串，或带 [`LEGACY_SHA256_PREFIX`] 的旧哈希；空串 = 没设）。
    pub fn user_password_hash(&self, id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT password_hash FROM users WHERE id = ?1", [id], |r| r.get(0))
            .optional()?)
    }

    /// 改密码哈希。`expected` 给了的话只在当前哈希仍是它时才改（旧哈希升级成 argon2 用，
    /// 防止与并发的改密码交错、把新密码盖回旧的）。返回是否改了。
    pub fn set_user_password_hash(
        &self,
        id: i64,
        hash: &str,
        expected: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let n = match expected {
            Some(old) => conn.execute(
                "UPDATE users SET password_hash = ?2, updated_at = unixepoch() \
                  WHERE id = ?1 AND password_hash = ?3",
                params![id, hash, old],
            )?,
            None => conn.execute(
                "UPDATE users SET password_hash = ?2, updated_at = unixepoch() WHERE id = ?1",
                params![id, hash],
            )?,
        };
        Ok(n > 0)
    }

    /// 重置别人的密码：核对管理关系（[`MANAGED_BY`]）、写哈希、作废它的全部会话，同一把锁、
    /// 同一个事务里做完。返回是否确有这个账号且管得着。
    pub fn reset_managed_user_password(
        &self,
        id: i64,
        hash: &str,
        manager: Option<i64>,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let n = tx.execute(
            &format!(
                "UPDATE users SET password_hash = :hash, updated_at = unixepoch() \
                  WHERE id = :id AND {MANAGED_BY}"
            ),
            rusqlite::named_params! { ":id": id, ":hash": hash, ":manager": manager },
        )?;
        if n > 0 {
            tx.execute("DELETE FROM sessions WHERE user_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// 新建代理或用户。用户名撞了（不区分大小写）返回 `Ok(None)`；上级不成立返回
    /// [`InvalidParent`] 错误。
    ///
    /// 核对上级与插入在同一把锁里：handler 先算密码哈希再来这里，算的那几十毫秒里上级可能
    /// 被删了，此前插进去的就是一个上级不存在、却能正常登录的账号。
    pub fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        role: UserRole,
        parent_id: i64,
    ) -> Result<Option<User>> {
        anyhow::ensure!(
            matches!(role, UserRole::Agent | UserRole::User),
            "only agents and users can be created"
        );
        let conn = self.conn.lock();
        if !parent_accepts(&conn, parent_id, role)? {
            return Err(InvalidParent.into());
        }
        let n = conn.execute(
            "INSERT OR IGNORE INTO users (username, password_hash, role, parent_id) \
             VALUES (?1, ?2, ?3, ?4)",
            params![username, password_hash, role.as_str(), parent_id],
        )?;
        if n == 0 {
            return Ok(None);
        }
        let id = conn.last_insert_rowid();
        Self::query_user(&conn, "u.id = ?1", [id])
    }

    /// 设置访客密码：没有访客行就建一行（用户名 `viewer`），有就改哈希。用户名 `viewer`
    /// 已被别的账号占了时报错。
    pub fn upsert_viewer(&self, password_hash: &str) -> Result<()> {
        let conn = self.conn.lock();
        let updated = conn.execute(
            "UPDATE users SET password_hash = ?1, updated_at = unixepoch() WHERE role = 'viewer'",
            [password_hash],
        )?;
        if updated == 0 {
            conn.execute(
                "INSERT INTO users (username, password_hash, role) VALUES ('viewer', ?1, 'viewer')",
                [password_hash],
            )
            .context("the username 'viewer' is already taken by another account")?;
        }
        Ok(())
    }

    /// 清除访客：删掉访客行和它的会话。返回是否确有删除。
    pub fn delete_viewer(&self) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM sessions WHERE user_id IN (SELECT id FROM users WHERE role = 'viewer')",
            [],
        )?;
        let n = tx.execute("DELETE FROM users WHERE role = 'viewer'", [])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// 列出账号。`parent` 为 None 时列出全部代理和用户（admin 用），为 `Some(id)` 时只列
    /// 该代理名下的用户。`with_cred_count` 决定带不带名下号数（只给 admin）。
    pub fn list_users(
        &self,
        parent: Option<i64>,
        with_cred_count: bool,
    ) -> Result<Vec<UserListItem>> {
        let conn = self.conn.lock();
        let filter = match parent {
            Some(_) => "u.parent_id = ?1 AND u.role = 'user'",
            None => "u.role IN ('agent', 'user') AND ?1 IS NULL",
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT {USER_COLS}, p.username, \
                    (SELECT COUNT(*) FROM credentials c WHERE c.owner_id = u.id), \
                    (SELECT COUNT(*) FROM users k WHERE k.parent_id = u.id) \
               FROM {USER_FROM} WHERE {filter} ORDER BY u.id ASC"
        ))?;
        let rows = stmt.query_map([parent], |row| {
            Ok(UserListItem {
                user: row_to_user(row)?,
                parent_username: row.get(9)?,
                credential_count: with_cred_count.then(|| row.get(10)).transpose()?,
                child_count: row.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 停用 / 启用一个账号。停用时连带作废它和下属的全部会话（下属因上级停用而生效停用）。
    /// `manager` 见 [`MANAGED_BY`]。返回是否确有这个账号且管得着。
    pub fn set_user_disabled(&self, id: i64, disabled: bool, manager: Option<i64>) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let n = tx.execute(
            &format!(
                "UPDATE users SET disabled = :disabled, updated_at = unixepoch() \
                  WHERE id = :id AND {MANAGED_BY}"
            ),
            rusqlite::named_params! {
                ":id": id, ":disabled": disabled as i64, ":manager": manager,
            },
        )?;
        if n > 0 && disabled {
            tx.execute(
                "DELETE FROM sessions WHERE user_id = ?1 \
                    OR user_id IN (SELECT id FROM users WHERE parent_id = ?1)",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// 把用户挂到另一个上级名下。上级不成立返回 [`InvalidParent`] 错误（核对与写入同一把锁，
    /// 理由同 [`Self::create_user`]）。
    pub fn set_user_parent(&self, id: i64, parent_id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        if !parent_accepts(&conn, parent_id, UserRole::User)? {
            return Err(InvalidParent.into());
        }
        let n = conn.execute(
            "UPDATE users SET parent_id = ?2, updated_at = unixepoch() WHERE id = ?1 AND role = 'user'",
            params![id, parent_id],
        )?;
        Ok(n > 0)
    }

    /// 删除一个代理或用户：名下还有下属用户、或还有号时拒绝。连带删掉它的会话与出口代理。
    /// `manager` 见 [`MANAGED_BY`]，管不着的按不存在处理。
    pub fn delete_user(
        &self,
        id: i64,
        manager: Option<i64>,
    ) -> Result<std::result::Result<(), DeleteUserError>> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let role: Option<String> =
            tx.query_row("SELECT role FROM users WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        match role.as_deref() {
            None => return Ok(Err(DeleteUserError::NotFound)),
            Some("admin" | "viewer") => return Ok(Err(DeleteUserError::Protected)),
            _ => {}
        }
        let managed: bool = tx.query_row(
            &format!("SELECT EXISTS (SELECT 1 FROM users WHERE id = :id AND {MANAGED_BY})"),
            rusqlite::named_params! { ":id": id, ":manager": manager },
            |r| r.get(0),
        )?;
        if !managed {
            return Ok(Err(DeleteUserError::NotFound));
        }
        let children: i64 =
            tx.query_row("SELECT COUNT(*) FROM users WHERE parent_id = ?1", [id], |r| r.get(0))?;
        if children > 0 {
            return Ok(Err(DeleteUserError::HasChildren(children)));
        }
        let creds: i64 =
            tx.query_row("SELECT COUNT(*) FROM credentials WHERE owner_id = ?1", [id], |r| {
                r.get(0)
            })?;
        if creds > 0 {
            return Ok(Err(DeleteUserError::HasCredentials(creds)));
        }
        tx.execute("DELETE FROM sessions WHERE user_id = ?1", [id])?;
        tx.execute("DELETE FROM proxies WHERE owner_id = ?1", [id])?;
        tx.execute("DELETE FROM users WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(Ok(()))
    }

    // ---------- 会话 ----------

    /// 落一条会话（存 token 的 sha256，不存明文），带上签发时的密码指纹。
    ///
    /// `expected_hash` 给了的话，只在库里的密码哈希**此刻**仍是它时才落：登录校验密码要几十
    /// 毫秒，期间被重置了密码的话，按旧密码校验通过的这次登录不该再拿到会话。核对与写入在
    /// 同一把锁里。环境变量接管的密码不在库里，传 None。返回是否落了。
    pub fn create_session(
        &self,
        token_hash: &str,
        user_id: i64,
        pw_tag: &str,
        expected_hash: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        if let Some(expected) = expected_hash {
            let current: Option<String> = conn
                .query_row("SELECT password_hash FROM users WHERE id = ?1", [user_id], |r| r.get(0))
                .optional()?;
            if current.as_deref() != Some(expected) {
                return Ok(false);
            }
        }
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, expires_at, pw_tag) \
             VALUES (?1, ?2, unixepoch() + ?3, ?4)",
            params![token_hash, user_id, SESSION_TTL_SECS, pw_tag],
        )?;
        Ok(true)
    }

    /// 按 token 哈希取会话：没有、已过期（顺手删掉）、账号已不存在回 None。账号停用与密码
    /// 指纹由调用方判（指纹要比对环境变量里的密码，存储层看不到）。
    pub fn session_lookup(&self, token_hash: &str) -> Result<Option<SessionRow>> {
        let conn = self.conn.lock();
        let hit: Option<(i64, i64, String)> = conn
            .query_row(
                "SELECT user_id, expires_at - unixepoch(), pw_tag FROM sessions \
                  WHERE token_hash = ?1",
                [token_hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((user_id, remaining_secs, pw_tag)) = hit else { return Ok(None) };
        if remaining_secs <= 0 {
            conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
            return Ok(None);
        }
        let Some(user) = Self::query_user(&conn, "u.id = ?1", [user_id])? else {
            conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
            return Ok(None);
        };
        let password_hash: String =
            conn.query_row("SELECT password_hash FROM users WHERE id = ?1", [user_id], |r| {
                r.get(0)
            })?;
        Ok(Some(SessionRow { user, pw_tag, password_hash, remaining_secs }))
    }

    /// 会话有效期顺延：剩余不到 [`SESSION_RENEW_BELOW_SECS`] 才写，一天顺延一次足够。
    pub fn renew_session_if_due(&self, token_hash: &str, remaining_secs: i64) -> Result<()> {
        if remaining_secs >= SESSION_RENEW_BELOW_SECS {
            return Ok(());
        }
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sessions SET expires_at = unixepoch() + ?2 WHERE token_hash = ?1",
            params![token_hash, SESSION_TTL_SECS],
        )?;
        Ok(())
    }

    /// 改自己的密码：**当前会话还在**、且库里的哈希**仍是 `old_hash`** 时才写；写了就作废
    /// 本人其余会话、当前会话换上新指纹。全在一个事务里。返回是否写了。
    ///
    /// 两个条件挡的是同一种交错：改密请求在算新哈希（几十毫秒）时，管理员重置了这个人的密码
    /// 并撤掉了他的会话——不核对的话，这条在途请求随后照样把自己的新密码写进去，被重置的人
    /// 又能用它登录。
    pub fn change_own_password(
        &self,
        user_id: i64,
        token_hash: &str,
        old_hash: &str,
        new_hash: &str,
        new_tag: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let live: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM sessions WHERE token_hash = ?1 AND user_id = ?2 \
                             AND expires_at > unixepoch())",
            params![token_hash, user_id],
            |r| r.get(0),
        )?;
        if !live {
            return Ok(false);
        }
        let n = tx.execute(
            "UPDATE users SET password_hash = ?3, updated_at = unixepoch() \
              WHERE id = ?1 AND password_hash = ?2",
            params![user_id, old_hash, new_hash],
        )?;
        if n == 0 {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND token_hash <> ?2",
            params![user_id, token_hash],
        )?;
        tx.execute(
            "UPDATE sessions SET pw_tag = ?2 WHERE token_hash = ?1",
            params![token_hash, new_tag],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// 退出登录：删掉这一条会话。
    pub fn delete_session(&self, token_hash: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
        Ok(())
    }

    /// 作废一个账号的全部会话（改密码、重置密码时）。`keep` 给了的话留下那一条
    /// （自己改密码时当前这个会话不踢）。
    pub fn delete_user_sessions(&self, user_id: i64, keep: Option<&str>) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND token_hash IS NOT ?2",
            params![user_id, keep],
        )?;
        Ok(())
    }

    /// 作废全部会话（清除管理密码时：控制台回到初始化状态，谁都得重新登录）。
    pub fn delete_all_sessions(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM sessions", [])?;
        Ok(())
    }

    /// 上次空闲页清理没清完就再清一次（见 `secret::scrub_if_pending`），挂在后台每小时的任务上。
    pub fn retry_pending_scrub(&self) -> Result<()> {
        let conn = self.conn.lock();
        scrub_if_pending(&conn)
    }

    /// 清掉过期会话，挂在后台定时任务上。
    pub fn prune_sessions(&self) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM sessions WHERE expires_at <= unixepoch()", [])?)
    }

    // ---------- 归属 ----------

    /// 号的主人。号不存在为 `Ok(None)`。
    pub fn credential_owner(&self, cred_id: i64) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT owner_id FROM credentials WHERE id = ?1", [cred_id], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .optional()?
            .flatten())
    }

    /// `ids` 里的号是不是**全都**存在且归 `owner`。
    pub fn credentials_owned_by(&self, ids: &[i64], owner: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM credentials WHERE id = ?1 AND owner_id = ?2)",
        )?;
        for id in ids {
            if !stmt.query_row(params![id, owner], |r| r.get::<_, bool>(0))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `ids` 里的出口代理是不是**全都**存在且归 `owner`。
    pub fn proxies_owned_by(&self, ids: &[i64], owner: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM proxies WHERE id = ?1 AND owner_id = ?2)",
        )?;
        for id in ids {
            if !stmt.query_row(params![id, owner], |r| r.get::<_, bool>(0))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 封号事件落在哪个号上。事件不存在为 `Ok(None)`。
    pub fn ban_event_credential(&self, event_id: i64) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT cred_id FROM ban_events WHERE id = ?1", [event_id], |r| r.get(0))
            .optional()?)
    }

    /// 出口代理的主人。不存在为 `Ok(None)`。
    pub fn proxy_owner(&self, proxy_id: i64) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT owner_id FROM proxies WHERE id = ?1", [proxy_id], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .optional()?
            .flatten())
    }

    /// 号的主人 id → 用户名，给 admin / 访客的账号列表显示「谁的号」。
    pub fn owner_names(&self) -> Result<HashMap<i64, String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, username FROM users")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
