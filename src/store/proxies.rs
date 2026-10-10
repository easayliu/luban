//! 代理池。

/// 代理池中的一条记录。
#[derive(serde::Serialize, Clone)]
pub struct SavedProxy {
    pub id: i64,
    pub label: String,
    pub url: String,
    pub created_at: u64,
}

/// 从代理 URL 中提取 `host:port` 作为人可读的标签。
///
/// 先去掉 `scheme://`，再去掉 `user:pass@`，保留剩余部分（`host:port`）。
/// 解析失败时回退到完整 URL。
pub(super) fn url_to_label(raw: &str) -> String {
    let after_scheme = raw.find("://").map(|i| &raw[i + 3..]).unwrap_or(raw);
    let after_auth =
        after_scheme.rfind('@').map(|i| &after_scheme[i + 1..]).unwrap_or(after_scheme);
    if after_auth.is_empty() { raw.to_string() } else { after_auth.to_string() }
}

use std::collections::HashMap;

use anyhow::{Context, Result};
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::CredentialStore;
use super::Scope;

/// 读一条代理池记录的列清单，与 [`row_to_proxy`] 一一对应。
const PROXY_COLS: &str = "id, label, url, created_at";

fn row_to_proxy(row: &PgRow) -> Result<SavedProxy> {
    Ok(SavedProxy {
        id: row.try_get(0)?,
        label: row.try_get(1)?,
        url: row.try_get(2)?,
        created_at: row.try_get::<i64, _>(3)? as u64,
    })
}

impl CredentialStore {
    /// 列出代理池中 `scope` 看得到的记录（[`Scope::All`] 是全部，否则只有那个人自己的）。
    pub async fn list_proxies(&self, scope: Scope) -> Result<Vec<SavedProxy>> {
        let rows = sqlx::query(
            "SELECT id, label, url, created_at FROM proxies \
              WHERE $1::BIGINT IS NULL OR owner_id = $1 ORDER BY id ASC",
        )
        .bind(scope.owner())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_proxy).collect()
    }

    /// 读取代理池中的单条记录。
    pub async fn get_proxy(&self, id: i64) -> Result<Option<SavedProxy>> {
        let row = sqlx::query("SELECT id, label, url, created_at FROM proxies WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(row_to_proxy).transpose()
    }

    /// 确保代理在 `owner` 的池中存在：不在则自动添加（label 取 host:port），已在则忽略。
    pub async fn ensure_proxy_in_pool(&self, owner: i64, url: &str) {
        let label = url_to_label(url);
        if let Err(e) = sqlx::query(
            "INSERT INTO proxies (label, url, owner_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(label)
        .bind(url)
        .bind(owner)
        .execute(&self.pool)
        .await
        {
            tracing::debug!(error = %e, url, "ensure_proxy_in_pool: insert ignored");
        }
    }

    /// 添加一条代理到 `owner` 的池中，返回新记录。`url` 应已经过
    /// `crate::clients::validate_proxy` 校验。
    pub async fn add_proxy(&self, owner: i64, label: &str, url: &str) -> Result<SavedProxy> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO proxies (label, url, owner_id) VALUES ($1, $2, $3) RETURNING {PROXY_COLS}"
        )))
        .bind(label)
        .bind(url)
        .bind(owner)
        .fetch_one(&self.pool)
        .await
        .context("failed to add proxy (the URL may already exist in the pool)")?;
        row_to_proxy(&row)
    }

    /// 批量添加代理，单事务内完成；返回与入参一一对应的结果，地址已在池里（唯一索引撞了）的
    /// 那条是 `None`，不报错也不影响其它条。`url` 应已经过 `crate::clients::validate_proxy` 校验。
    ///
    /// 撞唯一索引用 `ON CONFLICT DO NOTHING` 吞掉而不是捕获错误：PG 里事务内任何一条语句报错，
    /// 整个事务就废了，后面的条都写不进去。
    pub async fn add_proxies(
        &self,
        owner: i64,
        items: &[(String, String)],
    ) -> Result<Vec<Option<SavedProxy>>> {
        let sql = format!(
            "INSERT INTO proxies (label, url, owner_id) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING RETURNING {PROXY_COLS}"
        );
        let mut tx = self.pool.begin().await?;
        let mut out = Vec::with_capacity(items.len());
        for (label, url) in items {
            let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(label)
                .bind(url)
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?;
            out.push(row.as_ref().map(row_to_proxy).transpose()?);
        }
        tx.commit().await?;
        Ok(out)
    }

    /// 更新代理池中一条记录的名称和/或地址。
    pub async fn update_proxy(&self, id: i64, label: &str, url: &str) -> Result<bool> {
        self.update_one(
            sqlx::query("UPDATE proxies SET label = $2, url = $3 WHERE id = $1")
                .bind(id)
                .bind(label)
                .bind(url),
        )
        .await
    }

    /// 从池中删除一条代理（不影响已配置该代理的凭证）。
    pub async fn delete_proxy(&self, id: i64) -> Result<bool> {
        self.update_one(sqlx::query("DELETE FROM proxies WHERE id = $1").bind(id)).await
    }

    /// 批量删除代理池记录，一条语句完成；返回实际删掉的条数（不存在的 id 不计）。
    /// 与 [Self::delete_proxy] 一样只动代理池，不改凭证上的代理设置。
    pub async fn delete_proxies(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        Ok(sqlx::query("DELETE FROM proxies WHERE id = ANY($1)")
            .bind(ids)
            .execute(&self.pool)
            .await?
            .rows_affected() as usize)
    }

    /// (主人, URL) → 代理池记录 id，给账号视图标出「用的是池里哪一条」。
    pub async fn proxy_ids_by_owner(&self) -> Result<HashMap<(i64, String), i64>> {
        let rows: Vec<(Option<i64>, String, i64)> =
            sqlx::query_as("SELECT owner_id, url, id FROM proxies").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(owner, url, id)| ((owner.unwrap_or(0), url), id)).collect())
    }

    /// 统计每个代理地址有多少凭证在使用（只数 `scope` 看得到的号）。键是代理 URL，值是使用
    /// 该 URL 的凭证数量。
    pub async fn proxy_usage_counts(&self, scope: Scope) -> Result<HashMap<String, i64>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT proxy, COUNT(*) FROM credentials \
             WHERE proxy IS NOT NULL AND proxy <> '' AND ($1::BIGINT IS NULL OR owner_id = $1) \
             GROUP BY proxy",
        )
        .bind(scope.owner())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// 返回每个代理 URL 对应的使用者标签列表（`proxy_url → [label1, label2, ...]`），
    /// 只列 `scope` 看得到的号。
    pub async fn proxy_usage_labels(&self, scope: Scope) -> Result<HashMap<String, Vec<String>>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT proxy, label FROM credentials \
             WHERE proxy IS NOT NULL AND proxy <> '' AND ($1::BIGINT IS NULL OR owner_id = $1) \
             ORDER BY proxy COLLATE \"C\", label COLLATE \"C\"",
        )
        .bind(scope.owner())
        .fetch_all(&self.pool)
        .await?;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for (url, label) in rows {
            out.entry(url).or_default().push(label);
        }
        Ok(out)
    }

    /// 批量设置出站代理：把 `ids` 里的账号统一改到 `proxy`（`None` 或空串改回直连）。
    /// 一条语句完成。
    pub async fn set_proxies(&self, ids: &[i64], proxy: Option<&str>) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let proxy = proxy.map(str::trim).filter(|s| !s.is_empty());
        Ok(sqlx::query(
            "UPDATE credentials SET proxy = $2, updated_at = unixepoch() WHERE id = ANY($1)",
        )
        .bind(ids)
        .bind(proxy)
        .execute(&self.pool)
        .await?
        .rows_affected() as usize)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    /// 批量删除只删池里的记录，不存在的 id 不计入条数。
    #[sqlx::test]
    async fn delete_proxies_removes_only_given_ids(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.add_proxy(1, "a", "socks5h://10.0.0.1:1080").await.unwrap();
        let b = store.add_proxy(1, "b", "socks5h://10.0.0.2:1080").await.unwrap();
        let c = store.add_proxy(1, "c", "socks5h://10.0.0.3:1080").await.unwrap();
        assert_eq!(store.delete_proxies(&[a.id, c.id, 9999]).await.unwrap(), 2);
        let left: Vec<i64> =
            store.list_proxies(Scope::All).await.unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(left, vec![b.id]);
        assert_eq!(store.delete_proxies(&[]).await.unwrap(), 0);
    }

    /// 批量添加：已在池里的地址返回 None，其余照常写入。
    #[sqlx::test]
    async fn add_proxies_skips_urls_already_in_the_pool(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        store.add_proxy(1, "old", "socks5h://10.0.0.1:1080").await.unwrap();
        let out = store
            .add_proxies(
                1,
                &[
                    ("a".into(), "socks5h://10.0.0.1:1080".into()),
                    ("b".into(), "socks5h://10.0.0.2:1080".into()),
                ],
            )
            .await
            .unwrap();
        assert!(out[0].is_none());
        assert_eq!(out[1].as_ref().unwrap().label, "b");
        assert_eq!(store.list_proxies(Scope::All).await.unwrap().len(), 2);
    }
}
