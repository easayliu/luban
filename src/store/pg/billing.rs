//! 费用汇总（`billing_hourly`）的 PG 版，对应 `store::billing`：按小时 × 号主 × 号 × 接入 Key ×
//! 分组 × 模型累加请求数、token 与等价 API 费用，长期保留，给分层账单读。
//!
//! 归属口径与 SQLite 版逐字相同（见 `store::billing` 的模块文档）：每条流水在写入的**同一事务**
//! 里累加一行（[`billing_record`]），号主取写入那一刻这个号的 owner，分组取「通过哪个分组选中的」，
//! 接入 Key 为 0 表示环境变量那把或没配 Key 时放行的来访。
//!
//! 存量流水的一次性回填（`migrate_billing`）不移植：PG 库从空表起步，数据靠导出 / 导入搬。

use std::collections::HashMap;

use anyhow::Result;
use sqlx::postgres::PgArguments;
use sqlx::{Arguments, PgConnection, Row};

use super::super::{BILLING_BUCKET_SECS, BillingDim, BillingFilter, BillingRow, UsageRecord};
use super::PgStore;

/// 写流水的同一事务里累加费用汇总（只记落在某个号上的；本地拒绝没号可记）。
///
/// `owner` 是调用方在同一事务里、给这个号加锁时顺手读到的号主（号已删时为 `None`，记 0）——
/// 号主就定在这一刻。分组的两段回落（Key 的分组顺序里第一个含这个号的 → 这个号所在分组里
/// id 最小的 → 0）并进同一条 INSERT 的子查询：这条在每条转发请求后都跑，少两次往返。
pub(super) async fn billing_record(
    conn: &mut PgConnection,
    ts: i64,
    rec: &UsageRecord,
    owner: Option<i64>,
) -> Result<()> {
    let Some(cred_id) = rec.cred_id else { return Ok(()) };
    sqlx::query(
        "INSERT INTO billing_hourly AS b
             (hour, owner_id, cred_id, key_id, group_id, model, requests, input_tokens,
              output_tokens, cache_write_tokens, cache_read_tokens, cost_usd)
         VALUES ($1, $2, $3, COALESCE($4, 0),
                 COALESCE(
                     (SELECT kg.group_id FROM api_key_groups kg
                        JOIN credential_groups cg ON cg.group_id = kg.group_id AND cg.cred_id = $3
                       WHERE kg.key_id = $4 ORDER BY kg.ord LIMIT 1),
                     (SELECT MIN(group_id) FROM credential_groups WHERE cred_id = $3),
                     0),
                 $5, 1, $6, $7, $8, $9, $10)
         ON CONFLICT (hour, owner_id, cred_id, key_id, group_id, model) DO UPDATE SET
             requests = b.requests + 1,
             input_tokens = b.input_tokens + excluded.input_tokens,
             output_tokens = b.output_tokens + excluded.output_tokens,
             cache_write_tokens = b.cache_write_tokens + excluded.cache_write_tokens,
             cache_read_tokens = b.cache_read_tokens + excluded.cache_read_tokens,
             cost_usd = b.cost_usd + excluded.cost_usd",
    )
    .bind((ts / BILLING_BUCKET_SECS) * BILLING_BUCKET_SECS)
    .bind(owner.unwrap_or(0))
    .bind(cred_id)
    .bind(rec.key_id)
    .bind(rec.model.clone().unwrap_or_default())
    .bind(rec.input_tokens.unwrap_or(0))
    .bind(rec.output_tokens.unwrap_or(0))
    .bind(rec.cache_creation_tokens.unwrap_or(0))
    .bind(rec.cache_read_tokens.unwrap_or(0))
    .bind(rec.cost_usd.unwrap_or(0.0))
    .execute(conn)
    .await?;
    Ok(())
}

impl PgStore {
    /// 按一维拆账单。费用从高到低；按日拆时按日期先后。
    pub async fn billing_breakdown(
        &self,
        f: &BillingFilter,
        dim: BillingDim,
    ) -> Result<Vec<BillingRow>> {
        let mut clauses = vec!["hour >= $1".to_string(), "hour < $2".to_string()];
        let mut args = PgArguments::default();
        args.add(f.since).map_err(anyhow::Error::from_boxed)?;
        args.add(f.until).map_err(anyhow::Error::from_boxed)?;
        let mut n = 2;
        if let Some(c) = f.cred_id {
            n += 1;
            clauses.push(format!("cred_id = ${n}"));
            args.add(c).map_err(anyhow::Error::from_boxed)?;
        }
        if let Some(k) = f.key_id {
            n += 1;
            clauses.push(format!("key_id = ${n}"));
            args.add(k).map_err(anyhow::Error::from_boxed)?;
        }
        if let Some(g) = f.group_id {
            n += 1;
            clauses.push(format!("group_id = ${n}"));
            args.add(g).map_err(anyhow::Error::from_boxed)?;
        }
        if let Some(m) = &f.model {
            n += 1;
            clauses.push(format!("model = ${n}"));
            args.add(m.clone()).map_err(anyhow::Error::from_boxed)?;
        }
        if let Some(owners) = &f.owners {
            if owners.is_empty() {
                return Ok(Vec::new());
            }
            n += 1;
            clauses.push(format!("owner_id = ANY(${n})"));
            args.add(owners.clone()).map_err(anyhow::Error::from_boxed)?;
        }
        // PG 的 ORDER BY 只认裸的输出列名，不能对别名再套表达式；按日拆时日界随 hour 单调，
        // 按组内最早的 hour 排就是按日期先后。
        let (key_expr, order) = match dim {
            BillingDim::Owner => ("owner_id::TEXT".to_string(), "cost DESC, k".to_string()),
            BillingDim::Cred => ("cred_id::TEXT".to_string(), "cost DESC, k".to_string()),
            BillingDim::Model => ("model".to_string(), "cost DESC, k".to_string()),
            BillingDim::Key => ("key_id::TEXT".to_string(), "cost DESC, k".to_string()),
            BillingDim::Group => ("group_id::TEXT".to_string(), "cost DESC, k".to_string()),
            BillingDim::Day => (
                format!("(((hour + {tz}) / 86400) * 86400 - {tz})::TEXT", tz = f.tz_offset_secs),
                "MIN(hour)".to_string(),
            ),
        };
        let sql = format!(
            "SELECT {key_expr} AS k, SUM(requests)::BIGINT, SUM(input_tokens)::BIGINT,
                    SUM(output_tokens)::BIGINT, SUM(cache_write_tokens)::BIGINT,
                    SUM(cache_read_tokens)::BIGINT, SUM(cost_usd) AS cost
               FROM billing_hourly WHERE {} GROUP BY k ORDER BY {order}",
            clauses.join(" AND ")
        );
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|r| {
                Ok(BillingRow {
                    key: r.try_get(0)?,
                    requests: r.try_get(1)?,
                    input_tokens: r.try_get(2)?,
                    output_tokens: r.try_get(3)?,
                    cache_write_tokens: r.try_get(4)?,
                    cache_read_tokens: r.try_get(5)?,
                    cost_usd: r.try_get(6)?,
                })
            })
            .collect()
    }

    /// 号的名称（含已删的号：账单里还挂着它们）。
    pub async fn credential_labels(&self) -> Result<HashMap<i64, String>> {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, label FROM credentials").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }

    /// 某个代理名下用户的 id。
    pub async fn child_user_ids(&self, parent: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar("SELECT id FROM users WHERE parent_id = $1 AND role = 'user'")
            .bind(parent)
            .fetch_all(&self.pool)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按日拆：日界按时区偏移切，行按日期先后（不是按费用）；SUM 出来的整数列解码得了；
    /// 号主取写入那一刻的、号主筛选生效。（归属与分组的完整口径见 B 移植的
    /// `billing_attribution_is_fixed_at_write_time` / `billing_days_follow_the_timezone`。）
    #[sqlx::test]
    async fn billing_day_rows_come_in_date_order(pool: sqlx::PgPool) {
        let store = PgStore::for_test(pool).await;
        let cred = store.insert("a", None, "t", "r", 0, None, None, 1).await.unwrap().id;
        let rec = |cost: f64| UsageRecord {
            cred_id: Some(cred),
            model: Some("m".into()),
            input_tokens: Some(10),
            cost_usd: Some(cost),
            ..Default::default()
        };
        let day0 = 1_800_000_000 - 1_800_000_000 % 86400;
        // 第一天便宜、第二天贵：按费用排会颠倒。
        store.insert_usage_log_at(&rec(1.0), Some(day0 + 3600)).await.unwrap();
        store.insert_usage_log_at(&rec(5.0), Some(day0 + 86400 + 3600)).await.unwrap();
        store.insert_usage_log_at(&rec(5.0), Some(day0 + 86400 + 7200)).await.unwrap();
        let f =
            BillingFilter { since: day0 - 86400, until: day0 + 3 * 86400, ..Default::default() };
        let days = store.billing_breakdown(&f, BillingDim::Day).await.unwrap();
        let keys: Vec<i64> = days.iter().map(|r| r.key.parse().unwrap()).collect();
        assert_eq!(keys, vec![day0, day0 + 86400]);
        assert_eq!((days[1].requests, days[1].input_tokens, days[1].cost_usd), (2, 20, 10.0));
        // 号主是写入时的 admin（id 1）。
        let owners = store.billing_breakdown(&f, BillingDim::Owner).await.unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].key, "1");
        let none = BillingFilter { owners: Some(vec![2]), ..f.clone() };
        assert!(store.billing_breakdown(&none, BillingDim::Cred).await.unwrap().is_empty());
        let empty = BillingFilter { owners: Some(vec![]), ..f.clone() };
        assert!(store.billing_breakdown(&empty, BillingDim::Cred).await.unwrap().is_empty());
        assert_eq!(store.credential_labels().await.unwrap()[&cred], "a");
        assert!(store.child_user_ids(1).await.unwrap().is_empty());
    }
}
