//! 设备 / 会话绑定的查询、解绑与过期清理。

use super::*;

impl CredentialStore {
    /// 这台设备是否已有绑定记录（任一凭证、不论是否仍在 TTL 内）。有记录就是「见过的设备」；
    /// 绑定被保留期清掉后它会再次被当成没见过，那时它也确实很久没来了。供探针判定
    /// （`proxy::probe_signature`）区分「老设备的一条小请求」与「凭空冒出来的一次性会话」。
    pub fn device_is_known(&self, device_id: &str) -> bool {
        self.conn
            .lock()
            .query_row(
                "SELECT 1 FROM device_bindings WHERE device_id = ?1 LIMIT 1",
                [device_id],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// 单条凭证当前**占名额**的设备数：已排除超过 TTL 未活跃的绑定（与选路时判上限的口径
    /// 一致），故后台显示会随时间自然回落，不必等下一次请求触发 sweep。
    /// TTL `<= 0`（永不过期）时按全量计。
    ///
    /// 数不到休眠中的软绑定是有意的：它们不占名额，只是还记着「这台设备上次用的是这个号」
    /// （见 [`Self::select_for_device`]），列进来会让「设备 x/y」这个名额口径失真。
    pub fn device_count(&self, cred_id: i64) -> Result<i64> {
        let ttl = self.device_binding_ttl();
        let conn = self.conn.lock();
        let n = if ttl > 0 {
            conn.query_row(
                "SELECT COUNT(*) FROM device_bindings \
                 WHERE cred_id = ?1 AND last_seen_at >= unixepoch() - ?2",
                params![cred_id, ttl],
                |r| r.get(0),
            )?
        } else {
            conn.query_row(
                "SELECT COUNT(*) FROM device_bindings WHERE cred_id = ?1",
                [cred_id],
                |r| r.get(0),
            )?
        };
        Ok(n)
    }

    /// 单条凭证当前**有效**绑定的设备明细（含费用），按最近活跃倒序。
    ///
    /// **真实绑定**那部分的过滤口径与 [`Self::device_count`] 完全一致（同一个 TTL），否则后台
    /// 会出现「设备数写着 2、展开却列出 5 条」这种自相矛盾的展示。末尾追加的模拟伪设备
    /// （`simulated` 为真）**不在这个口径内**——它们不写绑定、不占名额，故 `device_count`
    /// 数不到它们，两者本就不该相等。前端要显示「设备数」时只能数 `!simulated` 那些。
    ///
    /// 费用来自 `device_costs` 账本（写日志时同事务累加），与绑定表是两套账，刻意不合并：
    /// 绑定行会被解绑/停用/TTL 清掉并从零重新计数，账本则终身累计。所以「本账号费用」
    /// 覆盖的时间范围可能比 `request_count` 长——它统计的是这台设备历史上经本账号花掉的钱，
    /// 而不是「本次绑定期间」。同时给出跨账号合计，便于识别换号仍在持续烧钱的同一台设备。
    pub fn list_devices(&self, cred_id: i64) -> Result<Vec<DeviceBinding>> {
        let ttl = self.device_binding_ttl();
        let conn = self.read_conn();
        let ttl_clause = if ttl > 0 { "AND b.last_seen_at >= unixepoch() - ?2" } else { "" };
        let sql = format!(
            "SELECT b.device_id, b.request_count, b.created_at, b.last_seen_at, \
                    COALESCE((SELECT dc.cost_usd FROM device_costs dc \
                               WHERE dc.cred_id = b.cred_id AND dc.device_id = b.device_id), 0), \
                    COALESCE((SELECT SUM(dc.cost_usd) FROM device_costs dc \
                               WHERE dc.device_id = b.device_id), 0) \
               FROM device_bindings b \
              WHERE b.cred_id = ?1 {ttl_clause} ORDER BY b.last_seen_at DESC, b.device_id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &Row| {
            Ok(DeviceBinding {
                device_id: r.get(0)?,
                request_count: r.get(1)?,
                created_at: r.get(2)?,
                last_seen_at: r.get(3)?,
                cost_usd: r.get(4)?,
                cost_usd_all: r.get(5)?,
                simulated: false,
            })
        };
        let mut rows: Vec<DeviceBinding> = if ttl > 0 {
            stmt.query_map(params![cred_id, ttl], map_row)?.collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map([cred_id], map_row)?.collect::<rusqlite::Result<_>>()?
        };
        drop(stmt);

        // 模拟客户端的伪设备：它们不写绑定（故上面那条 SQL 一条都查不到），但用量与费用
        // 照常落进 `device_costs`。不接 TTL——那是绑定的过期规则，这里没有绑定可过期。
        // 排在真实设备之后：真实设备是「谁在用这个号」的主线，伪设备是一条汇总。
        let mut sim = conn.prepare(
            "SELECT dc.device_id, dc.request_count, dc.cost_usd, \
                    COALESCE((SELECT SUM(d2.cost_usd) FROM device_costs d2 \
                               WHERE d2.device_id = dc.device_id), 0) \
               FROM device_costs dc \
              WHERE dc.cred_id = ?1 AND dc.device_id LIKE 'sim:%' \
              ORDER BY dc.request_count DESC, dc.device_id ASC",
        )?;
        let sim_rows = sim.query_map([cred_id], |r| {
            Ok(DeviceBinding {
                device_id: r.get(0)?,
                request_count: r.get(1)?,
                created_at: None,
                last_seen_at: None,
                cost_usd: r.get(2)?,
                cost_usd_all: r.get(3)?,
                simulated: true,
            })
        })?;
        for row in sim_rows {
            rows.push(row?);
        }
        Ok(rows)
    }

    /// 手动解除一条设备绑定，返回是否确有删除。
    ///
    /// 按 `(cred_id, device_id)` 双条件删除，而不是只按 `device_id`：后台拿到的设备列表可能
    /// 已经过期（设备刚被换到别的号上），只按 device_id 删会把它从**当前**所在账号上摘掉。
    ///
    /// 不受绑定 TTL 影响：TTL 外那些休眠的软绑定虽然不占名额，但还留着亲和性，解绑就是要把
    /// 这份记忆一并抹掉（下次来当新设备重新分号）。明细按 TTL 过滤，后台能点到的必然是活跃
    /// 绑定，休眠那些只能等保留期到点自己消失。
    pub fn unbind_device(&self, cred_id: i64, device_id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "DELETE FROM device_bindings WHERE cred_id = ?1 AND device_id = ?2",
            params![cred_id, device_id],
        )?;
        Ok(n > 0)
    }

    /// 所有凭证当前**有效**绑定的设备数（cred_id → count）；口径同 [`Self::device_count`]，
    /// 排除超过 TTL 未活跃的绑定。TTL `<= 0` 时按全量计。
    pub fn device_counts(&self) -> Result<HashMap<i64, i64>> {
        let ttl = self.device_binding_ttl();
        let conn = self.read_conn();
        let mut stmt = conn.prepare(&active_counts_sql("device_bindings", ttl))?;
        let map_row = |r: &Row| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?));
        let rows =
            if ttl > 0 { stmt.query_map([ttl], map_row)? } else { stmt.query_map([], map_row)? };
        let mut out = HashMap::new();
        for row in rows {
            let (cid, n) = row?;
            out.insert(cid, n);
        }
        Ok(out)
    }

    /// 单条凭证当前**占名额**的模拟会话数；口径同 [`Self::device_count`]（TTL 内活跃的绑定）。
    pub fn session_count(&self, cred_id: i64) -> Result<i64> {
        let ttl = self.session_binding_ttl();
        let conn = self.conn.lock();
        let n = if ttl > 0 {
            conn.query_row(
                "SELECT COUNT(*) FROM session_bindings \
                 WHERE cred_id = ?1 AND last_seen_at >= unixepoch() - ?2",
                params![cred_id, ttl],
                |r| r.get(0),
            )?
        } else {
            conn.query_row(
                "SELECT COUNT(*) FROM session_bindings WHERE cred_id = ?1",
                [cred_id],
                |r| r.get(0),
            )?
        };
        Ok(n)
    }

    /// 所有凭证当前**有效**的模拟会话绑定数（cred_id → count）；口径同 [`Self::session_count`]。
    pub fn session_counts(&self) -> Result<HashMap<i64, i64>> {
        let ttl = self.session_binding_ttl();
        let conn = self.read_conn();
        let mut stmt = conn.prepare(&active_counts_sql("session_bindings", ttl))?;
        let map_row = |r: &Row| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?));
        let rows =
            if ttl > 0 { stmt.query_map([ttl], map_row)? } else { stmt.query_map([], map_row)? };
        let mut out = HashMap::new();
        for row in rows {
            let (cid, n) = row?;
            out.insert(cid, n);
        }
        Ok(out)
    }

    /// 单条凭证当前**有效**的模拟会话明细，按最近活跃倒序；过滤口径与 [`Self::session_count`]
    /// 完全一致，否则后台会出现「会话数写着 2、展开却列出 5 条」。
    pub fn list_sessions(&self, cred_id: i64) -> Result<Vec<SessionBinding>> {
        let ttl = self.session_binding_ttl();
        let conn = self.read_conn();
        // 会话 id 要账号 uuid 才算得出；凭证不存在时按空 uuid 算（调用方已先判过 404）。
        let account_uuid: Option<String> = conn
            .query_row("SELECT account_uuid FROM credentials WHERE id = ?1", [cred_id], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        let ttl_clause = if ttl > 0 { "AND last_seen_at >= unixepoch() - ?2" } else { "" };
        let sql = format!(
            "SELECT session_key, slot, request_count, created_at, last_seen_at, last_model, device_id \
               FROM session_bindings \
              WHERE cred_id = ?1 {ttl_clause} ORDER BY last_seen_at DESC, session_key ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &Row| {
            let session_key: String = r.get(0)?;
            let slot: i64 = r.get(1)?;
            // 真实客户端的会话键带来访会话 id 时（`lb:v2:sid:<id>`）取最后一段按账号钉住；钉不出来
            // （号没有 account_uuid）时上游收到的就是来访原值。按前缀指纹分的（`…:pfx:…`）来访
            // 根本没带会话 id，出站也就没有，留空。
            let session_id = if slot < 0 {
                match session_key.rsplit_once(':') {
                    Some((head, sid)) if head.ends_with(":sid") => {
                        crate::credentials::pinned_session_id(account_uuid.as_deref(), sid)
                            .unwrap_or_else(|| sid.to_string())
                    }
                    _ => String::new(),
                }
            } else {
                crate::credentials::sim_slot_session_id(account_uuid.as_deref(), cred_id, slot)
            };
            Ok(SessionBinding {
                session_key,
                slot,
                session_id,
                request_count: r.get(2)?,
                created_at: r.get(3)?,
                last_seen_at: r.get(4)?,
                last_model: r.get(5)?,
                device_id: r.get(6)?,
            })
        };
        let rows = if ttl > 0 {
            stmt.query_map(params![cred_id, ttl], map_row)?.collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map([cred_id], map_row)?.collect::<rusqlite::Result<_>>()?
        };
        Ok(rows)
    }

    /// 这条会话键在该凭证上占的槽位；没绑在这个号上为 `None`。转发路径不再用它——槽位随
    /// 选号结果一起返回（[`Self::select_with_slot`]），这里只给测试核对绑定行。
    #[cfg(test)]
    pub fn session_slot(&self, cred_id: i64, session_key: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT slot FROM session_bindings WHERE session_key = ?1 AND cred_id = ?2",
                params![session_key, cred_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// 一键清掉该凭证的**全部**模拟会话绑定（含休眠的软绑定），返回删掉的条数。会话比设备
    /// 多得多、又是 luban 自己派生的键，逐条解绑不现实；清掉只是腾名额、抹亲和性，下一条请求
    /// 照常重新选号。
    pub fn unbind_all_sessions(&self, cred_id: i64) -> Result<usize> {
        let conn = self.conn.lock();
        log_removed(&conn, "unbound", Some("clear"), "cred_id = ?1", [cred_id])?;
        Ok(conn.execute("DELETE FROM session_bindings WHERE cred_id = ?1", [cred_id])?)
    }

    /// 手动解除一条模拟会话绑定，返回是否确有删除。按 `(cred_id, session_key)` 双条件删，
    /// 理由同 [`Self::unbind_device`]。
    pub fn unbind_session(&self, cred_id: i64, session_key: &str) -> Result<bool> {
        let conn = self.conn.lock();
        log_removed(
            &conn,
            "unbound",
            Some("manual"),
            "cred_id = ?1 AND session_key = ?2",
            params![cred_id, session_key],
        )?;
        let n = conn.execute(
            "DELETE FROM session_bindings WHERE cred_id = ?1 AND session_key = ?2",
            params![cred_id, session_key],
        )?;
        Ok(n > 0)
    }
}

/// 一条设备绑定明细（凭证卡片展开「已绑定设备」时展示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceBinding {
    /// 客户端 `metadata.user_id` 里的原始 device_id（非伪装后的那个）。
    pub device_id: String,
    /// 该设备经此凭证转发过的累计请求数（终身，来自 `device_costs` 账本）。
    pub request_count: i64,
    /// 首次绑定到该凭证的时间（Unix 秒）。模拟客户端没有绑定行，故为 `None`。
    pub created_at: Option<i64>,
    /// 最近一次活跃时间（Unix 秒）；TTL 就是按它算的。模拟客户端不参与 TTL，故为 `None`。
    pub last_seen_at: Option<i64>,
    /// 是否是**模拟客户端**的伪设备（`sim:` 前缀，见 [`crate::proxy::sim_device_id`]）：
    /// 不写绑定、不占 [`Self::device_count`] 名额、不能解绑，只有用量与费用是真的。
    pub simulated: bool,
    /// 该设备经**本凭证**花掉的等价 API 费用（USD 合计，来自 `usage_logs`）。
    ///
    /// 与 `request_count` 不同源：绑定行会被解绑/停用清掉并从零重数，用量日志不会，
    /// 所以这个数覆盖的时间范围可能比 `request_count` 更长。
    pub cost_usd: f64,
    /// 该设备在**所有凭证**上的累计费用（USD）；用来看清换号后仍在烧钱的同一台设备。
    pub cost_usd_all: f64,
}

/// 一条**模拟会话**绑定（`session_bindings` 的一行），供后台列表用。口径与
/// [`CredentialStore::session_count`] 一致：只含 TTL 内仍活跃的。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionBinding {
    /// 会话键：`lb:v2:sid:<来访自带的会话 id>` 或 `lb:v2:pfx:<缓存前缀加首条用户消息的指纹>`，
    /// 见 `crate::proxy::session_binding_key` 与 `Select::session_key`。
    pub session_key: String,
    /// 在该凭证上占的槽位（0 起）；会话 id 由它派生，释放后被下一个对话复用。
    /// [`PASSTHROUGH_SLOT`]（`-1`）表示真实客户端的会话：沿用来访自己的会话 id，不占槽位。
    pub slot: i64,
    /// 上游看到的会话 id（`X-Claude-Code-Session-Id`）。模拟会话按「账号 + 槽位」派生
    /// （`crate::credentials::sim_slot_session_id`），真实客户端的会话是来访 id 按账号钉住后的值
    /// （`crate::credentials::pinned_session_id`），都与转发路径同一个函数。真实客户端没带会话 id
    /// （按前缀指纹分会话）时为空串：出站也没有。
    pub session_id: String,
    /// 绑定之后再命中的请求数（建行那一轮不计，口径同 `device_bindings.request_count`；
    /// 随绑定行走，解绑即归零）。
    pub request_count: i64,
    /// 首次绑定到该凭证的时间（Unix 秒）。
    pub created_at: i64,
    /// 最近一次活跃时间（Unix 秒）；TTL 按它算。
    pub last_seen_at: i64,
    /// 最近一轮请求的模型；**不参与键**（理由见 `crate::proxy::session_binding_key`），只用来
    /// 在后台一眼看出这条会话在跑什么。旧库补列出来是 `None`，下次命中即回填。
    pub last_model: Option<String>,
    /// 来访设备 ID（客户端 `metadata.user_id` 里的原始 device_id）。模拟路径没有设备身份
    /// 时为 `None`；旧绑定补列出来也是 `None`，下次命中该绑定即回填。
    pub device_id: Option<String>,
}

impl CredentialStore {
    /// 删掉「连保留期都过了」的设备绑定与会话绑定，返回两张表各删了多少行。
    ///
    /// 这件事此前挂在 [`Self::select_with_slot`] 里、每条转发请求跑一遍，而两条 DELETE 都按
    /// `last_seen_at` 划线——那一列当时没有索引，于是每请求两次全表扫加两次写事务，全程还
    /// 压着那把全局 `conn` 锁。现在索引补上了（`idx_*_bindings_seen`），清理本身也挪到了
    /// 后台定时（见 `web::run`）。
    ///
    /// **清理时机不影响选号**：过了保留期、还没被删掉的行在
    /// [`Self::select_with_slot`] 那侧已被显式滤掉（`bound` 查询上那道保留期条件），
    /// 故「还在表里」与「已经删了」对选号是同一个结果，这里跑得早跑得晚都一样。
    ///
    /// TTL 与保留期都取当前配置：两项都可以在后台改，改小之后下一次清理就按新值收。
    /// 任一项配成 `<= 0`（永不过期／永久保留）时那张表整张不动，见 [`effective_retention`]。
    pub fn prune_expired_bindings(&self) -> Result<(usize, usize)> {
        // 两个 getter 内部各自取锁，parking_lot 不可重入，故都在拿 `conn` 之前读完。
        let device =
            effective_retention(self.device_binding_ttl(), self.device_binding_retention());
        let session =
            effective_retention(self.session_binding_ttl(), self.session_binding_retention());
        let conn = self.conn.lock();
        let devices = match device {
            Some(secs) => conn.execute(
                "DELETE FROM device_bindings WHERE last_seen_at < unixepoch() - ?1",
                [secs],
            )?,
            None => 0,
        };
        let sessions = match session {
            Some(secs) => {
                // 记事件与删行共用同一个截止时刻：两条语句各算一次 unixepoch()，跨秒时会删掉
                // 没记 expired 的行。
                let cutoff: i64 =
                    conn.query_row("SELECT unixepoch() - ?1", [secs], |r| r.get(0))?;
                log_removed(&conn, "expired", None, "last_seen_at < ?1", [cutoff])?;
                conn.execute("DELETE FROM session_bindings WHERE last_seen_at < ?1", [cutoff])?
            }
            None => 0,
        };
        // 会话历史事件保留 7 天，与绑定的保留期无关。
        conn.execute(
            "DELETE FROM session_binding_events WHERE ts < unixepoch() - ?1",
            [SESSION_EVENT_RETENTION_SECS],
        )?;
        Ok((devices, sessions))
    }
}

/// 按凭证数「TTL 内活跃」绑定的 SQL（`table` 是 `device_bindings` 或 `session_bindings`，
/// 只由代码里的常量传入）。TTL `<= 0` 时不过滤、按全量计。选号与后台列表共用，口径才一致。
///
/// **`GROUP BY +cred_id` 的一元加号不能删**：它让 SQLite 不再拿 `(cred_id)` 索引来分组。
/// 不加时，没跑过 ANALYZE 的库（luban 从不跑）会选择「按 cred 索引走全表、逐行回表判
/// last_seen_at」，代价随保留期内的**总行数**线性涨——会话表攒到 3 万行时单次 5–12ms，
/// 而且是在选号那把全局锁里、每条转发请求一次。加号之后改走 `last_seen_at` 索引，只扫
/// TTL 内的那一小段（同样 3 万行约 80µs）。不过滤时没有范围可走，保留原样让它扫索引。
pub(super) fn active_counts_sql(table: &str, ttl_secs: i64) -> String {
    if ttl_secs > 0 {
        format!(
            "SELECT cred_id, COUNT(*) FROM {table} \
             WHERE last_seen_at >= unixepoch() - ?1 GROUP BY +cred_id"
        )
    } else {
        format!("SELECT cred_id, COUNT(*) FROM {table} GROUP BY cred_id")
    }
}
