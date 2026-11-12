//! 分享链接：admin 签发、带有效期的只读访问凭据。
//!
//! 访客密码只有一份，发给几个人就是几个人共用，想收回其中一个只能整体换密码。分享链接每人
//! （或每个场合）一条，各带到期时间：持有者以访客身份查看控制台（权限与访客完全相同，见
//! `auth` 里的鉴权分支），到期即失效，删掉即撤销，互不牵连。
//!
//! 链接不挂在任何控制台账号名下，也不随谁的密码变：它是 admin 单独发出去的东西，靠到期与
//! 删除收回。清除管理密码（控制台回到初始化状态）时一并清空，见 `auth::change_password`。
//! 库里只存 sha256，明文只在新建时回一次。

use anyhow::Result;

use super::provision::KEY_PREFIX_LEN;
use super::{CredentialStore, token_fingerprint};

/// 分享链接凭据的明文前缀。鉴权中间件按它把 Bearer 分流到这里，不去查会话表。
pub const SHARE_KEY_PREFIX: &str = "lbs-";

/// 有效期下限与上限（秒）：5 分钟到 90 天。再长就该给人开个正式账号了。
pub const SHARE_MIN_TTL_SECS: u64 = 5 * 60;
pub const SHARE_MAX_TTL_SECS: u64 = 90 * 86_400;

/// 过期多久之后从库里删掉（秒）。过期的先留一阵，列表里还看得到「已过期」，好知道发过什么。
const SHARE_EXPIRED_RETENTION_SECS: i64 = 30 * 86_400;

/// 一条分享链接（不含明文）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ShareLink {
    pub id: i64,
    pub label: String,
    /// 明文开头几位，列表里认链接用。
    pub prefix: String,
    pub created_at: u64,
    pub expires_at: u64,
    /// 最近一次被用来打开控制台的时间；从没用过为 None。
    pub last_used_at: Option<u64>,
}

/// 按明文认出的有效链接。
#[derive(Debug, Clone)]
pub struct ShareLinkHit {
    pub id: i64,
    pub label: String,
    pub expires_at: u64,
}

type Row = (i64, String, String, i64, i64, Option<i64>);

fn from_row((id, label, prefix, created_at, expires_at, last_used_at): Row) -> ShareLink {
    ShareLink {
        id,
        label,
        prefix,
        created_at: created_at as u64,
        expires_at: expires_at as u64,
        last_used_at: last_used_at.map(|v| v as u64),
    }
}

impl CredentialStore {
    /// 全部分享链接，新建的在前；顺手删掉过期超过 [`SHARE_EXPIRED_RETENTION_SECS`] 的。
    pub async fn list_share_links(&self) -> Result<Vec<ShareLink>> {
        sqlx::query("DELETE FROM share_links WHERE expires_at < unixepoch() - $1")
            .bind(SHARE_EXPIRED_RETENTION_SECS)
            .execute(&self.pool)
            .await?;
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, label, key_prefix, created_at, expires_at, last_used_at \
               FROM share_links ORDER BY id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(from_row).collect())
    }

    /// 新建一条，`ttl_secs` 秒后到期。回新建的那条（不含明文）。
    pub async fn create_share_link(
        &self,
        label: &str,
        key: &str,
        ttl_secs: u64,
    ) -> Result<ShareLink> {
        let row: Row = sqlx::query_as(
            "INSERT INTO share_links (label, key_hash, key_prefix, expires_at) \
             VALUES ($1, $2, $3, unixepoch() + $4) \
             RETURNING id, label, key_prefix, created_at, expires_at, last_used_at",
        )
        .bind(label)
        .bind(token_fingerprint(key))
        .bind(key.chars().take(KEY_PREFIX_LEN).collect::<String>())
        .bind(ttl_secs as i64)
        .fetch_one(&self.pool)
        .await?;
        Ok(from_row(row))
    }

    /// 删一条（撤销）。返回是否确有删除。
    pub async fn delete_share_link(&self, id: i64) -> Result<bool> {
        self.update_one(sqlx::query("DELETE FROM share_links WHERE id = $1").bind(id)).await
    }

    /// 全部删掉（清除管理密码时）。
    pub async fn delete_all_share_links(&self) -> Result<()> {
        sqlx::query("DELETE FROM share_links").execute(&self.pool).await?;
        Ok(())
    }

    /// 按明文认链接：还没到期的才算。
    pub async fn share_link_lookup(&self, key: &str) -> Result<Option<ShareLinkHit>> {
        let hit: Option<(i64, String, i64)> = sqlx::query_as(
            "SELECT id, label, expires_at FROM share_links \
              WHERE key_hash = $1 AND expires_at > unixepoch()",
        )
        .bind(token_fingerprint(key))
        .fetch_optional(&self.pool)
        .await?;
        Ok(hit.map(|(id, label, expires_at)| ShareLinkHit {
            id,
            label,
            expires_at: expires_at as u64,
        }))
    }

    /// 记下使用时间。一分钟内已记过则不再写：打开控制台一屏就是十几个请求。
    pub async fn touch_share_link(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE share_links SET last_used_at = unixepoch() \
              WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < unixepoch() - 60)",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::store::generate_prefixed_key;

    /// 建、认、删走一遍；过期的认不出，过期太久的列表时清掉。
    #[sqlx::test]
    async fn share_link_lifecycle(pool: PgPool) {
        let store = CredentialStore::for_test(pool.clone()).await;
        let key = generate_prefixed_key(SHARE_KEY_PREFIX);
        let link = store.create_share_link("客户 A", &key, 3600).await.unwrap();
        assert_eq!(link.expires_at - link.created_at, 3600);
        assert_eq!(link.prefix, key[..KEY_PREFIX_LEN]);
        assert!(store.create_share_link("dup", &key, 3600).await.is_err());

        let hit = store.share_link_lookup(&key).await.unwrap().unwrap();
        assert_eq!((hit.id, hit.label.as_str()), (link.id, "客户 A"));
        assert!(store.share_link_lookup("lbs-nope").await.unwrap().is_none());

        store.touch_share_link(link.id).await.unwrap();
        let listed = store.list_share_links().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].last_used_at.is_some());

        // 到期：认不出，但列表里还在。
        sqlx::query("UPDATE share_links SET expires_at = unixepoch() - 1")
            .execute(&pool)
            .await
            .unwrap();
        assert!(store.share_link_lookup(&key).await.unwrap().is_none());
        assert_eq!(store.list_share_links().await.unwrap().len(), 1);
        // 过期超过保留期：列表时删掉。
        sqlx::query("UPDATE share_links SET expires_at = unixepoch() - $1")
            .bind(SHARE_EXPIRED_RETENTION_SECS + 10)
            .execute(&pool)
            .await
            .unwrap();
        assert!(store.list_share_links().await.unwrap().is_empty());

        let key2 = generate_prefixed_key(SHARE_KEY_PREFIX);
        let l2 = store.create_share_link("", &key2, 3600).await.unwrap();
        assert!(store.delete_share_link(l2.id).await.unwrap());
        assert!(!store.delete_share_link(l2.id).await.unwrap());
        assert!(store.share_link_lookup(&key2).await.unwrap().is_none());
    }
}
