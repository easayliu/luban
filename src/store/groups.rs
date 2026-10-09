//! 号池分组与接入 Key。
//!
//! **分组**由 admin 建。号可以同时在多个分组里（上号时必选至少一个）；系统恒有一个默认分组，
//! 对所有人开放、不能删。其余分组 admin 开放给代理（代理名下的用户自动继承）或 admin 直属的
//! 用户。代理和用户上号、改分组时只能选开放给自己的分组。
//!
//! **接入 Key**也只有 admin 能建，给外部系统对接用。一把 Key 按顺序绑定若干分组：请求只在这些
//! 分组的号里选，排在前面的分组优先（见 `CredentialStore::select_with_slot`）。不绑定分组的 Key
//! 用全部号。库里存 Key 的 sha256（校验用）和加密后的明文（admin 随时可以查看、复制）。

use super::*;

/// 一个分组（admin 视角，带号数与开放名单）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PoolGroup {
    pub id: i64,
    pub name: String,
    pub note: String,
    pub is_default: bool,
    pub created_at: u64,
    /// 组里的号数。只给 admin。
    pub credential_count: Option<i64>,
    /// 开放给了谁（控制台账号 id）。只给 admin。
    pub grants: Option<Vec<i64>>,
}

/// 一把接入 Key（不含明文）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiKey {
    pub id: i64,
    pub label: String,
    /// 明文开头几位，列表里认 Key 用。
    pub prefix: String,
    pub disabled: bool,
    pub created_at: u64,
    /// 可用全部号（建 / 改时没选任何分组）。为假时只能用 `groups` 里的分组——绑定的分组被
    /// 删光了也不会因此变成全部号，而是一个号都选不到。
    pub all_groups: bool,
    /// 绑定的分组，按优先顺序。
    pub groups: Vec<i64>,
}

/// 校验通过的接入 Key：它是哪一把、能用哪些号。
#[derive(Debug, Clone)]
pub struct KeyAccess {
    /// 哪一把（环境变量那把与「没配任何 Key 时放行」为 None）。流水按 Key 记账时用。
    #[allow(dead_code)]
    pub key_id: Option<i64>,
    /// `None` = 全部号；`Some` = 只能用这些分组（按优先顺序），空列表即一个号都不能用。
    pub groups: Option<Vec<i64>>,
}

/// 配过接入 Key 的标记：有了它，转发就恒要求带 Key——哪怕后来把 Key 全删了，也只是谁都
/// 进不来，而不是退回「没配 Key 就不校验」。
pub const API_KEYS_CONFIGURED: &str = "api_keys_configured";

/// 默认分组的名称（建库时）。
pub(super) const DEFAULT_GROUP_NAME: &str = "默认分组";

/// 明文显示几位前缀。
const KEY_PREFIX_LEN: usize = 10;

/// 建表与迁移，由 `init_schema` 调用。幂等，每次启动都跑。
///
/// - 默认分组恒存在；不在任何分组里的号（存量号、直接写库的号）补进默认分组；
/// - 旧版的全局接入 Key（settings 的 `client_api_key`）迁成一把不绑定分组的 Key，再删掉
///   那个设置项。
///
/// 搬过旧版全局 Key（明文从 settings 里删掉了）就在同一个事务里落下清理标记，迁移跑完由
/// 调用方清空闲页。
pub(super) fn migrate_groups(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pool_groups (
             id         INTEGER PRIMARY KEY AUTOINCREMENT,
             name       TEXT    NOT NULL COLLATE NOCASE UNIQUE,
             note       TEXT    NOT NULL DEFAULT '',
             is_default INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0,1)),
             created_at INTEGER NOT NULL DEFAULT (unixepoch())
         ) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS uq_pool_groups_default
             ON pool_groups(is_default) WHERE is_default = 1;
         CREATE TABLE IF NOT EXISTS credential_groups (
             cred_id  INTEGER NOT NULL,
             group_id INTEGER NOT NULL,
             PRIMARY KEY (cred_id, group_id)
         ) STRICT, WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS idx_credential_groups_group ON credential_groups(group_id);
         CREATE TABLE IF NOT EXISTS group_grants (
             group_id INTEGER NOT NULL,
             user_id  INTEGER NOT NULL,
             PRIMARY KEY (group_id, user_id)
         ) STRICT, WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS idx_group_grants_user ON group_grants(user_id);
         CREATE TABLE IF NOT EXISTS api_keys (
             id         INTEGER PRIMARY KEY AUTOINCREMENT,
             label      TEXT    NOT NULL DEFAULT '',
             key_hash   TEXT    NOT NULL UNIQUE,
             key_sealed TEXT    NOT NULL,
             key_prefix TEXT    NOT NULL DEFAULT '',
             all_groups INTEGER NOT NULL DEFAULT 1 CHECK (all_groups IN (0,1)),
             disabled   INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0,1)),
             created_at INTEGER NOT NULL DEFAULT (unixepoch())
         ) STRICT;
         CREATE TABLE IF NOT EXISTS api_key_groups (
             key_id   INTEGER NOT NULL,
             group_id INTEGER NOT NULL,
             ord      INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (key_id, group_id)
         ) STRICT, WITHOUT ROWID;",
    )
    .context("failed to create the group and API key tables")?;

    // 新插入的号先落进默认分组：上号接口随后按所选分组整体替换；迁移文件导入、直接写库的
    // 号就留在默认分组里，不会因为一个分组都不在而永远调度不到。
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS trg_credentials_default_group
             AFTER INSERT ON credentials
         BEGIN
             INSERT OR IGNORE INTO credential_groups (cred_id, group_id)
             SELECT NEW.id, id FROM pool_groups WHERE is_default = 1;
         END;",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO pool_groups (name, is_default) \
         SELECT ?1, 1 WHERE NOT EXISTS (SELECT 1 FROM pool_groups WHERE is_default = 1)",
        [DEFAULT_GROUP_NAME],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO credential_groups (cred_id, group_id) \
         SELECT c.id, (SELECT id FROM pool_groups WHERE is_default = 1) FROM credentials c \
          WHERE NOT EXISTS (SELECT 1 FROM credential_groups g WHERE g.cred_id = c.id)",
        [],
    )?;
    // 删号不顺手清关联（`remove` 在别处），这里兜底把孤儿行扫掉。
    conn.execute(
        "DELETE FROM credential_groups WHERE cred_id NOT IN (SELECT id FROM credentials)",
        [],
    )?;

    // 早先建的表没有 all_groups：补列时把已绑了分组的 Key 标成「只限分组」。
    if conn
        .execute("ALTER TABLE api_keys ADD COLUMN all_groups INTEGER NOT NULL DEFAULT 1", [])
        .is_ok()
    {
        conn.execute(
            "UPDATE api_keys SET all_groups = 0 WHERE id IN (SELECT key_id FROM api_key_groups)",
            [],
        )?;
    }
    // 库里有 Key 就一定配过：补上标记（升级前建的库）。
    conn.execute(
        "INSERT OR IGNORE INTO settings (key, value) SELECT ?1, '1' WHERE EXISTS (SELECT 1 FROM api_keys)",
        [API_KEYS_CONFIGURED],
    )?;

    let legacy: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [CLIENT_API_KEY], |r| r.get(0))
        .optional()?
        .map(|v: String| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    if let Some(key) = legacy {
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO api_keys (label, key_hash, key_sealed, key_prefix) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["默认 Key", key_hash(&key), seal(&key), key_prefix(&key)],
        )?;
        tx.execute("DELETE FROM settings WHERE key = ?1", [CLIENT_API_KEY])?;
        tx.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, '1')",
            [API_KEYS_CONFIGURED],
        )?;
        mark_scrub_pending(&tx)?;
        tx.commit()?;
    }
    Ok(())
}

pub(super) fn key_hash(key: &str) -> String {
    token_fingerprint(key)
}

pub(super) fn key_prefix(key: &str) -> String {
    key.chars().take(KEY_PREFIX_LEN).collect()
}

/// 生成一把新 Key：`sk-lb-` + 40 位字母数字。
pub fn generate_api_key() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 40];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
    let body: String =
        bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    format!("sk-lb-{body}")
}

/// 分组相关写操作被拒的原因。
#[derive(Debug, PartialEq, Eq)]
pub enum GroupError {
    NotFound,
    /// 默认分组不能删、不能改成非默认。
    DefaultGroup,
    /// 名称撞了（不区分大小写）。
    NameTaken,
    /// 指定的分组里有不存在的。
    UnknownGroup,
    /// 开放对象只能是代理或 admin 直属的用户。
    InvalidGrantee,
    /// 号至少得在一个分组里。
    Empty,
}

impl std::fmt::Display for GroupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            GroupError::NotFound => "group not found",
            GroupError::DefaultGroup => "the default group cannot be deleted",
            GroupError::NameTaken => "the group name is already taken",
            GroupError::UnknownGroup => "one of the selected groups does not exist",
            GroupError::InvalidGrantee => {
                "groups can only be opened to agents or users directly under the admin"
            }
            GroupError::Empty => "select at least one group",
        })
    }
}

impl std::error::Error for GroupError {}

/// `ids` 里的分组是不是全都存在。
fn groups_exist(conn: &Connection, ids: &[i64]) -> Result<bool> {
    let mut stmt =
        conn.prepare_cached("SELECT EXISTS (SELECT 1 FROM pool_groups WHERE id = ?1)")?;
    for id in ids {
        if !stmt.query_row([id], |r| r.get::<_, bool>(0))? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 去重、保持首次出现的顺序。
pub(super) fn dedup_ordered(ids: &[i64]) -> Vec<i64> {
    let mut seen = HashSet::new();
    ids.iter().copied().filter(|id| seen.insert(*id)).collect()
}

impl CredentialStore {
    /// 默认分组的 id。
    pub fn default_group_id(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT id FROM pool_groups WHERE is_default = 1", [], |r| r.get(0))?)
    }

    /// 全部分组（admin / 访客用），带号数与开放名单。默认分组排第一。
    pub fn list_groups(&self) -> Result<Vec<PoolGroup>> {
        let conn = self.conn.lock();
        let mut grants: HashMap<i64, Vec<i64>> = HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT group_id, user_id FROM group_grants ORDER BY user_id")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
                let (g, u) = row?;
                grants.entry(g).or_default().push(u);
            }
        }
        let mut stmt = conn.prepare(
            "SELECT g.id, g.name, g.note, g.is_default, g.created_at, \
                    (SELECT COUNT(*) FROM credential_groups c WHERE c.group_id = g.id) \
               FROM pool_groups g ORDER BY g.is_default DESC, g.id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            let id: i64 = r.get(0)?;
            Ok(PoolGroup {
                id,
                name: r.get(1)?,
                note: r.get(2)?,
                is_default: r.get::<_, i64>(3)? != 0,
                created_at: r.get::<_, i64>(4)? as u64,
                credential_count: Some(r.get(5)?),
                grants: Some(grants.get(&id).cloned().unwrap_or_default()),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 某个代理或用户能用的分组 id：默认分组，加上开放给他的；用户挂在代理名下时，开放给
    /// 那个代理的也算（继承）。
    pub fn visible_group_ids(&self, user: &User) -> Result<HashSet<i64>> {
        let conn = self.conn.lock();
        let inherit = match (user.role, user.parent_id) {
            (UserRole::User, Some(parent)) => Some(parent),
            _ => None,
        };
        let mut stmt = conn.prepare(
            "SELECT id FROM pool_groups WHERE is_default = 1 \
             UNION SELECT group_id FROM group_grants WHERE user_id = ?1 \
             UNION SELECT group_id FROM group_grants WHERE user_id = ?2 \
                   AND EXISTS (SELECT 1 FROM users WHERE id = ?2 AND role = 'agent')",
        )?;
        let rows = stmt.query_map(params![user.id, inherit], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 某个代理或用户能看到的分组（只有名称与说明，不带号数和开放名单）。
    pub fn visible_groups(&self, user: &User) -> Result<Vec<PoolGroup>> {
        let ids = self.visible_group_ids(user)?;
        Ok(self
            .list_groups()?
            .into_iter()
            .filter(|g| ids.contains(&g.id))
            .map(|g| PoolGroup { credential_count: None, grants: None, ..g })
            .collect())
    }

    /// 新建分组。
    pub fn create_group(
        &self,
        name: &str,
        note: &str,
    ) -> Result<std::result::Result<i64, GroupError>> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO pool_groups (name, note) VALUES (?1, ?2)",
            params![name, note],
        )?;
        if n == 0 {
            return Ok(Err(GroupError::NameTaken));
        }
        Ok(Ok(conn.last_insert_rowid()))
    }

    /// 改分组的名称与说明。
    pub fn update_group(
        &self,
        id: i64,
        name: &str,
        note: &str,
    ) -> Result<std::result::Result<(), GroupError>> {
        let conn = self.conn.lock();
        let taken: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pool_groups WHERE name = ?1 AND id <> ?2)",
            params![name, id],
            |r| r.get(0),
        )?;
        if taken {
            return Ok(Err(GroupError::NameTaken));
        }
        let n = conn.execute(
            "UPDATE pool_groups SET name = ?2, note = ?3 WHERE id = ?1",
            params![id, name, note],
        )?;
        Ok(if n > 0 { Ok(()) } else { Err(GroupError::NotFound) })
    }

    /// 删分组：默认分组不能删。组里的号若因此一个分组都不剩，挪进默认分组；开放名单与接入
    /// Key 上的绑定一并清掉。
    pub fn delete_group(&self, id: i64) -> Result<std::result::Result<(), GroupError>> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let is_default: Option<bool> = tx
            .query_row("SELECT is_default FROM pool_groups WHERE id = ?1", [id], |r| {
                Ok(r.get::<_, i64>(0)? != 0)
            })
            .optional()?;
        match is_default {
            None => return Ok(Err(GroupError::NotFound)),
            Some(true) => return Ok(Err(GroupError::DefaultGroup)),
            Some(false) => {}
        }
        tx.execute(
            "INSERT OR IGNORE INTO credential_groups (cred_id, group_id) \
             SELECT cg.cred_id, (SELECT id FROM pool_groups WHERE is_default = 1) \
               FROM credential_groups cg WHERE cg.group_id = ?1 \
                AND NOT EXISTS (SELECT 1 FROM credential_groups o \
                                 WHERE o.cred_id = cg.cred_id AND o.group_id <> ?1)",
            [id],
        )?;
        tx.execute("DELETE FROM credential_groups WHERE group_id = ?1", [id])?;
        tx.execute("DELETE FROM group_grants WHERE group_id = ?1", [id])?;
        tx.execute("DELETE FROM api_key_groups WHERE group_id = ?1", [id])?;
        tx.execute("DELETE FROM pool_groups WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(Ok(()))
    }

    /// 整体替换一个分组的开放名单。开放对象只能是代理，或 admin 直属的用户。默认分组本来
    /// 就对所有人开放，不需要名单。
    pub fn set_group_grants(
        &self,
        id: i64,
        user_ids: &[i64],
    ) -> Result<std::result::Result<(), GroupError>> {
        let user_ids = dedup_ordered(user_ids);
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let is_default: Option<bool> = tx
            .query_row("SELECT is_default FROM pool_groups WHERE id = ?1", [id], |r| {
                Ok(r.get::<_, i64>(0)? != 0)
            })
            .optional()?;
        match is_default {
            None => return Ok(Err(GroupError::NotFound)),
            Some(true) => return Ok(Err(GroupError::DefaultGroup)),
            Some(false) => {}
        }
        {
            let mut ok = tx.prepare(
                "SELECT EXISTS (SELECT 1 FROM users u WHERE u.id = ?1 AND (u.role = 'agent' \
                    OR (u.role = 'user' AND u.parent_id = (SELECT id FROM users WHERE role = 'admin'))))",
            )?;
            for uid in &user_ids {
                if !ok.query_row([uid], |r| r.get::<_, bool>(0))? {
                    return Ok(Err(GroupError::InvalidGrantee));
                }
            }
        }
        tx.execute("DELETE FROM group_grants WHERE group_id = ?1", [id])?;
        {
            let mut ins =
                tx.prepare("INSERT INTO group_grants (group_id, user_id) VALUES (?1, ?2)")?;
            for uid in &user_ids {
                ins.execute(params![id, uid])?;
            }
        }
        tx.commit()?;
        Ok(Ok(()))
    }

    /// 号 → 所在分组（升序）。
    pub fn credential_group_map(&self) -> Result<HashMap<i64, Vec<i64>>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT cred_id, group_id FROM credential_groups ORDER BY group_id")?;
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (c, g) = row?;
            out.entry(c).or_default().push(g);
        }
        Ok(out)
    }

    /// 一个号所在的分组（升序）。
    pub fn credential_groups(&self, cred_id: i64) -> Result<Vec<i64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT group_id FROM credential_groups WHERE cred_id = ?1 ORDER BY group_id",
        )?;
        let rows = stmt.query_map([cred_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 整体替换一批号的分组。至少一个分组，且都得存在；能不能选由调用方按身份先核对。
    pub fn set_credential_groups(
        &self,
        cred_ids: &[i64],
        group_ids: &[i64],
    ) -> Result<std::result::Result<(), GroupError>> {
        let group_ids = dedup_ordered(group_ids);
        if group_ids.is_empty() {
            return Ok(Err(GroupError::Empty));
        }
        let conn = self.conn.lock();
        if !groups_exist(&conn, &group_ids)? {
            return Ok(Err(GroupError::UnknownGroup));
        }
        let tx = conn.unchecked_transaction()?;
        {
            let mut del = tx.prepare("DELETE FROM credential_groups WHERE cred_id = ?1")?;
            let mut ins =
                tx.prepare("INSERT INTO credential_groups (cred_id, group_id) VALUES (?1, ?2)")?;
            for cid in cred_ids {
                del.execute([cid])?;
                for gid in &group_ids {
                    ins.execute(params![cid, gid])?;
                }
            }
        }
        tx.commit()?;
        Ok(Ok(()))
    }

    // ---------- 接入 Key ----------

    /// 全部接入 Key（不含明文）。
    pub fn list_api_keys(&self) -> Result<Vec<ApiKey>> {
        let conn = self.conn.lock();
        let mut groups: HashMap<i64, Vec<i64>> = HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT key_id, group_id FROM api_key_groups ORDER BY key_id, ord")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
                let (k, g) = row?;
                groups.entry(k).or_default().push(g);
            }
        }
        let mut stmt = conn.prepare(
            "SELECT id, label, key_prefix, disabled, created_at, all_groups FROM api_keys ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            let id: i64 = r.get(0)?;
            Ok(ApiKey {
                id,
                label: r.get(1)?,
                prefix: r.get(2)?,
                disabled: r.get::<_, i64>(3)? != 0,
                created_at: r.get::<_, i64>(4)? as u64,
                all_groups: r.get::<_, i64>(5)? != 0,
                groups: groups.get(&id).cloned().unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn write_key_groups(tx: &Connection, key_id: i64, group_ids: &[i64]) -> Result<()> {
        tx.execute("DELETE FROM api_key_groups WHERE key_id = ?1", [key_id])?;
        let mut ins =
            tx.prepare("INSERT INTO api_key_groups (key_id, group_id, ord) VALUES (?1, ?2, ?3)")?;
        for (ord, gid) in group_ids.iter().enumerate() {
            ins.execute(params![key_id, gid, ord as i64])?;
        }
        Ok(())
    }

    /// 新建一把接入 Key，回它的 id。`group_ids` 按优先顺序，空 = 用全部号。建过一把之后
    /// 转发就恒要求带 Key（[`API_KEYS_CONFIGURED`]）。
    pub fn create_api_key(
        &self,
        label: &str,
        key: &str,
        group_ids: &[i64],
    ) -> Result<std::result::Result<i64, GroupError>> {
        let group_ids = dedup_ordered(group_ids);
        let id = {
            let conn = self.conn.lock();
            if !groups_exist(&conn, &group_ids)? {
                return Ok(Err(GroupError::UnknownGroup));
            }
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO api_keys (label, key_hash, key_sealed, key_prefix, all_groups) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    label,
                    key_hash(key),
                    seal(key),
                    key_prefix(key),
                    group_ids.is_empty() as i64
                ],
            )
            .context("this API key already exists")?;
            let id = tx.last_insert_rowid();
            Self::write_key_groups(&tx, id, &group_ids)?;
            tx.commit()?;
            id
        };
        // 走 set_setting 而不是直接写表：设置在内存里有一份镜像。
        self.set_setting(API_KEYS_CONFIGURED, "1")?;
        Ok(Ok(id))
    }

    /// 改一把 Key 的名称、停用状态与能用的号。
    ///
    /// `all_groups` 必须显式给才会改范围，**绝不从「分组列表为空」推断成全部号**：
    /// - `Some(true)`：可用全部号，清掉分组绑定；
    /// - `Some(false)`：只限 `group_ids`（按优先顺序）。空列表就是一个号都不能用；
    /// - `None`：范围原样不动（`group_ids` 被忽略），只改名称与启停。
    ///
    /// 绑定的分组被删光的 Key 是「只限分组、列表为空」：只改个名字、停用再启用，不能因此
    /// 变成全部号。
    pub fn update_api_key(
        &self,
        id: i64,
        label: &str,
        disabled: bool,
        group_ids: &[i64],
        all_groups: Option<bool>,
    ) -> Result<std::result::Result<(), GroupError>> {
        let group_ids = dedup_ordered(group_ids);
        let conn = self.conn.lock();
        if all_groups == Some(false) && !groups_exist(&conn, &group_ids)? {
            return Ok(Err(GroupError::UnknownGroup));
        }
        let tx = conn.unchecked_transaction()?;
        let n = tx.execute(
            "UPDATE api_keys SET label = ?2, disabled = ?3 WHERE id = ?1",
            params![id, label, disabled as i64],
        )?;
        if n == 0 {
            return Ok(Err(GroupError::NotFound));
        }
        match all_groups {
            Some(true) => {
                tx.execute("UPDATE api_keys SET all_groups = 1 WHERE id = ?1", [id])?;
                Self::write_key_groups(&tx, id, &[])?;
            }
            Some(false) => {
                tx.execute("UPDATE api_keys SET all_groups = 0 WHERE id = ?1", [id])?;
                Self::write_key_groups(&tx, id, &group_ids)?;
            }
            None => {}
        }
        tx.commit()?;
        Ok(Ok(()))
    }

    /// 删一把 Key。返回是否确有删除。
    pub fn delete_api_key(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM api_key_groups WHERE key_id = ?1", [id])?;
        let n = tx.execute("DELETE FROM api_keys WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// 一把 Key 的明文（admin 查看、复制用）。
    pub fn reveal_api_key(&self, id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let sealed: Option<String> = conn
            .query_row("SELECT key_sealed FROM api_keys WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        sealed.map(|s| secret::open(&s)).transpose()
    }

    /// 转发要不要求带接入 Key：库里有 Key，或者配过（[`API_KEYS_CONFIGURED`]）。从没配过、
    /// 环境变量也没设时才不校验来访身份（与老版本「没配接入 Key 就不校验」一致）；配过之后
    /// 把 Key 全删了也不会因此敞开。
    pub fn api_keys_required(&self) -> Result<bool> {
        if self.get_setting(API_KEYS_CONFIGURED)?.is_some() {
            return Ok(true);
        }
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT EXISTS (SELECT 1 FROM api_keys)", [], |r| r.get(0))?)
    }

    /// 按来访带的 Key 明文认身份：启用中的 Key 才算，回它能用的分组（按优先顺序）。
    pub fn api_key_access(&self, key: &str) -> Result<Option<KeyAccess>> {
        let conn = self.conn.lock();
        let hit: Option<(i64, bool)> = conn
            .query_row(
                "SELECT id, all_groups FROM api_keys WHERE key_hash = ?1 AND disabled = 0",
                [key_hash(key)],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)),
            )
            .optional()?;
        let Some((id, all)) = hit else { return Ok(None) };
        if all {
            return Ok(Some(KeyAccess { key_id: Some(id), groups: None }));
        }
        let mut stmt = conn
            .prepare_cached("SELECT group_id FROM api_key_groups WHERE key_id = ?1 ORDER BY ord")?;
        let groups = stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(Some(KeyAccess { key_id: Some(id), groups: Some(groups) }))
    }
}
