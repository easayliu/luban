//! 凭证的增删改：启停、暂停与恢复、优先级、名额上限、账号资料回填。

use super::*;

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

/// [`is_subscription_pause_reason`] 的 SQL 版（`ban_reason` 列上的 GLOB 条件，区分大小写）。
/// `[[]` 是 GLOB 里字面的 `[`；标签与片段里只有字母、连字符与空格，不含 GLOB 元字符。
const SUBSCRIPTION_PAUSE_SQL: &str = "ban_reason GLOB \
     '[[]subscription-inactive [0-9][0-9][0-9]] organization does not allow OAuth authentication*'";

/// 人工停用（单个 [`CredentialStore::set_disabled`] 与批量 [`CredentialStore::set_disabled_many`]
/// 共用）：停用、清 `resume_at`，并清掉两种暂停留下的原因——限流暂停的「几点恢复」、订阅未生效
/// 的那句——号就是普通的「手动停用」。不清的话，限流那句会在 `resume_at` 清空后被当成封号原因
/// 显示，订阅那句会让连通性测试通过时把管理员关掉的号又打开。封号原因不动。
///
/// SQLite 的 `UPDATE` 里各表达式读的都是改之前的行，`CASE` 看到的 `resume_at` 是旧值。
fn manual_disable_sql() -> String {
    format!(
        "UPDATE credentials SET disabled = 1, resume_at = NULL, \
                ban_reason = CASE WHEN resume_at IS NOT NULL OR {SUBSCRIPTION_PAUSE_SQL} \
                                  THEN NULL ELSE ban_reason END, \
                updated_at = unixepoch() \
         WHERE id = ?1"
    )
}

impl CredentialStore {
    /// 插入一条新凭证，返回带 id 的完整记录。
    // 参数多是因为一条凭证本来就有这么多字段，且调用点只有「加号」那一处；
    // 打包成结构体只会多一个只用一次的类型。
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &self,
        label: &str,
        tier: Option<&str>,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
        account_uuid: Option<&str>,
        org_type: Option<&str>,
    ) -> Result<Credential> {
        let conn = self.conn.lock();
        // 新凭证一律落在默认档 P2：同档内按设备数负载均衡，新账号立刻参与分摊。
        // 需要瀑布式（榨干一个再用下一个）时，手动/批量把账号调到不同优先级即可。
        // 显式写 priority：老库的列默认值还是 0，不能指望它。
        conn.execute(
            "INSERT INTO credentials
                 (label, tier, access_token, refresh_token, expires_at, account_uuid, org_type,
                  priority)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                label,
                tier,
                access_token,
                refresh_token,
                expires_at as i64,
                account_uuid,
                org_type,
                PRIORITY_DEFAULT
            ],
        )
        .context("failed to insert credential (the refresh_token may already exist)")?;
        let id = conn.last_insert_rowid();
        conn.query_row(&format!("SELECT {COLS} FROM credentials WHERE id = ?1"), [id], row_to_cred)
            .context("failed to read the newly inserted credential")
    }

    /// 列出全部凭证，按 (priority, id) 升序。
    ///
    /// 先惰性恢复到点的限流暂停号（[`Self::resume_due`]），否则后台会一直显示成「已停用」，
    /// 直到下一条转发请求碰巧来触发恢复——控制台上看到的必须是此刻真实的调度状态。
    pub fn list(&self) -> Result<Vec<Credential>> {
        let conn = self.conn.lock();
        Self::resume_due(&conn)?;
        let mut stmt =
            conn.prepare(&format!("SELECT {COLS} FROM credentials ORDER BY priority ASC, id ASC"))?;
        let rows = stmt.query_map([], row_to_cred)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 按 id 读取单条。
    pub fn get(&self, id: i64) -> Result<Option<Credential>> {
        let conn = self.conn.lock();
        conn.query_row(&format!("SELECT {COLS} FROM credentials WHERE id = ?1"), [id], row_to_cred)
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
    }

    /// 删除一条，返回是否确有删除。口径同 [`Self::remove`]（仅测试用）。
    #[cfg(test)]
    pub fn delete(&self, id: i64) -> Result<bool> {
        Ok(self.remove(&[id])? > 0)
    }

    /// 清空所有凭证，返回删除条数。连带清空设备绑定与全部用量日志（口径同
    /// [`Self::delete`]：账号没了，历史用量不再保留）。
    pub fn clear(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM usage_logs", [])?;
        tx.execute("DELETE FROM device_bindings", [])?;
        tx.execute("DELETE FROM session_bindings", [])?;
        tx.execute("DELETE FROM credential_stats", [])?;
        tx.execute("DELETE FROM device_costs", [])?;
        tx.execute("DELETE FROM model_denials", [])?;
        let n = tx.execute("DELETE FROM credentials", [])?;
        tx.commit()?;
        Ok(n)
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
    pub fn set_disabled(&self, id: i64, disabled: bool) -> Result<bool> {
        let conn = self.conn.lock();
        if disabled {
            conn.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
            conn.execute("DELETE FROM session_bindings WHERE cred_id = ?1", [id])?;
            // 两种暂停留下的原因也一并清掉，理由见 [`manual_disable_sql`]。
            Ok(conn.execute(&manual_disable_sql(), [id])? > 0)
        } else {
            Ok(conn.execute(
                "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                        updated_at = unixepoch() \
                 WHERE id = ?1",
                [id],
            )? > 0)
        }
    }

    /// 上游确认限流（账号级 429）时调用：把这个号停用并记下**到点自动恢复的时刻**，
    /// 同时清空其设备绑定，让绑在它上面的设备下一条请求立刻改选别的号。
    ///
    /// 与 [`Self::record_ban`] 的唯一结构差别是多写一个 `resume_at`，而那正是
    /// 「限流暂停」与「封号/人工停用」的分界：`resume_at` 非空的号会被
    /// [`Self::resume_due`] 到点自动启用、也会被连通性测试成功时自动启用
    /// （见 [`Self::resume_if_rate_limited`]），另外两种则必须人工介入。
    ///
    /// 为什么落库而不是只记内存（原来的 [`RateLimitCooldown`] 做法）：额度耗尽动辄几小时到
    /// 几天，远长于一次进程重启；记内存则重启即忘，一重启就又拿这个号去撞一发 429。
    /// 落库之后重启也记得，代价是必须自己保证「到点恢复」不依赖进程一直活着——所以恢复做成
    /// 惰性的（选号时顺手扫一遍），而不是挂一个后台定时器。
    ///
    /// `reason` 直接写进 `ban_reason`，后台卡片原样展示，故调用方应带上人话的恢复时刻。
    ///
    /// 只动**启用中或已在限流暂停中**的号（见 [`Self::park_row`]）：先回来的那条把号封了 /
    /// 按订阅停了，后回来的 429 不能把它改写成「到点自己回来」。返回是否确有写入。
    pub fn pause_for_rate_limit(&self, id: i64, reason: &str, resume_at: u64) -> Result<bool> {
        self.park_row(id, reason, Some(resume_at as i64))
    }

    /// 两种自动暂停（[`Self::pause_for_rate_limit`]、[`Self::suspend_for_inactive_subscription`]）
    /// 共用的落库：停用、写原因与恢复时刻（`None` = 不会到点自己回来）、清绑定。
    ///
    /// 守卫只在这一处：只动**启用中或已在限流暂停中**的号，封号、人工停用、订阅未生效暂停
    /// 一概不碰——同一个号常有几条请求同时在飞，先回来的那条已经把号处置了，后回来的不能
    /// 改写它。返回是否确有写入。
    fn park_row(&self, id: i64, reason: &str, resume_at: Option<i64>) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let updated = tx.execute(
            "UPDATE credentials SET disabled = 1, ban_reason = ?2, resume_at = ?3, \
                    updated_at = unixepoch() \
             WHERE id = ?1 AND (disabled = 0 OR resume_at IS NOT NULL)",
            params![id, reason, resume_at],
        )? > 0;
        if updated {
            tx.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
            tx.execute("DELETE FROM session_bindings WHERE cred_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(updated)
    }

    /// 订阅未生效——付费档到期未续费、Free 档没订阅（见 [`crate::proxy::park_org_oauth_disallowed`]）：停调度、清绑定，
    /// **不写 `resume_at`**——不会到点自己回来，只有两条路放回池子：控制台手动启用
    /// （[`Self::set_disabled`]），或连通性测试通过（[`Self::resume_if_subscription_suspended`]）。
    ///
    /// 不写恢复时刻是有意的：`resume_at` 非空在选号那边的意思是「等一会就好」，全池都在等时
    /// 回 429 + 最早恢复时刻（见 [`Self::select_for_device`]）；这里等多久都没用，得有人去
    /// 续费或订阅，与封号同形（`disabled + ban_reason`，`resume_at` 空）。
    /// 与封号的区别只在原因文案（含 [`ORG_OAUTH_SUSPEND_MARKER`]）、不落封号事件，以及
    /// 连通性测试通过能放回来。
    ///
    /// 只动**启用中或限时暂停中**的号：人工停用、封禁、已经这样暂停的不碰——保活对人工停用的
    /// 号照发，不能让一发 403 改掉管理员的决定或覆盖封号原因。额度暂停中的号会被改成这一档：
    /// 额度回来了，没订阅照样不放行。
    ///
    /// 返回是否确有写入；`false` 即号已在池外（或不存在），调用方不必再记一遍。
    pub fn suspend_for_inactive_subscription(&self, id: i64, reason: &str) -> Result<bool> {
        self.park_row(id, reason, None)
    }

    /// 连通性测试通过时调用：若该号是 [`Self::suspend_for_inactive_subscription`] 停下的，当场恢复调度。
    /// 测试通过说明已经续费 / 订阅。认的是 luban 自己写的原因开头（[`is_subscription_pause_reason`] 同一口径），
    /// 封号、人工停用不受影响。返回是否确有恢复。
    pub fn resume_if_subscription_suspended(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            &format!(
                "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                    updated_at = unixepoch() \
             WHERE id = ?1 AND disabled = 1 AND resume_at IS NULL AND {SUBSCRIPTION_PAUSE_SQL}"
            ),
            [id],
        )? > 0)
    }

    /// 把所有「限流暂停且已到恢复时刻」的号重新启用，返回实际恢复的条数。
    ///
    /// 惰性执行（选号与列表各调一次，见 [`Self::select_for_device`]/[`Self::list`]），
    /// 和设备绑定的 TTL 过期同一套路子：不挂后台定时器，进程没在跑的时候也不需要它跑——
    /// 反正没人发请求。条件里的 `resume_at IS NOT NULL` 是关键，它保证只碰限流暂停的号，
    /// 封号与人工停用的不会被顺手打开。
    pub(super) fn resume_due(conn: &Connection) -> Result<usize> {
        Ok(conn.execute(
            "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                    updated_at = unixepoch() \
             WHERE disabled = 1 AND resume_at IS NOT NULL AND resume_at <= unixepoch()",
            [],
        )?)
    }

    /// 连通性测试通过时调用：若该号是被限流自动停用的（`resume_at` 非空），当场恢复调度。
    ///
    /// 测试成功是「上游此刻确实放这个号过」的一手证据，比我们从限流头算出来的恢复时刻更硬——
    /// 那个时刻偏保守时，好号会被白白晾着。返回是否确有恢复。
    ///
    /// 只认 `resume_at` 非空的号：人工关掉的号不该被一次连通性测试打开，那是管理员的决定。
    pub fn resume_if_rate_limited(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                    updated_at = unixepoch() \
             WHERE id = ?1 AND resume_at IS NOT NULL",
            [id],
        )? > 0)
    }

    /// 自动检测到上游账号级错误（如封号）时调用：停用凭证并记录原因，
    /// 同时清空其设备绑定，使下一次请求立即改选其它凭证。
    ///
    /// 与 [`Self::set_disabled`] 的区别在于会写入 `ban_reason`，供后台 UI 区分
    /// 「管理员手动停用」与「上游自动判定停用」。封号是需要人工介入的终态，不写
    /// `resume_at`（对比 [`Self::pause_for_rate_limit`]）。
    ///
    /// 这是 [`Self::record_ban`] 的简写：只有一句原因、没有别的上下文（事件来源记为
    /// `manual`）。**已弃用**：生产路径一律走 `record_ban`，把状态码、完整报文、请求 id
    /// 一并存进封号事件——此前保活循环拿它记 401/403，事件里只剩「upstream 401/403」，
    /// token 吊销、组织权限、区域限制与真封号分不开（见 `web::handle_keepalive_rejection`）。
    /// 保留为兼容入口并标 deprecated，新调用点编译时会被警告；测试里用它造「已封禁」状态。
    #[allow(dead_code)] // 兼容入口：本 crate 内只剩测试在用
    #[deprecated(
        since = "0.3.97",
        note = "走 record_ban 并带上 BanContext（状态码、错误正文、request id），别只留一句原因"
    )]
    pub fn mark_banned(&self, id: i64, reason: &str) -> Result<bool> {
        self.record_ban(
            id,
            &BanContext { reason: reason.to_string(), source: "manual", ..Default::default() },
        )
    }

    /// 设置优先级。范围由调用方（admin API）校验，这里不再截断。
    pub fn set_priority(&self, id: i64, priority: i64) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET priority = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, priority],
        )
    }

    /// 批量设置优先级：把 `ids` 里的账号统一改到 `priority`，返回实际更新的条数。
    /// 单事务内完成，避免中途失败留下一半新一半旧的调度档位。`ids` 为空时直接返回 0。
    pub fn set_priorities(&self, ids: &[i64], priority: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET priority = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, priority])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量平移优先级：`ids` 里的账号各自在原值上加 `delta`（负数 = 提高），超出
    /// [`PRIORITY_MIN`]..=[`PRIORITY_MAX`] 的截到边界。选中账号之间的先后顺序不变
    /// （碰到边界的除外）。单事务，返回实际更新的条数。`ids` 里重复的只平移一次。
    pub fn shift_priorities(&self, ids: &[i64], delta: i64) -> Result<usize> {
        // 平移不是幂等的：同一个 id 出现两次就会被加两次 delta，先去重。
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET priority = MIN(MAX(priority + ?2, ?3), ?4),
                     updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in &ids {
                n += stmt.execute(params![id, delta, PRIORITY_MIN, PRIORITY_MAX])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量删除：口径同 [`Self::remove`]，返回实际删除的条数（仅测试用）。
    #[cfg(test)]
    pub fn delete_many(&self, ids: &[i64]) -> Result<usize> {
        self.remove(ids)
    }

    /// 删号：账号行与挂在它上面的小表（绑定、账本、设备费用、模型拒绝）在同一个短事务里
    /// 删掉，返回实际删除的账号数。
    ///
    /// **用量流水不删**，留给 [`Self::prune_usage_logs`] 按保留期自然裁掉。此前删号时
    /// 顺手把这个号的流水也清掉，一个号保留期内的流水动辄几万行，表又宽、挂着十来条索引，
    /// 分批删也要连着几分钟和转发抢 `conn` 这把锁，删号期间整个后台和转发都跟着慢。而留着
    /// 这些行并没有害处：
    /// - 账号 id 自增不复用（见 migrates_and_stops_id_reuse），不会被记到新号头上；
    /// - 每行自带 `cred_label`，请求日志、按账号拆分照样显示得出是哪个号；
    /// - 账号列表的费用/最近使用走账本（这里一并删了），选号与 RPM 只看在册账号。
    ///
    /// 全局口径的统计（总览、请求日志、按账号拆分）因此会带上已删账号保留期内的用量——
    /// 那些请求确实发生过、钱也确实花了，算进去才对得上账。
    pub fn remove(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut binds = tx.prepare("DELETE FROM device_bindings WHERE cred_id = ?1")?;
            let mut sbinds = tx.prepare("DELETE FROM session_bindings WHERE cred_id = ?1")?;
            let mut stats = tx.prepare("DELETE FROM credential_stats WHERE cred_id = ?1")?;
            let mut costs = tx.prepare("DELETE FROM device_costs WHERE cred_id = ?1")?;
            let mut denials = tx.prepare("DELETE FROM model_denials WHERE cred_id = ?1")?;
            let mut cred = tx.prepare("DELETE FROM credentials WHERE id = ?1")?;
            for id in ids {
                binds.execute([id])?;
                sbinds.execute([id])?;
                stats.execute([id])?;
                costs.execute([id])?;
                denials.execute([id])?;
                n += cred.execute([id])?;
            }
        }
        tx.commit()?;
        // 号没了，它们在内存里的限流窗口与冷却也留着没用（id 不会被复用，见
        // migrates_and_stops_id_reuse）。
        for id in ids {
            self.bare_rate.forget(id);
            self.rpm_rate.forget(id);
            self.cooldown.forget(*id);
        }
        Ok(n)
    }

    /// 批量启停：语义与 [`Self::set_disabled`] 一致（停用时清设备绑定使其立即改选其它
    /// 凭证；启用时清 `ban_reason`），返回实际更新的条数。单事务内完成。
    pub fn set_disabled_many(&self, ids: &[i64], disabled: bool) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            if disabled {
                let mut binds = tx.prepare("DELETE FROM device_bindings WHERE cred_id = ?1")?;
                let mut sbinds = tx.prepare("DELETE FROM session_bindings WHERE cred_id = ?1")?;
                // 同 `set_disabled`：人工操作两个方向都清 `resume_at`，
                // 限流那套惰性恢复不该越过管理员的决定。
                // 两种暂停留下的原因也一并清掉，理由见 [`manual_disable_sql`]。
                let mut stmt = tx.prepare(&manual_disable_sql())?;
                for id in ids {
                    binds.execute([id])?;
                    sbinds.execute([id])?;
                    n += stmt.execute([id])?;
                }
            } else {
                let mut stmt = tx.prepare(
                    "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                     updated_at = unixepoch() WHERE id = ?1",
                )?;
                for id in ids {
                    n += stmt.execute([id])?;
                }
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量设置设备数上限（三态语义同 [`Self::set_device_limit`]），返回实际更新的条数。
    pub fn set_device_limits(&self, ids: &[i64], limit: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET device_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, limit])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 设置该账号的设备数上限。三态：`> 0` 本账号独立上限；`0` 跟随全局默认
    /// （见 [`DEFAULT_DEVICE_LIMIT`]）；`< 0` 本账号明确不限（不受全局默认约束）。
    pub fn set_device_limit(&self, id: i64, limit: i64) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET device_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, limit],
        )
    }

    /// 批量设置模拟会话数上限（三态语义同 [`Self::set_session_limit`]），返回实际更新的条数。
    pub fn set_session_limits(&self, ids: &[i64], limit: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET session_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, limit])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 设置模拟会话数上限，返回是否确有更新。三态同 [`Self::set_device_limit`]：`> 0` 本账号
    /// 独立上限；`0` 跟随全局默认（[`DEFAULT_SESSION_LIMIT`]）；`< 0` 本账号明确不限。
    pub fn set_session_limit(&self, id: i64, limit: i64) -> Result<bool> {
        let n = self.conn.lock().execute(
            "UPDATE credentials SET session_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, limit],
        )?;
        Ok(n > 0)
    }

    /// 批量设置账号自己的提前停调度阈值（两档整份覆盖）；三态同 [`Self::set_quota_pause_pcts`]。
    pub fn set_quota_pause_pcts_many(
        &self,
        ids: &[i64],
        short_pct: Option<i64>,
        long_pct: Option<i64>,
    ) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let short_pct = short_pct.map(|p| p.clamp(0, 100));
        let long_pct = long_pct.map(|p| p.clamp(0, 100));
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET quota_pause_pct = ?2, quota_pause_pct_7d = ?3, \
                 updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, short_pct, long_pct])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量设置账号 RPM 上限；三态同 [`Self::set_rpm_limit`]。
    pub fn set_rpm_limits(&self, ids: &[i64], limit: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET rpm_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, limit])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 设置该账号每分钟最多转发多少条请求。三态同设备上限：`> 0` 本账号独立上限；
    /// `0` 跟随全局默认（见 [`DEFAULT_RPM_LIMIT`]）；`< 0` 本账号明确不限。
    ///
    /// 计数在进程内存里（见 [`RateWindow`]），改完即时生效，不影响已经记在窗口里的那些。
    pub fn set_rpm_limit(&self, id: i64, limit: i64) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET rpm_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, limit],
        )
    }

    /// 设置该账号自己的「额度用到多少就提前停调度」阈值（5h / 7d 两档，百分比）。
    /// 每档 `None` = 跟随全局、`Some(0)` = 本账号这一档不停、`Some(1..=100)` = 独立阈值；
    /// 取值夹到 `0..=100`。生效值见 [`effective_quota_pause_pct`]，判定在
    /// `crate::proxy::park_if_quota_nearly_exhausted`——下一条带限流头的响应起生效。
    pub fn set_quota_pause_pcts(
        &self,
        id: i64,
        short_pct: Option<i64>,
        long_pct: Option<i64>,
    ) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET quota_pause_pct = ?2, quota_pause_pct_7d = ?3, \
             updated_at = unixepoch() WHERE id = ?1",
            params![id, short_pct.map(|p| p.clamp(0, 100)), long_pct.map(|p| p.clamp(0, 100))],
        )
    }

    /// 更新账号等级。
    /// 写回组织类型（`claude_team` 等）。与 [`Self::set_tier`] 分开：等级会随额度档变，
    /// 组织类型只在换账号时才变，两者的来源虽同是 profile，语义不是一回事。
    pub fn set_org_type(&self, id: i64, org_type: Option<&str>) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "UPDATE credentials SET org_type = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, org_type],
        )? > 0)
    }

    /// 写回额度档原值（`default_claude_max_5x` 之类）。
    /// 见 [`crate::credentials::Credential::rate_limit_tier`]。
    pub fn set_rate_limit_tier(&self, id: i64, raw: Option<&str>) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "UPDATE credentials SET rate_limit_tier = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, raw],
        )? > 0)
    }

    /// 把一份刚拉到的 profile 写回凭证：等级、账号 UUID、组织类型、额度档原值、组织 UUID、
    /// 订阅创建时刻。**每一项只在 profile 给了值时才写**——profile 缺项不能把库里已有的清掉。
    /// `fallback_org_uuid` 是 profile 没给组织 id 时的兜底（交换响应里那个），官方同一次序。
    ///
    /// 三条路共用：登录、手动刷新、**自动刷新**（[`ensure_fresh_token`]）。此前只有前两条写
    /// profile 字段，自动刷新只换 token，于是「旧库刷新一次即回填」对绝大多数号——它们只会
    /// 被自动刷新——根本不成立。
    pub fn apply_profile(
        &self,
        id: i64,
        profile: &crate::oauth::Profile,
        fallback_org_uuid: Option<&str>,
    ) -> Result<()> {
        if profile.tier.is_some() {
            self.set_tier(id, profile.tier.as_deref())?;
        }
        if let Some(uuid) = profile.account_uuid.as_deref() {
            self.set_account_uuid(id, uuid)?;
        }
        if profile.org_type.is_some() {
            self.set_org_type(id, profile.org_type.as_deref())?;
        }
        if profile.rate_limit_tier.is_some() {
            self.set_rate_limit_tier(id, profile.rate_limit_tier.as_deref())?;
        }
        let org_uuid = profile.org_uuid.as_deref().or(fallback_org_uuid);
        if org_uuid.is_some() {
            self.set_org_uuid(id, org_uuid)?;
        }
        if profile.subscription_created_at.is_some() {
            self.set_subscription_created_at(id, profile.subscription_created_at.as_deref())?;
        }
        // 只给后台看的几列：拉到就整组覆盖（席位档个人号本来就没有，缺了要写回空）。
        // 至少拿到组织名称才算这一组有效，免得一份残缺的响应把已有的值清掉。
        if profile.org_name.is_some() {
            self.conn.lock().execute(
                "UPDATE credentials SET org_name = ?2, seat_tier = ?3, subscription_status = ?4,
                        extra_usage_enabled = ?5, updated_at = unixepoch()
                  WHERE id = ?1",
                params![
                    id,
                    profile.org_name,
                    profile.seat_tier,
                    profile.subscription_status,
                    profile.extra_usage_enabled.map(i64::from),
                ],
            )?;
        }
        Ok(())
    }

    /// 写回组织 UUID。见 [`crate::credentials::Credential::org_uuid`]。
    pub fn set_org_uuid(&self, id: i64, org_uuid: Option<&str>) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "UPDATE credentials SET org_uuid = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, org_uuid],
        )? > 0)
    }

    /// 写回订阅创建时刻原串。见 [`crate::credentials::Credential::subscription_created_at`]。
    pub fn set_subscription_created_at(&self, id: i64, raw: Option<&str>) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "UPDATE credentials SET subscription_created_at = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, raw],
        )? > 0)
    }

    /// 写回账号等级。**等级变了就把它的模型准入记录全清掉**：那些记录是在旧套餐下学到的
    /// （Pro 号不含 fable），升级到 Max 后再留着就等于把新买的额度锁在门外。
    pub fn set_tier(&self, id: i64, tier: Option<&str>) -> Result<bool> {
        let conn = self.conn.lock();
        let previous: Option<Option<String>> = conn
            .query_row("SELECT tier FROM credentials WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        let Some(previous) = previous else { return Ok(false) };
        if previous.as_deref() != tier {
            conn.execute("DELETE FROM model_denials WHERE cred_id = ?1", [id])?;
        }
        Ok(conn.execute(
            "UPDATE credentials SET tier = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, tier],
        )? > 0)
    }

    /// 回填账号 UUID（旧库凭证登录时未存、刷新 token 时补上）。仅在非空时覆盖。
    pub fn set_account_uuid(&self, id: i64, account_uuid: &str) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET account_uuid = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, account_uuid],
        )
    }

    /// 重命名（设置显示名）。
    /// 设置/清除该凭证的专用出站代理。`None` 或空串写成 NULL（直连）。
    ///
    /// 入参必须是 [`crate::clients::validate_proxy`] 校验过的串——这里只负责存，
    /// 校验放在入库之前那一层，免得存进去一条建不出客户端的代理，等到下次真有请求
    /// 选中这个号才炸。
    pub fn set_proxy(&self, id: i64, proxy: Option<&str>) -> Result<bool> {
        let proxy = proxy.map(str::trim).filter(|s| !s.is_empty());
        self.update_one(
            "UPDATE credentials SET proxy = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, proxy],
        )
    }

    pub fn set_label(&self, id: i64, label: &str) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET label = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, label],
        )
    }

    /// 刷新后回写新的 token 三元组（单行 UPDATE）。
    pub fn update_tokens(
        &self,
        id: i64,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
    ) -> Result<bool> {
        self.update_one(
            "UPDATE credentials
                SET access_token = ?2, refresh_token = ?3, expires_at = ?4, updated_at = unixepoch()
              WHERE id = ?1",
            params![id, access_token, refresh_token, expires_at as i64],
        )
    }
}
