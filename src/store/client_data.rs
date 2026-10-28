//! 服务端 bootstrap 回包里的 `client_data`，按（号, 模型）记最近一份（`client_data` 表），见
//! [`crate::proxy::client_data`]。

use super::CredentialStore;
use anyhow::Result;

impl CredentialStore {
    /// 记下这个号用 `model` 拉到的最近一份 `client_data`（JSON 文本，覆盖旧的）。
    ///
    /// 号已经不在了就不写：外键拒掉按「没写」处理，不往上抛——这是握手 / 保活的后台路径，号都没了记它也没意义。
    pub async fn save_client_data(&self, cred_id: i64, model: &str, data: &str) -> Result<()> {
        let res = sqlx::query(
            "INSERT INTO client_data (cred_id, model, data, updated_at) \
             VALUES ($1, $2, $3, unixepoch()) \
             ON CONFLICT (cred_id, model) \
             DO UPDATE SET data = EXCLUDED.data, updated_at = EXCLUDED.updated_at",
        )
        .bind(cred_id)
        .bind(model)
        .bind(data)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            // 23503 = foreign_key_violation：号已经删了。
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23503") => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// 删掉这个号这几个模型的记录（按号限条数时挤出去的，见 [`crate::proxy::client_data`]）。
    pub async fn delete_client_data(&self, cred_id: i64, models: &[String]) -> Result<()> {
        sqlx::query("DELETE FROM client_data WHERE cred_id = $1 AND model = ANY($2)")
            .bind(cred_id)
            .bind(models)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 全部记录（号, 模型, `client_data` 的 JSON 文本），按号、最近更新的在前排好（同一刻的按模型）。启动时回填内存用。
    pub async fn all_client_data(&self) -> Result<Vec<(i64, String, String)>> {
        Ok(sqlx::query_as(
            "SELECT cred_id, model, data FROM client_data \
             ORDER BY cred_id, updated_at DESC, model",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::super::select::tests::soft_store;
    use sqlx::PgPool;

    /// 按（号, 模型）各记一份、后写的覆盖先写的；号不存在不写；号删了一并没了。
    #[sqlx::test]
    async fn client_data_is_kept_per_account_and_model(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let off = r#"{"tengu_lapis_anchor":"off"}"#;
        store.save_client_data(a, "claude-opus-5-5", off).await.unwrap();
        store.save_client_data(a, "claude-haiku-5-5", "{}").await.unwrap();
        store.save_client_data(b, "claude-opus-5-5", off).await.unwrap();
        store.save_client_data(b, "claude-opus-5-5", "{}").await.unwrap();
        store.save_client_data(999_999, "claude-opus-5-5", off).await.unwrap();
        let row = |id: i64, model: &str, data: &str| (id, model.to_string(), data.to_string());
        assert_eq!(
            store.all_client_data().await.unwrap(),
            vec![
                row(a, "claude-haiku-5-5", "{}"),
                row(a, "claude-opus-5-5", off),
                row(b, "claude-opus-5-5", "{}"),
            ]
        );
        assert!(store.delete(a).await.unwrap());
        assert_eq!(store.all_client_data().await.unwrap(), vec![row(b, "claude-opus-5-5", "{}")]);
    }
}
