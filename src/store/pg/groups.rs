//! 号池分组与接入 Key（PG 版），对应 `store::groups`。概念见旧模块的文档。
//!
//! 与 SQLite 版的差别：
//! - 分组名原来是 `COLLATE NOCASE`，PG 里唯一索引建在 `lower(name)` 上，比名字一律比 `lower(...)`，
//!   撞名的识别从 rusqlite 的约束错误换成 PG 的 23505；
//! - 「先核对分组 / 开放对象存在、再写」的都走 [`PgStore::begin_write`]：核对与写入之间分组
//!   被删掉的话，会留下指向不存在分组的绑定。
//! - 建表、默认分组补齐、旧版全局接入 Key 的搬家（`migrate_groups`）不移植：schema 在
//!   `migrations/` 里，默认分组由 `seed` 补，旧数据靠导出 / 导入搬。

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use sqlx::{PgConnection, Row};

use super::super::{
    API_KEYS_CONFIGURED, ApiKey, GroupError, KeyAccess, PoolGroup, User, UserRole, dedup_ordered,
    key_hash, key_prefix, seal,
};
use super::PgStore;

/// `ids` 里的分组是不是全都存在。
async fn groups_exist(conn: &mut PgConnection, ids: &[i64]) -> Result<bool> {
    let ids = dedup_ordered(ids);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pool_groups WHERE id = ANY($1)")
        .bind(&ids)
        .fetch_one(conn)
        .await?;
    Ok(n as usize == ids.len())
}

/// 分组是不是默认分组；不存在为 None。
async fn group_is_default(conn: &mut PgConnection, id: i64) -> Result<Option<bool>> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT is_default FROM pool_groups WHERE id = $1")
        .bind(id)
        .fetch_optional(conn)
        .await?
        .map(|v| v != 0))
}

/// 整体替换一把 Key 的分组绑定（按给定顺序记 `ord`）。
async fn write_key_groups(conn: &mut PgConnection, key_id: i64, group_ids: &[i64]) -> Result<()> {
    sqlx::query("DELETE FROM api_key_groups WHERE key_id = $1")
        .bind(key_id)
        .execute(&mut *conn)
        .await?;
    for (ord, gid) in group_ids.iter().enumerate() {
        sqlx::query("INSERT INTO api_key_groups (key_id, group_id, ord) VALUES ($1, $2, $3)")
            .bind(key_id)
            .bind(gid)
            .bind(ord as i64)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// 是不是唯一约束冲突（PG 的 23505）。
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.is_unique_violation())
}

impl PgStore {
    /// 默认分组的 id。
    pub async fn default_group_id(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT id FROM pool_groups WHERE is_default = 1")
            .fetch_one(&self.pool)
            .await?)
    }

    /// 全部分组（admin / 访客用），带号数与开放名单。默认分组排第一。
    pub async fn list_groups(&self) -> Result<Vec<PoolGroup>> {
        let mut grants: HashMap<i64, Vec<i64>> = HashMap::new();
        let rows: Vec<(i64, i64)> =
            sqlx::query_as("SELECT group_id, user_id FROM group_grants ORDER BY user_id")
                .fetch_all(&self.pool)
                .await?;
        for (g, u) in rows {
            grants.entry(g).or_default().push(u);
        }
        let rows = sqlx::query(
            "SELECT g.id, g.name, g.note, g.is_default, g.created_at, \
                    (SELECT COUNT(*) FROM credential_groups c WHERE c.group_id = g.id) \
               FROM pool_groups g ORDER BY g.is_default DESC, g.id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let id: i64 = r.try_get(0)?;
                Ok(PoolGroup {
                    id,
                    name: r.try_get(1)?,
                    note: r.try_get(2)?,
                    is_default: r.try_get::<i64, _>(3)? != 0,
                    created_at: r.try_get::<i64, _>(4)? as u64,
                    credential_count: Some(r.try_get(5)?),
                    grants: Some(grants.get(&id).cloned().unwrap_or_default()),
                })
            })
            .collect()
    }

    /// 某个代理或用户能用的分组 id：默认分组，加上开放给他的；用户挂在代理名下时，开放给
    /// 那个代理的也算（继承）。
    pub async fn visible_group_ids(&self, user: &User) -> Result<HashSet<i64>> {
        let inherit = match (user.role, user.parent_id) {
            (UserRole::User, Some(parent)) => Some(parent),
            _ => None,
        };
        let rows: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM pool_groups WHERE is_default = 1 \
             UNION SELECT group_id FROM group_grants WHERE user_id = $1 \
             UNION SELECT group_id FROM group_grants WHERE user_id = $2 \
                   AND EXISTS (SELECT 1 FROM users WHERE id = $2 AND role = 'agent')",
        )
        .bind(user.id)
        .bind(inherit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// 某个代理或用户能看到的分组（只有名称与说明，不带号数和开放名单）。
    pub async fn visible_groups(&self, user: &User) -> Result<Vec<PoolGroup>> {
        let ids = self.visible_group_ids(user).await?;
        Ok(self
            .list_groups()
            .await?
            .into_iter()
            .filter(|g| ids.contains(&g.id))
            .map(|g| PoolGroup { credential_count: None, grants: None, ..g })
            .collect())
    }

    /// 新建分组。名称撞了（不区分大小写）回 [`GroupError::NameTaken`]。
    pub async fn create_group(
        &self,
        name: &str,
        note: &str,
    ) -> Result<std::result::Result<i64, GroupError>> {
        // 不写冲突目标：撞的是 lower(name) 上的唯一索引。
        let id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO pool_groups (name, note) VALUES ($1, $2) \
             ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(name)
        .bind(note)
        .fetch_optional(&self.pool)
        .await?;
        Ok(id.ok_or(GroupError::NameTaken))
    }

    /// 改分组的名称与说明。
    pub async fn update_group(
        &self,
        id: i64,
        name: &str,
        note: &str,
    ) -> Result<std::result::Result<(), GroupError>> {
        let mut tx = self.begin_write().await?;
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pool_groups WHERE lower(name) = lower($1) AND id <> $2)",
        )
        .bind(name)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Ok(Err(GroupError::NameTaken));
        }
        let n = match sqlx::query("UPDATE pool_groups SET name = $2, note = $3 WHERE id = $1")
            .bind(id)
            .bind(name)
            .bind(note)
            .execute(&mut *tx)
            .await
        {
            Ok(r) => r.rows_affected(),
            // 上面已在写锁下核对过，这里兜底：不走 begin_write 的写入（直接改库）撞上时同样报撞名。
            Err(e) if is_unique_violation(&e) => return Ok(Err(GroupError::NameTaken)),
            Err(e) => return Err(e.into()),
        };
        tx.commit().await?;
        Ok(if n > 0 { Ok(()) } else { Err(GroupError::NotFound) })
    }

    /// 删分组：默认分组不能删。组里的号若因此一个分组都不剩，挪进默认分组；开放名单与接入
    /// Key 上的绑定一并清掉。
    pub async fn delete_group(&self, id: i64) -> Result<std::result::Result<(), GroupError>> {
        let mut tx = self.begin_write().await?;
        match group_is_default(&mut tx, id).await? {
            None => return Ok(Err(GroupError::NotFound)),
            Some(true) => return Ok(Err(GroupError::DefaultGroup)),
            Some(false) => {}
        }
        sqlx::query(
            "INSERT INTO credential_groups (cred_id, group_id) \
             SELECT cg.cred_id, (SELECT id FROM pool_groups WHERE is_default = 1) \
               FROM credential_groups cg WHERE cg.group_id = $1 \
                AND NOT EXISTS (SELECT 1 FROM credential_groups o \
                                 WHERE o.cred_id = cg.cred_id AND o.group_id <> $1) \
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        for sql in [
            "DELETE FROM credential_groups WHERE group_id = $1",
            "DELETE FROM group_grants WHERE group_id = $1",
            "DELETE FROM api_key_groups WHERE group_id = $1",
            "DELETE FROM pool_groups WHERE id = $1",
        ] {
            sqlx::query(sql).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(Ok(()))
    }

    /// 整体替换一个分组的开放名单。开放对象只能是代理，或 admin 直属的用户。默认分组本来
    /// 就对所有人开放，不需要名单。
    pub async fn set_group_grants(
        &self,
        id: i64,
        user_ids: &[i64],
    ) -> Result<std::result::Result<(), GroupError>> {
        let user_ids = dedup_ordered(user_ids);
        let mut tx = self.begin_write().await?;
        match group_is_default(&mut tx, id).await? {
            None => return Ok(Err(GroupError::NotFound)),
            Some(true) => return Ok(Err(GroupError::DefaultGroup)),
            Some(false) => {}
        }
        for uid in &user_ids {
            let ok: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM users u WHERE u.id = $1 AND (u.role = 'agent' \
                    OR (u.role = 'user' AND u.parent_id = (SELECT id FROM users WHERE role = 'admin'))))",
            )
            .bind(uid)
            .fetch_one(&mut *tx)
            .await?;
            if !ok {
                return Ok(Err(GroupError::InvalidGrantee));
            }
        }
        sqlx::query("DELETE FROM group_grants WHERE group_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for uid in &user_ids {
            sqlx::query("INSERT INTO group_grants (group_id, user_id) VALUES ($1, $2)")
                .bind(id)
                .bind(uid)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(Ok(()))
    }

    /// 号 → 所在分组（升序）。
    pub async fn credential_group_map(&self) -> Result<HashMap<i64, Vec<i64>>> {
        let rows: Vec<(i64, i64)> =
            sqlx::query_as("SELECT cred_id, group_id FROM credential_groups ORDER BY group_id")
                .fetch_all(&self.pool)
                .await?;
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        for (c, g) in rows {
            out.entry(c).or_default().push(g);
        }
        Ok(out)
    }

    /// 一个号所在的分组（升序）。
    pub async fn credential_groups(&self, cred_id: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT group_id FROM credential_groups WHERE cred_id = $1 ORDER BY group_id",
        )
        .bind(cred_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 整体替换一批号的分组。至少一个分组，且都得存在；能不能选由调用方按身份先核对。
    pub async fn set_credential_groups(
        &self,
        cred_ids: &[i64],
        group_ids: &[i64],
    ) -> Result<std::result::Result<(), GroupError>> {
        let group_ids = dedup_ordered(group_ids);
        if group_ids.is_empty() {
            return Ok(Err(GroupError::Empty));
        }
        let mut tx = self.begin_write().await?;
        if !groups_exist(&mut tx, &group_ids).await? {
            return Ok(Err(GroupError::UnknownGroup));
        }
        for cid in cred_ids {
            sqlx::query("DELETE FROM credential_groups WHERE cred_id = $1")
                .bind(cid)
                .execute(&mut *tx)
                .await?;
            for gid in &group_ids {
                // 同一个号在 `cred_ids` 里出现两次时第二轮会先删再插，不会撞主键；这里照旧
                // 不吞冲突，与旧版一致。
                sqlx::query("INSERT INTO credential_groups (cred_id, group_id) VALUES ($1, $2)")
                    .bind(cid)
                    .bind(gid)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(Ok(()))
    }

    // ---------- 接入 Key ----------

    /// 全部接入 Key（不含明文）。
    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>> {
        let mut groups: HashMap<i64, Vec<i64>> = HashMap::new();
        let rows: Vec<(i64, i64)> =
            sqlx::query_as("SELECT key_id, group_id FROM api_key_groups ORDER BY key_id, ord")
                .fetch_all(&self.pool)
                .await?;
        for (k, g) in rows {
            groups.entry(k).or_default().push(g);
        }
        let rows = sqlx::query(
            "SELECT id, label, key_prefix, disabled, created_at, all_groups \
               FROM api_keys ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let id: i64 = r.try_get(0)?;
                Ok(ApiKey {
                    id,
                    label: r.try_get(1)?,
                    prefix: r.try_get(2)?,
                    disabled: r.try_get::<i64, _>(3)? != 0,
                    created_at: r.try_get::<i64, _>(4)? as u64,
                    all_groups: r.try_get::<i64, _>(5)? != 0,
                    groups: groups.get(&id).cloned().unwrap_or_default(),
                })
            })
            .collect()
    }

    /// 新建一把接入 Key，回它的 id。`group_ids` 按优先顺序，空 = 用全部号。建过一把之后
    /// 转发就恒要求带 Key（[`API_KEYS_CONFIGURED`]）。
    ///
    /// 标记与 Key 在同一个事务里落库（旧版是建完 Key 再单独写设置），提交后再同步内存镜像。
    pub async fn create_api_key(
        &self,
        label: &str,
        key: &str,
        group_ids: &[i64],
    ) -> Result<std::result::Result<i64, GroupError>> {
        let group_ids = dedup_ordered(group_ids);
        let mut tx = self.begin_write().await?;
        if !groups_exist(&mut tx, &group_ids).await? {
            return Ok(Err(GroupError::UnknownGroup));
        }
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO api_keys (label, key_hash, key_sealed, key_prefix, all_groups) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(label)
        .bind(key_hash(key))
        .bind(seal(key))
        .bind(key_prefix(key))
        .bind(group_ids.is_empty() as i64)
        .fetch_one(&mut *tx)
        .await
        .context("this API key already exists")?;
        write_key_groups(&mut tx, id, &group_ids).await?;
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ($1, '1') \
             ON CONFLICT (key) DO UPDATE SET value = '1'",
        )
        .bind(API_KEYS_CONFIGURED)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        // 设置在内存里有一份镜像（见 `PgStore::settings`），落库成功后同步。
        self.settings.write().insert(API_KEYS_CONFIGURED.to_string(), "1".to_string());
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
    pub async fn update_api_key(
        &self,
        id: i64,
        label: &str,
        disabled: bool,
        group_ids: &[i64],
        all_groups: Option<bool>,
    ) -> Result<std::result::Result<(), GroupError>> {
        let group_ids = dedup_ordered(group_ids);
        let mut tx = self.begin_write().await?;
        if all_groups == Some(false) && !groups_exist(&mut tx, &group_ids).await? {
            return Ok(Err(GroupError::UnknownGroup));
        }
        let n = sqlx::query("UPDATE api_keys SET label = $2, disabled = $3 WHERE id = $1")
            .bind(id)
            .bind(label)
            .bind(disabled as i64)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Ok(Err(GroupError::NotFound));
        }
        if let Some(all) = all_groups {
            sqlx::query("UPDATE api_keys SET all_groups = $2 WHERE id = $1")
                .bind(id)
                .bind(all as i64)
                .execute(&mut *tx)
                .await?;
            write_key_groups(&mut tx, id, if all { &[] } else { &group_ids }).await?;
        }
        tx.commit().await?;
        Ok(Ok(()))
    }

    /// 删一把 Key。返回是否确有删除。
    pub async fn delete_api_key(&self, id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM api_key_groups WHERE key_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let n = sqlx::query("DELETE FROM api_keys WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }

    /// 一把 Key 的明文（admin 查看、复制用）。
    pub async fn reveal_api_key(&self, id: i64) -> Result<Option<String>> {
        let sealed: Option<String> =
            sqlx::query_scalar("SELECT key_sealed FROM api_keys WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        sealed.map(|s| super::super::open(&s)).transpose()
    }

    /// 转发要不要求带接入 Key：库里有 Key，或者配过（[`API_KEYS_CONFIGURED`]）。从没配过、
    /// 环境变量也没设时才不校验来访身份（与老版本「没配接入 Key 就不校验」一致）；配过之后
    /// 把 Key 全删了也不会因此敞开。
    pub async fn api_keys_required(&self) -> Result<bool> {
        if self.get_setting(API_KEYS_CONFIGURED)?.is_some() {
            return Ok(true);
        }
        Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM api_keys)")
            .fetch_one(&self.pool)
            .await?)
    }

    /// 按来访带的 Key 明文认身份：启用中的 Key 才算，回它能用的分组（按优先顺序）。
    pub async fn api_key_access(&self, key: &str) -> Result<Option<KeyAccess>> {
        let hit: Option<(i64, i64)> = sqlx::query_as(
            "SELECT id, all_groups FROM api_keys WHERE key_hash = $1 AND disabled = 0",
        )
        .bind(key_hash(key))
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, all)) = hit else { return Ok(None) };
        if all != 0 {
            return Ok(Some(KeyAccess { key_id: Some(id), groups: None }));
        }
        let groups: Vec<i64> = sqlx::query_scalar(
            "SELECT group_id FROM api_key_groups WHERE key_id = $1 ORDER BY ord",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(Some(KeyAccess { key_id: Some(id), groups: Some(groups) }))
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::super::token_fingerprint;
    use super::*;

    /// 直接写库插一个号（上号走 credential 模块，不归这里），回 id。插入时由触发器落进默认分组。
    async fn insert_cred(store: &PgStore, label: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO credentials (label, access_token, refresh_token, refresh_token_hash, \
                                      expires_at) \
             VALUES ($1, $2, $3, $4, 0) RETURNING id",
        )
        .bind(label)
        .bind(seal(&format!("t-{label}")))
        .bind(seal(&format!("r-{label}")))
        .bind(token_fingerprint(&format!("r-{label}")))
        .fetch_one(&store.pool)
        .await
        .unwrap()
    }

    /// 分组可见范围：默认分组人人可见；开放给代理的，代理和它名下的用户都能用；admin 直属用户
    /// 只看开放给自己的；用户不能单独被开放（只能开放给代理或 admin 直属用户）。
    #[sqlx::test]
    async fn group_visibility_follows_grants_and_inheritance(pool: PgPool) {
        let store = PgStore::for_test(pool).await;
        let admin = store.admin_user().await.unwrap().id;
        let default = store.default_group_id().await.unwrap();
        let agent =
            store.create_user("agent", "", UserRole::Agent, admin).await.unwrap().unwrap().id;
        let sub = store.create_user("sub", "", UserRole::User, agent).await.unwrap().unwrap().id;
        let direct =
            store.create_user("direct", "", UserRole::User, admin).await.unwrap().unwrap().id;
        let g1 = store.create_group("g1", "").await.unwrap().unwrap();
        let g2 = store.create_group("g2", "").await.unwrap().unwrap();
        assert_eq!(store.create_group("G1", "").await.unwrap(), Err(GroupError::NameTaken));
        store.set_group_grants(g1, &[agent]).await.unwrap().unwrap();
        store.set_group_grants(g2, &[direct]).await.unwrap().unwrap();
        assert_eq!(
            store.set_group_grants(g2, &[sub]).await.unwrap(),
            Err(GroupError::InvalidGrantee)
        );
        assert_eq!(
            store.set_group_grants(default, &[agent]).await.unwrap(),
            Err(GroupError::DefaultGroup)
        );
        let vis = async |id: i64| {
            let u = store.user_by_id(id).await.unwrap().unwrap();
            let mut v: Vec<i64> = store.visible_group_ids(&u).await.unwrap().into_iter().collect();
            v.sort();
            v
        };
        assert_eq!(vis(agent).await, vec![default, g1]);
        assert_eq!(vis(sub).await, vec![default, g1], "代理名下的用户继承代理的分组");
        assert_eq!(vis(direct).await, vec![default, g2]);
    }

    /// 改名撞名不区分大小写；改自己的大小写不算撞。
    #[sqlx::test]
    async fn renaming_a_group_checks_names_case_insensitively(pool: PgPool) {
        let store = PgStore::for_test(pool).await;
        let g1 = store.create_group("g1", "").await.unwrap().unwrap();
        let g2 = store.create_group("g2", "").await.unwrap().unwrap();
        assert_eq!(store.update_group(g2, "G1", "").await.unwrap(), Err(GroupError::NameTaken));
        store.update_group(g1, "G1", "note").await.unwrap().unwrap();
        assert_eq!(store.update_group(9999, "x", "").await.unwrap(), Err(GroupError::NotFound));
        let g = store.list_groups().await.unwrap().into_iter().find(|g| g.id == g1).unwrap();
        assert_eq!((g.name.as_str(), g.note.as_str()), ("G1", "note"));
    }

    /// 删分组：默认分组删不了；只剩这一个分组的号挪进默认分组，还有别的分组的不动；Key 上的
    /// 绑定一并清掉。
    #[sqlx::test]
    async fn deleting_a_group_rehomes_its_only_members(pool: PgPool) {
        let store = PgStore::for_test(pool).await;
        let default = store.default_group_id().await.unwrap();
        let g1 = store.create_group("g1", "").await.unwrap().unwrap();
        let g2 = store.create_group("g2", "").await.unwrap().unwrap();
        let a = insert_cred(&store, "a").await;
        let b = insert_cred(&store, "b").await;
        store.set_credential_groups(&[a], &[g1]).await.unwrap().unwrap();
        store.set_credential_groups(&[b], &[g1, g2]).await.unwrap().unwrap();
        assert_eq!(store.set_credential_groups(&[a], &[]).await.unwrap(), Err(GroupError::Empty));
        assert_eq!(
            store.set_credential_groups(&[a], &[9999]).await.unwrap(),
            Err(GroupError::UnknownGroup)
        );
        let key = store.create_api_key("k", "key-1", &[g1, g2]).await.unwrap().unwrap();
        assert_eq!(store.delete_group(default).await.unwrap(), Err(GroupError::DefaultGroup));
        store.delete_group(g1).await.unwrap().unwrap();
        assert_eq!(store.credential_groups(a).await.unwrap(), vec![default]);
        assert_eq!(store.credential_groups(b).await.unwrap(), vec![g2]);
        assert_eq!(store.api_key_access("key-1").await.unwrap().unwrap().groups, Some(vec![g2]));
        assert_eq!(store.list_api_keys().await.unwrap()[0].id, key);
    }

    /// 停用的 Key 认不出来；库里有 Key 时 `has_api_keys` 为真（停用的也算，不会因此变成放行）。
    #[sqlx::test]
    async fn disabled_api_keys_are_rejected(pool: PgPool) {
        let store = PgStore::for_test(pool).await;
        assert!(!store.api_keys_required().await.unwrap());
        let id = store.create_api_key("k", "key-x", &[]).await.unwrap().unwrap();
        assert!(store.api_key_access("key-x").await.unwrap().is_some());
        store.update_api_key(id, "k", true, &[], None).await.unwrap().unwrap();
        assert!(store.api_key_access("key-x").await.unwrap().is_none());
        assert!(store.api_keys_required().await.unwrap());
        assert_eq!(store.reveal_api_key(id).await.unwrap().as_deref(), Some("key-x"));
        let sealed: String = sqlx::query_scalar("SELECT key_sealed FROM api_keys WHERE id = $1")
            .bind(id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!(sealed.starts_with("enc1:"), "库里存的是密文");
    }

    /// 配过接入 Key 之后把 Key 全删了，转发仍要求带 Key（不退回「不校验」）。
    #[sqlx::test]
    async fn deleting_every_key_keeps_auth_required(pool: PgPool) {
        let store = PgStore::for_test(pool.clone()).await;
        assert!(!store.api_keys_required().await.unwrap(), "从没配过：不校验");
        let id = store.create_api_key("k", "key-only", &[]).await.unwrap().unwrap();
        store.delete_api_key(id).await.unwrap();
        assert!(store.api_keys_required().await.unwrap(), "删光了也仍要求带 Key");
        assert!(store.api_key_access("key-only").await.unwrap().is_none());
        // 标记落了库：重启（重新读设置）之后照样要求。
        let reopened = PgStore::for_test(pool).await;
        assert!(reopened.api_keys_required().await.unwrap());
    }

    /// Key 唯一绑定的分组被删掉：这把 Key 变成一个号都不能用，而不是全部号；显式不绑分组的
    /// Key 才是全部号。（旧测试里「拿这把 Key 选号失败」那一句归 select 模块。）
    #[sqlx::test]
    async fn deleting_a_keys_only_group_fails_closed(pool: PgPool) {
        let store = PgStore::for_test(pool).await;
        let g = store.create_group("g", "").await.unwrap().unwrap();
        insert_cred(&store, "a").await;
        store.create_api_key("bound", "key-bound", &[g]).await.unwrap().unwrap();
        store.create_api_key("all", "key-all", &[]).await.unwrap().unwrap();
        store.delete_group(g).await.unwrap().unwrap();
        let bound = store.api_key_access("key-bound").await.unwrap().unwrap();
        assert_eq!(bound.groups, Some(vec![]));
        assert_eq!(store.api_key_access("key-all").await.unwrap().unwrap().groups, None);
        let keys = store.list_api_keys().await.unwrap();
        assert!(!keys.iter().find(|k| k.label == "bound").unwrap().all_groups);
    }

    /// 绑定的分组删光后，只改名字、停用再启用都不会把这把 Key 变成全部号；显式选「全部号」才是。
    #[sqlx::test]
    async fn editing_an_orphaned_key_never_widens_it(pool: PgPool) {
        let store = PgStore::for_test(pool).await;
        let g = store.create_group("g", "").await.unwrap().unwrap();
        let id = store.create_api_key("bound", "key-orphan", &[g]).await.unwrap().unwrap();
        store.delete_group(g).await.unwrap().unwrap();
        let scope = async || store.api_key_access("key-orphan").await.unwrap().map(|a| a.groups);
        store.update_api_key(id, "renamed", false, &[], None).await.unwrap().unwrap();
        assert_eq!(scope().await, Some(Some(vec![])), "改名不放开");
        store.update_api_key(id, "renamed", true, &[], None).await.unwrap().unwrap();
        store.update_api_key(id, "renamed", false, &[], None).await.unwrap().unwrap();
        assert_eq!(scope().await, Some(Some(vec![])), "停用再启用不放开");
        store.update_api_key(id, "renamed", false, &[], Some(false)).await.unwrap().unwrap();
        assert_eq!(scope().await, Some(Some(vec![])), "显式只限分组、列表为空：仍是一个号都不能用");
        store.update_api_key(id, "renamed", false, &[], Some(true)).await.unwrap().unwrap();
        assert_eq!(scope().await, Some(None), "显式选全部号才是全部号");
    }

    /// 同一把 Key 不能建两次。
    #[sqlx::test]
    async fn duplicate_api_keys_are_refused(pool: PgPool) {
        let store = PgStore::for_test(pool).await;
        store.create_api_key("a", "key-dup", &[]).await.unwrap().unwrap();
        assert!(store.create_api_key("b", "key-dup", &[]).await.is_err());
        assert_eq!(store.list_api_keys().await.unwrap().len(), 1);
    }
}
