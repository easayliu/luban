//! 上号 Key：给自动化脚本用的控制台凭据，只能走「添加账号」那两步（`/authorize` 取链接、
//! `/exchange` 交授权码），外加列出能选的分组。
//!
//! 一把 Key 挂在建它的控制台账号名下，脚本拿它上的号就落在这个人名下、受他的分组权限约束；
//! 这个人被停用或删掉，Key 随即失效。建 Key 时记下这个人的密码指纹（与会话同一套，见
//! `auth::password_tag`），改密码、重置密码、换掉环境变量里的密码之后指纹对不上，Key 作废。
//! 库里只存 sha256，明文只在新建时回一次。

use super::*;

/// 一把上号 Key（不含明文）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProvisionKey {
    pub id: i64,
    pub user_id: i64,
    /// 所属账号的用户名，列表里给 admin 认人用。
    pub username: String,
    pub label: String,
    /// 明文开头几位，列表里认 Key 用。
    pub prefix: String,
    pub disabled: bool,
    pub created_at: u64,
    /// 最近一次被用来调接口的时间；从没用过为 None。
    pub last_used_at: Option<u64>,
    /// 建 Key 时所属账号的密码指纹。
    #[serde(skip)]
    pub pw_tag: String,
    /// 所属账号的角色与此刻库里的密码哈希，用来算当前指纹。账号不在了为 None。
    #[serde(skip)]
    pub owner: Option<(UserRole, String)>,
}

/// 按明文认出的 Key：所属账号、建 Key 时的密码指纹与此刻库里的密码哈希。
pub struct ProvisionKeyHit {
    pub id: i64,
    pub user: User,
    pub pw_tag: String,
    pub password_hash: String,
}

/// 上号 Key 的明文前缀。鉴权中间件按它把 Bearer 分流到这里，不去查会话表。
pub const PROVISION_KEY_PREFIX: &str = "lbp-";

/// 明文显示几位前缀。
pub(super) const KEY_PREFIX_LEN: usize = 10;

/// 建表，由 `init_schema` 调用。幂等，每次启动都跑。
pub(super) fn migrate_provision_keys(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS provision_keys (
             id           INTEGER PRIMARY KEY AUTOINCREMENT,
             user_id      INTEGER NOT NULL,
             label        TEXT    NOT NULL DEFAULT '',
             key_hash     TEXT    NOT NULL UNIQUE,
             key_prefix   TEXT    NOT NULL DEFAULT '',
             disabled     INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0,1)),
             created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
             last_used_at INTEGER,
             pw_tag       TEXT    NOT NULL DEFAULT ''
         ) STRICT;
         CREATE INDEX IF NOT EXISTS idx_provision_keys_user ON provision_keys(user_id);
         DELETE FROM provision_keys WHERE user_id NOT IN (SELECT id FROM users);",
    )
    .context("failed to create the provision key table")?;
    // 早先建的表没有 pw_tag：补上空串，空指纹恒对不上，那几把 Key 作废、需要重建。
    let _ =
        conn.execute("ALTER TABLE provision_keys ADD COLUMN pw_tag TEXT NOT NULL DEFAULT ''", []);
    Ok(())
}

/// 生成一把新上号 Key：`lbp-` + 40 位字母数字。
pub fn generate_provision_key() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 40];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
    let body: String =
        bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    format!("{PROVISION_KEY_PREFIX}{body}")
}

impl CredentialStore {
    /// 上号 Key 列表。`owner` 为 None 列全部（admin），否则只列这个人名下的。
    pub fn list_provision_keys(&self, owner: Option<i64>) -> Result<Vec<ProvisionKey>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT k.id, k.user_id, COALESCE(u.username, ''), k.label, k.key_prefix, k.disabled, \
                    k.created_at, k.last_used_at, k.pw_tag, u.role, u.password_hash \
               FROM provision_keys k LEFT JOIN users u ON u.id = k.user_id \
              WHERE ?1 IS NULL OR k.user_id = ?1 ORDER BY k.id",
        )?;
        let rows = stmt.query_map([owner], |r| {
            Ok(ProvisionKey {
                id: r.get(0)?,
                user_id: r.get(1)?,
                username: r.get(2)?,
                label: r.get(3)?,
                prefix: r.get(4)?,
                disabled: r.get::<_, i64>(5)? != 0,
                created_at: r.get::<_, i64>(6)? as u64,
                last_used_at: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                pw_tag: r.get(8)?,
                owner: match r.get::<_, Option<String>>(9)? {
                    Some(role) => Some((
                        UserRole::parse(&role)?,
                        r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                    )),
                    None => None,
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 给 `user_id` 新建一把上号 Key，回它的 id。`pw_tag` 是这个人此刻的密码指纹。
    pub fn create_provision_key(
        &self,
        user_id: i64,
        label: &str,
        key: &str,
        pw_tag: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO provision_keys (user_id, label, key_hash, key_prefix, pw_tag) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                user_id,
                label,
                token_fingerprint(key),
                key.chars().take(KEY_PREFIX_LEN).collect::<String>(),
                pw_tag
            ],
        )
        .context("this provision key already exists")?;
        Ok(conn.last_insert_rowid())
    }

    /// 改名称与停用状态。`owner` 给了的话只改这个人名下的。返回是否确有这把 Key。
    pub fn update_provision_key(
        &self,
        id: i64,
        owner: Option<i64>,
        label: &str,
        disabled: bool,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE provision_keys SET label = ?3, disabled = ?4 \
              WHERE id = ?1 AND (?2 IS NULL OR user_id = ?2)",
            params![id, owner, label, disabled as i64],
        )?;
        Ok(n > 0)
    }

    /// 删一把 Key。`owner` 给了的话只删这个人名下的。返回是否确有删除。
    pub fn delete_provision_key(&self, id: i64, owner: Option<i64>) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "DELETE FROM provision_keys WHERE id = ?1 AND (?2 IS NULL OR user_id = ?2)",
            params![id, owner],
        )?;
        Ok(n > 0)
    }

    /// 按明文认 Key：启用中、所属账号还在的才算。账号停用与密码指纹由调用方判（与会话一致，
    /// 指纹要比对环境变量里的密码，存储层看不到）。
    pub fn provision_key_lookup(&self, key: &str) -> Result<Option<ProvisionKeyHit>> {
        let conn = self.conn.lock();
        let hit: Option<(i64, i64, String)> = conn
            .query_row(
                "SELECT id, user_id, pw_tag FROM provision_keys WHERE key_hash = ?1 AND disabled = 0",
                [token_fingerprint(key)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((id, user_id, pw_tag)) = hit else { return Ok(None) };
        let Some(user) = Self::query_user(&conn, "u.id = ?1", [user_id])? else {
            return Ok(None);
        };
        let password_hash: String =
            conn.query_row("SELECT password_hash FROM users WHERE id = ?1", [user_id], |r| {
                r.get(0)
            })?;
        Ok(Some(ProvisionKeyHit { id, user, pw_tag, password_hash }))
    }

    /// 记下一把 Key 的使用时间（认证通过之后）。
    pub fn touch_provision_key(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("UPDATE provision_keys SET last_used_at = unixepoch() WHERE id = ?1", [id])?;
        Ok(())
    }
}
