//! 凭证的增删改：启停、暂停与恢复、优先级、名额上限、账号资料回填。
//!
//! 「订阅未生效」暂停原因在 SQL 里按正则认，见 [`SUBSCRIPTION_PAUSE_SQL`]。

/// 「订阅未生效」那档暂停写进 `ban_reason` 的原因里的固定片段（上游原话是组织不允许 OAuth）。
/// 完整原因见 [`crate::proxy::park_org_oauth_disallowed`]，形如
/// `[subscription-inactive 403] <片段> (…); paused until …`。
///
/// 认这一档一律按 luban 自己的格式认：**开头**是 [`SUBSCRIPTION_PAUSE_TAG`] 前缀。不用
/// `[403] ` 这种开头——封号原因在上游错误没带类型时写的就是 `[403] <上游原话>`，上游哪天的
/// 措辞恰好以这几个词开头，一个真封号就会被当成可以自动恢复的暂停；其余 luban 写的原因
/// （`[keepalive/…]`、`[refresh …]`、`[proxy]`）也都拼不出这个开头。三处同一口径、都区分大小写：
/// [`is_subscription_pause_reason`]、[`SUBSCRIPTION_PAUSE_SQL`]（库里）、前端 `isSubscriptionPause`
/// （`credential-shared.tsx`）。改文案须三处一起改。
pub const ORG_OAUTH_SUSPEND_MARKER: &str = "organization does not allow OAuth authentication";

/// 订阅未生效暂停原因的开头标签：`[` + 它 + ` <三位状态码>] `，见 [`ORG_OAUTH_SUSPEND_MARKER`]。
pub const SUBSCRIPTION_PAUSE_TAG: &str = "subscription-inactive";

/// `ban_reason` 是不是「订阅未生效」那档暂停写的，见 [`ORG_OAUTH_SUSPEND_MARKER`]。
pub fn is_subscription_pause_reason(reason: &str) -> bool {
    let Some(rest) = reason.strip_prefix('[').and_then(|r| r.strip_prefix(SUBSCRIPTION_PAUSE_TAG))
    else {
        return false;
    };
    let b = rest.as_bytes();
    b.len() > 6
        && b[0] == b' '
        && b[1..4].iter().all(u8::is_ascii_digit)
        && &b[4..6] == b"] "
        && rest[6..].starts_with(ORG_OAUTH_SUSPEND_MARKER)
}

use anyhow::{Context, Result};
use sqlx::PgConnection;

use super::session_events::{args1, delete_logged};
use super::users::owner_exists;
use super::{
    COLS, Credential, OwnerGone, PRIORITY_DEFAULT, PRIORITY_MAX, PRIORITY_MIN, Scope, seal,
    token_fingerprint,
};
use super::{CredentialStore, row_to_cred};

/// `is_subscription_pause_reason` 的 SQL 版（`ban_reason` 列上的正则，区分大小写）：开头是
/// `[subscription-inactive <三位状态码>] ` 加固定片段 `ORG_OAUTH_SUSPEND_MARKER`。三处同一
/// 口径（Rust、这里、前端 `isSubscriptionPause`），改文案须一起改。片段里只有字母与空格，
/// 不含正则元字符。
pub(super) const SUBSCRIPTION_PAUSE_SQL: &str = "ban_reason ~ \
     '^\\[subscription-inactive [0-9]{3}\\] organization does not allow OAuth authentication'";

/// 人工停用（单个 [`CredentialStore::set_disabled`] 与批量 [`CredentialStore::set_disabled_many`] 共用）：停用、
/// 清 `resume_at`，并清掉两种暂停留下的原因——限流暂停的「几点恢复」、订阅未生效的那句——
/// 号就是普通的「手动停用」。不清的话，限流那句会在 `resume_at` 清空后被当成封号原因显示，
/// 订阅那句会让连通性测试通过时把管理员关掉的号又打开。封号原因不动。
///
/// PG 的 `UPDATE` 里各表达式读的都是改之前的行，`CASE` 看到的 `resume_at` 是旧值。`$1` 是
/// 一组 id（`= ANY($1)`）。
fn manual_disable_sql() -> String {
    format!(
        "UPDATE credentials SET disabled = 1, resume_at = NULL, \
                ban_reason = CASE WHEN resume_at IS NOT NULL OR {SUBSCRIPTION_PAUSE_SQL} \
                                  THEN NULL ELSE ban_reason END, \
                updated_at = unixepoch() \
         WHERE id = ANY($1)"
    )
}

/// 停用 / 删号前清掉这些号上的设备绑定与会话绑定（会话绑定先记 `unbound` 事件，`reason` 是
/// 解绑的方式）。调用方须在写事务里调用。
async fn release_bindings(
    conn: &mut PgConnection,
    ids: &[i64],
    reason: &'static str,
) -> Result<()> {
    sqlx::query("DELETE FROM device_bindings WHERE cred_id = ANY($1)")
        .bind(ids)
        .execute(&mut *conn)
        .await?;
    delete_logged(&mut *conn, "unbound", Some(reason), "cred_id = ANY($1)", args1(ids)?).await?;
    Ok(())
}

impl CredentialStore {
    /// 插入一条新凭证，返回带 id 的完整记录。
    // 参数多是因为一条凭证本来就有这么多字段，且调用点只有「加号」那一处；
    // 打包成结构体只会多一个只用一次的类型。
    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
        &self,
        label: &str,
        tier: Option<&str>,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
        account_uuid: Option<&str>,
        org_type: Option<&str>,
        owner_id: i64,
    ) -> Result<Credential> {
        // 号主核对与插入在同一个串行化事务里：上号要先换码、拉 profile，等上几秒，期间号主
        // 被删的话不能再插进来一个挂在不存在的人名下的号。
        let mut tx = self.begin_write().await?;
        if !owner_exists(&mut tx, owner_id).await? {
            return Err(OwnerGone.into());
        }
        // 新凭证一律落在默认档 P2：同档内按设备数负载均衡，新账号立刻参与分摊。
        // 需要瀑布式（榨干一个再用下一个）时，手动/批量把账号调到不同优先级即可。
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO credentials
                 (label, tier, access_token, refresh_token, expires_at, account_uuid, org_type,
                  priority, owner_id, refresh_token_hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             RETURNING {COLS}"
        )))
        .bind(label)
        .bind(tier)
        .bind(seal(access_token))
        .bind(seal(refresh_token))
        .bind(expires_at as i64)
        .bind(account_uuid)
        .bind(org_type)
        .bind(PRIORITY_DEFAULT)
        .bind(owner_id)
        .bind(token_fingerprint(refresh_token))
        .fetch_one(&mut *tx)
        .await
        .context("failed to insert credential (the refresh_token may already exist)")?;
        let cred = row_to_cred(&row).context("failed to read the newly inserted credential")?;
        tx.commit().await?;
        Ok(cred)
    }

    /// 列出全部凭证，按 (priority, id) 升序。
    ///
    /// 先惰性恢复到点的限流暂停号（[`Self::resume_due`]），否则后台会一直显示成「已停用」，
    /// 直到下一条转发请求碰巧来触发恢复——控制台上看到的必须是此刻真实的调度状态。
    pub async fn list(&self) -> Result<Vec<Credential>> {
        self.list_scoped(Scope::All).await
    }

    /// 同 [`Self::list`]，只列 `scope` 看得到的号。
    pub async fn list_scoped(&self, scope: Scope) -> Result<Vec<Credential>> {
        self.list_where("$1::BIGINT IS NULL OR owner_id = $1", scope.owner()).await
    }

    /// 代理 `lead` 本人及下属用户名下的号，排序同 [`Self::list_scoped`]。只读看用，见
    /// `auth::Actor::team_lead`。
    pub async fn list_team(&self, lead: i64) -> Result<Vec<Credential>> {
        self.list_where(super::users::TEAM_OWNED, Some(lead)).await
    }

    async fn list_where(&self, cond: &str, bind: Option<i64>) -> Result<Vec<Credential>> {
        let mut conn = self.pool.acquire().await?;
        Self::resume_due(&mut conn).await?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM credentials WHERE {cond} ORDER BY priority ASC, id ASC"
        )))
        .bind(bind)
        .fetch_all(&mut *conn)
        .await?;
        rows.iter().map(row_to_cred).collect()
    }

    /// 按 id 读取单条。
    pub async fn get(&self, id: i64) -> Result<Option<Credential>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM credentials WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(row_to_cred).transpose()
    }

    /// 删除一条，返回是否确有删除。口径同 [`Self::remove`]（仅测试用）。
    #[cfg(test)]
    pub async fn delete(&self, id: i64) -> Result<bool> {
        Ok(self.remove(&[id]).await? > 0)
    }

    /// 清空所有凭证，返回删除条数。连带清空设备绑定与全部用量日志（口径同
    /// [`Self::delete`]：账号没了，历史用量不再保留）。
    pub async fn clear(&self) -> Result<usize> {
        // 串行化：不能和选号交错——选号刚读到的号在这里被删掉后，它照样会把绑定写回来。
        let mut tx = self.begin_write().await?;
        // 先锁全部账号行，理由同 [`Self::remove`]。
        sqlx::query("SELECT id FROM credentials FOR UPDATE").execute(&mut *tx).await?;
        for table in [
            "usage_logs",
            "usage_rollup",
            "device_bindings",
            "session_bindings",
            "session_binding_events",
            "credential_stats",
            "device_costs",
            "model_denials",
            "credential_groups",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
                .execute(&mut *tx)
                .await?;
        }
        let n = sqlx::query("DELETE FROM credentials").execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(n as usize)
    }

    /// 设置停用状态（管理员手动开关）。
    ///
    /// 停用时立即清空其设备绑定，让已绑定设备的下一次请求马上改选其它凭证，
    /// 而不必等绑定 TTL 惰性过期；重新启用时清除 `ban_reason`（若之前是被自动停用）。
    ///
    /// **两个方向都清 `resume_at`**：手动开 = 立刻回调度池，不该再留着一个到点又要动它的
    /// 时间戳；手动关 = 管理员的意思是「关着」，绝不能被限流那套惰性恢复自己打开。
    /// 于是「限流自动停用」这一状态只可能由 [`Self::pause_for_rate_limit`] 产生，
    /// 任何一次人工干预都会把它降级成普通的手动状态。
    pub async fn set_disabled(&self, id: i64, disabled: bool) -> Result<bool> {
        Ok(self.set_disabled_many(&[id], disabled).await? > 0)
    }

    /// 上游确认限流（账号级 429）时调用：把这个号停用并记下**到点自动恢复的时刻**，
    /// 同时清空其设备绑定，让绑在它上面的设备下一条请求立刻改选别的号。
    ///
    /// 与 `record_ban` 的唯一结构差别是多写一个 `resume_at`，而那正是「限流暂停」与
    /// 「封号/人工停用」的分界：`resume_at` 非空的号会被 [`Self::resume_due`] 到点自动启用、
    /// 也会被连通性测试成功时自动启用（见 [`Self::resume_if_rate_limited`]），另外两种则必须
    /// 人工介入。
    ///
    /// 为什么落库而不是只记内存（`RateLimitCooldown` 的做法）：额度耗尽动辄几小时到几天，
    /// 远长于一次进程重启；记内存则重启即忘。落库之后重启也记得，代价是必须自己保证「到点
    /// 恢复」不依赖进程一直活着——所以恢复做成惰性的（选号时顺手扫一遍）。
    ///
    /// `reason` 直接写进 `ban_reason`，后台卡片原样展示，故调用方应带上人话的恢复时刻。
    ///
    /// 只动**启用中或已在限流暂停中**的号（见 [`Self::park_row`]）：先回来的那条把号封了 /
    /// 按订阅停了，后回来的 429 不能把它改写成「到点自己回来」。返回是否确有写入。
    pub async fn pause_for_rate_limit(
        &self,
        id: i64,
        reason: &str,
        resume_at: u64,
    ) -> Result<bool> {
        self.park_row(id, reason, Some(resume_at as i64)).await
    }

    /// 两种自动暂停（[`Self::pause_for_rate_limit`]、[`Self::suspend_for_inactive_subscription`]）
    /// 共用的落库：停用、写原因与恢复时刻（`None` = 不会到点自己回来）、清绑定。
    ///
    /// 守卫只在这一处：只动**启用中或已在限流暂停中**的号，封号、人工停用、订阅未生效暂停
    /// 一概不碰——同一个号常有几条请求同时在飞，先回来的那条已经把号处置了，后回来的不能
    /// 改写它。返回是否确有写入。
    async fn park_row(&self, id: i64, reason: &str, resume_at: Option<i64>) -> Result<bool> {
        // 串行化写事务：与选号互斥，选号不会在这里清完绑定之后又把设备绑回这个号上。
        let mut tx = self.begin_write().await?;
        let updated = sqlx::query(
            "UPDATE credentials SET disabled = 1, ban_reason = $2, resume_at = $3, \
                    updated_at = unixepoch() \
             WHERE id = $1 AND (disabled = 0 OR resume_at IS NOT NULL)",
        )
        .bind(id)
        .bind(reason)
        .bind(resume_at)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if updated {
            release_bindings(&mut tx, &[id], "account_paused").await?;
        }
        tx.commit().await?;
        Ok(updated)
    }

    /// 订阅未生效——付费档到期未续费、Free 档没订阅（见 `crate::proxy::park_org_oauth_disallowed`）：
    /// 停调度、清绑定，**不写 `resume_at`**——不会到点自己回来，只有两条路放回池子：控制台手动
    /// 启用（[`Self::set_disabled`]），或连通性测试通过（[`Self::resume_if_subscription_suspended`]）。
    ///
    /// 只动**启用中或限时暂停中**的号：人工停用、封禁、已经这样暂停的不碰。额度暂停中的号会被
    /// 改成这一档：额度回来了，没订阅照样不放行。
    ///
    /// 返回是否确有写入；`false` 即号已在池外（或不存在），调用方不必再记一遍。
    pub async fn suspend_for_inactive_subscription(&self, id: i64, reason: &str) -> Result<bool> {
        self.park_row(id, reason, None).await
    }

    /// 连通性测试通过时调用：若该号是 [`Self::suspend_for_inactive_subscription`] 停下的，当场恢复
    /// 调度。认的是 luban 自己写的原因开头（`is_subscription_pause_reason` 同一口径），封号、
    /// 人工停用不受影响。返回是否确有恢复。
    pub async fn resume_if_subscription_suspended(&self, id: i64) -> Result<bool> {
        self.update_one(
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                    updated_at = unixepoch() \
                 WHERE id = $1 AND disabled = 1 AND resume_at IS NULL AND {SUBSCRIPTION_PAUSE_SQL}"
            )))
            .bind(id),
        )
        .await
    }

    /// 把所有「限流暂停且已到恢复时刻」的号重新启用，返回实际恢复的条数。
    ///
    /// 惰性执行（选号与列表各调一次，见 [`Self::select_for_device`]/[`Self::list`]），
    /// 和设备绑定的 TTL 过期同一套路子：不挂后台定时器。条件里的 `resume_at IS NOT NULL` 是
    /// 关键，它保证只碰限流暂停的号，封号与人工停用的不会被顺手打开。
    pub(super) async fn resume_due(conn: &mut PgConnection) -> Result<usize> {
        Ok(sqlx::query(
            "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                    updated_at = unixepoch() \
             WHERE disabled = 1 AND resume_at IS NOT NULL AND resume_at <= unixepoch()",
        )
        .execute(conn)
        .await?
        .rows_affected() as usize)
    }

    /// 连通性测试通过时调用：若该号是被限流自动停用的（`resume_at` 非空），当场恢复调度。
    ///
    /// 测试成功是「上游此刻确实放这个号过」的一手证据，比我们从限流头算出来的恢复时刻更硬。
    /// 只认 `resume_at` 非空的号：人工关掉的号不该被一次连通性测试打开。返回是否确有恢复。
    pub async fn resume_if_rate_limited(&self, id: i64) -> Result<bool> {
        self.update_one(
            sqlx::query(
                "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                        updated_at = unixepoch() \
                 WHERE id = $1 AND resume_at IS NOT NULL",
            )
            .bind(id),
        )
        .await
    }

    /// 测试用：[`Self::record_ban`] 的简写，一句原因造出「已封禁」状态（来源记为 `manual`）。
    #[cfg(test)]
    pub async fn mark_banned(&self, id: i64, reason: &str) -> Result<bool> {
        self.record_ban(
            id,
            &super::BanContext {
                reason: reason.to_string(),
                source: "manual",
                ..Default::default()
            },
        )
        .await
    }

    /// 设置优先级（范围由调用方校验，这里不截断），但不许比 `floor` 更高——原本就比 `floor`
    /// 高的号只许往下调（不比现值更高）。判断与写入在同一条语句里。返回是否写了：号不存在或
    /// 越界都是 false，由调用方区分。admin 给 [`PRIORITY_MIN`]，等于不设限。
    pub async fn set_priority(&self, id: i64, priority: i64, floor: i64) -> Result<bool> {
        self.update_one(
            sqlx::query(
                "UPDATE credentials SET priority = $2, updated_at = unixepoch() \
                  WHERE id = $1 AND ($2 >= $3 OR $2 >= priority)",
            )
            .bind(id)
            .bind(priority)
            .bind(floor),
        )
        .await
    }

    /// 批量设置优先级：把 `ids` 里的账号统一改到 `priority`，返回实际更新的条数。
    /// 单条语句，天然原子，不会留下一半新一半旧的调度档位。`ids` 为空时直接返回 0。
    pub async fn set_priorities(&self, ids: &[i64], priority: i64) -> Result<usize> {
        self.update_many("priority", ids, priority).await
    }

    /// 批量平移优先级：`ids` 里的账号各自在原值上加 `delta`（负数 = 提高），超出
    /// `floor`..=[`PRIORITY_MAX`] 的截到边界。选中账号之间的先后顺序不变
    /// （碰到边界的除外）。单条语句，返回实际更新的条数。`ids` 里重复的只平移一次。
    ///
    /// `floor` 是往上调能到的最高档：admin 给 [`PRIORITY_MIN`]，代理和用户给 P2。原本就在
    /// `floor` 之上的号往上调时原地不动、不会被压回 `floor`。
    pub async fn shift_priorities(&self, ids: &[i64], delta: i64, floor: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        // `= ANY` 对重复的 id 只命中一行，天然只平移一次。
        Ok(sqlx::query(
            "UPDATE credentials SET priority = LEAST(GREATEST(priority + $2, LEAST(priority, $3)), $4), \
                 updated_at = unixepoch() WHERE id = ANY($1)",
        )
        .bind(ids)
        .bind(delta)
        .bind(floor.max(PRIORITY_MIN))
        .bind(PRIORITY_MAX)
        .execute(&self.pool)
        .await?
        .rows_affected() as usize)
    }

    /// 批量删除：口径同 [`Self::remove`]，返回实际删除的条数（仅测试用）。
    #[cfg(test)]
    pub async fn delete_many(&self, ids: &[i64]) -> Result<usize> {
        self.remove(ids).await
    }

    /// 删号：账号行与挂在它上面的小表（绑定、账本、设备费用、模型拒绝、分组成员）在同一个短
    /// 事务里删掉，返回实际删除的账号数。
    ///
    /// **用量流水不删**，留给 `prune_usage_logs` 按保留期自然裁掉：账号 id 不复用、每行自带
    /// `cred_label`，留着这些行没有害处；全局口径的统计因此会带上已删账号保留期内的用量——
    /// 那些请求确实发生过、钱也确实花了，算进去才对得上账。
    pub async fn remove(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        // 串行化：与选号互斥，选号不会把设备绑回一个刚删掉的号。
        let mut tx = self.begin_write().await?;
        // 先锁账号行，再动挂在它上面的小表：在途的流水写入是「账号行 FOR KEY SHARE → 账本 upsert」
        // 这个顺序（见 `insert_usage_log_at`），这里反过来先删账本行就会和它互相等、被判死锁。
        sqlx::query("SELECT id FROM credentials WHERE id = ANY($1) FOR UPDATE")
            .bind(ids)
            .execute(&mut *tx)
            .await?;
        release_bindings(&mut tx, ids, "account_removed").await?;
        for table in ["credential_stats", "device_costs", "model_denials", "credential_groups"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE cred_id = ANY($1)"
            )))
            .bind(ids)
            .execute(&mut *tx)
            .await?;
        }
        let n = sqlx::query("DELETE FROM credentials WHERE id = ANY($1)")
            .bind(ids)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        // 号没了，它们在内存里的限流窗口与冷却也留着没用（id 不会被复用）。
        for id in ids {
            self.bare_rate.forget(id);
            self.rpm_rate.forget(id);
            self.cooldown.forget(*id);
        }
        Ok(n as usize)
    }

    /// 批量启停：语义与 [`Self::set_disabled`] 一致（停用时清设备绑定使其立即改选其它
    /// 凭证；启用时清 `ban_reason`），返回实际更新的条数。单事务内完成。
    pub async fn set_disabled_many(&self, ids: &[i64], disabled: bool) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        if disabled {
            // 串行化：与选号互斥，理由同 [`Self::park_row`]。
            let mut tx = self.begin_write().await?;
            release_bindings(&mut tx, ids, "account_disabled").await?;
            // 人工操作两个方向都清 `resume_at`，限流那套惰性恢复不该越过管理员的决定。
            // 两种暂停留下的原因也一并清掉，理由见 [`manual_disable_sql`]。
            let n = sqlx::query(sqlx::AssertSqlSafe(manual_disable_sql()))
                .bind(ids)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            tx.commit().await?;
            Ok(n as usize)
        } else {
            Ok(sqlx::query(
                "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                 updated_at = unixepoch() WHERE id = ANY($1)",
            )
            .bind(ids)
            .execute(&self.pool)
            .await?
            .rows_affected() as usize)
        }
    }

    /// 把 `ids` 里各号的整数列 `column`（只由代码里的常量传入）统一写成 `value`，返回更新条数。
    async fn update_many(&self, column: &'static str, ids: &[i64], value: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE credentials SET {column} = $2, updated_at = unixepoch() WHERE id = ANY($1)"
        )))
        .bind(ids)
        .bind(value)
        .execute(&self.pool)
        .await?
        .rows_affected() as usize)
    }

    /// 把单个号的整数列 `column`（只由代码里的常量传入）写成 `value`，返回是否确有更新。
    async fn update_column(&self, column: &'static str, id: i64, value: i64) -> Result<bool> {
        Ok(self.update_many(column, &[id], value).await? > 0)
    }

    /// 把单个号的文本列 `column`（只由代码里的常量传入）写成 `value`，返回是否确有更新。
    async fn update_text(
        &self,
        column: &'static str,
        id: i64,
        value: Option<&str>,
    ) -> Result<bool> {
        self.update_one(
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE credentials SET {column} = $2, updated_at = unixepoch() WHERE id = $1"
            )))
            .bind(id)
            .bind(value),
        )
        .await
    }

    /// 批量设置设备数上限（三态语义同 [`Self::set_device_limit`]），返回实际更新的条数。
    pub async fn set_device_limits(&self, ids: &[i64], limit: i64) -> Result<usize> {
        self.update_many("device_limit", ids, limit).await
    }

    /// 设置该账号的设备数上限。三态：`> 0` 本账号独立上限；`0` 跟随全局默认
    /// （见 `DEFAULT_DEVICE_LIMIT`）；`< 0` 本账号明确不限（不受全局默认约束）。
    pub async fn set_device_limit(&self, id: i64, limit: i64) -> Result<bool> {
        self.update_column("device_limit", id, limit).await
    }

    /// 批量设置模拟会话数上限（三态语义同 [`Self::set_session_limit`]），返回实际更新的条数。
    pub async fn set_session_limits(&self, ids: &[i64], limit: i64) -> Result<usize> {
        self.update_many("session_limit", ids, limit).await
    }

    /// 设置模拟会话数上限，返回是否确有更新。三态同 [`Self::set_device_limit`]：`> 0` 本账号
    /// 独立上限；`0` 跟随全局默认（`DEFAULT_SESSION_LIMIT`）；`< 0` 本账号明确不限。
    pub async fn set_session_limit(&self, id: i64, limit: i64) -> Result<bool> {
        self.update_column("session_limit", id, limit).await
    }

    /// 批量设置账号自己的提前停调度阈值（两档整份覆盖）；三态同 [`Self::set_quota_pause_pcts`]。
    pub async fn set_quota_pause_pcts_many(
        &self,
        ids: &[i64],
        short_pct: Option<i64>,
        long_pct: Option<i64>,
    ) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        Ok(sqlx::query(
            "UPDATE credentials SET quota_pause_pct = $2, quota_pause_pct_7d = $3, \
             updated_at = unixepoch() WHERE id = ANY($1)",
        )
        .bind(ids)
        .bind(short_pct.map(|p| p.clamp(0, 100)))
        .bind(long_pct.map(|p| p.clamp(0, 100)))
        .execute(&self.pool)
        .await?
        .rows_affected() as usize)
    }

    /// 批量设置账号 RPM 上限；三态同 [`Self::set_rpm_limit`]。
    pub async fn set_rpm_limits(&self, ids: &[i64], limit: i64) -> Result<usize> {
        self.update_many("rpm_limit", ids, limit).await
    }

    /// 设置该账号每分钟最多转发多少条请求。三态同设备上限：`> 0` 本账号独立上限；
    /// `0` 跟随全局默认（见 `DEFAULT_RPM_LIMIT`）；`< 0` 本账号明确不限。
    ///
    /// 计数在进程内存里（见 `RateWindow`），改完即时生效，不影响已经记在窗口里的那些。
    pub async fn set_rpm_limit(&self, id: i64, limit: i64) -> Result<bool> {
        self.update_column("rpm_limit", id, limit).await
    }

    /// 设置该账号自己的「额度用到多少就提前停调度」阈值（5h / 7d 两档，百分比）。
    /// 每档 `None` = 跟随全局、`Some(0)` = 本账号这一档不停、`Some(1..=100)` = 独立阈值；
    /// 取值夹到 `0..=100`。生效值见 `effective_quota_pause_pct`。两档整份覆盖；接口里用的是
    /// 只改给了的那几档的 [`Self::set_quota_pause_pcts_partial`]，这个只剩测试用。
    #[cfg(test)]
    pub async fn set_quota_pause_pcts(
        &self,
        id: i64,
        short_pct: Option<i64>,
        long_pct: Option<i64>,
    ) -> Result<bool> {
        Ok(self.set_quota_pause_pcts_many(&[id], short_pct, long_pct).await? > 0)
    }

    /// 只改两档提前停调度阈值里给了的那几档（外层 `None` 的那档原样不动），单条语句。
    /// 号主只改一档时用它：另一档不写，就不会把这期间 admin 刚改的值盖回去。
    pub async fn set_quota_pause_pcts_partial(
        &self,
        id: i64,
        short_pct: Option<Option<i64>>,
        long_pct: Option<Option<i64>>,
    ) -> Result<bool> {
        self.update_one(
            sqlx::query(
                "UPDATE credentials SET \
                   quota_pause_pct = CASE WHEN $2 THEN $3 ELSE quota_pause_pct END, \
                   quota_pause_pct_7d = CASE WHEN $4 THEN $5 ELSE quota_pause_pct_7d END, \
                   updated_at = unixepoch() WHERE id = $1",
            )
            .bind(id)
            .bind(short_pct.is_some())
            .bind(short_pct.flatten().map(|p| p.clamp(0, 100)))
            .bind(long_pct.is_some())
            .bind(long_pct.flatten().map(|p| p.clamp(0, 100))),
        )
        .await
    }

    /// 写回组织类型（`claude_team` 等）。与 [`Self::set_tier`] 分开：等级会随额度档变，
    /// 组织类型只在换账号时才变，两者的来源虽同是 profile，语义不是一回事。
    pub async fn set_org_type(&self, id: i64, org_type: Option<&str>) -> Result<bool> {
        self.update_text("org_type", id, org_type).await
    }

    /// 写回额度档原值（`default_claude_max_5x` 之类）。
    /// 见 [`crate::credentials::Credential::rate_limit_tier`]。
    pub async fn set_rate_limit_tier(&self, id: i64, raw: Option<&str>) -> Result<bool> {
        self.update_text("rate_limit_tier", id, raw).await
    }

    /// 把一份刚拉到的 profile 写回凭证：等级、账号 UUID、组织类型、额度档原值、组织 UUID、
    /// 订阅创建时刻。**每一项只在 profile 给了值时才写**——profile 缺项不能把库里已有的清掉。
    /// `fallback_org_uuid` 是 profile 没给组织 id 时的兜底（交换响应里那个），官方同一次序。
    ///
    /// 三条路共用：登录、手动刷新、**自动刷新**（`ensure_fresh_token`）。
    pub async fn apply_profile(
        &self,
        id: i64,
        profile: &crate::oauth::Profile,
        fallback_org_uuid: Option<&str>,
    ) -> Result<()> {
        if profile.tier.is_some() {
            self.set_tier(id, profile.tier.as_deref()).await?;
        }
        if let Some(uuid) = profile.account_uuid.as_deref() {
            self.set_account_uuid(id, uuid).await?;
        }
        if profile.org_type.is_some() {
            self.set_org_type(id, profile.org_type.as_deref()).await?;
        }
        if profile.rate_limit_tier.is_some() {
            self.set_rate_limit_tier(id, profile.rate_limit_tier.as_deref()).await?;
        }
        let org_uuid = profile.org_uuid.as_deref().or(fallback_org_uuid);
        if org_uuid.is_some() {
            self.set_org_uuid(id, org_uuid).await?;
        }
        if profile.subscription_created_at.is_some() {
            self.set_subscription_created_at(id, profile.subscription_created_at.as_deref())
                .await?;
        }
        // 只给后台看的几列：拉到就整组覆盖（席位档个人号本来就没有，缺了要写回空）。
        // 至少拿到组织名称才算这一组有效，免得一份残缺的响应把已有的值清掉。
        if profile.org_name.is_some() {
            sqlx::query(
                "UPDATE credentials SET org_name = $2, seat_tier = $3, subscription_status = $4,
                        extra_usage_enabled = $5, updated_at = unixepoch()
                  WHERE id = $1",
            )
            .bind(id)
            .bind(&profile.org_name)
            .bind(&profile.seat_tier)
            .bind(&profile.subscription_status)
            .bind(profile.extra_usage_enabled.map(i64::from))
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// 写回组织 UUID。见 [`crate::credentials::Credential::org_uuid`]。
    pub async fn set_org_uuid(&self, id: i64, org_uuid: Option<&str>) -> Result<bool> {
        self.update_text("org_uuid", id, org_uuid).await
    }

    /// 写回订阅创建时刻原串。见 [`crate::credentials::Credential::subscription_created_at`]。
    pub async fn set_subscription_created_at(&self, id: i64, raw: Option<&str>) -> Result<bool> {
        self.update_text("subscription_created_at", id, raw).await
    }

    /// 写回账号等级。**等级变了就把它的模型准入记录全清掉**：那些记录是在旧套餐下学到的
    /// （Pro 号不含 fable），升级到 Max 后再留着就等于把新买的额度锁在门外。
    pub async fn set_tier(&self, id: i64, tier: Option<&str>) -> Result<bool> {
        // 先读旧值、再按它决定清不清——读后写，走串行化事务。
        let mut tx = self.begin_write().await?;
        let previous: Option<Option<String>> =
            sqlx::query_scalar("SELECT tier FROM credentials WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(previous) = previous else { return Ok(false) };
        if previous.as_deref() != tier {
            sqlx::query("DELETE FROM model_denials WHERE cred_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        let n =
            sqlx::query("UPDATE credentials SET tier = $2, updated_at = unixepoch() WHERE id = $1")
                .bind(id)
                .bind(tier)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }

    /// 回填账号 UUID（旧库凭证登录时未存、刷新 token 时补上）。仅在非空时覆盖。
    pub async fn set_account_uuid(&self, id: i64, account_uuid: &str) -> Result<bool> {
        self.update_text("account_uuid", id, Some(account_uuid)).await
    }

    /// 设置/清除该凭证的专用出站代理。`None` 或空串写成 NULL（直连）。
    ///
    /// 入参必须是 [`crate::clients::validate_proxy`] 校验过的串——这里只负责存，
    /// 校验放在入库之前那一层。
    pub async fn set_proxy(&self, id: i64, proxy: Option<&str>) -> Result<bool> {
        let proxy = proxy.map(str::trim).filter(|s| !s.is_empty());
        self.update_text("proxy", id, proxy).await
    }

    /// 重命名（设置显示名）。
    pub async fn set_label(&self, id: i64, label: &str) -> Result<bool> {
        self.update_text("label", id, Some(label)).await
    }

    /// 刷新后回写新的 token 三元组（单行 UPDATE，加密落库、同步更新 refresh_token 指纹）。
    pub async fn update_tokens(
        &self,
        id: i64,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
    ) -> Result<bool> {
        write_tokens(&self.pool, id, access_token, refresh_token, expires_at).await
    }
}

/// [`CredentialStore::update_tokens`] 的实现，只要一个连接池：刷新落库在脱离请求的后台任务里
/// 做，见 [`super::refresh`]。
pub(super) async fn write_tokens(
    pool: &sqlx::PgPool,
    id: i64,
    access_token: &str,
    refresh_token: &str,
    expires_at: u64,
) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE credentials
            SET access_token = $2, refresh_token = $3, expires_at = $4,
                refresh_token_hash = $5, updated_at = unixepoch()
          WHERE id = $1",
    )
    .bind(id)
    .bind(seal(access_token))
    .bind(seal(refresh_token))
    .bind(expires_at as i64)
    .bind(token_fingerprint(refresh_token))
    .execute(pool)
    .await?;
    Ok(n.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use sqlx::PgPool;

    use super::super::CredentialStore;
    use super::super::select::tests::{scalar, store_with};
    use super::super::*;

    /// 执行一条带一个整数参数（`$1`）的写语句。
    async fn exec(store: &CredentialStore, sql: &'static str, arg: i64) {
        sqlx::query(sql).bind(arg).execute(&store.pool).await.unwrap();
    }

    /// `usage_logs` 里每行的 cred_id，按插入顺序。
    async fn usage_log_creds(store: &CredentialStore) -> Vec<i64> {
        sqlx::query_scalar("SELECT cred_id FROM usage_logs ORDER BY id")
            .fetch_all(&store.pool)
            .await
            .unwrap()
    }

    /// 走真实写入口落一条带限流头的流水（ts / 费用 / 两个 reset 由调用方指定）。
    /// 刻意不裸 INSERT：快照与费用如今是写时落账（credential_stats），绕过写入口
    /// 的行只进流水不进账本，测出来的就不是线上那条路径了。
    ///
    /// 每条顺带记 10 个 token（输入/输出/缓存写/缓存读 各 1 + 3 + 2 + 4），于是窗口 token 数
    /// 恒为「窗口内条数 × 10」，费用与请求数怎么断，token 就该怎么断。
    async fn log_row(
        store: &CredentialStore,
        cred_id: i64,
        ts: i64,
        cost: f64,
        r5: Option<i64>,
        r7: Option<i64>,
    ) {
        let rec = UsageRecord {
            cred_id: Some(cred_id),
            cost_usd: Some(cost),
            has_usage: true,
            input_tokens: Some(1),
            output_tokens: Some(3),
            cache_creation_tokens: Some(2),
            cache_read_tokens: Some(4),
            rl_5h_utilization: r5.map(|_| 0.5),
            rl_5h_reset: r5,
            rl_7d_utilization: r7.map(|_| 0.25),
            rl_7d_reset: r7,
            ..Default::default()
        };
        store.insert_usage_log_at(&rec, Some(ts)).await.unwrap();
    }

    fn win(name: &str, util: f64, reset: i64, status: &str) -> QuotaWindow {
        QuotaWindow {
            name: name.into(),
            status: Some(status.into()),
            utilization: Some(util),
            reset: Some(reset),
        }
    }

    /// token 落库只存密文、唯一约束挂在指纹上，读出来照旧是明文；刷新回写同样加密、同步指纹。
    /// （rusqlite 版 `plaintext_tokens_are_encrypted_at_startup` 里不涉及启动迁移的那一半。）
    #[sqlx::test]
    async fn tokens_are_sealed_and_fingerprinted(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let id = ids[0];
        let (at, rt, hash): (String, String, String) = sqlx::query_as(
            "SELECT access_token, refresh_token, refresh_token_hash FROM credentials WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert!(at.starts_with("enc1:") && rt.starts_with("enc1:"), "库里只剩密文");
        assert_eq!(hash, token_fingerprint("refresh-a"));
        let cred = store.get(id).await.unwrap().unwrap();
        assert_eq!(
            (cred.access_token.as_str(), cred.refresh_token.as_str()),
            ("tok-a", "refresh-a")
        );
        // 同一个 refresh_token 再插一次撞指纹的唯一约束。
        assert!(store.insert("dup", None, "at-2", "refresh-a", 0, None, None, 1).await.is_err());
        // 刷新回写同样加密、同步指纹。
        assert!(store.update_tokens(id, "at-3", "rt-3", 0).await.unwrap());
        assert_eq!(store.get(id).await.unwrap().unwrap().refresh_token, "rt-3");
        let hash: String =
            sqlx::query_scalar("SELECT refresh_token_hash FROM credentials WHERE id = $1")
                .bind(id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(hash, token_fingerprint("rt-3"));
    }

    /// 号主不存在（或是访客）时不插：上号要等几秒，期间号主可能被删。
    #[sqlx::test]
    async fn insert_refuses_a_missing_owner(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let err = store.insert("a", None, "t", "r", 0, None, None, 9999).await.unwrap_err();
        assert!(err.downcast_ref::<OwnerGone>().is_some(), "{err}");
        assert!(store.list().await.unwrap().is_empty());
    }

    /// `apply_profile` 是登录 / 手动刷新 / 自动刷新三条路共用的写回：只写 profile 给了的项，
    /// 缺项不清库里已有的值；组织 id 退回交换响应里的兜底。`profile_incomplete` 决定自动刷新
    /// 要不要顺手拉一次 profile——四列齐了就不再拉。
    #[sqlx::test]
    async fn apply_profile_backfills_only_what_the_profile_gives(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let id = ids[0];
        assert!(
            store.get(id).await.unwrap().unwrap().profile_incomplete(),
            "刚建的号 profile 列全空"
        );

        let partial = crate::oauth::Profile {
            tier: Some("Max 5x".into()),
            account_uuid: Some("acct".into()),
            rate_limit_tier: Some("default_claude_max_5x".into()),
            ..Default::default()
        };
        store.apply_profile(id, &partial, Some("org-from-token")).await.unwrap();
        let c = store.get(id).await.unwrap().unwrap();
        assert_eq!(c.tier.as_deref(), Some("Max 5x"));
        assert_eq!(c.account_uuid.as_deref(), Some("acct"));
        assert_eq!(c.rate_limit_tier.as_deref(), Some("default_claude_max_5x"));
        assert_eq!(
            c.org_uuid.as_deref(),
            Some("org-from-token"),
            "profile 没给组织 id 就用交换响应的"
        );
        assert!(c.subscription_created_at.is_none());
        assert!(c.profile_incomplete(), "订阅创建时刻还缺着，下次刷新还要拉");

        let full = crate::oauth::Profile {
            org_uuid: Some("org-from-profile".into()),
            subscription_created_at: Some("2026-04-15T13:03:55.239Z".into()),
            org_name: Some("Acme".into()),
            seat_tier: Some("team_standard".into()),
            subscription_status: Some("active".into()),
            extra_usage_enabled: Some(true),
            ..Default::default()
        };
        store.apply_profile(id, &full, Some("org-from-token")).await.unwrap();
        let c = store.get(id).await.unwrap().unwrap();
        assert_eq!(
            c.org_uuid.as_deref(),
            Some("org-from-profile"),
            "profile 给了就以 profile 为准"
        );
        assert_eq!(c.subscription_created_at.as_deref(), Some("2026-04-15T13:03:55.239Z"));
        assert_eq!(c.tier.as_deref(), Some("Max 5x"), "这次 profile 没给的项不能被清掉");
        assert_eq!(c.account_uuid.as_deref(), Some("acct"));
        assert_eq!(c.org_name.as_deref(), Some("Acme"));
        assert_eq!(c.seat_tier.as_deref(), Some("team_standard"));
        assert_eq!(c.subscription_status.as_deref(), Some("active"));
        assert_eq!(c.extra_usage_enabled, Some(true));
        assert!(!c.profile_incomplete(), "五列齐了就不再拉");

        // 换成个人号（没有席位档）：组织那一组整组覆盖，席位档清空；没给组织名称的残缺响应不动它们。
        let personal = crate::oauth::Profile {
            org_name: Some("someone's Organization".into()),
            subscription_status: Some("active".into()),
            ..Default::default()
        };
        store.apply_profile(id, &personal, None).await.unwrap();
        let c = store.get(id).await.unwrap().unwrap();
        assert!(c.seat_tier.is_none() && c.extra_usage_enabled.is_none());
        store.apply_profile(id, &crate::oauth::Profile::default(), None).await.unwrap();
        let c = store.get(id).await.unwrap().unwrap();
        assert_eq!(c.org_name.as_deref(), Some("someone's Organization"));
    }

    /// 删号清掉设备绑定，但用量流水留着（随保留期自然裁掉），其它账号不受影响。
    #[sqlx::test]
    async fn delete_keeps_usage_logs_but_drops_bindings(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).await.unwrap();
        for cid in [a.id, b.id] {
            exec(&store, "INSERT INTO usage_logs (cred_id) VALUES ($1)", cid).await;
        }
        exec(&store, "INSERT INTO device_bindings (device_id, cred_id) VALUES ('d1', $1)", a.id)
            .await;

        assert!(store.delete(a.id).await.unwrap());

        assert_eq!(usage_log_creds(&store).await, vec![a.id, b.id], "删号不删流水");
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM device_bindings").await, 0);
    }

    /// 新增账号一律落在 P2；批量改优先级把选中的账号统一调档、其余不动。
    #[sqlx::test]
    async fn insert_defaults_to_p2_and_batch_priority(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).await.unwrap();
        let c = store.insert("c", None, "tc", "rc", 0, None, None, 1).await.unwrap();
        assert_eq!((a.priority, b.priority, c.priority), (2, 2, 2), "新账号都应是 P2");

        assert_eq!(store.set_priorities(&[a.id, c.id], 0).await.unwrap(), 2);
        let by_id: HashMap<i64, i64> =
            store.list().await.unwrap().into_iter().map(|x| (x.id, x.priority)).collect();
        assert_eq!(by_id[&a.id], 0);
        assert_eq!(by_id[&c.id], 0);
        assert_eq!(by_id[&b.id], 2, "未选中的账号不应被改动");
        assert_eq!(store.set_priorities(&[], 4).await.unwrap(), 0, "空列表为 no-op");
    }

    /// 批量升降档：各自加减、保留相对顺序，越界截到 P0..=P4，未选中的不动。
    #[sqlx::test]
    async fn shift_priorities_keeps_order_and_clamps(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b", "c"]).await;
        let (a, b, c) = (ids[0], ids[1], ids[2]);
        store.set_priority(a, 0, PRIORITY_MIN).await.unwrap();
        store.set_priority(b, 1, PRIORITY_MIN).await.unwrap();
        store.set_priority(c, 3, PRIORITY_MIN).await.unwrap();
        let prio = async |id| store.get(id).await.unwrap().unwrap().priority;

        assert_eq!(store.shift_priorities(&[a, b], 1, PRIORITY_MIN).await.unwrap(), 2);
        assert_eq!(
            (prio(a).await, prio(b).await, prio(c).await),
            (1, 2, 3),
            "只动选中的，各自降一档"
        );
        store.shift_priorities(&[a, b, c], -2, PRIORITY_MIN).await.unwrap();
        assert_eq!((prio(a).await, prio(b).await, prio(c).await), (0, 0, 1), "提高到顶截在 P0");
        store.shift_priorities(&[c], 4, PRIORITY_MIN).await.unwrap();
        assert_eq!(prio(c).await, 4, "降低到底截在 P4");
        assert_eq!(store.shift_priorities(&[b, b], 1, PRIORITY_MIN).await.unwrap(), 1);
        assert_eq!(prio(b).await, 1, "重复的 id 只升降一次");

        // 带 floor（代理和用户）：往上最多到 floor，原本就在 floor 之上的不动、也不被压下去。
        store.set_priority(c, 4, PRIORITY_MIN).await.unwrap();
        store.shift_priorities(&[a, b, c], -3, 2).await.unwrap();
        assert_eq!((prio(a).await, prio(b).await, prio(c).await), (0, 1, 2), "截在 floor");
        store.shift_priorities(&[a], 1, 2).await.unwrap();
        assert_eq!(prio(a).await, 1, "往下调照常");
    }

    /// 批量启停 / 设备上限 / 删除：只作用于选中的 id，且各自保持单账号接口的语义。
    #[sqlx::test]
    async fn batch_ops_only_touch_selected(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).await.unwrap();
        let c = store.insert("c", None, "tc", "rc", 0, None, None, 1).await.unwrap();
        // 给 a、b 各造一条设备绑定与用量日志：删号要清绑定、留流水。
        for (did, cid) in [("d1", a.id), ("d2", b.id)] {
            sqlx::query("INSERT INTO device_bindings (device_id, cred_id) VALUES ($1, $2)")
                .bind(did)
                .bind(cid)
                .execute(&store.pool)
                .await
                .unwrap();
            exec(&store, "INSERT INTO usage_logs (cred_id) VALUES ($1)", cid).await;
        }

        // 批量停用 a、b：c 不受影响；停用会清掉被选中账号的设备绑定。
        assert_eq!(store.set_disabled_many(&[a.id, b.id], true).await.unwrap(), 2);
        let by_id = async |s: &CredentialStore| -> HashMap<i64, Credential> {
            s.list().await.unwrap().into_iter().map(|x| (x.id, x)).collect()
        };
        let m = by_id(&store).await;
        assert!(m[&a.id].disabled && m[&b.id].disabled);
        assert!(!m[&c.id].disabled, "未选中的账号不应被停用");
        let n = scalar(&store, "SELECT COUNT(*) FROM device_bindings").await;
        assert_eq!(n, 0, "停用应清掉这两个账号的设备绑定");

        // 批量启用要清 ban_reason（模拟先被自动封禁）。
        store.mark_banned(a.id, "banned").await.unwrap();
        assert!(by_id(&store).await[&a.id].ban_reason.is_some());
        assert_eq!(store.set_disabled_many(&[a.id], false).await.unwrap(), 1);
        let m = by_id(&store).await;
        assert!(!m[&a.id].disabled && m[&a.id].ban_reason.is_none(), "启用应清除封禁原因");

        // 批量设备上限：负值由 web 层收敛，这里验证按传入值原样落库。
        assert_eq!(store.set_device_limits(&[a.id, c.id], 5).await.unwrap(), 2);
        let m = by_id(&store).await;
        assert_eq!((m[&a.id].device_limit, m[&c.id].device_limit), (5, 5));
        assert_eq!(m[&b.id].device_limit, 0, "未选中的账号不应被改动");

        // 批量删除：账号没了，流水留着（随保留期自然裁掉）。
        assert_eq!(store.delete_many(&[a.id]).await.unwrap(), 1);
        let m = by_id(&store).await;
        assert!(!m.contains_key(&a.id) && m.contains_key(&b.id) && m.contains_key(&c.id));
        assert_eq!(usage_log_creds(&store).await, vec![a.id, b.id], "批量删号同样不删流水");

        // 空列表一律 no-op，不误伤全表。
        assert_eq!(store.delete_many(&[]).await.unwrap(), 0);
        assert_eq!(store.set_disabled_many(&[], true).await.unwrap(), 0);
        assert_eq!(store.set_device_limits(&[], 9).await.unwrap(), 0);
        assert_eq!(store.list().await.unwrap().len(), 2, "空列表操作不应改动任何账号");
    }

    /// 等级变了（升级到 Max）就把旧套餐下学到的记录清掉；等级没变则留着。
    #[sqlx::test]
    async fn tier_change_clears_denials(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];
        store.set_tier(a, Some("Pro")).await.unwrap();
        store.deny_model(a, "claude-fable-5", "plan", None).await.unwrap();
        store.set_tier(a, Some("Pro")).await.unwrap();
        assert_eq!(store.denied_models(a).await.unwrap().len(), 1, "等级没变，记录留着");
        store.set_tier(a, Some("Max 5x")).await.unwrap();
        assert!(store.denied_models(a).await.unwrap().is_empty(), "升级后记录作废");
    }

    /// 账号级限流把调度开关**落库关掉**，到点惰性自动打开；人工关的号不会被自动打开。
    #[sqlx::test]
    async fn rate_limit_pause_persists_and_auto_resumes_when_due(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let pick = async || store.select_for_device(Select::default()).await.map(|c| c.id);
        let now = crate::credentials::now_secs();

        // a 被限流暂停（还有一小时才到点）→ 落库停用，选号自然落到 b。
        store
            .pause_for_rate_limit(a, "上游限流：约 1 小时后自动恢复调度", now + 3600)
            .await
            .unwrap();
        let paused = store.get(a).await.unwrap().unwrap();
        assert!(paused.disabled && paused.resume_at == Some(now + 3600));
        assert_eq!(pick().await.unwrap(), b, "被限流暂停的号不该再被选中");

        // 到点：不需要任何后台任务，下一次选号顺手把它放回来。
        store.pause_for_rate_limit(a, "已到点", now - 1).await.unwrap();
        assert_eq!(pick().await.unwrap(), a, "到点应自动回到调度池并按 (priority, id) 重新胜出");
        let back = store.get(a).await.unwrap().unwrap();
        assert!(!back.disabled && back.resume_at.is_none() && back.ban_reason.is_none());

        // 人工停用没有 resume_at，怎么等都不会自己打开。
        store.set_disabled(a, true).await.unwrap();
        assert_eq!(pick().await.unwrap(), b, "人工停用的号不参与调度");
        assert_eq!(store.get(a).await.unwrap().unwrap().resume_at, None, "人工停用不该有恢复时刻");
        assert_eq!(
            CredentialStore::resume_due(&mut *store.pool.acquire().await.unwrap()).await.unwrap(),
            0,
            "没有该恢复的号"
        );
        assert!(store.get(a).await.unwrap().unwrap().disabled, "惰性恢复不该越过管理员的决定");
    }

    /// 订阅未生效的暂停：不写恢复时刻、到点不回来；只动启用中 / 限时暂停中的号，
    /// 人工停用与封禁不碰；连通性测试那条恢复只认这一档，手动启用照常能打开。
    #[sqlx::test]
    async fn inactive_subscription_suspension_waits_for_a_human_or_a_passing_probe(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b", "c", "d"]).await;
        let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);
        let pick = async || store.select_for_device(Select::default()).await.map(|c| c.id);
        let now = crate::credentials::now_secs();
        let reason = format!("[{SUBSCRIPTION_PAUSE_TAG} 403] {ORG_OAUTH_SUSPEND_MARKER}");

        assert!(store.suspend_for_inactive_subscription(a, &reason).await.unwrap());
        let got = store.get(a).await.unwrap().unwrap();
        assert!(got.disabled && got.resume_at.is_none());
        assert_eq!(got.ban_reason.as_deref(), Some(reason.as_str()));
        // 与封号同形但不算封号：保活 / 遥测照跑，refresh_token 才不会在等续费时过期。
        assert!(got.is_subscription_paused() && !got.is_banned());
        assert_ne!(pick().await.unwrap(), a, "暂停的号不该再被选中");
        assert_eq!(
            CredentialStore::resume_due(&mut *store.pool.acquire().await.unwrap()).await.unwrap(),
            0,
            "不会到点自己回来"
        );
        assert!(!store.resume_if_rate_limited(a).await.unwrap(), "手动解除限流不该放回这一档");
        assert!(
            !store.suspend_for_inactive_subscription(a, "again").await.unwrap(),
            "已暂停的不重写"
        );

        // 额度暂停中的号：改成这一档（额度回来了没订阅照样不放行）。
        store.pause_for_rate_limit(b, "quota", now + 3 * 86400).await.unwrap();
        assert!(store.suspend_for_inactive_subscription(b, &reason).await.unwrap());
        assert_eq!(store.get(b).await.unwrap().unwrap().resume_at, None);

        // 人工停用 / 封禁：不碰。
        store.set_disabled(c, true).await.unwrap();
        assert!(!store.suspend_for_inactive_subscription(c, &reason).await.unwrap());
        assert_eq!(store.get(c).await.unwrap().unwrap().ban_reason, None);
        store.mark_banned(d, "封号").await.unwrap();
        assert!(!store.suspend_for_inactive_subscription(d, &reason).await.unwrap());
        let got = store.get(d).await.unwrap().unwrap();
        assert_eq!(got.ban_reason.as_deref(), Some("封号"));
        assert!(got.is_banned() && !got.is_subscription_paused());

        // 连通性测试通过：只放回这一档。
        assert!(!store.resume_if_subscription_suspended(c).await.unwrap());
        assert!(!store.resume_if_subscription_suspended(d).await.unwrap());
        assert!(store.get(d).await.unwrap().unwrap().is_banned());
        assert!(store.resume_if_subscription_suspended(a).await.unwrap());
        let back = store.get(a).await.unwrap().unwrap();
        assert!(!back.disabled && back.ban_reason.is_none());
        // 手动启用照常能打开。
        store.set_disabled(b, false).await.unwrap();
        assert!(!store.get(b).await.unwrap().unwrap().disabled);

        // 并发在飞的另一条后回来一发 429：不能把订阅暂停改写成「到点自己回来」。
        assert!(store.suspend_for_inactive_subscription(b, &reason).await.unwrap());
        assert!(!store.pause_for_rate_limit(b, "quota", now + 3600).await.unwrap());
        let got = store.get(b).await.unwrap().unwrap();
        assert!(got.is_subscription_paused() && got.resume_at.is_none());
        // 封号、人工停用同样不被 429 改写。
        assert!(!store.pause_for_rate_limit(d, "quota", now + 3600).await.unwrap());
        assert!(store.get(d).await.unwrap().unwrap().is_banned());
        assert!(!store.pause_for_rate_limit(c, "quota", now + 3600).await.unwrap());
        assert_eq!(store.get(c).await.unwrap().unwrap().resume_at, None);

        // 管理员把暂停中的号手动关掉：降成普通的手动停用，连通性测试通过也不再打开它。
        store.set_disabled(b, true).await.unwrap();
        let got = store.get(b).await.unwrap().unwrap();
        assert!(got.disabled && got.ban_reason.is_none() && !got.is_subscription_paused());
        assert!(!store.resume_if_subscription_suspended(b).await.unwrap());
        assert!(store.get(b).await.unwrap().unwrap().disabled, "人工停用不该被一次测试打开");
        // 限流暂停中的号被手动关掉同理：那句「几点恢复」不留下来冒充封号原因。
        store.set_disabled(b, false).await.unwrap();
        store.pause_for_rate_limit(b, "quota", now + 3600).await.unwrap();
        store.set_disabled(b, true).await.unwrap();
        let got = store.get(b).await.unwrap().unwrap();
        assert!(got.disabled && got.ban_reason.is_none() && got.resume_at.is_none());
        // 封号原因不因手动关而清掉。
        store.set_disabled(d, true).await.unwrap();
        assert_eq!(store.get(d).await.unwrap().unwrap().ban_reason.as_deref(), Some("封号"));
    }

    /// 订阅暂停只认 luban 自己写的格式（开头 `[subscription-inactive NNN] ` + 固定片段，区分
    /// 大小写）：封号原因里抄进来的上游原文——包括上游错误没带类型时 `[403] <原话>` 那种与旧格式
    /// 同形的——哪怕含同样几个词，也不能被当成可以自动恢复的暂停。库里（GLOB）与内存里同一口径。
    #[sqlx::test]
    async fn subscription_pause_is_recognised_only_by_lubans_own_prefix(pool: PgPool) {
        let ours = format!(
            "[{SUBSCRIPTION_PAUSE_TAG} 403] {ORG_OAUTH_SUSPEND_MARKER} (subscription lapsed, or a Free plan without one); paused until enabled manually or a connectivity test passes"
        );
        // 上游错误没带类型时封号原因就是 `[403] <原话>`：原话以这几个词开头也不算。
        let ban = format!("[403] {ORG_OAUTH_SUSPEND_MARKER}; this organization has been disabled");
        assert!(is_subscription_pause_reason(&ours));
        assert!(is_subscription_pause_reason(&format!(
            "[{SUBSCRIPTION_PAUSE_TAG} 401] {ORG_OAUTH_SUSPEND_MARKER}"
        )));
        let upper = ours.replace("organization does not", "Organization does not");
        let mid = format!("[403] permission_error: your {ORG_OAUTH_SUSPEND_MARKER}");
        for other in [
            ban.as_str(),
            mid.as_str(),
            upper.as_str(),
            ORG_OAUTH_SUSPEND_MARKER,
            "[subscription-inactive 40] x",
            "[subscription-inactive 403]",
            "",
        ] {
            assert!(!is_subscription_pause_reason(other), "{other}");
        }

        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        store.suspend_for_inactive_subscription(a, &ours).await.unwrap();
        store
            .record_ban(
                b,
                &BanContext { reason: ban.clone(), source: "forward", ..Default::default() },
            )
            .await
            .unwrap();
        let got = store.get(b).await.unwrap().unwrap();
        assert!(got.is_banned() && !got.is_subscription_paused(), "真封号不该被认成订阅暂停");
        assert!(!store.resume_if_subscription_suspended(b).await.unwrap(), "真封号不该被测试打开");
        assert!(store.resume_if_subscription_suspended(a).await.unwrap());
        // 库里的 GLOB 同样区分大小写。
        store.suspend_for_inactive_subscription(a, &upper).await.unwrap();
        assert!(
            !store.resume_if_subscription_suspended(a).await.unwrap(),
            "大小写不同就不是我们写的"
        );

        // 批量停用与单个同口径：清掉暂停原因，之后测试通过也不再打开它。
        store.set_disabled(a, false).await.unwrap();
        store.suspend_for_inactive_subscription(a, &ours).await.unwrap();
        assert_eq!(store.set_disabled_many(&[a, b], true).await.unwrap(), 2);
        let got = store.get(a).await.unwrap().unwrap();
        assert!(got.disabled && got.ban_reason.is_none());
        assert!(!store.resume_if_subscription_suspended(a).await.unwrap());
        assert_eq!(store.get(b).await.unwrap().unwrap().ban_reason, Some(ban), "封号原因不动");
    }

    /// 连通性测试通过 → 自动回调度池；但只对**被限流暂停**的号生效。
    #[sqlx::test]
    async fn probe_success_resumes_only_rate_limited_pauses(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let now = crate::credentials::now_secs();

        store.pause_for_rate_limit(a, "上游限流", now + 7 * 24 * 3600).await.unwrap();
        assert!(store.resume_if_rate_limited(a).await.unwrap(), "限流暂停的号该被测试结果放回来");
        let back = store.get(a).await.unwrap().unwrap();
        assert!(!back.disabled && back.resume_at.is_none());

        // 人工停用 / 封号：测试通过也不动它——那是管理员的决定，或需要人工介入的终态。
        store.set_disabled(b, true).await.unwrap();
        assert!(!store.resume_if_rate_limited(b).await.unwrap());
        assert!(store.get(b).await.unwrap().unwrap().disabled, "人工停用不该被一次连通性测试打开");
        store.mark_banned(b, "封号").await.unwrap();
        assert!(!store.resume_if_rate_limited(b).await.unwrap());
        assert!(store.get(b).await.unwrap().unwrap().disabled, "封号更不该被测试打开");
    }

    /// 单账号统计：只算这个号、按起点截断；桶按时区偏移切；四个维度各自按请求数降序；
    /// 错误与本地拒绝分开数；缓存写在只有细分档的老记录上拿两档相加。
    #[sqlx::test]
    async fn credential_stats_buckets_and_groups(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let day = 86_400;
        // 窗口起点取某天 UTC 零点，下面的时刻都相对它算，桶边界才好核对。
        let base = 20_000 * day;
        let rec =
            |cred: i64, model: &str, device: Option<&str>, status: u16, cost: f64| UsageRecord {
                cred_id: Some(cred),
                cred_label: "x".into(),
                model: Some(model.into()),
                device_id: device.map(Into::into),
                ua: Some("claude-cli/2.1.280".into()),
                status,
                has_usage: true,
                input_tokens: Some(10),
                output_tokens: Some(20),
                cache_creation_tokens: Some(3),
                cache_read_tokens: Some(100),
                cost_usd: Some(cost),
                ..Default::default()
            };
        let insert =
            async |r: &UsageRecord, ts: i64| store.insert_usage_log_at(r, Some(ts)).await.unwrap();
        // 第一天：opus 两条成功（设备 d1）、sonnet 一条 429。
        insert(&rec(a, "claude-opus-5", Some("d1"), 200, 0.5), base + 3600).await;
        insert(&rec(a, "claude-opus-5", Some("d1"), 200, 0.5), base + 7200).await;
        insert(&rec(a, "claude-sonnet-5", None, 429, 0.0), base + 7300).await;
        // 第二天：一条本地拒绝，一条只有细分档缓存写的老记录。
        insert(
            &UsageRecord {
                cred_id: Some(a),
                status: 429,
                forensics: Forensics {
                    rewrites: Some("rejected_locally:device-limit".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            base + day + 60,
        )
        .await;
        insert(
            &UsageRecord {
                cache_creation_tokens: None,
                cache_5m_tokens: Some(4),
                cache_1h_tokens: Some(6),
                ..rec(a, "claude-opus-5", Some("d2"), 200, 1.0)
            },
            base + day + 120,
        )
        .await;
        // 别的号、窗口之前的记录都不算。
        insert(&rec(b, "claude-opus-5", Some("d1"), 200, 9.0), base + 3600).await;
        insert(&rec(a, "claude-opus-5", Some("d1"), 200, 9.0), base - 10).await;

        let s = store.credential_stats(a, base, day, 0, 10).await.unwrap();
        assert_eq!(s.summary.requests, 5);
        assert_eq!(s.summary.errors, 2, "429 两条（含本地拒绝）");
        assert_eq!(s.summary.rejected, 1);
        assert!((s.summary.cost_usd - 2.0).abs() < 1e-9);
        assert_eq!(s.summary.cache_write_tokens, 3 * 3 + 10);
        assert_eq!(
            s.points.iter().map(|p| (p.ts, p.requests)).collect::<Vec<_>>(),
            vec![(base, 3), (base + day, 2)]
        );
        assert_eq!(s.by_model[0].key, "claude-opus-5");
        assert_eq!(s.by_model[0].requests, 3);
        assert_eq!(s.by_model[0].last_ts, base + day + 120);
        // d1 与空设备（sonnet 那条 + 本地拒绝）都是 2 条，并列按 key 排，空串在前。
        assert_eq!(
            s.by_device.iter().map(|g| (g.key.as_str(), g.requests)).collect::<Vec<_>>(),
            vec![("", 2), ("d1", 2), ("d2", 1)],
            "没带设备身份的归空串"
        );
        assert_eq!(s.by_device[1].tokens, 2 * (10 + 20 + 3 + 100));
        assert_eq!(s.by_status[0].key, "200");
        assert_eq!(
            s.by_status[1],
            CredentialStatsGroup {
                key: "429".into(),
                requests: 2,
                errors: 2,
                tokens: 133,
                cost_usd: 0.0,
                last_ts: base + day + 60,
            }
        );
        assert_eq!(s.by_client[0].key, "claude-cli/2.1.280");

        // 东八区：UTC 当天 17:00 之后落到本地第二天。
        let tz = 8 * 3600;
        insert(&rec(a, "claude-opus-5", Some("d1"), 200, 0.0), base + 17 * 3600).await;
        let local = store.credential_stats(a, base, day, tz, 10).await.unwrap();
        assert!(local.points.iter().any(|p| p.ts == base + day - tz));
        // group_limit 生效。
        assert_eq!(store.credential_stats(a, base, day, 0, 1).await.unwrap().by_model.len(), 1);
    }

    /// 额度快照取「最新一条带限流信息的行」，窗口费用和请求数只算 `reset - 窗口` 之后的日志，
    /// 且不串号。这条 SQL 从 1+2N 条查询合成了一条，口径必须逐项对上。
    #[sqlx::test]
    async fn latest_quotas_sums_only_current_window(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap().id;
        let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).await.unwrap().id;

        // 账号 a：reset=100_000，故 5h 窗口起点 82_000、7d 窗口起点 -504_800（含全部行）。
        let r5 = 100_000;
        log_row(&store, a, 10_000, 1.0, Some(r5), Some(r5)).await; // 5h 窗口外
        log_row(&store, a, 90_000, 2.0, Some(r5), Some(r5)).await; // 窗口内
        log_row(&store, a, 95_000, 4.0, Some(r5), Some(r5)).await; // 窗口内，且是最新快照行
        // 更晚但不带限流头的行：不该覆盖快照，费用仍要计入窗口。
        store
            .insert_usage_log_at(
                &UsageRecord { cred_id: Some(a), cost_usd: Some(8.0), ..Default::default() },
                Some(99_000),
            )
            .await
            .unwrap();
        log_row(&store, b, 95_000, 16.0, Some(r5), Some(r5)).await; // 他号，不得混入

        let q = store.latest_quotas().await.unwrap();
        let qa = q.get(&a).expect("a 应有快照");
        assert_eq!(qa.ts, 95_000, "快照应取最新一条带限流信息的行");
        assert_eq!(qa.cost_5h, Some(14.0), "只应含 ts >= reset-5h 的 2+4+8");
        assert_eq!(qa.cost_7d, Some(15.0), "7d 窗口覆盖全部 1+2+4+8");
        assert_eq!(qa.requests_5h, Some(3), "5h 窗口应计入 3 次请求");
        assert_eq!(qa.requests_7d, Some(4), "7d 窗口应计入 4 次请求");
        // token 与费用/请求数同窗口同断点：窗口内两条带 token 的流水各 10 个，那条没嗅探到
        // usage 的（各列 NULL）按 0 计而不是把整个和抹成 NULL。
        assert_eq!(qa.tokens_5h, Some(20), "5h 窗口内两条 ×10，无 usage 的那条按 0");
        assert_eq!(qa.tokens_7d, Some(30), "7d 窗口覆盖三条带 token 的流水");
        assert_eq!(q.get(&b).unwrap().cost_5h, Some(16.0), "费用不得跨账号串");
        assert_eq!(q.get(&b).unwrap().requests_5h, Some(1), "请求数不得跨账号串");
        assert_eq!(q.get(&b).unwrap().tokens_5h, Some(10), "token 不得跨账号串");

        // 单账号入口与批量入口必须给出同一份结果。
        assert_eq!(store.latest_quota(a).await.unwrap().unwrap().cost_5h, qa.cost_5h);
        assert_eq!(store.latest_quota(a).await.unwrap().unwrap().ts, qa.ts);
        assert!(store.latest_quota(999).await.unwrap().is_none(), "不存在的账号应为 None");
    }

    /// 只有一个窗口带 `reset` 时，窗口统计不得被连接条件的下界误伤成 0。
    ///
    /// 这条护栏针对的是那个下界本身：它取「两个窗口起点里更早的那个」，而 SQLite 的
    /// `min(NULL, x)` 是 **NULL**——不加 COALESCE 兜底的话，缺一个 reset 就会让整个
    /// ON 条件恒假、一行流水都连不上，窗口费用与请求数齐刷刷变成 0。
    #[sqlx::test]
    async fn window_stats_survive_a_missing_reset(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap().id;
        let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).await.unwrap().id;

        // a：只有 5h 有 reset（窗口起点 82_000），7d 一直为空。
        log_row(&store, a, 10_000, 1.0, Some(100_000), None).await; // 5h 窗口外
        log_row(&store, a, 90_000, 2.0, Some(100_000), None).await; // 窗口内
        log_row(&store, a, 95_000, 4.0, Some(100_000), None).await; // 窗口内
        let qa = store.latest_quota(a).await.unwrap().unwrap();
        assert_eq!(qa.cost_5h, Some(6.0), "缺 7d reset 不该把 5h 窗口打成 0");
        assert_eq!(qa.requests_5h, Some(2));
        assert_eq!(qa.tokens_5h, Some(20));
        assert_eq!(qa.cost_7d, None, "没有 7d reset 就没有 7d 窗口可算");
        assert_eq!(qa.requests_7d, None);
        assert_eq!(qa.tokens_7d, None, "没有窗口就没有 token 可算，不能给 0");

        // b：反过来只有 7d 有 reset（窗口起点 100_000 - 604_800，含全部行）。
        log_row(&store, b, 90_000, 8.0, None, Some(100_000)).await;
        log_row(&store, b, 95_000, 16.0, None, Some(100_000)).await;
        let qb = store.latest_quota(b).await.unwrap().unwrap();
        assert_eq!(qb.cost_7d, Some(24.0), "缺 5h reset 不该把 7d 窗口打成 0");
        assert_eq!(qb.requests_7d, Some(2));
        assert_eq!(qb.tokens_7d, Some(20));
        assert_eq!(qb.cost_5h, None);
        assert_eq!(qb.tokens_5h, None);
    }

    /// 窗口 token 数按官方 `usage` 的四项相加，且**不看模型认不认得**。
    ///
    /// 两处容易算漏，各钉一条：
    /// - 缓存写只报了 5m/1h 细分、没报合计时要退回两档之和，否则这类响应的缓存写整段丢失；
    /// - 模型不在价目表里时 `cost_usd` 为 NULL（费用算不出），但 token 是上游实报的，
    ///   该照数——把它跟着费用一起吞掉，卡片上就会出现「有请求、0 token」。
    #[sqlx::test]
    async fn window_tokens_sum_official_usage_fields(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap().id;
        let reset = 100_000; // 5h 窗口起点 82_000

        // 只有 5m/1h 细分的一条：输入 10 + 输出 20 + 缓存写 (30+40) + 缓存读 50 = 150。
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    has_usage: true,
                    input_tokens: Some(10),
                    output_tokens: Some(20),
                    cache_5m_tokens: Some(30),
                    cache_1h_tokens: Some(40),
                    cache_read_tokens: Some(50),
                    rl_5h_utilization: Some(0.5),
                    rl_5h_reset: Some(reset),
                    ..Default::default()
                },
                Some(90_000),
            )
            .await
            .unwrap();
        // 模型未知（cost_usd 为 None）但有 token 的一条：1 + 2 = 3。
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    has_usage: true,
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    ..Default::default()
                },
                Some(95_000),
            )
            .await
            .unwrap();

        let q = store.latest_quota(a).await.unwrap().unwrap();
        assert_eq!(q.tokens_5h, Some(153), "细分缓存写与未计价的行都要计入");
        assert_eq!(q.cost_5h, Some(0.0), "两条都没有 cost_usd，费用仍是 0");
        assert_eq!(q.requests_5h, Some(2));
    }

    /// **只**上报没有专用列的窗口时，照样要写出快照。
    ///
    /// 旧判据是 `rl_5h_utilization.is_some() || rl_7d_utilization.is_some()`，于是这种账号
    /// 永远写不进 credential_stats，卡片恒为「暂无数据」——哪怕它此刻正靠 usage credits 放行。
    #[sqlx::test]
    async fn snapshot_is_written_even_without_5h_or_7d(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap().id;

        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_overage_in_use: Some(true),
                    windows: vec![win("7d_oi", 1.02, 400_000, "rejected")],
                    ..Default::default()
                },
                Some(1_000),
            )
            .await
            .unwrap();

        let q = store.latest_quota(a).await.unwrap().expect("只有 7d_oi 的账号也必须有快照");
        assert_eq!(q.overage_in_use, Some(true));
        assert_eq!(q.windows.len(), 1);
        assert_eq!(q.windows[0].name, "7d_oi");
        // 没有 5h/7d 就没有窗口起点可反推，窗口内费用/请求数保持空。
        assert_eq!(q.cost_5h, None);
        assert_eq!(q.requests_7d, None);
    }

    /// 一个窗口都没有的响应（CDN 拦截页那类，只剩 unified-status）不得覆盖已有快照——
    /// 否则等于拿一条信息更少的记录抹掉信息更多的。
    #[sqlx::test]
    async fn windowless_response_does_not_erase_snapshot(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap().id;

        let windows = vec![win("5h", 0.5, 9_000, "allowed")];
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_5h_utilization: Some(0.5),
                    rl_5h_reset: Some(9_000),
                    windows: windows.clone(),
                    ..Default::default()
                },
                Some(1_000),
            )
            .await
            .unwrap();
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    unified_status: Some("rejected".into()),
                    ..Default::default()
                },
                Some(2_000),
            )
            .await
            .unwrap();

        let q = store.latest_quota(a).await.unwrap().unwrap();
        assert_eq!(q.ts, 1_000, "无窗口的响应不该顶掉快照");
        assert_eq!(q.windows, windows);
    }

    /// reset 为空时对应窗口的费用与请求数留空（而非 0）：分不清「没用」和「不知道窗口起点」
    /// 会让卡片把未知显示成已用 0。
    #[sqlx::test]
    async fn latest_quotas_leaves_cost_none_without_reset(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap().id;
        log_row(&store, a, 40_000, 3.0, Some(50_000), None).await; // 在 5h 窗口(32_000 起)内

        let q = store.latest_quota(a).await.unwrap().unwrap();
        assert_eq!(q.cost_5h, Some(3.0));
        assert_eq!(q.requests_5h, Some(1));
        assert_eq!(q.cost_7d, None, "无 7d reset 时不应给出 0");
        assert_eq!(q.requests_7d, None, "无 7d reset 时请求数也应未知");
    }

    /// 删号时正好有一条流水在写：两边都得成功，不能死锁。
    ///
    /// 写流水的事务先对账号行上 `FOR KEY SHARE`，再 upsert 账本（见 `insert_usage_log_at`）。
    /// 删号若先删账本行、最后才删账号行，就是反向加锁：它拿着账本行等账号行，流水拿着账号行
    /// 等账本行，PG 判死锁、杀掉其中一个。这里手工复现流水事务的那两步，把删号夹在中间。
    #[sqlx::test]
    async fn removing_a_credential_does_not_deadlock_with_an_in_flight_usage_log(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let id = ids[0];
        log_row(&store, id, 1_000, 1.0, None, None).await;
        let store = std::sync::Arc::new(store);

        let mut usage = store.pool.begin().await.unwrap();
        sqlx::query("SELECT owner_id FROM credentials WHERE id = $1 FOR KEY SHARE")
            .bind(id)
            .execute(&mut *usage)
            .await
            .unwrap();
        let s = store.clone();
        let removal = tokio::spawn(async move { s.remove(&[id]).await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        sqlx::query(
            "UPDATE credential_stats SET cost_total_usd = cost_total_usd + 1 WHERE cred_id = $1",
        )
        .bind(id)
        .execute(&mut *usage)
        .await
        .expect("the in-flight usage log must not be chosen as a deadlock victim");
        usage.commit().await.unwrap();
        assert_eq!(removal.await.unwrap().unwrap(), 1);
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM credential_stats").await, 0);
    }
}
