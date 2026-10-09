//! 选号：按设备 / 会话粘性挑凭证、占名额，以及名额耗尽时的错误。

use super::*;

/// 硬性设备上限触发：所有启用凭证的设备名额均已占满。
///
/// 通过 `anyhow` 向上传递，代理层 `downcast` 后映射为 HTTP 429。
#[derive(Debug)]
pub struct DeviceLimitReached;

impl std::fmt::Display for DeviceLimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "all credentials have reached their device limits; no slot is available")
    }
}

impl std::error::Error for DeviceLimitReached {}

/// 硬性**模拟会话**上限触发：所有启用凭证的会话名额均已占满。
///
/// 与 [`DeviceLimitReached`] 是同一件事的另一个粒度：设备上限管带设备身份的来访，这个管
/// **模拟路径上没有设备身份**的来访——它们按会话键（[`Select::session_key`]）粘住账号并占
/// 名额。同样经 `anyhow` 上传、代理层映射为 429。
#[derive(Debug)]
pub struct SessionLimitReached;

impl std::fmt::Display for SessionLimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "all credentials have reached their session limits; no slot is available")
    }
}

impl std::error::Error for SessionLimitReached {}

/// 请求的模型在**所有**可调度的号上都已被上游判成「套餐不含」（见
/// [`CredentialStore::deny_model`]）：不是限流、等多久都没用，换台机器也没用。
///
/// 同 [`DeviceLimitReached`] 走 `anyhow` 上传，代理层 `downcast` 后映射为 403
/// `permission_error`——照上游拒绝一个没权限模型时的口径回，客户端能一眼看懂「换模型」。
#[derive(Debug)]
pub struct ModelUnsupported {
    pub model: String,
    /// 有几个启用中的号被判过不支持它（即被排除掉的那些）。
    pub accounts: usize,
}

impl std::fmt::Display for ModelUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "none of the {} enabled account(s) can use model {}: upstream reported that their plans do not include it (add a Max account, or clear the block from the console after enabling extra usage)",
            self.accounts, self.model
        )
    }
}

impl std::error::Error for ModelUnsupported {}

impl CredentialStore {
    /// 限流暂停中、且**不在** `denied` 里的号里最早的 `resume_at`；没有这样的号则 `None`。
    /// 选号时用，调用方已持锁。
    fn soonest_paused_resume(conn: &Connection, denied: &HashSet<i64>) -> Result<Option<i64>> {
        let mut stmt = conn.prepare(
            "SELECT id, resume_at FROM credentials WHERE disabled = 1 AND resume_at IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut soonest: Option<i64> = None;
        for row in rows {
            let (id, at) = row?;
            if !denied.contains(&id) {
                soonest = Some(soonest.map_or(at, |s| s.min(at)));
            }
        }
        Ok(soonest)
    }

    /// 被判过不支持 `model` 的号（已到期的不算）。选号时用，调用方已持锁。
    fn denied_creds_for(conn: &Connection, model: &str) -> Result<HashSet<i64>> {
        let mut stmt = conn.prepare(
            "SELECT cred_id FROM model_denials WHERE model = ?1                AND (expires_at IS NULL OR expires_at > unixepoch())",
        )?;
        let rows = stmt.query_map([model_denial_key(model)], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }
}

/// 写一条设备绑定（新建或改绑到 `cred_id`）。按设备占名额的选号与按会话占名额时的设备亲和
/// 记录（[`Select::per_session`]）共用这一份。
///
/// 过了保留期、后台还没来得及删的那一行会在这里被撞上（此前它已被删掉，走的是纯 INSERT）。
/// 那是一条**新**绑定，故 created_at 与 request_count 归零重来——否则设备明细里会显示一个
/// 几天前建立、请求数接着往上加的绑定，而那台设备其实刚被重新调度过。`SET` 右侧读的都是
/// 冲突前那一行的值（SQLite 语义），故 CASE 里的 last_seen_at 是旧值，与放在哪一行无关。
fn upsert_device_binding(
    conn: &Connection,
    device_id: &str,
    cred_id: i64,
    retention_secs: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO device_bindings (device_id, cred_id) VALUES (?1, ?2)
         ON CONFLICT(device_id) DO UPDATE
            SET cred_id = ?2, last_seen_at = unixepoch(), \
                created_at = CASE WHEN ?3 > 0 \
                                    AND last_seen_at < unixepoch() - ?3 \
                                  THEN unixepoch() ELSE created_at END, \
                request_count = CASE WHEN ?3 > 0 \
                                       AND last_seen_at < unixepoch() - ?3 \
                                     THEN 1 ELSE request_count + 1 END",
        params![device_id, cred_id, retention_secs],
    )?;
    Ok(())
}

/// 沿用来访会话 id、不派生的会话绑定（[`Select::passthrough_session`]）在 `slot` 列记的值。
/// 不占槽位：[`free_session_slot`] 只从 0 起找空位，负数永远不与之相撞。
pub const PASSTHROUGH_SLOT: i64 = -1;

/// 该凭证当前**空着**的最小会话槽位：活跃（TTL 内）绑定占着的槽位之外，从 0 起最小的那个；
/// `prefer` 给出的槽位空着就直接用它（休眠的软绑定回来优先回原槽位，会话 id 才不换）。
/// 休眠绑定占过的槽位算空——它们不占名额，槽位（也就是会话 id）让给活跃的对话复用。
fn free_session_slot(
    conn: &Connection,
    cred_id: i64,
    ttl_secs: i64,
    prefer: Option<i64>,
) -> Result<i64> {
    let active = if ttl_secs > 0 { "AND last_seen_at >= unixepoch() - ?2" } else { "" };
    let mut stmt = conn.prepare(&format!(
        "SELECT slot FROM session_bindings WHERE cred_id = ?1 {active} ORDER BY slot ASC"
    ))?;
    let taken: Vec<i64> = if ttl_secs > 0 {
        stmt.query_map(params![cred_id, ttl_secs], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    } else {
        stmt.query_map([cred_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    if let Some(p) = prefer
        && p >= 0
        && !taken.contains(&p)
    {
        return Ok(p);
    }
    let mut slot = 0i64;
    for t in taken {
        if t > slot {
            break;
        }
        if t == slot {
            slot += 1;
        }
    }
    Ok(slot)
}

/// [`CredentialStore::select_for_device`] 里这条请求按哪张表粘住账号、占哪种名额。
/// 两张表列名不同、上限字段不同、拒绝的错误类型不同，其余规则逐条相同。
#[derive(Clone, Copy)]
enum Binding<'a> {
    /// 客户端自带的设备身份，`device_bindings`。
    Device(&'a str),
    /// 模拟路径上没有设备身份的来访，按会话键，`session_bindings`。见 [`Select::session_key`]。
    Session(&'a str),
}

impl<'a> Binding<'a> {
    fn table(self) -> &'static str {
        match self {
            Self::Device(_) => "device_bindings",
            Self::Session(_) => "session_bindings",
        }
    }

    fn column(self) -> &'static str {
        match self {
            Self::Device(_) => "device_id",
            Self::Session(_) => "session_key",
        }
    }

    fn key(self) -> &'a str {
        match self {
            Self::Device(k) | Self::Session(k) => k,
        }
    }

    /// 所有可调度的号名额都满了时的拒绝理由。
    fn limit_error(self) -> anyhow::Error {
        match self {
            Self::Device(_) => DeviceLimitReached.into(),
            Self::Session(_) => SessionLimitReached.into(),
        }
    }
}

/// [`CredentialStore::select_for_device`] 的入参。
///
/// 做成结构体而不是一串位置参数：两个 `Option<&str>`（`device_id` 与 `model`）挨在一起，
/// 位置传参写反了照样编译得过，而那是一个「设备粘性按模型名走」的静默错误。
#[derive(Default, Clone, Copy)]
pub struct Select<'a> {
    /// 客户端设备标识；`None` 即裸请求（不绑定、不占设备名额）。
    pub device_id: Option<&'a str>,
    /// **模拟会话键**：来访走模拟路径且没有设备身份时，代理算出来的这条会话的键，形如
    /// `lb:v2:sid:<来访自带的会话 id>` 或 `lb:v2:pfx:<缓存前缀加首条用户消息的指纹>`——命名空间
    /// 加口径版本加来源段，取法见 `crate::proxy::session_binding_key`。
    /// `Some` 且 `device_id` 为 `None` 时按它粘住账号并占该账号
    /// 的**会话名额**（`session_bindings`，上限 `session_limit` / [`DEFAULT_SESSION_LIMIT`]），
    /// 规则与设备绑定逐条相同（TTL、软绑定、改绑、全满时拒——[`SessionLimitReached`]）。
    /// `device_id` 有值时只在 [`Self::per_session`] 下才用它，否则按设备绑定，一条请求不占两份
    /// 名额。非模拟路径只有 `per_session` 时才有值（真实客户端自带的会话 id，没带时按前缀指纹）。
    ///
    /// 绑定行还记着这条会话在该号上占的**槽位**（[`free_session_slot`]）：出站会话 id 由
    /// 「账号 + 槽位」派生，槽位释放后下一个对话复用同一个 id，见 [`CredentialStore::session_slot`]。
    pub session_key: Option<&'a str>,
    /// 带设备身份的来访也**按会话**占名额（[`Self::session_key`] 那张表与 `session_limit`），
    /// 设备上限不再生效。代理在「上游看到的设备身份已经收敛」时置真：设备指纹归一化开着、
    /// 出站 device_id 由「账号 + 平台 + 客户端版本」派生，同一个号在上游只呈现寥寥几台设备，
    /// 绑了几台真实机器上游根本看不见，看得见的是每台设备下同时活跃几条会话。
    ///
    /// 设备绑定行照写，但只当**亲和记录**用：同一台设备新开的会话优先落在它上次那个号上，
    /// 一个人的活不会被均衡到一圈号上去。置真而 `session_key` 为 `None`（额度探测那类不该占
    /// 名额的请求）时只按亲和选号，不写会话绑定、不受任何名额约束。
    pub per_session: bool,
    /// 这条会话出站沿用来访自己的会话 id（真实客户端，不走模拟），不分配派生用的槽位：
    /// 绑定行 `slot` 记 `-1`，后台列会话时据此按来访 id 算上游看到的那个。
    pub passthrough_session: bool,
    /// 匿名侧查询（标题、分类、探测这类一次性请求，见 `crate::proxy::session_plan`）：按
    /// [`Self::session_key`] 只**跟随**——那个键有活跃绑定、原号可调度就落到原号并沿用它的
    /// 槽位（出站会话 id 与主线程一致），否则当裸请求按负载选号。两种情况都**不写绑定、不占
    /// 会话名额**，原号 RPM 满了也不就地拒、直接分散（没有要续的 thinking 签名）。
    pub follow_only: bool,
    /// 设备绑定**占名额**的有效期（秒）；`<= 0` 表示永不过期。
    pub ttl_secs: i64,
    /// 软绑定保留期（秒）：绑定行超过 [`Self::ttl_secs`] 后不再占名额，但在这个时长内仍然
    /// 留着，设备回来时优先回原号。`<= 0` 表示永久保留（只要不被解绑/停号就一直在）。
    pub retention_secs: i64,
    /// 模拟会话绑定的有效期与保留期，语义同上面两项，只是作用在 `session_bindings` 上、
    /// 单独配置（[`SESSION_BINDING_TTL`] / [`SESSION_BINDING_RETENTION`]）。
    pub session_ttl_secs: i64,
    pub session_retention_secs: i64,
    /// 本次请求是否计入裸请求速率上限（只有真正消耗额度的路径才该计，见
    /// `crate::proxy::is_billable_messages`）。
    pub rate_limited: bool,
    /// 本次请求已经试过的凭证（上游 429 换号重试时传入），一律出局。
    pub exclude: &'a [i64],
    /// 请求的模型名，用于按模型判定冷却（fable 那类模型级 429 不该拖累整个账号）。
    pub model: Option<&'a str>,
}

impl CredentialStore {
    /// 按 device_id 做粘性选择，返回选中的凭证（刷新在锁外由调用方处理）。
    ///
    /// 规则：
    /// 1. TTL 内的绑定（**活跃**）且该凭证仍启用 → 复用（更新 last_seen / request_count），
    ///    已占名额的设备不再受上限约束。
    /// 2. TTL 外但仍在保留期内的绑定（**软绑定**）→ 仍优先回原号，但要重新占名额：
    ///    原号必须仍启用、不在冷却、未被本轮排除，且还有空位；不满足就当新设备重选并**改绑**。
    /// 3. 绑定的凭证已停用或删除 → 作为新设备重新选择（选中谁就改绑到谁）。
    /// 4. 新设备 → 在仍有名额的启用凭证中做负载均衡：选“当前设备数最少”者并绑定；
    ///    同数时按 (priority, id) 决定，保持确定性。
    /// 5. 所有启用凭证均达设备上限 → 硬性拒绝，返回 [`DeviceLimitReached`]（代理映射为 429）。
    ///
    /// 被上游 429 打过冷却的号（见 [`RateLimitCooldown`]）在**任何**分支之前就被剔出候选，
    /// 包括已有绑定命中那一支——绑定的号在冷却中会被解绑并改选到别的号上。冷却是硬门禁：
    /// 候选被冷却清空时返回 [`AllRateLimited`]（代理映射为 429 + `retry-after`），
    /// 不再退回「忽略冷却照常选」。
    ///
    /// `device_id` 为 `None`（请求未带 metadata）时无从绑定/计数：退化为负载均衡挑选，
    /// 不写绑定、也不受**设备**上限约束——但在 `rate_limited` 为真时受**裸请求速率上限**
    /// 约束（见 [`Self::bare_rate_limit`]）：已发满的凭证在本轮被跳过，自然分流到其它号；
    /// 所有号都满才返回 [`BareRateLimited`]（代理映射为 429 + `retry-after`）。
    ///
    /// **例外是带 `session_key` 的**（模拟路径、没有设备身份，见 [`Select::session_key`]）：
    /// 它们按会话键走与设备绑定**逐条相同**的规则——`session_bindings` 表、`session_limit` /
    /// [`DEFAULT_SESSION_LIMIT`] 上限、同一套 TTL 与保留期、同样的软绑定与改绑，全满时返回
    /// [`SessionLimitReached`]。与设备绑定只差一处：它们仍是裸请求，裸请求速率上限照旧管着
    /// （命中的原号裸窗口打满时当作没位置、往下改选）。
    ///
    /// **账号 RPM 上限**（见 [`Self::default_rpm_limit`]）是所有分支共同的最后一道门，
    /// 且两个分支的行为**故意不同**：
    ///
    /// - 还没定下号的（新设备、裸请求、原号不可用要改选）→ 打满的号在本轮被跳过，
    ///   自然分流到别的号，全部打满才返回 [`RpmLimited`]；
    /// - **已经粘在某个号上的**（命中既有绑定）→ 该号打满就**直接拒**，不改选别的号。
    ///   换号意味着把设备改绑过去，而 thinking 块的签名是跟着账号走的，这条会话之后每一轮
    ///   都要先撞一次 400 再降级重发（见 `crate::proxy::retry_demoted_thinking`）；
    ///   让客户端照 `retry-after` 退避几秒，等这个号的窗口滚出名额，会话就还在原来的号上。
    ///
    /// 与裸请求上限不同，RPM **不看 `rate_limited`，每一次选号都计**：口径要和账号列表里
    /// 那个「当前 RPM」对得上（那是 `usage_logs` 最近 60 秒的条数，`count_tokens` 一样在内），
    /// 否则会出现「上限 30、显示 45」这种解释不清的画面。
    ///
    /// `rate_limited` 由调用方判定——代理只对**真正消耗额度的**路径置真
    /// （`/v1/messages`，见 `crate::proxy::is_billable_messages`）。`count_tokens` 这类
    /// 既不产生 usage、也不消耗额度的路径不计：拿它占名额只会把真正的请求挤掉，
    /// 而客户端的 `/context` 显示与压缩前预估全靠它。
    ///
    /// `ttl_secs > 0` 时超时未活跃的绑定**不再占名额**（惰性过期），但绑定行本身留到保留期
    /// （`retention_secs`）满才删——这就是「软绑定」：设备隔了几小时再来，只要原号还有空位就
    /// 回原号。thinking 块的签名是跟着账号走的，中途换号会让这条会话之后每一轮都先撞一次 400
    /// 再降级重发（见 `crate::proxy::retry_demoted_thinking`），软绑定就是为了少踩这个。
    /// `ttl_secs <= 0` 表示绑定永不过期，此时保留期无从谈起（不删任何行）。
    /// 全部操作在单次持锁内完成，避免与其它写入竞态。
    ///
    /// **限流按「选一次号」计，不是按「客户端请求」计**：刷新失败换号那条路
    /// （[`select_with_refresh_failover`]）每轮都会重选，故一次客户端请求最多可能扣掉几个
    /// 名额。那条路只在凭证被上游作废时才走（罕见），宁可多扣也好过给它开一个绕过限流的口子。
    ///
    /// 反过来，**不经选号的那些请求一条都不计**：连通性测试指定打哪个号（不走这里），却照样
    /// 写 `usage_logs`。所以列表里的 RPM 可能比限流器数到的略高一点点——探活是人手点出来的，
    /// 量级上不构成干扰，但对不上时要知道差在哪。
    #[cfg(test)]
    pub fn select_for_device(&self, sel: Select<'_>) -> Result<Credential> {
        self.select_with_slot(sel).map(|(cred, _)| cred)
    }

    /// [`Self::select_for_device`] 的完整版：连同这条请求在选中的号上占的**会话槽位**一起返回
    /// （按会话键绑定时为 `Some`，其余 `None`）。转发路径要用槽位派生会话 id，选号时刚写过
    /// 绑定行、值就在手上，不必再按键查一遍。
    pub fn select_with_slot(&self, sel: Select<'_>) -> Result<(Credential, Option<i64>)> {
        let Select {
            device_id,
            session_key,
            per_session,
            passthrough_session,
            follow_only,
            ttl_secs,
            retention_secs,
            session_ttl_secs,
            session_retention_secs,
            rate_limited,
            exclude,
            model,
        } = sel;
        // 这条请求按什么粘住账号、占哪种名额：有设备身份按设备（`per_session` 时改按会话）；
        // 没有设备身份但带会话键（模拟路径）按会话；都没有就是裸请求。一条请求只占一份名额。
        // `per_session` 而没有会话键的（额度探测）不绑定，只按下面的设备亲和选号。
        let binding = match (device_id, session_key) {
            (Some(d), _) if !per_session => Some(Binding::Device(d)),
            (_, Some(k)) => Some(Binding::Session(k)),
            (_, None) => None,
        };
        // 按会话占名额的带设备来访，设备绑定行只当亲和记录：选号时优先它指的那个号，选完改写
        // 成这次落的号（见 [`Select::per_session`]）。
        let affinity_device = device_id.filter(|_| per_session);
        // 这几项须在取锁前读（内部自己会取锁，parking_lot 不可重入）。
        let default_limit = self.default_device_limit();
        let default_session_limit = self.default_session_limit();
        let (rate_limit, rate_window) = (self.bare_rate_limit(), self.bare_rate_window_secs());
        let default_rpm = self.default_rpm_limit();
        let conn = self.conn.lock();

        // RPM 的窗口就是账号列表那一列的窗口（60 秒），两处共用同一个常量：限的和看到的
        // 必须是同一个口径，否则「上限 30」和列表里的「RPM 45」谁也解释不了谁。
        let rpm_window = Duration::from_secs(RPM_WINDOW_SECS as u64);
        let rpm_limit_of = |c: &Credential| effective_rpm_limit(c.rpm_limit, default_rpm);
        let rpm_room = |c: &Credential| self.rpm_rate.has_room(c.id, rpm_limit_of(c), rpm_window);
        // 全员打满时的 `retry-after`：取最早腾出名额的那个号——早一秒重试都是白撞。
        let rpm_full = |cands: &[&Credential]| -> anyhow::Error {
            let retry_after_secs = cands
                .iter()
                .map(|c| self.rpm_rate.retry_after_secs(&c.id, rpm_window))
                .min()
                .unwrap_or(1);
            RpmLimited { retry_after_secs, sticky: false }.into()
        };

        // 「连保留期都过了」的绑定行由后台定时清（[`Self::prune_expired_bindings`]，
        // 挂在 `web::run` 里），**不在这条路上删**：两条 DELETE 都按 last_seen_at 划线，
        // 而这里是每条转发请求都要走一遍的选号路径，等于每请求两次写事务。
        //
        // 清理时机与判定因此解耦：下面命中既有绑定那一步自己按保留期过滤（见 `bound` 的
        // 查询），所以「行还在但已过保留期」与「行已被删掉」对选号是同一个结果——后台
        // 什么时候跑都不影响这里选出谁。TTL 到点的那些照旧不删：它们从那一刻起就不占名额
        //（下面的 counts 按 TTL 过滤），但行还在，设备回来时还能循着它回原号。
        let device_retention = effective_retention(ttl_secs, retention_secs).unwrap_or(0);
        let session_retention =
            effective_retention(session_ttl_secs, session_retention_secs).unwrap_or(0);

        // 限流暂停到点的号先放回来，再挑——否则它们要等到有人打开控制台列表才回得了池子。
        Self::resume_due(&conn)?;

        // 启用凭证，按 (priority, id) 升序。
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM credentials WHERE disabled = 0 AND {OWNER_ACTIVE} \
             ORDER BY priority ASC, id ASC"
        ))?;
        let all: Vec<Credential> =
            stmt.query_map([], row_to_cred)?.collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        if all.is_empty() {
            // 一个能用的都没有。若其中有「限流暂停、还没到点」的，这不是配置问题而是限流：
            // 回 429 + 最早那个的恢复时刻，比一句「没有可用凭证，请先登录」诚实得多
            // （后者会把运维引去查登录，而实际上号都在、只是在等额度回血）。
            let soonest: Option<(i64, String, Option<String>, i64)> = conn
                .query_row(
                    "SELECT id, label, ban_reason, resume_at FROM credentials \
                      WHERE disabled = 1 AND resume_at IS NOT NULL \
                      ORDER BY resume_at ASC, id ASC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            if let Some((id, label, reason, at)) = soonest {
                let retry_after_secs = (at - crate::credentials::now_secs() as i64).max(1);
                let refresh_failed = reason
                    .as_deref()
                    .and_then(refresh_pause_detail)
                    .map(|detail| RefreshFailed::paused(id, label, detail));
                return Err(AllRateLimited { retry_after_secs, refresh_failed }.into());
            }
            anyhow::bail!("no available credentials; add an account first");
        }

        // 上游判过「套餐不含这个模型」的号先出局（见 [`Self::deny_model`]）：这不是等一会就好
        // 的事，也不该拿去撞。**所有启用号**都被判过才是「换模型」——这一判必须排在下面「排除
        // 已试过的号」之前：换号重试时刚被判的那个号就在 `exclude` 里，先排除再看会把它漏数，
        // 单个 Pro 号的池子永远报不出这条、只会把上游 429 原样透传。
        //
        // 但先看有没有**只是被限流暂停**（`disabled = 1` 且 `resume_at` 非空）且没被判过的号：
        // 那是「等一会」不是「换模型」——两小时后回来的 Max 号被说成不存在、客户端拿着 403 去
        // 换模型，比一发带恢复时刻的 429 糟得多。
        let denied: HashSet<i64> = match model {
            Some(m) => Self::denied_creds_for(&conn, m)?,
            None => HashSet::new(),
        };
        if let Some(m) = model
            && all.iter().all(|c| denied.contains(&c.id))
        {
            if let Some(at) = Self::soonest_paused_resume(&conn, &denied)? {
                let retry_after_secs = (at - crate::credentials::now_secs() as i64).max(1);
                return Err(AllRateLimited { retry_after_secs, refresh_failed: None }.into());
            }
            return Err(ModelUnsupported { model: m.to_string(), accounts: all.len() }.into());
        }
        // 本次请求已经试过的号（上游 429 换号重试时传进来）直接出局——重试再撞同一个号毫无意义。
        // 改绑事件要分清原号为什么回不去（见下面命中既有绑定那一步），停用/删除的号此后就不在
        // 池子里了，先记下启用的全集。
        let enabled: HashSet<i64> = all.iter().map(|c| c.id).collect();
        let mut pool = all;
        pool.retain(|c| !exclude.contains(&c.id) && !denied.contains(&c.id));
        if pool.is_empty() {
            anyhow::bail!(
                "no other available credentials remain after excluding those already tried"
            );
        }
        // 冷却中的号让位给还能用的；**全部都在冷却就直接拒**——冷却是硬门禁，被上游 429 过的
        // 号在解冻前一律不调度。等待时间取最早解冻的那个，客户端照它重试即可。
        let creds: Vec<Credential> =
            pool.iter().filter(|c| !self.cooldown.is_cooling(c.id, model)).cloned().collect();
        if creds.is_empty() {
            let retry_after_secs = pool
                .iter()
                .map(|c| self.cooldown.remaining_for(c.id, model))
                .min()
                .unwrap_or(0)
                .max(1);
            return Err(AllRateLimited { retry_after_secs, refresh_failed: None }.into());
        }

        // 裸请求速率上限：没有设备身份的都算裸请求，按会话键绑定的也是——那道上限限的是「没有
        // 设备身份可依据」的流量，会话键是 luban 自己从体里算的，不是客户端的身份。
        let bare_window = Duration::from_secs(rate_window.max(1) as u64);
        let bare_ok = |c: &Credential| {
            device_id.is_some()
                || !rate_limited
                || self.bare_rate.has_room(c.id, rate_limit, bare_window)
        };

        // 匿名侧查询先找主线程（[`Select::follow_only`]）：同一个会话键有活跃绑定、原号还能调度
        // 就跟过去，绑定行一个字不动；跟不上就当裸请求往下按负载选，同样不写绑定。
        if follow_only && let Some(Binding::Session(key)) = binding {
            let bound: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT cred_id, slot FROM session_bindings WHERE session_key = ?1 \
                        AND (?2 <= 0 OR last_seen_at >= unixepoch() - ?2)",
                    params![key, session_ttl_secs],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((cid, slot)) = bound
                && let Some(c) = creds.iter().find(|c| c.id == cid)
                && bare_ok(c)
                && rpm_room(c)
            {
                if let Some(did) = affinity_device {
                    upsert_device_binding(&conn, did, c.id, device_retention)?;
                }
                self.rpm_rate.take(c.id, rpm_limit_of(c), rpm_window);
                if device_id.is_none() && rate_limited {
                    self.bare_rate.take(c.id, rate_limit, bare_window);
                }
                return Ok((c.clone(), (slot >= 0).then_some(slot)));
            }
        }
        let binding = binding.filter(|_| !follow_only);

        // 各凭证当前**占名额**的设备数或模拟会话数：只数 TTL 内活跃的绑定，休眠的软绑定不占位
        // （口径与 [`Self::device_counts`] / [`Self::session_counts`] 一致，后台看到的数就是这里
        // 用来判上限的数）。只数本次绑定的那一种：名额与负载均衡都只看它，另一张表数了也不用。
        let active_counts = |table: &str, ttl_secs: i64| -> Result<HashMap<i64, i64>> {
            let mut cstmt = conn.prepare(&active_counts_sql(table, ttl_secs))?;
            let map_row = |r: &Row| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?));
            let rows = if ttl_secs > 0 {
                cstmt.query_map([ttl_secs], map_row)?
            } else {
                cstmt.query_map([], map_row)?
            };
            let mut counts = HashMap::new();
            for row in rows {
                let (cid, n) = row?;
                counts.insert(cid, n);
            }
            Ok(counts)
        };
        let counts = match binding {
            Some(Binding::Session(_)) => active_counts("session_bindings", session_ttl_secs)?,
            _ => active_counts("device_bindings", ttl_secs)?,
        };
        // 这条请求走哪张表，TTL 与保留期就都用哪张表的：下面判「绑定还在有效期内吗」与分槽位
        // 按 TTL，判「这条绑定还算不算数」按保留期。
        let (binding_ttl, binding_retention) = match binding {
            Some(Binding::Session(_)) => (session_ttl_secs, session_retention),
            _ => (ttl_secs, device_retention),
        };

        // 当前占名额的数（已排除 TTL 外的休眠绑定）：按会话绑定时数会话，其余数设备——裸请求
        // 不占名额，但负载均衡仍按设备数排，与原来一样。
        let used = |c: &Credential| counts.get(&c.id).copied().unwrap_or(0);
        // 生效上限：账号未单独配置（== 0）时套用对应的全局默认。
        let limit_of = |c: &Credential| match binding {
            Some(Binding::Session(_)) => {
                effective_session_limit(c.session_limit, default_session_limit)
            }
            _ => effective_device_limit(c.device_limit, default_limit),
        };
        // 还塞得下一台设备 / 一条会话吗（上限 <= 0 即不限）。
        let has_room = |c: &Credential| limit_of(c) <= 0 || used(c) < limit_of(c);

        // 1/2/3) 命中既有绑定。会话绑定回不去原号、往下改选时，记下原号与原因，改绑事件用。
        let mut rebind_from: Option<(i64, i64, &'static str)> = None;
        if let Some(b) = binding {
            // 第二列是「这条绑定还在 TTL 内吗」，交给 SQLite 与清理/计数用同一个 unixepoch()
            // 时钟判定，免得和进程时钟差出一个边界。
            //
            // `WHERE` 上那道保留期过滤是**清理挪去后台之后**补的：过了保留期的行在被后台删掉
            // 之前还留在表里，不滤掉的话它会被当成休眠软绑定续上，设备就回到了一个本该已经
            // 忘掉的号。滤掉之后，「行还在但过期了」与「行已删」对这里是同一个结果——后台多久
            // 跑一次都不改变选号结果。走的是主键点查，多一个条件不增加代价。
            let bound: Option<(i64, bool)> = conn
                .query_row(
                    &format!(
                        "SELECT cred_id, (?2 <= 0 OR last_seen_at >= unixepoch() - ?2) \
                           FROM {} WHERE {} = ?1 \
                            AND (?3 <= 0 OR last_seen_at >= unixepoch() - ?3)",
                        b.table(),
                        b.column()
                    ),
                    params![b.key(), binding_ttl, binding_retention],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((cid, active)) = bound {
                // 原号仍可调度（启用、不在冷却、本轮没试过）时才谈复用。
                if let Some(c) = creds.iter().find(|c| c.id == cid) {
                    // 活跃绑定本来就占着名额，直接续；休眠的软绑定要重新占一个位置，
                    // 原号满了就只能改选——否则设备上限形同虚设。按会话绑定的还要过裸请求
                    // 速率上限：原号的裸窗口打满了就当没位置，往下改选（设备绑定不受这道管）。
                    if (active || has_room(c)) && bare_ok(c) {
                        // RPM 打满 → **就地拒**，不往下走改选那条路（理由见本函数文档：
                        // 改选会改绑，而改绑会让这条会话每一轮先撞一次 thinking 签名 400）。
                        if !rpm_room(c) {
                            return Err(RpmLimited {
                                retry_after_secs: self.rpm_rate.retry_after_secs(&c.id, rpm_window),
                                sticky: true,
                            }
                            .into());
                        }
                        let slot = match b {
                            Binding::Session(key) => {
                                let old: i64 = conn.query_row(
                                    "SELECT slot FROM session_bindings WHERE session_key = ?1",
                                    [key],
                                    |r| r.get(0),
                                )?;
                                // 活跃绑定续用原槽位；休眠的会话软绑定回来要重新占槽位：原槽位
                                // 空着就还用它，被别的对话拿走了就取最小的空位——会话 id 随槽位变，
                                // 这条对话在上游成了另一条会话。沿用来访会话 id 的不占槽位（-1）；
                                // 原来记的是 -1 而这次要派生的，也得新取一个。
                                let slot = if passthrough_session {
                                    PASSTHROUGH_SLOT
                                } else if active && old >= 0 {
                                    old
                                } else {
                                    free_session_slot(&conn, c.id, session_ttl_secs, Some(old))?
                                };
                                // 休眠后回来记一条恢复（槽位变没变都记）；活跃中换了槽位（沿用
                                // 来访 ID 与派生槽位之间切换，0 ↔ -1）记一条换槽位。新拿的槽位若是
                                // 从别的休眠绑定手里接过来的，再记一对接手事件。
                                if !active || slot != old {
                                    log_event(
                                        &conn,
                                        NewEvent {
                                            key,
                                            event: if active { "reslotted" } else { "resumed" },
                                            cred_id: Some(c.id),
                                            prev_cred_id: Some(c.id),
                                            slot: Some(slot),
                                            prev_slot: Some(old),
                                            ..Default::default()
                                        },
                                    )?;
                                    note_slot_takeover(&conn, c.id, slot, key, session_ttl_secs)?;
                                }
                                conn.execute(
                                    "UPDATE session_bindings \
                                        SET slot = ?2, slot_lost = 0, last_seen_at = unixepoch(), \
                                            request_count = request_count + 1, \
                                            last_model = COALESCE(?3, last_model), \
                                            device_id = COALESCE(?4, device_id) \
                                      WHERE session_key = ?1",
                                    params![key, slot, model, device_id],
                                )?;
                                (slot >= 0).then_some(slot)
                            }
                            Binding::Device(did) => {
                                conn.execute(
                                    "UPDATE device_bindings \
                                        SET last_seen_at = unixepoch(), \
                                            request_count = request_count + 1 \
                                      WHERE device_id = ?1",
                                    [did],
                                )?;
                                None
                            }
                        };
                        if let Some(did) = affinity_device {
                            upsert_device_binding(&conn, did, c.id, device_retention)?;
                        }
                        self.rpm_rate.take(c.id, rpm_limit_of(c), rpm_window);
                        if device_id.is_none() && rate_limited {
                            self.bare_rate.take(c.id, rate_limit, bare_window);
                        }
                        return Ok((c.clone(), slot));
                    }
                }
                if let Binding::Session(key) = b {
                    // 判定顺序与上面过滤候选的顺序一致：停用/删除 → 模型不支持 → 本轮已试过 →
                    // 冷却 → 名额 → 裸请求速率。
                    let reason = match creds.iter().find(|c| c.id == cid) {
                        _ if !enabled.contains(&cid) => "disabled",
                        _ if denied.contains(&cid) => "model_denied",
                        _ if exclude.contains(&cid) => "retried",
                        None => "cooling",
                        Some(c) if !(active || has_room(c)) => "full",
                        Some(_) => "bare_limit",
                    };
                    let old: i64 = conn.query_row(
                        "SELECT slot FROM session_bindings WHERE session_key = ?1",
                        [key],
                        |r| r.get(0),
                    )?;
                    rebind_from = Some((cid, old, reason));
                }
                // 回不去原号（停用/删除/冷却中/本轮已试过/名额已满）：往下重新选择，
                // 选中谁就**改绑**到谁（`INSERT … ON CONFLICT DO UPDATE cred_id`）。
                // 冷却结束后这台设备不会自己回到原号——粘性以最后一次选择为准，
                // 这正是「429 换号重试要改绑」想要的语义。
            }
        }

        // 4/5) 优先级分档调度：优先级为主键（数值小者优先），同一档内再按设备数
        //      负载均衡，最后 id 兜底。低优先级档仅在高优先级档全部占满/不可用后才触及。
        // (priority, 设备数, id) 是唯一的排序口径；两个分支都从这一份有序表里挑，
        // 逐道门过滤，第一个全过的即中。
        // fable/mythos 这类只有高档套餐才含的模型，同一优先级档内 Max 号排前面，等级未知的
        // 与团队/企业席位（`Team Standard` 之类，含不含这些模型说不准）其次，Pro/Free 垫底。**只排序不剔除**：准入以上游的判决为准（见上面的 denied），
        // 这里猜错也只是多换一次号；而排在前面能让绝大多数请求第一发就落在能用的号上。
        let premium = model.is_some_and(premium_model);
        let plan_rank = |c: &Credential| -> u8 {
            if !premium {
                return 0;
            }
            match c.tier.as_deref() {
                Some(t) if t.starts_with("Max") => 0,
                None => 1,
                Some(t) if t.starts_with("Team") || t.starts_with("Enterprise") => 1,
                Some(_) => 2,
            }
        };
        let mut ordered: Vec<&Credential> = creds.iter().collect();
        ordered.sort_by_key(|c| (c.priority, plan_rank(c), used(c), c.id));
        // 设备亲和（按会话占名额的带设备来访，见 [`Select::per_session`]）：这台设备上次落的号
        // 提到最前，新会话跟着它走——一台机器的活集中在一个号上，才像一个真实用户。它照样要过
        // 下面的名额与 RPM 两道门，过不去就按原顺序溢出。premium 模型下不越过档次更高的号：
        // 亲和是软偏好，不该把一条 fable 请求从 Max 号拉到 Pro 号上去撞。
        if let Some(did) = affinity_device
            && let Some(home) = conn
                .query_row(
                    "SELECT cred_id FROM device_bindings WHERE device_id = ?1 \
                        AND (?2 <= 0 OR last_seen_at >= unixepoch() - ?2)",
                    params![did, device_retention],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            && let Some(pos) = ordered.iter().position(|c| c.id == home)
            && ordered.first().is_some_and(|f| plan_rank(ordered[pos]) <= plan_rank(f))
        {
            let c = ordered.remove(pos);
            ordered.insert(0, c);
        }
        let chosen = match binding {
            Some(b) => {
                // 硬限制：仅在仍有名额者（生效上限 <=0 不限，或 used<上限）中选；
                // 当前优先级档全满时其成员被过滤掉，自然溢出到下一档；全部满则拒绝。
                let with_room: Vec<&Credential> =
                    ordered.iter().copied().filter(|c| has_room(c)).collect();
                if with_room.is_empty() {
                    return Err(b.limit_error());
                }
                // 按会话绑定的还是裸请求，要过裸请求速率上限（设备绑定的 `bare_ok` 恒真）。
                let with_bare: Vec<&Credential> =
                    with_room.iter().copied().filter(|c| bare_ok(c)).collect();
                if with_bare.is_empty() {
                    return Err(BareRateLimited { retry_after_secs: rate_window }.into());
                }
                // 名额与 RPM 是两回事，故两道门分开判：都过不去时要能说清是哪一道拦的
                // ——名额满是「换台机器也没用」，RPM 满是「等几秒就好」。
                match with_bare.iter().copied().find(|c| rpm_room(c)) {
                    Some(c) => c,
                    None => return Err(rpm_full(&with_bare)),
                }
            }
            None => {
                // 无 device_id 也无会话键：不占名额，但要过裸请求速率上限。
                let with_bare: Vec<&Credential> =
                    ordered.iter().copied().filter(|c| bare_ok(c)).collect();
                if with_bare.is_empty() {
                    return Err(BareRateLimited { retry_after_secs: rate_window }.into());
                }
                match with_bare.iter().copied().find(|c| rpm_room(c)) {
                    Some(c) => c,
                    None => return Err(rpm_full(&with_bare)),
                }
            }
        };

        let slot = match binding {
            Some(Binding::Device(did)) => {
                upsert_device_binding(&conn, did, chosen.id, device_retention)?;
                None
            }
            Some(Binding::Session(key)) => {
                // 新对话（或改绑到别的号的对话）在选中的号上取最小的空槽位。上面刚判过
                // has_room，所以上限内必有空位；不限时槽位按需增长、释放后复用。沿用来访
                // 会话 id 的不派生、不占槽位。
                let slot = if passthrough_session {
                    PASSTHROUGH_SLOT
                } else {
                    free_session_slot(&conn, chosen.id, session_ttl_secs, None)?
                };
                // 原号回不去而改选的记改绑（带原号、原槽位与原因），其余是新建；拿到的槽位若是
                // 从休眠绑定手里接过来的，另记一对接手事件。
                let (event, prev_cred_id, prev_slot, reason) = match rebind_from {
                    Some((cid, old, reason)) => ("rebound", Some(cid), Some(old), Some(reason)),
                    None => ("bound", None, None, None),
                };
                log_event(
                    &conn,
                    NewEvent {
                        key,
                        event,
                        cred_id: Some(chosen.id),
                        prev_cred_id,
                        slot: Some(slot),
                        prev_slot,
                        reason,
                        ..Default::default()
                    },
                )?;
                note_slot_takeover(&conn, chosen.id, slot, key, session_ttl_secs)?;
                conn.execute(
                    "INSERT INTO session_bindings (session_key, cred_id, slot, last_model, device_id) \
                     VALUES (?1, ?2, ?3, ?4, ?6)
                     ON CONFLICT(session_key) DO UPDATE
                        SET cred_id = ?2, slot = ?3, slot_lost = 0, last_seen_at = unixepoch(), \
                            created_at = CASE WHEN ?5 > 0 \
                                                AND last_seen_at < unixepoch() - ?5 \
                                              THEN unixepoch() ELSE created_at END, \
                            request_count = CASE WHEN ?5 > 0 \
                                                   AND last_seen_at < unixepoch() - ?5 \
                                                 THEN 1 ELSE request_count + 1 END, \
                            last_model = COALESCE(?4, last_model), \
                            device_id = COALESCE(?6, device_id)",
                    params![key, chosen.id, slot, model, session_retention, device_id],
                )?;
                (slot >= 0).then_some(slot)
            }
            None => None,
        };
        if let Some(did) = affinity_device {
            upsert_device_binding(&conn, did, chosen.id, device_retention)?;
        }
        // 两个窗口都在**选定之后**才记账（而不是边问边记）：一次选号要连过两道窗口，
        // 边问边记的话，过了第一道却卡在第二道的那个号会白扣一个名额。理由详见
        // [`RateWindow::has_room`]——选号全程持着 `conn` 锁，中间插不进第二次选号。
        self.rpm_rate.take(chosen.id, rpm_limit_of(chosen), rpm_window);
        if device_id.is_none() && rate_limited {
            self.bare_rate.take(chosen.id, rate_limit, bare_window);
        }
        Ok((chosen.clone(), slot))
    }
}
