//! 代理池。

use super::*;

/// 代理池中的一条记录。
#[derive(serde::Serialize, Clone)]
pub struct SavedProxy {
    pub id: i64,
    pub label: String,
    pub url: String,
    pub created_at: u64,
}

impl CredentialStore {
    /// 列出代理池中 `scope` 看得到的记录（[`Scope::All`] 是全部，否则只有那个人自己的）。
    pub fn list_proxies(&self, scope: Scope) -> Result<Vec<SavedProxy>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, label, url, created_at FROM proxies \
              WHERE ?1 IS NULL OR owner_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([scope.owner()], |row| {
            Ok(SavedProxy {
                id: row.get(0)?,
                label: row.get(1)?,
                url: row.get(2)?,
                created_at: row.get::<_, i64>(3)? as u64,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 读取代理池中的单条记录。
    pub fn get_proxy(&self, id: i64) -> Result<Option<SavedProxy>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, label, url, created_at FROM proxies WHERE id = ?1",
            [id],
            |row| {
                Ok(SavedProxy {
                    id: row.get(0)?,
                    label: row.get(1)?,
                    url: row.get(2)?,
                    created_at: row.get::<_, i64>(3)? as u64,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other.into()),
        })
    }

    /// 确保代理在 `owner` 的池中存在：不在则自动添加（label 取 host:port），已在则忽略。
    pub fn ensure_proxy_in_pool(&self, owner: i64, url: &str) {
        let conn = self.conn.lock();
        let label = url_to_label(url);
        if let Err(e) = conn.execute(
            "INSERT OR IGNORE INTO proxies (label, url, owner_id) VALUES (?1, ?2, ?3)",
            params![label, url, owner],
        ) {
            tracing::debug!(error = %e, url, "ensure_proxy_in_pool: insert ignored");
        }
    }

    /// 添加一条代理到 `owner` 的池中，返回新记录。`url` 应已经过
    /// `crate::clients::validate_proxy` 校验。
    pub fn add_proxy(&self, owner: i64, label: &str, url: &str) -> Result<SavedProxy> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO proxies (label, url, owner_id) VALUES (?1, ?2, ?3)",
            params![label, url, owner],
        )
        .context("failed to add proxy (the URL may already exist in the pool)")?;
        let id = conn.last_insert_rowid();
        conn.query_row(
            "SELECT id, label, url, created_at FROM proxies WHERE id = ?1",
            [id],
            |row| {
                Ok(SavedProxy {
                    id: row.get(0)?,
                    label: row.get(1)?,
                    url: row.get(2)?,
                    created_at: row.get::<_, i64>(3)? as u64,
                })
            },
        )
        .context("failed to read the newly inserted proxy")
    }

    /// 批量添加代理，单事务内完成；返回与入参一一对应的结果，地址已在池里（唯一索引撞了）的
    /// 那条是 `None`，不报错也不影响其它条。`url` 应已经过 `crate::clients::validate_proxy` 校验。
    pub fn add_proxies(
        &self,
        owner: i64,
        items: &[(String, String)],
    ) -> Result<Vec<Option<SavedProxy>>> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut out = Vec::with_capacity(items.len());
        {
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO proxies (label, url, owner_id) VALUES (?1, ?2, ?3)",
            )?;
            let mut read =
                tx.prepare("SELECT id, label, url, created_at FROM proxies WHERE id = ?1")?;
            for (label, url) in items {
                if insert.execute(params![label, url, owner])? == 0 {
                    out.push(None);
                    continue;
                }
                let id = tx.last_insert_rowid();
                out.push(Some(read.query_row([id], |row| {
                    Ok(SavedProxy {
                        id: row.get(0)?,
                        label: row.get(1)?,
                        url: row.get(2)?,
                        created_at: row.get::<_, i64>(3)? as u64,
                    })
                })?));
            }
        }
        tx.commit()?;
        Ok(out)
    }

    /// 更新代理池中一条记录的名称和/或地址。
    pub fn update_proxy(&self, id: i64, label: &str, url: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE proxies SET label = ?2, url = ?3 WHERE id = ?1",
            params![id, label, url],
        )?;
        Ok(n > 0)
    }

    /// 从池中删除一条代理（不影响已配置该代理的凭证）。
    pub fn delete_proxy(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute("DELETE FROM proxies WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    /// 批量删除代理池记录，单事务内完成；返回实际删掉的条数（不存在的 id 不计）。
    /// 与 [Self::delete_proxy] 一样只动代理池，不改凭证上的代理设置。
    pub fn delete_proxies(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM proxies WHERE id = ?1")?;
            for id in ids {
                n += stmt.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// (主人, URL) → 代理池记录 id，给账号视图标出「用的是池里哪一条」。
    pub fn proxy_ids_by_owner(&self) -> Result<HashMap<(i64, String), i64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT owner_id, url, id FROM proxies")?;
        let rows = stmt.query_map([], |r| {
            Ok(((r.get::<_, Option<i64>>(0)?.unwrap_or(0), r.get::<_, String>(1)?), r.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 统计每个代理地址有多少凭证在使用（只数 `scope` 看得到的号）。键是代理 URL，值是使用
    /// 该 URL 的凭证数量。
    pub fn proxy_usage_counts(&self, scope: Scope) -> Result<HashMap<String, i64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT proxy, COUNT(*) FROM credentials \
             WHERE proxy IS NOT NULL AND proxy != '' AND (?1 IS NULL OR owner_id = ?1) \
             GROUP BY proxy",
        )?;
        let rows = stmt.query_map([scope.owner()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = HashMap::new();
        for r in rows {
            let (url, count) = r?;
            out.insert(url, count);
        }
        Ok(out)
    }

    /// 返回每个代理 URL 对应的使用者标签列表（`proxy_url → [label1, label2, ...]`），
    /// 只列 `scope` 看得到的号。
    pub fn proxy_usage_labels(&self, scope: Scope) -> Result<HashMap<String, Vec<String>>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT proxy, label FROM credentials \
             WHERE proxy IS NOT NULL AND proxy != '' AND (?1 IS NULL OR owner_id = ?1) \
             ORDER BY proxy, label",
        )?;
        let rows = stmt.query_map([scope.owner()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for r in rows {
            let (url, label) = r?;
            out.entry(url).or_default().push(label);
        }
        Ok(out)
    }

    /// 批量设置出站代理：把 `ids` 里的账号统一改到 `proxy`（`None` 或空串改回直连）。
    /// 单事务内完成。
    pub fn set_proxies(&self, ids: &[i64], proxy: Option<&str>) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let proxy = proxy.map(str::trim).filter(|s| !s.is_empty());
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET proxy = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, proxy])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }
}

/// 从代理 URL 中提取 `host:port` 作为人可读的标签。
///
/// 先去掉 `scheme://`，再去掉 `user:pass@`，保留剩余部分（`host:port`）。
/// 解析失败时回退到完整 URL。
fn url_to_label(raw: &str) -> String {
    let after_scheme = raw.find("://").map(|i| &raw[i + 3..]).unwrap_or(raw);
    let after_auth =
        after_scheme.rfind('@').map(|i| &after_scheme[i + 1..]).unwrap_or(after_scheme);
    if after_auth.is_empty() { raw.to_string() } else { after_auth.to_string() }
}
