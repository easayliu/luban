//! 费用汇总（`billing_hourly`）：按小时 × 号主 × 号 × 接入 Key × 分组 × 模型累加请求数、
//! token 与等价 API 费用，长期保留，给分层账单读。
//!
//! 流水只留 8 天、`usage_rollup` 不记费用，都撑不起「看上个月花了多少」，故另起一张。
//! 每条流水在写入的**同一事务**里累加一行（[`billing_record`]），归属在那一刻就定死：
//!
//! - **号主**取写入时这个号的 owner——号以后转给别人，历史费用不跟着走；
//! - **分组**取这条请求「是通过哪个分组选中的」：Key 绑定了分组就取 Key 的分组顺序里第一个
//!   含这个号的；Key 没绑（或环境变量那把、没配 Key 时）取这个号所在分组里 id 最小的。
//!   于是每条请求只算进一个分组，按分组拆开的费用加起来正好是总数；
//! - **接入 Key** 为 0 表示环境变量那把，或一把都没配时放行的来访。
//!
//! 桶宽一小时：按日拆分时以调用方给的时区偏移切日界。整点偏移的时区（含东八区）分毫不差，
//! 半点 / 45 分偏移的时区日界最多偏半小时。

use super::*;

/// 汇总的时间桶宽（秒）。
const BILLING_BUCKET_SECS: i64 = 3600;

/// 回填过存量流水的标记（只回填一次）。
pub(super) const BILLING_BACKFILLED: &str = "billing_backfilled";

pub(super) fn migrate_billing(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS billing_hourly (
             hour               INTEGER NOT NULL,
             owner_id           INTEGER NOT NULL DEFAULT 0,
             cred_id            INTEGER NOT NULL,
             key_id             INTEGER NOT NULL DEFAULT 0,
             group_id           INTEGER NOT NULL DEFAULT 0,
             model              TEXT    NOT NULL DEFAULT '',
             requests           INTEGER NOT NULL DEFAULT 0,
             input_tokens       INTEGER NOT NULL DEFAULT 0,
             output_tokens      INTEGER NOT NULL DEFAULT 0,
             cache_write_tokens INTEGER NOT NULL DEFAULT 0,
             cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
             cost_usd           REAL    NOT NULL DEFAULT 0,
             PRIMARY KEY (hour, owner_id, cred_id, key_id, group_id, model)
         ) STRICT, WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS idx_billing_owner_hour ON billing_hourly(owner_id, hour);",
    )
    .context("failed to create the billing table")?;
    let done: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM settings WHERE key = ?1)",
        [BILLING_BACKFILLED],
        |r| r.get(0),
    )?;
    if done {
        return Ok(());
    }
    // 存量流水（最多 8 天）回填一次：当时没有 Key 与分组的记录，Key 记 0、分组取号现在所在的
    // id 最小的那个，号主取号现在的 owner。
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO billing_hourly
             (hour, owner_id, cred_id, key_id, group_id, model, requests, input_tokens,
              output_tokens, cache_write_tokens, cache_read_tokens, cost_usd)
         SELECT (u.ts / ?1) * ?1, COALESCE(c.owner_id, 0), u.cred_id, 0,
                COALESCE((SELECT MIN(group_id) FROM credential_groups g WHERE g.cred_id = u.cred_id), 0),
                COALESCE(u.model, ''), COUNT(*), COALESCE(SUM(u.input_tokens), 0),
                COALESCE(SUM(u.output_tokens), 0), COALESCE(SUM(u.cache_creation_tokens), 0),
                COALESCE(SUM(u.cache_read_tokens), 0), COALESCE(SUM(u.cost_usd), 0)
           FROM usage_logs u LEFT JOIN credentials c ON c.id = u.cred_id
          WHERE u.cred_id IS NOT NULL
          GROUP BY 1, 2, 3, 5, 6
         ON CONFLICT DO NOTHING",
        [BILLING_BUCKET_SECS],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, '1')",
        [BILLING_BACKFILLED],
    )?;
    tx.commit()?;
    Ok(())
}

/// 写流水的同一事务里累加费用汇总（只记落在某个号上的；本地拒绝没号可记）。
pub(super) fn billing_record(tx: &Connection, ts: i64, rec: &UsageRecord) -> Result<()> {
    let Some(cred_id) = rec.cred_id else { return Ok(()) };
    let owner: i64 = tx
        .query_row("SELECT owner_id FROM credentials WHERE id = ?1", [cred_id], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten()
        .unwrap_or(0);
    let key_id = rec.key_id.unwrap_or(0);
    let via_key: Option<i64> = match rec.key_id {
        Some(k) => tx
            .query_row(
                "SELECT kg.group_id FROM api_key_groups kg
                   JOIN credential_groups cg ON cg.group_id = kg.group_id AND cg.cred_id = ?2
                  WHERE kg.key_id = ?1 ORDER BY kg.ord LIMIT 1",
                params![k, cred_id],
                |r| r.get(0),
            )
            .optional()?,
        None => None,
    };
    let group_id: i64 = match via_key {
        Some(g) => g,
        None => tx.query_row(
            "SELECT COALESCE(MIN(group_id), 0) FROM credential_groups WHERE cred_id = ?1",
            [cred_id],
            |r| r.get(0),
        )?,
    };
    tx.execute(
        "INSERT INTO billing_hourly
             (hour, owner_id, cred_id, key_id, group_id, model, requests, input_tokens,
              output_tokens, cache_write_tokens, cache_read_tokens, cost_usd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (hour, owner_id, cred_id, key_id, group_id, model) DO UPDATE SET
             requests = requests + 1,
             input_tokens = input_tokens + excluded.input_tokens,
             output_tokens = output_tokens + excluded.output_tokens,
             cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens,
             cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
             cost_usd = cost_usd + excluded.cost_usd",
        params![
            (ts / BILLING_BUCKET_SECS) * BILLING_BUCKET_SECS,
            owner,
            cred_id,
            key_id,
            group_id,
            rec.model.clone().unwrap_or_default(),
            rec.input_tokens.unwrap_or(0),
            rec.output_tokens.unwrap_or(0),
            rec.cache_creation_tokens.unwrap_or(0),
            rec.cache_read_tokens.unwrap_or(0),
            rec.cost_usd.unwrap_or(0.0),
        ],
    )?;
    Ok(())
}

/// 账单按哪一维拆。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BillingDim {
    Owner,
    Cred,
    Model,
    Key,
    Group,
    Day,
}

/// 账单的筛选条件。时间是 `[since, until)`，按小时桶对齐。
#[derive(Debug, Clone, Default)]
pub struct BillingFilter {
    pub since: i64,
    pub until: i64,
    /// 只看这些号主（`None` = 不限）。
    pub owners: Option<Vec<i64>>,
    pub cred_id: Option<i64>,
    pub key_id: Option<i64>,
    pub group_id: Option<i64>,
    pub model: Option<String>,
    /// 按日拆分时的时区偏移（秒，东为正）。
    pub tz_offset_secs: i64,
}

/// 账单的一行（某一维的一个取值）。`key` 是这一维的取值：id、模型名，或按日拆时那天零点的
/// Unix 秒（按 `tz_offset_secs` 的本地零点）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BillingRow {
    pub key: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

impl CredentialStore {
    /// 按一维拆账单。费用从高到低；按日拆时按日期先后。
    pub fn billing_breakdown(&self, f: &BillingFilter, dim: BillingDim) -> Result<Vec<BillingRow>> {
        use rusqlite::types::Value;
        let mut clauses = vec!["hour >= ?1".to_string(), "hour < ?2".to_string()];
        let mut params: Vec<Value> = vec![Value::Integer(f.since), Value::Integer(f.until)];
        let mut push = |clause: &str, v: Value| {
            params.push(v);
            clauses.push(clause.replace('?', &format!("?{}", params.len())));
        };
        if let Some(c) = f.cred_id {
            push("cred_id = ?", Value::Integer(c));
        }
        if let Some(k) = f.key_id {
            push("key_id = ?", Value::Integer(k));
        }
        if let Some(g) = f.group_id {
            push("group_id = ?", Value::Integer(g));
        }
        if let Some(m) = &f.model {
            push("model = ?", Value::Text(m.clone()));
        }
        if let Some(owners) = &f.owners {
            if owners.is_empty() {
                return Ok(Vec::new());
            }
            let list: Vec<String> = owners.iter().map(i64::to_string).collect();
            clauses.push(format!("owner_id IN ({})", list.join(",")));
        }
        let (key_expr, order) = match dim {
            BillingDim::Owner => ("CAST(owner_id AS TEXT)".to_string(), "cost DESC, k"),
            BillingDim::Cred => ("CAST(cred_id AS TEXT)".to_string(), "cost DESC, k"),
            BillingDim::Model => ("model".to_string(), "cost DESC, k"),
            BillingDim::Key => ("CAST(key_id AS TEXT)".to_string(), "cost DESC, k"),
            BillingDim::Group => ("CAST(group_id AS TEXT)".to_string(), "cost DESC, k"),
            BillingDim::Day => (
                format!(
                    "CAST(((hour + {tz}) / 86400) * 86400 - {tz} AS TEXT)",
                    tz = f.tz_offset_secs
                ),
                "CAST(k AS INTEGER)",
            ),
        };
        let sql = format!(
            "SELECT {key_expr} AS k, SUM(requests), SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_write_tokens), SUM(cache_read_tokens), SUM(cost_usd) AS cost
               FROM billing_hourly WHERE {} GROUP BY k ORDER BY {order}",
            clauses.join(" AND ")
        );
        let conn = self.read_conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
            Ok(BillingRow {
                key: r.get(0)?,
                requests: r.get(1)?,
                input_tokens: r.get(2)?,
                output_tokens: r.get(3)?,
                cache_write_tokens: r.get(4)?,
                cache_read_tokens: r.get(5)?,
                cost_usd: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 号的名称（含已删的号：账单里还挂着它们）。
    pub fn credential_labels(&self) -> Result<HashMap<i64, String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, label FROM credentials")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 某个代理名下用户的 id。
    pub fn child_user_ids(&self, parent: i64) -> Result<Vec<i64>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT id FROM users WHERE parent_id = ?1 AND role = 'user'")?;
        let rows = stmt.query_map([parent], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
