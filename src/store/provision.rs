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

/// 生成一把新上号 Key：`lbp-` + 40 位字母数字。
pub fn generate_provision_key() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 40];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
    let body: String =
        bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    format!("{PROVISION_KEY_PREFIX}{body}")
}

use anyhow::{Context, Result};
use sqlx::Row;

use super::CredentialStore;
use super::users::user_by_id_on;
use super::{UserRole, token_fingerprint};

impl CredentialStore {
    /// 上号 Key 列表。`owner` 为 None 列全部（admin），否则只列这个人名下的。
    pub async fn list_provision_keys(&self, owner: Option<i64>) -> Result<Vec<ProvisionKey>> {
        let rows = sqlx::query(
            "SELECT k.id, k.user_id, COALESCE(u.username, ''), k.label, k.key_prefix, k.disabled, \
                    k.created_at, k.last_used_at, k.pw_tag, u.role, u.password_hash \
               FROM provision_keys k LEFT JOIN users u ON u.id = k.user_id \
              WHERE $1::BIGINT IS NULL OR k.user_id = $1 ORDER BY k.id",
        )
        .bind(owner)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(ProvisionKey {
                    id: r.try_get(0)?,
                    user_id: r.try_get(1)?,
                    username: r.try_get(2)?,
                    label: r.try_get(3)?,
                    prefix: r.try_get(4)?,
                    disabled: r.try_get::<i64, _>(5)? != 0,
                    created_at: r.try_get::<i64, _>(6)? as u64,
                    last_used_at: r.try_get::<Option<i64>, _>(7)?.map(|v| v as u64),
                    pw_tag: r.try_get(8)?,
                    owner: match r.try_get::<Option<String>, _>(9)? {
                        Some(role) => Some((
                            UserRole::parse(&role)?,
                            r.try_get::<Option<String>, _>(10)?.unwrap_or_default(),
                        )),
                        None => None,
                    },
                })
            })
            .collect()
    }

    /// 给 `user_id` 新建一把上号 Key，回它的 id。`pw_tag` 是这个人此刻的密码指纹。
    pub async fn create_provision_key(
        &self,
        user_id: i64,
        label: &str,
        key: &str,
        pw_tag: &str,
    ) -> Result<i64> {
        sqlx::query_scalar(
            "INSERT INTO provision_keys (user_id, label, key_hash, key_prefix, pw_tag) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(user_id)
        .bind(label)
        .bind(token_fingerprint(key))
        .bind(key.chars().take(KEY_PREFIX_LEN).collect::<String>())
        .bind(pw_tag)
        .fetch_one(&self.pool)
        .await
        .context("this provision key already exists")
    }

    /// 改名称与停用状态。`owner` 给了的话只改这个人名下的。返回是否确有这把 Key。
    pub async fn update_provision_key(
        &self,
        id: i64,
        owner: Option<i64>,
        label: &str,
        disabled: bool,
    ) -> Result<bool> {
        self.update_one(
            sqlx::query(
                "UPDATE provision_keys SET label = $3, disabled = $4 \
                  WHERE id = $1 AND ($2::BIGINT IS NULL OR user_id = $2)",
            )
            .bind(id)
            .bind(owner)
            .bind(label)
            .bind(disabled as i64),
        )
        .await
    }

    /// 删一把 Key。`owner` 给了的话只删这个人名下的。返回是否确有删除。
    pub async fn delete_provision_key(&self, id: i64, owner: Option<i64>) -> Result<bool> {
        self.update_one(
            sqlx::query(
                "DELETE FROM provision_keys WHERE id = $1 AND ($2::BIGINT IS NULL OR user_id = $2)",
            )
            .bind(id)
            .bind(owner),
        )
        .await
    }

    /// 按明文认 Key：启用中、所属账号还在的才算。账号停用与密码指纹由调用方判，见
    /// [`super::CredentialStore::provision_key_lookup`]。
    pub async fn provision_key_lookup(&self, key: &str) -> Result<Option<ProvisionKeyHit>> {
        let mut conn = self.pool.acquire().await?;
        let hit: Option<(i64, i64, String)> = sqlx::query_as(
            "SELECT id, user_id, pw_tag FROM provision_keys WHERE key_hash = $1 AND disabled = 0",
        )
        .bind(token_fingerprint(key))
        .fetch_optional(&mut *conn)
        .await?;
        let Some((id, user_id, pw_tag)) = hit else { return Ok(None) };
        let Some(user) = user_by_id_on(&mut conn, user_id).await? else {
            return Ok(None);
        };
        let password_hash: String =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_one(&mut *conn)
                .await?;
        Ok(Some(ProvisionKeyHit { id, user, pw_tag, password_hash }))
    }

    /// 记下一把 Key 的使用时间（认证通过之后）。
    pub async fn touch_provision_key(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE provision_keys SET last_used_at = unixepoch() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::generate_provision_key;
    use super::*;

    /// 建、认、改、删走一遍：停用的认不出，别人名下的改不动也删不掉。
    #[sqlx::test]
    async fn provision_key_lifecycle(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let admin = store.admin_user().await.unwrap().id;
        let user = store.create_user("user", "h", UserRole::User, admin).await.unwrap().unwrap().id;
        let key = generate_provision_key();
        let id = store.create_provision_key(user, "script", &key, "tag").await.unwrap();
        assert!(store.create_provision_key(user, "dup", &key, "tag").await.is_err());

        let hit = store.provision_key_lookup(&key).await.unwrap().unwrap();
        assert_eq!((hit.id, hit.user.id, hit.pw_tag.as_str()), (id, user, "tag"));
        assert_eq!(hit.password_hash, "h");
        assert!(store.provision_key_lookup("lbp-nope").await.unwrap().is_none());

        store.touch_provision_key(id).await.unwrap();
        let listed = store.list_provision_keys(Some(user)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].username, "user");
        assert_eq!(listed[0].prefix, key[..KEY_PREFIX_LEN]);
        assert!(listed[0].last_used_at.is_some());
        assert_eq!(listed[0].owner, Some((UserRole::User, "h".to_string())));
        assert!(store.list_provision_keys(Some(admin)).await.unwrap().is_empty());
        assert_eq!(store.list_provision_keys(None).await.unwrap().len(), 1);

        assert!(!store.update_provision_key(id, Some(admin), "x", true).await.unwrap());
        assert!(store.update_provision_key(id, Some(user), "renamed", true).await.unwrap());
        assert!(store.provision_key_lookup(&key).await.unwrap().is_none(), "停用的认不出");
        assert_eq!(store.list_provision_keys(None).await.unwrap()[0].label, "renamed");

        assert!(!store.delete_provision_key(id, Some(admin)).await.unwrap());
        assert!(store.delete_provision_key(id, None).await.unwrap());
        assert!(store.list_provision_keys(None).await.unwrap().is_empty());
    }

    /// 删人时连带删掉他名下的 Key。
    #[sqlx::test]
    async fn deleting_user_drops_their_provision_keys(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let admin = store.admin_user().await.unwrap().id;
        let user = store.create_user("user", "", UserRole::User, admin).await.unwrap().unwrap().id;
        let key = generate_provision_key();
        store.create_provision_key(user, "", &key, "").await.unwrap();
        store.delete_user(user, None).await.unwrap().unwrap();
        assert!(store.list_provision_keys(None).await.unwrap().is_empty());
        assert!(store.provision_key_lookup(&key).await.unwrap().is_none());
    }
}
