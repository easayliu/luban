//! 控制台账号体系：admin / 访客 / 代理 / 用户四种角色，以及登录会话。
//!
//! 层级最多三层：admin 下挂代理和用户，代理下只挂用户。admin 与访客各唯一（部分唯一索引兜底）。
//! 号（`credentials.owner_id`）与出口代理（`proxies.owner_id`）都挂在上号的人名下；代理看不到
//! 下属用户的号，只能管下属的登录账号。
//!
//! 「停用」按**生效**口径算：自己停用、或上级停用，都等于停用——登录不了、名下的号不接流量。
//! 停代理就是把这一支整个停掉，不必逐个去停下属用户。
//!
//! 用户名不区分大小写唯一：唯一索引建在 `lower(username)` 上，按名字查一律比 `lower(...)`。
//! 「核对 + 写入」要在同一把写锁下做完的（核对上级、核对管理关系、核对密码哈希），一律走
//! [`CredentialStore::begin_write`]。

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

    pub(super) fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "admin" => Ok(UserRole::Admin),
            "viewer" => Ok(UserRole::Viewer),
            "agent" => Ok(UserRole::Agent),
            "user" => Ok(UserRole::User),
            other => anyhow::bail!("unknown user role: {other}"),
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
    /// 名下的号数。
    pub credential_count: i64,
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

/// 号落在代理 `$1` 本人或其下属用户名下（拼在 `credentials` 的 WHERE 里）。
pub(super) const TEAM_OWNED: &str =
    "owner_id IN (SELECT id FROM users WHERE id = $1 OR parent_id = $1)";

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

use std::collections::HashMap;

use super::CredentialStore;
use anyhow::{Context, Result};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgConnection, Row};

/// 这个人存在、且能当号主（访客不能）。给上号 / 转交号时校验 owner 用。
pub(super) async fn owner_exists(conn: &mut PgConnection, owner_id: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND role <> 'viewer')")
        .bind(owner_id)
        .fetch_one(conn)
        .await?)
}

/// 「谁能管这个账号」：admin 管全部代理和用户（`$n` 为 NULL），代理只管自己名下的用户
/// （`$n` 为代理 id）。拼在写语句的 WHERE 里，权限核对与写入是同一条 SQL，中间不留被改掉的
/// 窗口——比如权限检查之后、等密码哈希算完之前，用户被转到了别的代理名下。
///
/// 旧版用具名参数 `:manager`；PG 只有位置参数，故按调用处的参数编号生成。
fn managed_by(n: usize) -> String {
    format!(
        "role IN ('agent', 'user') AND (${n}::BIGINT IS NULL OR (role = 'user' AND parent_id = ${n}))"
    )
}

/// `SELECT {USER_COLS} FROM {USER_FROM} WHERE {filter}`。
fn user_sql(filter: &str) -> String {
    format!("SELECT {USER_COLS} FROM {USER_FROM} WHERE {filter}")
}

/// 把一行 [`USER_COLS`] 读成 [`User`]。
fn row_to_user(row: &PgRow) -> Result<User> {
    Ok(User {
        id: row.try_get(0)?,
        username: row.try_get(1)?,
        role: UserRole::parse(&row.try_get::<String, _>(2)?)?,
        parent_id: row.try_get(3)?,
        disabled: row.try_get::<i64, _>(4)? != 0,
        parent_disabled: row.try_get::<i64, _>(5)? != 0,
        password_set: row.try_get(6)?,
        created_at: row.try_get::<i64, _>(7)? as u64,
        updated_at: row.try_get::<i64, _>(8)? as u64,
    })
}

/// 按 id 取用户（带上级停用标记）。
pub(super) async fn user_by_id_on(conn: &mut PgConnection, id: i64) -> Result<Option<User>> {
    sqlx::query(AssertSqlSafe(user_sql("u.id = $1")))
        .bind(id)
        .fetch_optional(conn)
        .await?
        .map(|r| row_to_user(&r))
        .transpose()
}

/// 在已持有的连接上核对上级：代理只能挂在 admin 名下，用户可以挂在 admin 或代理名下。
async fn parent_accepts(conn: &mut PgConnection, parent_id: i64, role: UserRole) -> Result<bool> {
    let parent_role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id = $1")
        .bind(parent_id)
        .fetch_optional(conn)
        .await?;
    Ok(matches!(
        (role, parent_role.as_deref()),
        (UserRole::Agent, Some("admin")) | (UserRole::User, Some("admin" | "agent"))
    ))
}

impl CredentialStore {
    /// admin 账号（恒存在，启动时由 `seed` 补上）。
    pub async fn admin_user(&self) -> Result<User> {
        sqlx::query(AssertSqlSafe(user_sql("u.role = 'admin'")))
            .fetch_optional(&self.pool)
            .await?
            .map(|r| row_to_user(&r))
            .transpose()?
            .context("the admin user row is missing")
    }

    /// 访客账号；没设过访客密码时为 None。
    pub async fn viewer_user(&self) -> Result<Option<User>> {
        sqlx::query(AssertSqlSafe(user_sql("u.role = 'viewer'")))
            .fetch_optional(&self.pool)
            .await?
            .map(|r| row_to_user(&r))
            .transpose()
    }

    pub async fn user_by_id(&self, id: i64) -> Result<Option<User>> {
        user_by_id_on(&mut *self.pool.acquire().await?, id).await
    }

    /// 按用户名取（不区分大小写）。
    pub async fn user_by_username(&self, username: &str) -> Result<Option<User>> {
        sqlx::query(AssertSqlSafe(user_sql("lower(u.username) = lower($1)")))
            .bind(username)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| row_to_user(&r))
            .transpose()
    }

    /// 存着的密码哈希（argon2 PHC 串，或带 `LEGACY_SHA256_PREFIX` 的旧哈希；空串 = 没设）。
    pub async fn user_password_hash(&self, id: i64) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// 改密码哈希。`expected` 给了的话只在当前哈希仍是它时才改（旧哈希升级成 argon2 用，
    /// 防止与并发的改密码交错、把新密码盖回旧的）。返回是否改了。
    ///
    /// 单条条件 UPDATE：核对与写入是同一条语句，不需要写锁。
    pub async fn set_user_password_hash(
        &self,
        id: i64,
        hash: &str,
        expected: Option<&str>,
    ) -> Result<bool> {
        let q = match expected {
            Some(old) => sqlx::query(
                "UPDATE users SET password_hash = $2, updated_at = unixepoch() \
                  WHERE id = $1 AND password_hash = $3",
            )
            .bind(id)
            .bind(hash)
            .bind(old),
            None => sqlx::query(
                "UPDATE users SET password_hash = $2, updated_at = unixepoch() WHERE id = $1",
            )
            .bind(id)
            .bind(hash),
        };
        self.update_one(q).await
    }

    /// 重置别人的密码：核对管理关系（[`managed_by`]）、写哈希、作废它的全部会话，同一个
    /// 串行化事务里做完（与 [`Self::create_session`]、[`Self::change_own_password`] 互斥，
    /// 在途的登录 / 改密不会在重置之后又落下一条按旧密码签的会话）。返回是否确有这个账号
    /// 且管得着。
    pub async fn reset_managed_user_password(
        &self,
        id: i64,
        hash: &str,
        manager: Option<i64>,
    ) -> Result<bool> {
        let mut tx = self.begin_write().await?;
        let n = sqlx::query(AssertSqlSafe(format!(
            "UPDATE users SET password_hash = $2, updated_at = unixepoch() \
              WHERE id = $1 AND {}",
            managed_by(3)
        )))
        .bind(id)
        .bind(hash)
        .bind(manager)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n > 0 {
            sqlx::query("DELETE FROM sessions WHERE user_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(n > 0)
    }

    /// 新建代理或用户。用户名撞了（不区分大小写）返回 `Ok(None)`；上级不成立返回
    /// [`InvalidParent`] 错误。
    ///
    /// 核对上级与插入在同一个串行化事务里：handler 先算密码哈希再来这里，算的那几十毫秒里
    /// 上级可能被删了，此前插进去的就是一个上级不存在、却能正常登录的账号。
    pub async fn create_user(
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
        let mut tx = self.begin_write().await?;
        if !parent_accepts(&mut tx, parent_id, role).await? {
            return Err(InvalidParent.into());
        }
        // 不写冲突目标：撞的是 lower(username) 上的唯一索引。
        let id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO users (username, password_hash, role, parent_id) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(username)
        .bind(password_hash)
        .bind(role.as_str())
        .bind(parent_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else { return Ok(None) };
        let user = user_by_id_on(&mut tx, id).await?;
        tx.commit().await?;
        Ok(user)
    }

    /// 设置访客密码：没有访客行就建一行（用户名 `viewer`），有就改哈希。用户名 `viewer`
    /// 已被别的账号占了时报错。
    pub async fn upsert_viewer(&self, password_hash: &str) -> Result<()> {
        let mut tx = self.begin_write().await?;
        let updated = sqlx::query(
            "UPDATE users SET password_hash = $1, updated_at = unixepoch() WHERE role = 'viewer'",
        )
        .bind(password_hash)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            sqlx::query(
                "INSERT INTO users (username, password_hash, role) VALUES ('viewer', $1, 'viewer')",
            )
            .bind(password_hash)
            .execute(&mut *tx)
            .await
            .context("the username 'viewer' is already taken by another account")?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 清除访客：删掉访客行和它的会话。返回是否确有删除。
    pub async fn delete_viewer(&self) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM sessions WHERE user_id IN (SELECT id FROM users WHERE role = 'viewer')",
        )
        .execute(&mut *tx)
        .await?;
        let n = sqlx::query("DELETE FROM users WHERE role = 'viewer'")
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }

    /// 列出账号。`parent` 为 None 时列出全部代理和用户（admin 用），为 `Some(id)` 时只列
    /// 该代理名下的用户。都带名下号数。
    pub async fn list_users(&self, parent: Option<i64>) -> Result<Vec<UserListItem>> {
        let filter = match parent {
            Some(_) => "u.parent_id = $1 AND u.role = 'user'",
            None => "u.role IN ('agent', 'user') AND $1::BIGINT IS NULL",
        };
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {USER_COLS}, p.username, \
                    (SELECT COUNT(*) FROM credentials c WHERE c.owner_id = u.id), \
                    (SELECT COUNT(*) FROM users k WHERE k.parent_id = u.id) \
               FROM {USER_FROM} WHERE {filter} ORDER BY u.id ASC"
        )))
        .bind(parent)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(UserListItem {
                    user: row_to_user(row)?,
                    parent_username: row.try_get(9)?,
                    credential_count: row.try_get(10)?,
                    child_count: row.try_get(11)?,
                })
            })
            .collect()
    }

    /// 停用 / 启用一个账号。停用时连带作废它和下属的全部会话（下属因上级停用而生效停用）。
    /// `manager` 见 [`managed_by`]。返回是否确有这个账号且管得着。
    pub async fn set_user_disabled(
        &self,
        id: i64,
        disabled: bool,
        manager: Option<i64>,
    ) -> Result<bool> {
        let mut tx = self.begin_write().await?;
        let n = sqlx::query(AssertSqlSafe(format!(
            "UPDATE users SET disabled = $2, updated_at = unixepoch() WHERE id = $1 AND {}",
            managed_by(3)
        )))
        .bind(id)
        .bind(disabled as i64)
        .bind(manager)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n > 0 && disabled {
            sqlx::query(
                "DELETE FROM sessions WHERE user_id = $1 \
                    OR user_id IN (SELECT id FROM users WHERE parent_id = $1)",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(n > 0)
    }

    /// 把用户挂到另一个上级名下。上级不成立返回 [`InvalidParent`] 错误（核对与写入同一个
    /// 串行化事务，理由同 [`Self::create_user`]）。
    pub async fn set_user_parent(&self, id: i64, parent_id: i64) -> Result<bool> {
        let mut tx = self.begin_write().await?;
        if !parent_accepts(&mut tx, parent_id, UserRole::User).await? {
            return Err(InvalidParent.into());
        }
        let n = sqlx::query(
            "UPDATE users SET parent_id = $2, updated_at = unixepoch() \
              WHERE id = $1 AND role = 'user'",
        )
        .bind(id)
        .bind(parent_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }

    /// 删除一个代理或用户：名下还有下属用户、或还有号时拒绝。连带删掉它的会话与出口代理。
    /// `manager` 见 [`managed_by`]，管不着的按不存在处理。
    pub async fn delete_user(
        &self,
        id: i64,
        manager: Option<i64>,
    ) -> Result<std::result::Result<(), DeleteUserError>> {
        let mut tx = self.begin_write().await?;
        let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        match role.as_deref() {
            None => return Ok(Err(DeleteUserError::NotFound)),
            Some("admin" | "viewer") => return Ok(Err(DeleteUserError::Protected)),
            _ => {}
        }
        let managed: bool = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND {})",
            managed_by(2)
        )))
        .bind(id)
        .bind(manager)
        .fetch_one(&mut *tx)
        .await?;
        if !managed {
            return Ok(Err(DeleteUserError::NotFound));
        }
        let children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE parent_id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        if children > 0 {
            return Ok(Err(DeleteUserError::HasChildren(children)));
        }
        let creds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credentials WHERE owner_id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        if creds > 0 {
            return Ok(Err(DeleteUserError::HasCredentials(creds)));
        }
        for sql in [
            "DELETE FROM sessions WHERE user_id = $1",
            "DELETE FROM proxies WHERE owner_id = $1",
            "DELETE FROM provision_keys WHERE user_id = $1",
            "DELETE FROM group_grants WHERE user_id = $1",
            "DELETE FROM users WHERE id = $1",
        ] {
            sqlx::query(sql).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(Ok(()))
    }

    // ---------- 会话 ----------

    /// 落一条会话（存 token 的 sha256，不存明文），带上签发时的密码指纹。
    ///
    /// `expected_hash` 给了的话，只在库里的密码哈希**此刻**仍是它时才落：登录校验密码要几十
    /// 毫秒，期间被重置了密码的话，按旧密码校验通过的这次登录不该再拿到会话。核对与写入在
    /// 同一个串行化事务里（重置密码也走它）。环境变量接管的密码不在库里，传 None。返回是否落了。
    pub async fn create_session(
        &self,
        token_hash: &str,
        user_id: i64,
        pw_tag: &str,
        expected_hash: Option<&str>,
    ) -> Result<bool> {
        let mut tx = self.begin_write().await?;
        if let Some(expected) = expected_hash {
            let current: Option<String> =
                sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
                    .bind(user_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if current.as_deref() != Some(expected) {
                return Ok(false);
            }
        }
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, expires_at, pw_tag) \
             VALUES ($1, $2, unixepoch() + $3, $4)",
        )
        .bind(token_hash)
        .bind(user_id)
        .bind(SESSION_TTL_SECS)
        .bind(pw_tag)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// 按 token 哈希取会话：没有、已过期（顺手删掉）、账号已不存在回 None。账号停用与密码
    /// 指纹由调用方判（指纹要比对环境变量里的密码，存储层看不到）。
    pub async fn session_lookup(&self, token_hash: &str) -> Result<Option<SessionRow>> {
        let mut conn = self.pool.acquire().await?;
        let hit: Option<(i64, i64, String)> = sqlx::query_as(
            "SELECT user_id, expires_at - unixepoch(), pw_tag FROM sessions WHERE token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((user_id, remaining_secs, pw_tag)) = hit else { return Ok(None) };
        let user = if remaining_secs > 0 { user_by_id_on(&mut conn, user_id).await? } else { None };
        let Some(user) = user else {
            sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
                .bind(token_hash)
                .execute(&mut *conn)
                .await?;
            return Ok(None);
        };
        let password_hash: String =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_one(&mut *conn)
                .await?;
        Ok(Some(SessionRow { user, pw_tag, password_hash, remaining_secs }))
    }

    /// 会话有效期顺延：剩余不到 [`SESSION_RENEW_BELOW_SECS`] 才写，一天顺延一次足够。
    pub async fn renew_session_if_due(&self, token_hash: &str, remaining_secs: i64) -> Result<()> {
        if remaining_secs >= SESSION_RENEW_BELOW_SECS {
            return Ok(());
        }
        sqlx::query("UPDATE sessions SET expires_at = unixepoch() + $2 WHERE token_hash = $1")
            .bind(token_hash)
            .bind(SESSION_TTL_SECS)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 改自己的密码：**当前会话还在**、且库里的哈希**仍是 `old_hash`** 时才写；写了就作废
    /// 本人其余会话、当前会话换上新指纹。全在一个串行化事务里。返回是否写了。
    ///
    /// 两个条件挡的是同一种交错：改密请求在算新哈希（几十毫秒）时，管理员重置了这个人的密码
    /// 并撤掉了他的会话——不核对的话，这条在途请求随后照样把自己的新密码写进去，被重置的人
    /// 又能用它登录。
    pub async fn change_own_password(
        &self,
        user_id: i64,
        token_hash: &str,
        old_hash: &str,
        new_hash: &str,
        new_tag: &str,
    ) -> Result<bool> {
        let mut tx = self.begin_write().await?;
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM sessions WHERE token_hash = $1 AND user_id = $2 \
                             AND expires_at > unixepoch())",
        )
        .bind(token_hash)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
        if !live {
            return Ok(false);
        }
        let n = sqlx::query(
            "UPDATE users SET password_hash = $3, updated_at = unixepoch() \
              WHERE id = $1 AND password_hash = $2",
        )
        .bind(user_id)
        .bind(old_hash)
        .bind(new_hash)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 0 {
            return Ok(false);
        }
        sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash <> $2")
            .bind(user_id)
            .bind(token_hash)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE sessions SET pw_tag = $2 WHERE token_hash = $1")
            .bind(token_hash)
            .bind(new_tag)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// 退出登录：删掉这一条会话。
    pub async fn delete_session(&self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 作废一个账号的全部会话（改密码、重置密码时）。`keep` 给了的话留下那一条
    /// （自己改密码时当前这个会话不踢）。
    pub async fn delete_user_sessions(&self, user_id: i64, keep: Option<&str>) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash IS DISTINCT FROM $2")
            .bind(user_id)
            .bind(keep)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 作废全部会话（清除管理密码时：控制台回到初始化状态，谁都得重新登录）。
    pub async fn delete_all_sessions(&self) -> Result<()> {
        sqlx::query("DELETE FROM sessions").execute(&self.pool).await?;
        Ok(())
    }

    /// 清掉过期会话，挂在后台定时任务上。
    pub async fn prune_sessions(&self) -> Result<usize> {
        Ok(sqlx::query("DELETE FROM sessions WHERE expires_at <= unixepoch()")
            .execute(&self.pool)
            .await?
            .rows_affected() as usize)
    }

    // ---------- 归属 ----------

    /// 号的主人。号不存在为 `Ok(None)`。
    pub async fn credential_owner(&self, cred_id: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar::<_, Option<i64>>("SELECT owner_id FROM credentials WHERE id = $1")
            .bind(cred_id)
            .fetch_optional(&self.pool)
            .await?
            .flatten())
    }

    /// `owner` 名下全部号的 id（升序）。
    pub async fn credential_ids_owned_by(&self, owner: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar("SELECT id FROM credentials WHERE owner_id = $1 ORDER BY id")
            .bind(owner)
            .fetch_all(&self.pool)
            .await?)
    }

    /// 代理 `lead` 本人及下属用户名下全部号的 id（升序）。
    pub async fn credential_ids_of_team(&self, lead: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT id FROM credentials WHERE {TEAM_OWNED} ORDER BY id"
        )))
        .bind(lead)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 号是不是在代理 `lead` 本人或下属用户名下。号不存在为 false。
    pub async fn credential_in_team(&self, cred_id: i64, lead: i64) -> Result<bool> {
        Ok(sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM credentials WHERE id = $2 AND {TEAM_OWNED})"
        )))
        .bind(lead)
        .bind(cred_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// `ids` 里的号是不是**全都**存在且归 `owner`。
    pub async fn credentials_owned_by(&self, ids: &[i64], owner: i64) -> Result<bool> {
        // 去重后数命中的行：重复的 id 不该让「全都」判错。
        let ids = super::dedup_ordered(ids);
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM credentials WHERE id = ANY($1) AND owner_id = $2",
        )
        .bind(&ids)
        .bind(owner)
        .fetch_one(&self.pool)
        .await?;
        Ok(n as usize == ids.len())
    }

    /// `ids` 里的出口代理是不是**全都**存在且归 `owner`。
    pub async fn proxies_owned_by(&self, ids: &[i64], owner: i64) -> Result<bool> {
        let ids = super::dedup_ordered(ids);
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM proxies WHERE id = ANY($1) AND owner_id = $2")
                .bind(&ids)
                .bind(owner)
                .fetch_one(&self.pool)
                .await?;
        Ok(n as usize == ids.len())
    }

    /// 出口代理的主人。不存在为 `Ok(None)`。
    pub async fn proxy_owner(&self, proxy_id: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar::<_, Option<i64>>("SELECT owner_id FROM proxies WHERE id = $1")
            .bind(proxy_id)
            .fetch_optional(&self.pool)
            .await?
            .flatten())
    }

    /// 号的主人 id → 用户名，给 admin / 访客的账号列表显示「谁的号」。
    pub async fn owner_names(&self) -> Result<HashMap<i64, String>> {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, username FROM users").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::{
        DEFAULT_DEVICE_LIMIT, DEFAULT_DEVICE_LIMIT_VALUE, DEFAULT_SESSION_LIMIT,
        DEFAULT_SESSION_LIMIT_VALUE, seal, token_fingerprint,
    };
    use super::*;

    /// 直接写库插一个号（上号走 credential 模块，不归这里），回 id。
    async fn insert_cred(store: &CredentialStore, label: &str, owner: i64) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO credentials (label, access_token, refresh_token, refresh_token_hash, \
                                      expires_at, owner_id) \
             VALUES ($1, $2, $3, $4, 0, $5) RETURNING id",
        )
        .bind(label)
        .bind(seal(&format!("t-{label}")))
        .bind(seal(&format!("r-{label}")))
        .bind(token_fingerprint(&format!("r-{label}")))
        .bind(owner)
        .fetch_one(&store.pool)
        .await
        .unwrap()
    }

    /// 删除账号：代理名下还有用户、或名下还有号时拒绝；admin / 访客不能删；删掉时连带出口代理。
    #[sqlx::test]
    async fn delete_user_refuses_while_it_still_owns_things(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let admin = store.admin_user().await.unwrap().id;
        let agent =
            store.create_user("agent", "", UserRole::Agent, admin).await.unwrap().unwrap().id;
        let user = store.create_user("user", "", UserRole::User, agent).await.unwrap().unwrap().id;
        assert!(
            store.create_user("AGENT", "", UserRole::User, admin).await.unwrap().is_none(),
            "用户名不区分大小写"
        );
        let cred = insert_cred(&store, "c", user).await;
        sqlx::query("INSERT INTO proxies (label, url, owner_id) VALUES ('p', 'http://h:1', $1)")
            .bind(user)
            .execute(&store.pool)
            .await
            .unwrap();

        assert_eq!(store.delete_user(admin, None).await.unwrap(), Err(DeleteUserError::Protected));
        assert_eq!(
            store.delete_user(agent, None).await.unwrap(),
            Err(DeleteUserError::HasChildren(1))
        );
        assert_eq!(
            store.delete_user(user, None).await.unwrap(),
            Err(DeleteUserError::HasCredentials(1))
        );
        sqlx::query("DELETE FROM credentials WHERE id = $1")
            .bind(cred)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.delete_user(user, None).await.unwrap(), Ok(()));
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM proxies WHERE owner_id = $1")
            .bind(user)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
        assert_eq!(store.delete_user(agent, None).await.unwrap(), Ok(()));
        assert_eq!(store.delete_user(agent, None).await.unwrap(), Err(DeleteUserError::NotFound));
    }

    /// 按用户名查不区分大小写。
    #[sqlx::test]
    async fn username_lookup_ignores_case(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let admin = store.admin_user().await.unwrap().id;
        let id = store.create_user("Alice", "", UserRole::User, admin).await.unwrap().unwrap().id;
        assert_eq!(store.user_by_username("aLICE").await.unwrap().unwrap().id, id);
        assert!(store.user_by_username("bob").await.unwrap().is_none());
    }

    /// 号归属核对：全都归 `owner` 才算。（旧测试里 `list_scoped` 那半归 credential 模块。）
    #[sqlx::test]
    async fn credentials_list_by_owner_scope(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let admin = store.admin_user().await.unwrap().id;
        let user = store.create_user("user", "", UserRole::User, admin).await.unwrap().unwrap().id;
        let a = insert_cred(&store, "a", admin).await;
        let b = insert_cred(&store, "b", user).await;
        assert!(store.credentials_owned_by(&[b], user).await.unwrap());
        assert!(!store.credentials_owned_by(&[a, b], user).await.unwrap());
        assert!(store.credentials_owned_by(&[b, b], user).await.unwrap(), "重复的 id 不影响");
        assert!(!store.credentials_owned_by(&[b, 9999], user).await.unwrap());
    }

    /// 管理关系在写语句里核对：用户被转走之后，原代理改不了它的密码、停不了它、删不了它。
    #[sqlx::test]
    async fn managed_writes_recheck_the_parent_at_write_time(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let admin = store.admin_user().await.unwrap().id;
        let a1 = store.create_user("a1", "", UserRole::Agent, admin).await.unwrap().unwrap().id;
        let a2 = store.create_user("a2", "", UserRole::Agent, admin).await.unwrap().unwrap().id;
        let u = store.create_user("u", "", UserRole::User, a1).await.unwrap().unwrap().id;
        assert!(store.reset_managed_user_password(u, "h1", Some(a1)).await.unwrap());
        store.set_user_parent(u, a2).await.unwrap();
        assert!(!store.reset_managed_user_password(u, "h2", Some(a1)).await.unwrap());
        assert!(!store.set_user_disabled(u, true, Some(a1)).await.unwrap());
        assert_eq!(store.delete_user(u, Some(a1)).await.unwrap(), Err(DeleteUserError::NotFound));
        assert_eq!(store.user_password_hash(u).await.unwrap().as_deref(), Some("h1"));
        // 代理管不了别的代理；admin 都管得了。
        assert!(!store.set_user_disabled(a2, true, Some(a1)).await.unwrap());
        assert!(store.reset_managed_user_password(u, "h3", Some(a2)).await.unwrap());
        assert!(store.set_user_disabled(a2, true, None).await.unwrap());
    }

    /// 改自己的密码：会话被撤了、或哈希已被别人改掉，在途的改密都写不进去。
    #[sqlx::test]
    async fn own_password_change_requires_a_live_session_and_the_old_hash(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let admin = store.admin_user().await.unwrap().id;
        let u = store.create_user("u", "h0", UserRole::User, admin).await.unwrap().unwrap().id;
        store.create_session("tok", u, "tag0", None).await.unwrap();
        // 管理员先重置（撤会话、换哈希），在途请求按旧哈希写入失败。
        assert!(store.reset_managed_user_password(u, "h-admin", None).await.unwrap());
        assert!(!store.change_own_password(u, "tok", "h0", "h-user", "t").await.unwrap());
        assert_eq!(store.user_password_hash(u).await.unwrap().as_deref(), Some("h-admin"));
        // 会话还在、哈希也对得上才写，写完其余会话作废、当前会话换指纹。
        store.create_session("tok2", u, "x", None).await.unwrap();
        store.create_session("tok3", u, "x", None).await.unwrap();
        assert!(store.change_own_password(u, "tok2", "h-admin", "h-new", "tag-new").await.unwrap());
        assert!(store.session_lookup("tok3").await.unwrap().is_none());
        assert_eq!(store.session_lookup("tok2").await.unwrap().unwrap().pw_tag, "tag-new");
    }

    /// 会话：带 `expected_hash` 时哈希对不上不落；过期的查不到并被删掉；`keep` 留下指定那条。
    #[sqlx::test]
    async fn sessions_check_hash_expiry_and_keep(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let admin = store.admin_user().await.unwrap().id;
        let u = store.create_user("u", "h0", UserRole::User, admin).await.unwrap().unwrap().id;
        assert!(!store.create_session("stale", u, "t", Some("old")).await.unwrap());
        assert!(store.create_session("ok", u, "t", Some("h0")).await.unwrap());
        let row = store.session_lookup("ok").await.unwrap().unwrap();
        assert_eq!((row.user.id, row.password_hash.as_str()), (u, "h0"));
        assert!(row.remaining_secs > SESSION_RENEW_BELOW_SECS);

        store.create_session("keep", u, "t", None).await.unwrap();
        store.delete_user_sessions(u, Some("keep")).await.unwrap();
        assert!(store.session_lookup("ok").await.unwrap().is_none());
        assert!(store.session_lookup("keep").await.unwrap().is_some());

        sqlx::query("UPDATE sessions SET expires_at = unixepoch() - 1 WHERE token_hash = 'keep'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.session_lookup("keep").await.unwrap().is_none());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "过期会话查的时候顺手删掉");
    }

    /// 建号与转移在存储层核对上级：上级不存在、或角色不对都拒。
    #[sqlx::test]
    async fn create_and_move_recheck_the_parent(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let admin = store.admin_user().await.unwrap().id;
        let agent =
            store.create_user("agent", "", UserRole::Agent, admin).await.unwrap().unwrap().id;
        let user = store.create_user("u", "", UserRole::User, agent).await.unwrap().unwrap().id;
        let invalid = |r: Result<Option<User>>| {
            r.err().is_some_and(|e| e.downcast_ref::<InvalidParent>().is_some())
        };
        assert!(
            invalid(store.create_user("a2", "", UserRole::Agent, agent).await),
            "代理只能挂在 admin 名下"
        );
        assert!(
            invalid(store.create_user("u2", "", UserRole::User, user).await),
            "用户下面不能再挂用户"
        );
        store.set_user_parent(user, admin).await.unwrap();
        assert_eq!(store.delete_user(agent, None).await.unwrap(), Ok(()));
        assert!(invalid(store.create_user("u3", "", UserRole::User, agent).await), "上级已删");
        assert!(store.set_user_parent(user, agent).await.is_err());
    }

    /// 访客：没有就建、有就改；清除时连带会话。
    #[sqlx::test]
    async fn viewer_upsert_and_delete(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        assert!(store.viewer_user().await.unwrap().is_none());
        store.upsert_viewer("v1").await.unwrap();
        store.upsert_viewer("v2").await.unwrap();
        let v = store.viewer_user().await.unwrap().unwrap();
        assert_eq!(store.user_password_hash(v.id).await.unwrap().as_deref(), Some("v2"));
        store.create_session("vt", v.id, "t", None).await.unwrap();
        assert!(store.delete_viewer().await.unwrap());
        assert!(store.session_lookup("vt").await.unwrap().is_none());
        assert!(!store.delete_viewer().await.unwrap());
    }

    /// 读一项设置的原始行（绕开内存镜像）。
    async fn raw_setting(pool: &PgPool, key: &str) -> Option<String> {
        sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
            .bind(key)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    /// 全局默认会话上限的播种：缺失才写、显式值（含 `0`）不动、重复启动不改。
    /// （旧版测的是 `init_schema`；PG 版的播种在 `seed`，随 `CredentialStore::open` 跑。）
    #[sqlx::test]
    async fn seeds_the_default_session_limit_only_when_absent(pool: PgPool) {
        let want = DEFAULT_SESSION_LIMIT_VALUE.to_string();
        CredentialStore::for_test(pool.clone()).await;
        assert_eq!(raw_setting(&pool, DEFAULT_SESSION_LIMIT).await.as_deref(), Some(want.as_str()));
        CredentialStore::for_test(pool.clone()).await;
        assert_eq!(raw_setting(&pool, DEFAULT_SESSION_LIMIT).await.as_deref(), Some(want.as_str()));
        for explicit in ["0", "12"] {
            sqlx::query("UPDATE settings SET value = $2 WHERE key = $1")
                .bind(DEFAULT_SESSION_LIMIT)
                .bind(explicit)
                .execute(&pool)
                .await
                .unwrap();
            CredentialStore::for_test(pool.clone()).await;
            assert_eq!(
                raw_setting(&pool, DEFAULT_SESSION_LIMIT).await.as_deref(),
                Some(explicit),
                "显式配置不该被启动改写"
            );
        }
    }

    /// 全局默认设备上限的播种：同上。（旧测试末尾「缺这一行时判定仍是同一个值」测的是
    /// `default_device_limit`，归 settings 模块。）
    #[sqlx::test]
    async fn seeds_the_default_device_limit_only_when_absent(pool: PgPool) {
        let want = DEFAULT_DEVICE_LIMIT_VALUE.to_string();
        CredentialStore::for_test(pool.clone()).await;
        assert_eq!(raw_setting(&pool, DEFAULT_DEVICE_LIMIT).await.as_deref(), Some(want.as_str()));
        CredentialStore::for_test(pool.clone()).await;
        assert_eq!(raw_setting(&pool, DEFAULT_DEVICE_LIMIT).await.as_deref(), Some(want.as_str()));
        for explicit in ["0", "12"] {
            sqlx::query("UPDATE settings SET value = $2 WHERE key = $1")
                .bind(DEFAULT_DEVICE_LIMIT)
                .bind(explicit)
                .execute(&pool)
                .await
                .unwrap();
            CredentialStore::for_test(pool.clone()).await;
            assert_eq!(
                raw_setting(&pool, DEFAULT_DEVICE_LIMIT).await.as_deref(),
                Some(explicit),
                "显式配置不该被启动改写"
            );
        }
    }

    /// 解不开的 token 密文（密钥不对）：打开直接报错，不带着错密钥跑起来。
    #[sqlx::test]
    async fn undecryptable_tokens_refuse_to_start(pool: PgPool) {
        CredentialStore::for_test(pool.clone()).await;
        sqlx::query(
            "INSERT INTO credentials (access_token, refresh_token, expires_at) \
             VALUES ('enc1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA', \
                     'enc1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let err = CredentialStore::open(pool).await.err().expect("密钥不对必须拒绝启动");
        assert!(format!("{err:#}").contains("secret key does not match"), "{err:#}");
    }

    /// 密钥校验值或接入 Key 的密文解不开（换了密钥）：拒绝启动，哪怕库里一个号都没有。
    #[sqlx::test]
    async fn a_wrong_secret_key_is_caught_without_any_credentials(pool: PgPool) {
        CredentialStore::for_test(pool.clone()).await;
        sqlx::query(
            "INSERT INTO api_keys (label, key_hash, key_sealed) \
             VALUES ('k', 'h', 'enc1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let err =
            CredentialStore::open(pool.clone()).await.err().expect("接入 Key 解不开必须拒绝启动");
        assert!(format!("{err:#}").contains("access key"), "{err:#}");

        sqlx::query("DELETE FROM api_keys").execute(&pool).await.unwrap();
        sqlx::query(
            "UPDATE settings SET value = 'enc1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' \
              WHERE key = 'secret_key_check'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let err = CredentialStore::open(pool).await.err().expect("校验值解不开必须拒绝启动");
        assert!(format!("{err:#}").contains("secret key does not match"), "{err:#}");
    }
}
