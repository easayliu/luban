//! eval 里决定带不带 `x-claude-code-sandy-plover` 的两个开关，按号记最近一份（`tools_used_flags` 表），见
//! [`crate::proxy::sandy_plover`]。

use super::CredentialStore;
use anyhow::Result;

impl CredentialStore {
    /// 记下这个号最近一份 eval 里的（`tengu_snug_mitten`，`tengu_silly_shore`），覆盖旧的。
    ///
    /// 号已经不在了就不写：外键拒掉按「没写」处理，不往上抛——这是握手 / 保活的后台路径，号都没了记它也没意义。
    pub async fn save_tools_used_flags(
        &self,
        cred_id: i64,
        snug_mitten: bool,
        silly_shore: bool,
    ) -> Result<()> {
        let res = sqlx::query(
            "INSERT INTO tools_used_flags (cred_id, snug_mitten, silly_shore, updated_at) \
             VALUES ($1, $2, $3, unixepoch()) \
             ON CONFLICT (cred_id) \
             DO UPDATE SET snug_mitten = EXCLUDED.snug_mitten, \
                           silly_shore = EXCLUDED.silly_shore, \
                           updated_at = EXCLUDED.updated_at",
        )
        .bind(cred_id)
        .bind(snug_mitten)
        .bind(silly_shore)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            // 23503 = foreign_key_violation：号已经删了。
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23503") => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// 全部记录（号, snug_mitten, silly_shore），按号排好。启动时回填内存用。
    pub async fn all_tools_used_flags(&self) -> Result<Vec<(i64, bool, bool)>> {
        Ok(sqlx::query_as(
            "SELECT cred_id, snug_mitten, silly_shore FROM tools_used_flags ORDER BY cred_id",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::super::select::tests::soft_store;
    use sqlx::PgPool;

    /// 按号各记一份、后写的覆盖先写的；号不存在不写；号删了一并没了。
    #[sqlx::test]
    async fn tools_used_flags_are_kept_per_account(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        store.save_tools_used_flags(a, true, true).await.unwrap();
        store.save_tools_used_flags(b, true, true).await.unwrap();
        store.save_tools_used_flags(b, false, true).await.unwrap();
        store.save_tools_used_flags(999_999, true, false).await.unwrap();
        assert_eq!(
            store.all_tools_used_flags().await.unwrap(),
            vec![(a, true, true), (b, false, true)]
        );
        assert!(store.delete(a).await.unwrap());
        assert_eq!(store.all_tools_used_flags().await.unwrap(), vec![(b, false, true)]);
    }
}
