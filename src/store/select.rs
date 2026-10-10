//! 选号：按设备 / 会话粘性挑凭证、占名额，以及名额耗尽时的错误。
//!
//! ## 串行化
//!
//! 选号是一整段「读 → 判断 → 写」：找空槽位、数名额判上限、改绑、以及进程内两道限流窗口的
//! 「先问再记」（[`RateWindow::has_room`](super::RateWindow::has_room) + `take`）。整段放进一个 [`CredentialStore::begin_write`]
//! 事务，选号彼此串行、也和其它串行写入串行。

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

/// 沿用来访会话 id、不派生的会话绑定（[`Select::passthrough_session`]）在 `slot` 列记的值。
/// 不占槽位：[`free_session_slot`] 只从 0 起找空位，负数永远不与之相撞。
pub const PASSTHROUGH_SLOT: i64 = -1;

/// [`CredentialStore::select_for_device`] 里这条请求按哪张表粘住账号、占哪种名额。
/// 两张表列名不同、上限字段不同、拒绝的错误类型不同，其余规则逐条相同。
#[derive(Clone, Copy)]
pub(super) enum Binding<'a> {
    /// 客户端自带的设备身份，`device_bindings`。
    Device(&'a str),
    /// 模拟路径上没有设备身份的来访，按会话键，`session_bindings`。见 [`Select::session_key`]。
    Session(&'a str),
}

impl<'a> Binding<'a> {
    pub(super) fn table(self) -> &'static str {
        match self {
            Self::Device(_) => "device_bindings",
            Self::Session(_) => "session_bindings",
        }
    }

    pub(super) fn column(self) -> &'static str {
        match self {
            Self::Device(_) => "device_id",
            Self::Session(_) => "session_key",
        }
    }

    pub(super) fn key(self) -> &'a str {
        match self {
            Self::Device(k) | Self::Session(k) => k,
        }
    }

    /// 所有可调度的号名额都满了时的拒绝理由。
    pub(super) fn limit_error(self) -> anyhow::Error {
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
    /// 接入 Key 能用的分组，按优先顺序；`None` = 全部号，`Some(空)` = 一个号都不能用（绑定的
    /// 分组被删光了，不放开成全部号）。只在这些分组的号里选，号在越
    /// 靠前的分组里越优先（排序第一键，排在优先级之前）；前面分组的号都用不了才溢出到后面的。
    /// 粘住的号不在这些分组里时，按「原号不可用」改选。
    pub groups: Option<&'a [i64]>,
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

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use sqlx::{PgConnection, Row};

use super::bindings::active_counts;
use super::session_events::{log_event, note_slot_takeover};
use super::{
    AllRateLimited, BareRateLimited, COLS, Credential, NewEvent, OWNER_ACTIVE, RPM_WINDOW_SECS,
    RefreshFailed, RpmLimited, effective_device_limit, effective_retention, effective_rpm_limit,
    effective_session_limit, model_denial_key, premium_model, refresh_pause_detail,
};
use super::{CredentialStore, nul_free, open_tokens, row_to_sealed_cred};

/// 选号的候选查询：读出全部启用、号主有效的号，按 (priority, id) 升序。到点的限流暂停号
/// 先由 [`CredentialStore::resume_due`] **单独一条语句**放回来，再跑这一条。
///
/// 不并成一条（可写 CTE 里 `UPDATE ... RETURNING` 再 `UNION` 启用号）：那样主查询读的是语句
/// 开始时的快照。别的连接（控制台列表也会惰性恢复、连通性测试、手动启用）正好在这期间恢复了
/// 某个号的话，快照里它还停着；CTE 的 `UPDATE` 等到行锁后按新版本重判 `disabled = 1` 又不成立、
/// 不返回它——两路都漏掉，池里只剩这一个可用号时直接报没有可用账号。分成两条语句后，
/// READ COMMITTED 下这条查询拿的是新快照，别处已提交的恢复都看得见。
///
/// 末尾多两列：
/// - `group_rank`：号所在的、在 `$1`（接入 Key 能用的分组，按优先顺序）里最靠前的那个分组的
///   名次（1 起）；不在这些分组里（或 `$1` 为 NULL，即不限分组）时为 NULL；
/// - `denied`：上游判过「套餐不含 `$2` 这个模型」且还没到期（见 `deny_model`）；`$2` 为 NULL
///   （请求没带模型）时恒假。
fn candidates_sql() -> String {
    format!(
        "SELECT {COLS}, group_rank, denied FROM ( \
             SELECT c.*, \
                    (SELECT MIN(array_position($1::BIGINT[], g.group_id))::BIGINT \
                       FROM credential_groups g \
                      WHERE g.cred_id = c.id AND g.group_id = ANY($1::BIGINT[])) AS group_rank, \
                    EXISTS (SELECT 1 FROM model_denials d \
                             WHERE d.cred_id = c.id AND d.model = $2 \
                               AND (d.expires_at IS NULL OR d.expires_at > unixepoch())) AS denied \
               FROM credentials c WHERE c.disabled = 0 \
         ) credentials \
         WHERE {OWNER_ACTIVE} \
         ORDER BY priority ASC, id ASC"
    )
}

/// 续用绑定时 `last_seen_at` 的写入粒度（秒）：离上次写入不到这么久就不改它。
///
/// 两张绑定表的 `last_seen_at` 上都有索引（清理与计数按它扫），每条请求都改它的话没有一次
/// 更新能走 HOT，表和索引跟着请求量膨胀（实测 5 万次续用后堆涨了二十多倍）。值不变就能原地
/// 更新。代价是「活跃 / 休眠」的判定最多提前这么久，故取 TTL 的十分之一、封顶 60 秒；
/// TTL 不设（`<= 0`）时只有后台列表在看它，按 60 秒算。
fn seen_step(ttl_secs: i64) -> i64 {
    if ttl_secs > 0 { (ttl_secs / 10).min(60) } else { 60 }
}

/// 写一条设备绑定（新建或改绑到 `cred_id`）。按设备占名额的选号与按会话占名额时的设备亲和
/// 记录（[`Select::per_session`]）共用这一份。
///
/// 过了保留期、后台还没来得及删的那一行会在这里被撞上。那是一条**新**绑定，故 created_at 与
/// request_count 归零重来——否则设备明细里会显示一个几天前建立、请求数接着往上加的绑定。
/// `SET` 右侧读的都是冲突前那一行的值（PG 语义同 SQLite），故 CASE 里的 last_seen_at 是旧值。
async fn upsert_device_binding(
    conn: &mut PgConnection,
    device_id: &str,
    cred_id: i64,
    retention_secs: i64,
    seen_step: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO device_bindings (device_id, cred_id) VALUES ($1, $2)
         ON CONFLICT (device_id) DO UPDATE
            SET cred_id = $2, \
                last_seen_at = CASE WHEN device_bindings.cred_id <> $2 \
                                      OR device_bindings.last_seen_at < unixepoch() - $4 \
                                    THEN unixepoch() ELSE device_bindings.last_seen_at END, \
                created_at = CASE WHEN $3 > 0 \
                                    AND device_bindings.last_seen_at < unixepoch() - $3 \
                                  THEN unixepoch() ELSE device_bindings.created_at END, \
                request_count = CASE WHEN $3 > 0 \
                                       AND device_bindings.last_seen_at < unixepoch() - $3 \
                                     THEN 1 ELSE device_bindings.request_count + 1 END",
    )
    .bind(device_id)
    .bind(cred_id)
    .bind(retention_secs)
    .bind(seen_step)
    .execute(conn)
    .await?;
    Ok(())
}

/// 该凭证当前**空着**的最小会话槽位：活跃（TTL 内）绑定占着的槽位之外，从 0 起最小的那个；
/// `prefer` 给出的槽位空着就直接用它（休眠的软绑定回来优先回原槽位，会话 id 才不换）。
/// 休眠绑定占过的槽位算空——它们不占名额，槽位（也就是会话 id）让给活跃的对话复用。
async fn free_session_slot(
    conn: &mut PgConnection,
    cred_id: i64,
    ttl_secs: i64,
    prefer: Option<i64>,
) -> Result<i64> {
    let taken: Vec<i64> = if ttl_secs > 0 {
        sqlx::query_scalar(
            "SELECT slot FROM session_bindings \
              WHERE cred_id = $1 AND last_seen_at >= unixepoch() - $2 ORDER BY slot ASC",
        )
        .bind(cred_id)
        .bind(ttl_secs)
        .fetch_all(conn)
        .await?
    } else {
        sqlx::query_scalar("SELECT slot FROM session_bindings WHERE cred_id = $1 ORDER BY slot ASC")
            .bind(cred_id)
            .fetch_all(conn)
            .await?
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

impl CredentialStore {
    /// 这把接入 Key 能用的号里，此刻还有没有**可调度**的：启用（或限流暂停已到点）、号主有效、
    /// 在 `groups` 里、没被判过「套餐不含 `model`」、不在冷却。只读——不放回到点的暂停号、
    /// 不写绑定、不占名额、不计裸请求与 RPM。
    ///
    /// 给探活本地应答用（`crate::proxy::probe_reply`）：号池真空了还回 200，下游会一直以为
    /// 渠道健康、不切走，真流量全部失败。名额满、RPM 满这类按请求的瞬时容量不算「没号」。
    pub async fn has_usable_credential(
        &self,
        groups: Option<&[i64]>,
        model: Option<&str>,
    ) -> Result<bool> {
        let model = model.map(nul_free);
        let model_key = model.as_deref().map(model_denial_key);
        let ids: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT id FROM credentials \
              WHERE (disabled = 0 OR (resume_at IS NOT NULL AND resume_at <= unixepoch())) \
                AND {OWNER_ACTIVE} \
                AND ($1::BIGINT[] IS NULL OR id IN \
                     (SELECT cred_id FROM credential_groups WHERE group_id = ANY($1::BIGINT[]))) \
                AND NOT EXISTS (SELECT 1 FROM model_denials d \
                                 WHERE d.cred_id = credentials.id AND d.model = $2 \
                                   AND (d.expires_at IS NULL OR d.expires_at > unixepoch()))"
        )))
        .bind(groups)
        .bind(model_key.as_deref())
        .fetch_all(&self.pool)
        .await?;
        Ok(ids.into_iter().any(|id| !self.cooldown.is_cooling(id, model.as_deref())))
    }

    /// 同 [`Self::select_with_slot`]，只要号（测试用）。
    #[cfg(test)]
    pub async fn select_for_device(&self, sel: Select<'_>) -> Result<Credential> {
        self.select_with_slot(sel).await.map(|(cred, _)| cred)
    }

    /// 按 device_id / 会话键做粘性选择，返回选中的凭证与这条请求在它上面占的**会话槽位**（按
    /// 会话键绑定时为 `Some`，其余 `None`）。刷新在事务外由调用方处理。
    ///
    /// 规则（详见 rusqlite 版 `CredentialStore::select_for_device` 的文档，逐条相同）：
    /// 1. TTL 内的绑定（**活跃**）且该凭证仍可调度 → 复用（更新 last_seen / request_count），
    ///    已占名额的不再受上限约束。
    /// 2. TTL 外但仍在保留期内的绑定（**软绑定**）→ 仍优先回原号，但要重新占名额：原号必须
    ///    仍启用、不在冷却、未被本轮排除，且还有空位；不满足就当新来访重选并**改绑**。
    /// 3. 绑定的凭证已停用或删除 → 作为新来访重新选择（选中谁就改绑到谁）。
    /// 4. 新来访 → 在仍有名额的可调度凭证中按 (分组名次, priority, 套餐档, 占用数, id) 挑第一个。
    /// 5. 全部满 → 硬性拒绝（[`DeviceLimitReached`] / [`SessionLimitReached`]）。
    ///
    /// 冷却中的号在任何分支之前就被剔出候选，全部都在冷却时返回 [`AllRateLimited`]。没有设备
    /// 身份的（裸请求、按会话键绑定的）在 `rate_limited` 时受裸请求速率上限约束
    /// （[`BareRateLimited`]）。账号 RPM 是所有分支共同的最后一道门：还没定下号的打满就跳过、
    /// 全满才 [`RpmLimited`]；已经粘在某个号上的打满就**就地拒**（`sticky`），不改绑——改绑
    /// 会让这条会话每一轮先撞一次 thinking 签名 400。
    ///
    /// 全部操作在一个串行化写事务里（见模块文档）。选号失败（名额满、限流……）时事务里已做的
    /// 写入（到点恢复的暂停号）照样提交——rusqlite 版那些语句是逐条自动提交的，结果相同。
    ///
    /// [`DeviceLimitReached`]: super::DeviceLimitReached
    /// [`SessionLimitReached`]: super::SessionLimitReached
    pub async fn select_with_slot(&self, sel: Select<'_>) -> Result<(Credential, Option<i64>)> {
        // 设备 / 会话标识与模型名都取自来访体，可能带 NUL，见 [`nul_free`]。
        let device_id = sel.device_id.map(nul_free);
        let session_key = sel.session_key.map(nul_free);
        let model = sel.model.map(nul_free);
        let sel = Select {
            device_id: device_id.as_deref(),
            session_key: session_key.as_deref(),
            model: model.as_deref(),
            ..sel
        };
        let mut tx = self.begin_write().await?;
        let out = self.select_in(&mut tx, sel).await;
        let committed = tx.commit().await;
        match (out, committed) {
            // 候选号读出来时 token 没解密（见 [`row_to_sealed_cred`]），只解选中这一个，
            // 而且放在提交之后、不占全局写锁。
            (Ok((mut cred, slot)), Ok(())) => {
                open_tokens(&mut cred)?;
                Ok((cred, slot))
            }
            // 选号本身的错误（含 SQL 出错使事务作废、提交随之失败的情况）优先上报。
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e.into()),
        }
    }

    /// [`Self::select_with_slot`] 的本体，在调用方开好的串行化写事务里跑。
    async fn select_in(
        &self,
        conn: &mut PgConnection,
        sel: Select<'_>,
    ) -> Result<(Credential, Option<i64>)> {
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
            groups,
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
        // 设置走内存缓存，零查询。
        let default_limit = self.default_device_limit();
        let default_session_limit = self.default_session_limit();
        let (rate_limit, rate_window) = (self.bare_rate_limit(), self.bare_rate_window_secs());
        let default_rpm = self.default_rpm_limit();

        // RPM 的窗口就是账号列表那一列的窗口（60 秒），两处共用同一个常量：限的和看到的
        // 必须是同一个口径。
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

        // 「连保留期都过了」的绑定行由后台定时清（[`Self::prune_expired_bindings`]），不在这条
        // 路上删；下面命中既有绑定那一步自己按保留期过滤，「行还在但已过保留期」与「行已被
        // 删掉」对选号是同一个结果。
        let device_retention = effective_retention(ttl_secs, retention_secs).unwrap_or(0);
        let device_step = seen_step(ttl_secs);
        let session_retention =
            effective_retention(session_ttl_secs, session_retention_secs).unwrap_or(0);

        // 限流暂停到点的号先放回来，再读启用的号（分两条语句，见 [`candidates_sql`]）。
        Self::resume_due(&mut *conn).await?;
        let model_key = model.map(model_denial_key);
        let rows = sqlx::query(sqlx::AssertSqlSafe(candidates_sql()))
            .bind(groups)
            .bind(model_key.as_deref())
            .fetch_all(&mut *conn)
            .await?;
        let mut all: Vec<Credential> = Vec::with_capacity(rows.len());
        // 号 → 它所在的、在 `groups` 里最靠前的那个分组的名次（只在绑了分组时有）。
        let mut group_rank: HashMap<i64, usize> = HashMap::new();
        let mut denied: HashSet<i64> = HashSet::new();
        let ncols = COLS.split(',').count();
        for row in &rows {
            let c = row_to_sealed_cred(row)?;
            if let Some(rank) = row.try_get::<Option<i64>, _>(ncols)? {
                group_rank.insert(c.id, rank as usize);
            }
            if row.try_get::<bool, _>(ncols + 1)? {
                denied.insert(c.id);
            }
            all.push(c);
        }
        drop(rows);
        // 接入 Key 绑定了分组：只留这些分组里的号。绑了分组但一个都不剩（分组被删光）时是空表：
        // 一个号都选不到，而不是放开成全部号。
        let group_rank: Option<HashMap<i64, usize>> = groups.map(|_| group_rank);
        if let Some(rank) = &group_rank {
            all.retain(|c| rank.contains_key(&c.id));
        }
        let rank_of =
            |c: &Credential| group_rank.as_ref().and_then(|m| m.get(&c.id).copied()).unwrap_or(0);
        if all.is_empty() {
            // 一个能用的都没有。若其中有「限流暂停、还没到点」的，这不是配置问题而是限流：
            // 回 429 + 最早那个的恢复时刻，比一句「没有可用凭证，请先登录」诚实得多。
            // 只看这把 Key 能用的号：别的分组里暂停的号不该让这里回 429。
            let soonest: Option<(i64, String, Option<String>, i64)> = sqlx::query_as(
                "SELECT id, label, ban_reason, resume_at FROM credentials \
                  WHERE disabled = 1 AND resume_at IS NOT NULL \
                    AND ($1::BIGINT[] IS NULL OR id IN \
                         (SELECT cred_id FROM credential_groups WHERE group_id = ANY($1::BIGINT[]))) \
                  ORDER BY resume_at ASC, id ASC LIMIT 1",
            )
            .bind(groups)
            .fetch_optional(&mut *conn)
            .await?;
            if let Some((id, label, reason, at)) = soonest {
                let retry_after_secs = (at - crate::credentials::now_secs() as i64).max(1);
                let refresh_failed = reason
                    .as_deref()
                    .and_then(refresh_pause_detail)
                    .map(|detail| RefreshFailed::paused(id, label, detail));
                return Err(AllRateLimited { retry_after_secs, refresh_failed }.into());
            }
            if group_rank.is_some() {
                anyhow::bail!("no available credentials in the groups bound to this API key");
            }
            anyhow::bail!("no available credentials; add an account first");
        }

        // 上游判过「套餐不含这个模型」的号先出局：这不是等一会就好的事，也不该拿去撞。**所有
        // 启用号**都被判过才是「换模型」——这一判必须排在下面「排除已试过的号」之前：换号重试时
        // 刚被判的那个号就在 `exclude` 里，先排除再看会把它漏数。
        //
        // 但先看有没有**只是被限流暂停**且没被判过的号：那是「等一会」不是「换模型」。
        if let Some(m) = model
            && all.iter().all(|c| denied.contains(&c.id))
        {
            let soonest: Option<i64> = sqlx::query_scalar(
                "SELECT MIN(resume_at) FROM credentials c \
                  WHERE disabled = 1 AND resume_at IS NOT NULL \
                    AND NOT EXISTS (SELECT 1 FROM model_denials d \
                                     WHERE d.cred_id = c.id AND d.model = $1 \
                                       AND (d.expires_at IS NULL OR d.expires_at > unixepoch())) \
                    AND ($2::BIGINT[] IS NULL OR c.id IN \
                         (SELECT cred_id FROM credential_groups WHERE group_id = ANY($2::BIGINT[])))",
            )
            .bind(model_key.as_deref())
            .bind(groups)
            .fetch_one(&mut *conn)
            .await?;
            if let Some(at) = soonest {
                let retry_after_secs = (at - crate::credentials::now_secs() as i64).max(1);
                return Err(AllRateLimited { retry_after_secs, refresh_failed: None }.into());
            }
            return Err(ModelUnsupported { model: m.to_string(), accounts: all.len() }.into());
        }
        // 本次请求已经试过的号直接出局——重试再撞同一个号毫无意义。改绑事件要分清原号为什么回
        // 不去，先记下启用的全集。
        let enabled: HashSet<i64> = all.iter().map(|c| c.id).collect();
        let mut pool = all;
        pool.retain(|c| !exclude.contains(&c.id) && !denied.contains(&c.id));
        if pool.is_empty() {
            anyhow::bail!(
                "no other available credentials remain after excluding those already tried"
            );
        }
        // 冷却中的号让位给还能用的；**全部都在冷却就直接拒**——冷却是硬门禁。等待时间取最早
        // 解冻的那个，客户端照它重试即可。
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

        // 裸请求速率上限：没有设备身份的都算裸请求，按会话键绑定的也是。
        let bare_window = Duration::from_secs(rate_window.max(1) as u64);
        let bare_ok = |c: &Credential| {
            device_id.is_some()
                || !rate_limited
                || self.bare_rate.has_room(c.id, rate_limit, bare_window)
        };

        // 匿名侧查询先找主线程（[`Select::follow_only`]）：同一个会话键有活跃绑定、原号还能调度
        // 就跟过去，绑定行一个字不动；跟不上就当裸请求往下按负载选，同样不写绑定。
        if follow_only && let Some(Binding::Session(key)) = binding {
            let bound: Option<(i64, i64)> = sqlx::query_as(
                "SELECT cred_id, slot FROM session_bindings WHERE session_key = $1 \
                    AND ($2 <= 0 OR last_seen_at >= unixepoch() - $2)",
            )
            .bind(key)
            .bind(session_ttl_secs)
            .fetch_optional(&mut *conn)
            .await?;
            if let Some((cid, slot)) = bound
                && let Some(c) = creds.iter().find(|c| c.id == cid)
                && bare_ok(c)
                && rpm_room(c)
            {
                if let Some(did) = affinity_device {
                    upsert_device_binding(&mut *conn, did, c.id, device_retention, device_step)
                        .await?;
                }
                self.rpm_rate.take(c.id, rpm_limit_of(c), rpm_window);
                if device_id.is_none() && rate_limited {
                    self.bare_rate.take(c.id, rate_limit, bare_window);
                }
                return Ok((c.clone(), (slot >= 0).then_some(slot)));
            }
        }
        let binding = binding.filter(|_| !follow_only);

        // 各凭证当前**占名额**的设备数或模拟会话数：只数 TTL 内活跃的绑定（口径与
        // [`Self::device_counts`] / [`Self::session_counts`] 一致）。只数本次绑定的那一种。
        //
        // **用到才数**：这条查询扫的是 TTL 内全部活跃绑定，随活跃设备数线性变慢，又在全局写锁里。
        // 大多数请求命中自己的活跃绑定直接续用，根本不看名额——那条路不数。
        let (count_table, count_ttl) = match binding {
            Some(Binding::Session(_)) => ("session_bindings", session_ttl_secs),
            _ => ("device_bindings", ttl_secs),
        };
        let mut counts: Option<HashMap<i64, i64>> = None;
        // 这条请求走哪张表，TTL 与保留期就都用哪张表的。
        let (binding_ttl, binding_retention) = match binding {
            Some(Binding::Session(_)) => (session_ttl_secs, session_retention),
            _ => (ttl_secs, device_retention),
        };

        // 生效上限：账号未单独配置（== 0）时套用对应的全局默认。
        let limit_of = |c: &Credential| match binding {
            Some(Binding::Session(_)) => {
                effective_session_limit(c.session_limit, default_session_limit)
            }
            _ => effective_device_limit(c.device_limit, default_limit),
        };
        // 还塞得下一台设备 / 一条会话吗（上限 <= 0 即不限）。
        let room_in = |counts: &HashMap<i64, i64>, c: &Credential| {
            limit_of(c) <= 0 || counts.get(&c.id).copied().unwrap_or(0) < limit_of(c)
        };

        // 1/2/3) 命中既有绑定。会话绑定回不去原号、往下改选时，记下原号与原因，改绑事件用。
        let mut rebind_from: Option<(i64, i64, &'static str)> = None;
        if let Some(b) = binding {
            // 第二列是「这条绑定还在 TTL 内吗」，交给库与清理/计数用同一个 unixepoch() 时钟
            // 判定。第三列是会话绑定原来的槽位（设备绑定没有槽位）。`WHERE` 上那道保留期过滤
            // 让「行还在但过期了」与「行已删」对这里是同一个结果；走的是主键点查。
            let slot_col = match b {
                Binding::Session(_) => "slot",
                Binding::Device(_) => "NULL::BIGINT",
            };
            let bound: Option<(i64, bool, Option<i64>)> =
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT cred_id, ($2 <= 0 OR last_seen_at >= unixepoch() - $2), {slot_col} \
                       FROM {} WHERE {} = $1 \
                        AND ($3 <= 0 OR last_seen_at >= unixepoch() - $3)",
                    b.table(),
                    b.column()
                )))
                .bind(b.key())
                .bind(binding_ttl)
                .bind(binding_retention)
                .fetch_optional(&mut *conn)
                .await?;
            if let Some((cid, active, old_slot)) = bound {
                let old = old_slot.unwrap_or(0);
                // 休眠的软绑定要重新占名额，这时才需要计数。
                if !active && counts.is_none() {
                    counts = Some(active_counts(&mut *conn, count_table, count_ttl).await?);
                }
                let fits =
                    |c: &Credential| active || counts.as_ref().is_some_and(|m| room_in(m, c));
                // 原号仍可调度（启用、不在冷却、本轮没试过）时才谈复用。
                if let Some(c) = creds.iter().find(|c| c.id == cid) {
                    // 活跃绑定本来就占着名额，直接续；休眠的软绑定要重新占一个位置，原号满了就
                    // 只能改选。按会话绑定的还要过裸请求速率上限。
                    if fits(c) && bare_ok(c) {
                        // RPM 打满 → **就地拒**，不往下走改选那条路。
                        if !rpm_room(c) {
                            return Err(RpmLimited {
                                retry_after_secs: self.rpm_rate.retry_after_secs(&c.id, rpm_window),
                                sticky: true,
                            }
                            .into());
                        }
                        let slot = match b {
                            Binding::Session(key) => {
                                // 活跃绑定续用原槽位；休眠的会话软绑定回来要重新占槽位：原槽位
                                // 空着就还用它，被别的对话拿走了就取最小的空位。沿用来访会话 id
                                // 的不占槽位（-1）；原来记的是 -1 而这次要派生的，也得新取一个。
                                let slot = if passthrough_session {
                                    PASSTHROUGH_SLOT
                                } else if active && old >= 0 {
                                    old
                                } else {
                                    free_session_slot(&mut *conn, c.id, session_ttl_secs, Some(old))
                                        .await?
                                };
                                // 休眠后回来记一条恢复（槽位变没变都记）；活跃中换了槽位记一条换
                                // 槽位。新拿的槽位若是从别的休眠绑定手里接过来的，再记一对接手事件。
                                if !active || slot != old {
                                    log_event(
                                        &mut *conn,
                                        NewEvent {
                                            key,
                                            event: if active { "reslotted" } else { "resumed" },
                                            cred_id: Some(c.id),
                                            prev_cred_id: Some(c.id),
                                            slot: Some(slot),
                                            prev_slot: Some(old),
                                            ..Default::default()
                                        },
                                    )
                                    .await?;
                                    note_slot_takeover(
                                        &mut *conn,
                                        c.id,
                                        slot,
                                        key,
                                        session_ttl_secs,
                                    )
                                    .await?;
                                }
                                sqlx::query(
                                    "UPDATE session_bindings \
                                        SET last_seen_at = CASE \
                                                WHEN slot <> $2 OR slot_lost <> 0 \
                                                  OR last_seen_at < unixepoch() - $5 \
                                                THEN unixepoch() ELSE last_seen_at END, \
                                            slot = $2, slot_lost = 0, \
                                            request_count = request_count + 1, \
                                            last_model = COALESCE($3, last_model), \
                                            device_id = COALESCE($4, device_id) \
                                      WHERE session_key = $1",
                                )
                                .bind(key)
                                .bind(slot)
                                .bind(model)
                                .bind(device_id)
                                .bind(seen_step(session_ttl_secs))
                                .execute(&mut *conn)
                                .await?;
                                (slot >= 0).then_some(slot)
                            }
                            Binding::Device(did) => {
                                sqlx::query(
                                    "UPDATE device_bindings \
                                        SET last_seen_at = CASE \
                                                WHEN last_seen_at < unixepoch() - $2 \
                                                THEN unixepoch() ELSE last_seen_at END, \
                                            request_count = request_count + 1 \
                                      WHERE device_id = $1",
                                )
                                .bind(did)
                                .bind(device_step)
                                .execute(&mut *conn)
                                .await?;
                                None
                            }
                        };
                        if let Some(did) = affinity_device {
                            upsert_device_binding(
                                &mut *conn,
                                did,
                                c.id,
                                device_retention,
                                device_step,
                            )
                            .await?;
                        }
                        self.rpm_rate.take(c.id, rpm_limit_of(c), rpm_window);
                        if device_id.is_none() && rate_limited {
                            self.bare_rate.take(c.id, rate_limit, bare_window);
                        }
                        return Ok((c.clone(), slot));
                    }
                }
                if let Binding::Session(_) = b {
                    // 判定顺序与上面过滤候选的顺序一致：停用/删除 → 模型不支持 → 本轮已试过 →
                    // 冷却 → 名额 → 裸请求速率。
                    let reason = match creds.iter().find(|c| c.id == cid) {
                        _ if !enabled.contains(&cid) => "disabled",
                        _ if denied.contains(&cid) => "model_denied",
                        _ if exclude.contains(&cid) => "retried",
                        None => "cooling",
                        Some(c) if !fits(c) => "full",
                        Some(_) => "bare_limit",
                    };
                    rebind_from = Some((cid, old, reason));
                }
                // 回不去原号：往下重新选择，选中谁就**改绑**到谁。冷却结束后这台设备不会自己
                // 回到原号——粘性以最后一次选择为准。
            }
        }

        // 往下要新选号：名额参与排序与过滤，此刻才真要数。
        let counts = match counts {
            Some(m) => m,
            None => active_counts(&mut *conn, count_table, count_ttl).await?,
        };
        let used = |c: &Credential| counts.get(&c.id).copied().unwrap_or(0);
        let has_room = |c: &Credential| room_in(&counts, c);

        // 4/5) 优先级分档调度：(分组名次, priority, 套餐档, 占用数, id) 是唯一的排序口径。
        // fable/mythos 这类只有高档套餐才含的模型，同一优先级档内 Max 号排前面，等级未知的
        // 与团队/企业席位其次，Pro/Free 垫底。**只排序不剔除**：准入以上游的判决为准。
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
        ordered.sort_by_key(|c| (rank_of(c), c.priority, plan_rank(c), used(c), c.id));
        // 设备亲和（[`Select::per_session`]）：这台设备上次落的号提到最前，新会话跟着它走。它照样
        // 要过下面的名额与 RPM 两道门，过不去就按原顺序溢出。premium 模型下不越过档次更高的号。
        if let Some(did) = affinity_device {
            let home: Option<i64> = sqlx::query_scalar(
                "SELECT cred_id FROM device_bindings WHERE device_id = $1 \
                    AND ($2 <= 0 OR last_seen_at >= unixepoch() - $2)",
            )
            .bind(did)
            .bind(device_retention)
            .fetch_optional(&mut *conn)
            .await?;
            if let Some(home) = home
                && let Some(pos) = ordered.iter().position(|c| c.id == home)
                && ordered.first().is_some_and(|f| {
                    rank_of(ordered[pos]) <= rank_of(f) && plan_rank(ordered[pos]) <= plan_rank(f)
                })
            {
                let c = ordered.remove(pos);
                ordered.insert(0, c);
            }
        }
        let chosen = match binding {
            Some(b) => {
                // 硬限制：仅在仍有名额者中选；当前优先级档全满时其成员被过滤掉，自然溢出到
                // 下一档；全部满则拒绝。
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
                // 名额与 RPM 是两回事，故两道门分开判：都过不去时要能说清是哪一道拦的。
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
                upsert_device_binding(&mut *conn, did, chosen.id, device_retention, device_step)
                    .await?;
                None
            }
            Some(Binding::Session(key)) => {
                // 新对话（或改绑到别的号的对话）在选中的号上取最小的空槽位。上面刚判过
                // has_room，所以上限内必有空位；不限时槽位按需增长、释放后复用。沿用来访
                // 会话 id 的不派生、不占槽位。
                let slot = if passthrough_session {
                    PASSTHROUGH_SLOT
                } else {
                    free_session_slot(&mut *conn, chosen.id, session_ttl_secs, None).await?
                };
                // 原号回不去而改选的记改绑（带原号、原槽位与原因），其余是新建；拿到的槽位若是
                // 从休眠绑定手里接过来的，另记一对接手事件。
                let (event, prev_cred_id, prev_slot, reason) = match rebind_from {
                    Some((cid, old, reason)) => ("rebound", Some(cid), Some(old), Some(reason)),
                    None => ("bound", None, None, None),
                };
                log_event(
                    &mut *conn,
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
                )
                .await?;
                note_slot_takeover(&mut *conn, chosen.id, slot, key, session_ttl_secs).await?;
                sqlx::query(
                    "INSERT INTO session_bindings (session_key, cred_id, slot, last_model, device_id) \
                     VALUES ($1, $2, $3, $4, $6)
                     ON CONFLICT (session_key) DO UPDATE
                        SET cred_id = $2, slot = $3, slot_lost = 0, last_seen_at = unixepoch(), \
                            created_at = CASE WHEN $5 > 0 \
                                                AND session_bindings.last_seen_at < unixepoch() - $5 \
                                              THEN unixepoch() ELSE session_bindings.created_at END, \
                            request_count = CASE WHEN $5 > 0 \
                                                   AND session_bindings.last_seen_at < unixepoch() - $5 \
                                                 THEN 1 ELSE session_bindings.request_count + 1 END, \
                            last_model = COALESCE($4, session_bindings.last_model), \
                            device_id = COALESCE($6, session_bindings.device_id)",
                )
                .bind(key)
                .bind(chosen.id)
                .bind(slot)
                .bind(model)
                .bind(session_retention)
                .bind(device_id)
                .execute(&mut *conn)
                .await?;
                (slot >= 0).then_some(slot)
            }
            None => None,
        };
        if let Some(did) = affinity_device {
            upsert_device_binding(&mut *conn, did, chosen.id, device_retention, device_step)
                .await?;
        }
        // 两个窗口都在**选定之后**才记账（而不是边问边记）：一次选号要连过两道窗口，边问边记
        // 的话，过了第一道却卡在第二道的那个号会白扣一个名额。选号全程在串行化写事务里，中间
        // 插不进第二次选号。
        self.rpm_rate.take(chosen.id, rpm_limit_of(chosen), rpm_window);
        if device_id.is_none() && rate_limited {
            self.bare_rate.take(chosen.id, rate_limit, bare_window);
        }
        Ok((chosen.clone(), slot))
    }
}

#[cfg(test)]
pub(super) mod tests {
    //! 选号的测试，以及 `pg` 下 C 那几个模块的测试共用的建库 / 造数辅助。

    use std::time::Duration;

    use sqlx::PgPool;

    use super::super::CredentialStore;
    use super::super::*;

    /// 建库并插入 `labels` 这几个号（号主是 admin，id 1），回 (store, 各号 id)。
    pub(in crate::store) async fn store_with(
        pool: PgPool,
        labels: &[&str],
    ) -> (CredentialStore, Vec<i64>) {
        let store = CredentialStore::for_test(pool).await;
        let mut ids = Vec::new();
        for l in labels {
            // refresh_token 有唯一约束（指纹），按 label 取值保证互不相同。
            let c = store
                .insert(l, None, &format!("tok-{l}"), &format!("refresh-{l}"), 0, None, None, 1)
                .await
                .unwrap();
            ids.push(c.id);
        }
        (store, ids)
    }

    /// 选号入参：TTL 一分钟、保留期一小时，即「名额一分钟就还、亲和性留一小时」。
    pub(in crate::store) fn soft(device_id: &str) -> Select<'_> {
        Select {
            device_id: Some(device_id),
            ttl_secs: 60,
            retention_secs: 3600,
            rate_limited: true,
            ..Default::default()
        }
    }

    /// 建库并把 TTL/保留期设成与 [`soft`] 一致——`device_count`/`list_devices` 读的是设置项，
    /// 不跟着 `Select` 走，两边不一致的话断言的就不是同一套口径了。
    pub(in crate::store) async fn soft_store(
        pool: PgPool,
        labels: &[&str],
    ) -> (CredentialStore, Vec<i64>) {
        let (store, ids) = store_with(pool, labels).await;
        store.set_setting(DEVICE_BINDING_TTL, "60").await.unwrap();
        store.set_setting(DEVICE_BINDING_RETENTION, "3600").await.unwrap();
        store.set_setting(SESSION_BINDING_TTL, "60").await.unwrap();
        store.set_setting(SESSION_BINDING_RETENTION, "3600").await.unwrap();
        (store, ids)
    }

    /// 模拟会话的选号入参：与 [`soft`] 同一套 TTL / 保留期，只是键换成会话键、没有设备身份。
    pub(in crate::store) fn soft_session(key: &str) -> Select<'_> {
        Select {
            session_key: Some(key),
            ttl_secs: 60,
            retention_secs: 3600,
            session_ttl_secs: 60,
            session_retention_secs: 3600,
            rate_limited: true,
            ..Default::default()
        }
    }

    /// 设备身份在上游已收敛时（[`Select::per_session`]）带设备来访的选号入参：真实客户端，
    /// 沿用来访会话 id，同 [`soft_session`] 的 TTL / 保留期。
    pub(in crate::store) fn per_session<'a>(device: &'a str, key: Option<&'a str>) -> Select<'a> {
        Select {
            device_id: Some(device),
            session_key: key,
            per_session: true,
            passthrough_session: true,
            ttl_secs: 60,
            retention_secs: 3600,
            session_ttl_secs: 60,
            session_retention_secs: 3600,
            rate_limited: true,
            ..Default::default()
        }
    }

    /// 把一条设备绑定的最后活跃时间往前推 `secs` 秒，模拟设备闲置。
    pub(in crate::store) async fn age_binding(store: &CredentialStore, device_id: &str, secs: i64) {
        let n = sqlx::query(
            "UPDATE device_bindings SET last_seen_at = unixepoch() - $2 WHERE device_id = $1",
        )
        .bind(device_id)
        .bind(secs)
        .execute(&store.pool)
        .await
        .unwrap()
        .rows_affected();
        assert_eq!(n, 1, "要推的绑定得先存在");
    }

    /// 同 [`age_binding`]，推的是会话绑定。
    pub(in crate::store) async fn age_session_binding(
        store: &CredentialStore,
        key: &str,
        secs: i64,
    ) {
        let n = sqlx::query(
            "UPDATE session_bindings SET last_seen_at = unixepoch() - $2 WHERE session_key = $1",
        )
        .bind(key)
        .bind(secs)
        .execute(&store.pool)
        .await
        .unwrap()
        .rows_affected();
        assert_eq!(n, 1, "要推的绑定得先存在");
    }

    /// 跑一条回单个整数的查询（测试里数行数用）。
    pub(in crate::store) async fn scalar(store: &CredentialStore, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&store.pool).await.unwrap()
    }

    /// 选号的 future 必须是 `Send`：转发链最终要塞进 axum handler。
    #[allow(dead_code)]
    fn select_futures_are_send(store: &CredentialStore, clients: &crate::clients::ClientPool) {
        fn assert_send<T: Send>(_: T) {}
        assert_send(store.select_with_slot(Select::default()));
        assert_send(super::super::refresh::valid_access_token_for_device(
            store,
            clients,
            Select::default(),
        ));
    }

    /// `mark_banned` 必须把它的 device_bindings 一并清掉，否则设备被钉死在坏号上，
    /// [`valid_access_token_for_device`] 的重选循环会一直选回同一个，白转满上限。
    #[sqlx::test]
    async fn banned_credential_releases_its_devices(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).await.unwrap();

        // 先把设备粘到 a 上（a 是 id 更小的那个，同优先级下会被先选中）。
        let first = store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(first.id, a.id);
        // 候选号读出来时没解密，选中的这个要解密后交出去。
        assert_eq!((first.access_token.as_str(), first.refresh_token.as_str()), ("ta", "ra"));
        // 再选一次仍命中既有绑定，确认粘性生效——这正是坏号会把设备钉死的原因。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            a.id
        );

        // 模拟「a 的 refresh_token 被作废」后的停用。
        assert!(store.mark_banned(a.id, "[refresh 400] invalid_grant").await.unwrap());

        // 重选必须换到 b，而不是继续返回 a 或直接报错。
        let after = store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(after.id, b.id, "停用坏号后设备应改选到其它账号");

        // a 确实被停用并记了原因。
        let a2 = store.get(a.id).await.unwrap().unwrap();
        assert!(a2.disabled);
        assert_eq!(a2.ban_reason.as_deref(), Some("[refresh 400] invalid_grant"));

        // 池子空了要报错，而不是把停用的号又选回来。
        assert!(store.mark_banned(b.id, "[refresh 400] invalid_grant").await.unwrap());
        assert!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .is_err(),
            "无可用凭证时应报错"
        );
    }

    /// 模拟路径上没有设备身份的来访按会话键粘住账号并占**会话名额**：同键回同号、名额满了溢到
    /// 别的号、全满时拒——与设备绑定逐条相同，但走另一张表、另一个上限，设备名额一个不占。
    #[sqlx::test]
    async fn session_key_binds_and_limits_simulated_sessions(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        store.set_setting(DEFAULT_SESSION_LIMIT, "1").await.unwrap();

        assert_eq!(store.select_for_device(soft_session("s1")).await.unwrap().id, a);
        assert_eq!(store.select_for_device(soft_session("s1")).await.unwrap().id, a, "同键回同号");
        assert_eq!(
            store.select_for_device(soft_session("s2")).await.unwrap().id,
            b,
            "a 满了溢到 b"
        );
        let err = store.select_for_device(soft_session("s3")).await.unwrap_err();
        assert!(err.downcast_ref::<SessionLimitReached>().is_some(), "全满时拒: {err}");
        assert!(err.downcast_ref::<DeviceLimitReached>().is_none(), "拒的理由是会话不是设备");
        // 设备名额一个不占，会话名额各占一个；计数与明细同一口径。
        assert_eq!(store.device_count(a).await.unwrap(), 0);
        assert!(store.list_devices(a).await.unwrap().is_empty());
        assert_eq!(store.session_count(a).await.unwrap(), 1);
        assert_eq!(store.session_counts().await.unwrap().get(&b).copied(), Some(1));
        let list = store.list_sessions(a).await.unwrap();
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].session_key, "s1");
        // 口径同设备绑定：建行那一轮不计，之后每命中一轮加一（`request_count` 的既有约定，
        // 见 `dormant_binding_still_steers_the_device_back_to_its_credential` 里的断言）。
        assert_eq!(list[0].request_count, 1, "第二轮命中既有绑定记一次");
        assert!(list[0].created_at > 0 && list[0].last_seen_at >= list[0].created_at);
        // 解绑腾出名额，s3 就能进来；再解一次是空操作。
        assert!(store.unbind_session(a, "s1").await.unwrap());
        assert!(!store.unbind_session(a, "s1").await.unwrap());
        assert_eq!(store.select_for_device(soft_session("s3")).await.unwrap().id, a);
        // 一键清空：只清这个号的，别的号不动；清完名额全空。
        assert_eq!(
            store.select_for_device(soft_session("s4")).await.unwrap_err().to_string(),
            SessionLimitReached.to_string()
        );
        assert_eq!(store.unbind_all_sessions(a).await.unwrap(), 1);
        assert_eq!(store.unbind_all_sessions(a).await.unwrap(), 0);
        assert_eq!(store.session_count(a).await.unwrap(), 0);
        assert_eq!(store.session_count(b).await.unwrap(), 1, "b 的不动");
        assert_eq!(store.select_for_device(soft_session("s4")).await.unwrap().id, a);
    }

    /// 按会话占名额的带设备来访：名额只看会话上限，设备上限不再生效；同一台设备的新会话跟着
    /// 设备亲和落在上次那个号上（而不是被均衡到会话更少的号），那个号满了才溢出，亲和随之
    /// 改到新号；真实客户端的会话不占派生槽位，后台列出的会话 id 就是来访那个。
    #[sqlx::test]
    async fn per_session_devices_are_limited_by_sessions_and_stick_to_their_home(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        store.set_setting(DEFAULT_DEVICE_LIMIT, "1").await.unwrap();
        store.set_setting(DEFAULT_SESSION_LIMIT, "2").await.unwrap();
        let sid1 = "lb:v2:sid:11111111-1111-4111-8111-111111111111";

        let (c, slot) = store.select_with_slot(per_session("d1", Some(sid1))).await.unwrap();
        assert_eq!((c.id, slot), (a, None), "沿用来访 id 的会话不带槽位回来");
        assert_eq!(store.session_slot(a, sid1).await.unwrap(), Some(PASSTHROUGH_SLOT));
        // 均衡会把 s2 放到会话更少的 b 上；亲和把它留在 d1 的号 a 上。
        assert_eq!(store.select_for_device(per_session("d1", Some("s2"))).await.unwrap().id, a);
        // 设备上限是 1，但不再生效：另一台设备照样进得来（a 满了，落到 b）。
        assert_eq!(store.select_for_device(per_session("d2", Some("s3"))).await.unwrap().id, b);
        // d1 的第三条会话：a 满了溢到 b，亲和跟着改到 b。
        assert_eq!(store.select_for_device(per_session("d1", Some("s4"))).await.unwrap().id, b);
        let home =
            scalar(&store, "SELECT cred_id FROM device_bindings WHERE device_id = 'd1'").await;
        assert_eq!(home, b);
        // 已有会话不受亲和影响，仍回原号。
        assert_eq!(store.select_for_device(per_session("d1", Some(sid1))).await.unwrap().id, a);
        // 全满时拒的是会话。
        let err = store.select_for_device(per_session("d3", Some("s5"))).await.unwrap_err();
        assert!(err.downcast_ref::<SessionLimitReached>().is_some(), "{err}");
        // 后台：真实会话的槽位是 -1，会话 id 是来访那个（这个号没有 account_uuid，钉不住）。
        let list = store.list_sessions(a).await.unwrap();
        let real = list.iter().find(|s| s.session_key == sid1).unwrap();
        assert_eq!(real.slot, PASSTHROUGH_SLOT);
        assert_eq!(real.session_id, "11111111-1111-4111-8111-111111111111");
        // 没带会话 id、按前缀指纹分的真实会话：出站没有会话 id，后台留空而不是把指纹当成 id。
        let pfx = "lb:v2:pfx:0123456789abcdef0123456789abcdef";
        assert!(store.unbind_session(a, sid1).await.unwrap());
        assert_eq!(store.select_for_device(per_session("d9", Some(pfx))).await.unwrap().id, a);
        let list = store.list_sessions(a).await.unwrap();
        assert_eq!(list.iter().find(|s| s.session_key == pfx).unwrap().session_id, "");
    }

    /// 按会话占名额而没有会话键的（额度探测）：按设备亲和选号，不写会话绑定、不占名额，
    /// 名额全满时照样放行。
    #[sqlx::test]
    async fn per_session_probe_follows_device_home_without_taking_a_slot(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let b = ids[1];
        store.set_setting(DEFAULT_SESSION_LIMIT, "1").await.unwrap();
        // d1 的家在 b（a 先被别的设备占满）。
        assert_eq!(
            store.select_for_device(per_session("d0", Some("s0"))).await.unwrap().id,
            ids[0]
        );
        assert_eq!(store.select_for_device(per_session("d1", Some("s1"))).await.unwrap().id, b);
        // 两个号都满了，探测仍按亲和落到 b，且一条会话绑定都不写。
        assert_eq!(store.select_for_device(per_session("d1", None)).await.unwrap().id, b);
        assert_eq!(store.session_count(b).await.unwrap(), 1);
        assert_eq!(store.session_counts().await.unwrap().values().sum::<i64>(), 2);
    }

    /// 槽位分配里混着真实会话（-1）时，模拟会话照样从 0 起取最小空位。
    #[sqlx::test]
    async fn passthrough_sessions_do_not_take_derived_slots(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a"]).await;
        let a = ids[0];
        assert_eq!(store.select_with_slot(per_session("d1", Some("real"))).await.unwrap().1, None);
        assert_eq!(store.select_with_slot(soft_session("sim1")).await.unwrap().1, Some(0));
        assert_eq!(store.select_with_slot(soft_session("sim2")).await.unwrap().1, Some(1));
        assert_eq!(store.session_count(a).await.unwrap(), 3, "真实会话照样占名额");
    }

    /// 匿名侧查询（[`Select::follow_only`]）：会话键有活跃绑定就跟到原号、带回原槽位，名额满了
    /// 也照跟；找不到或已休眠就按负载选号。两种情况都不写绑定、不占名额，原绑定行一个字不动。
    #[sqlx::test]
    async fn follow_only_side_queries_never_bind(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        store.set_setting(DEFAULT_SESSION_LIMIT, "1").await.unwrap();
        let side = |key| Select { follow_only: true, ..soft_session(key) };

        let (c, slot) = store.select_with_slot(soft_session("s1")).await.unwrap();
        assert_eq!((c.id, slot), (a, Some(0)));
        let before = store.list_sessions(a).await.unwrap();
        // a 的名额已满，侧查询照样跟过去，带回主线程那个槽位。
        let (c, slot) = store.select_with_slot(side("s1")).await.unwrap();
        assert_eq!((c.id, slot), (a, Some(0)), "跟随主线程");
        assert_eq!(
            store.list_sessions(a).await.unwrap()[0].request_count,
            before[0].request_count,
            "不动绑定行"
        );
        // 没有主线程的键：不建绑定、不占名额，也不因名额满被拒。
        let (_, slot) = store.select_with_slot(side("lost")).await.unwrap();
        assert_eq!(slot, None);
        assert_eq!(store.session_slot(a, "lost").await.unwrap(), None);
        assert_eq!(store.session_slot(b, "lost").await.unwrap(), None);
        assert_eq!(
            store.session_counts().await.unwrap().get(&b).copied(),
            None,
            "b 一个名额都没占"
        );
        // 主线程休眠：不跟（槽位可能已让给别的对话），按负载选、槽位不带回。
        age_session_binding(&store, "s1", 600).await;
        assert_eq!(store.select_with_slot(side("s1")).await.unwrap().1, None);
        assert_eq!(store.session_count(a).await.unwrap(), 0, "休眠的那条没被侧查询续上");
    }

    /// 会话槽位：新对话取该号上最小的空位，同键续用原槽位；休眠后原槽位被别的对话拿走就换
    /// 最小空位、空着就回原位；解绑腾出的槽位被下一个对话复用；槽位派生的会话 id 恒定且各不同。
    #[sqlx::test]
    async fn session_slots_are_reused_by_later_conversations(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a"]).await;
        let a = ids[0];
        let slot = async |k: &str| store.session_slot(a, k).await.unwrap();
        // 选号直接把槽位带回来，与事后按键查的一致；设备绑定与裸请求没有槽位。
        let (c, s) = store.select_with_slot(soft_session("s1")).await.unwrap();
        assert_eq!((c.id, s), (a, Some(0)));
        let (c, s) = store.select_with_slot(soft_session("s2")).await.unwrap();
        assert_eq!((c.id, s), (a, Some(1)));
        let (c, s) = store.select_with_slot(soft_session("s1")).await.unwrap();
        assert_eq!((c.id, s), (a, Some(0)), "续用原槽位也带回来");
        assert_eq!(store.select_with_slot(soft("dev-1")).await.unwrap().1, None);
        assert_eq!(
            store
                .select_with_slot(Select { rate_limited: true, ..Default::default() })
                .await
                .unwrap()
                .1,
            None
        );
        assert_eq!(
            (slot("s1").await, slot("s2").await),
            (Some(0), Some(1)),
            "按最小空位分，同键续用"
        );
        assert_eq!(slot("nope").await, None);
        // s1 休眠，s3 来了拿走槽位 0；s1 回来只能拿 2。
        age_session_binding(&store, "s1", 600).await;
        assert_eq!(store.select_for_device(soft_session("s3")).await.unwrap().id, a);
        assert_eq!(slot("s3").await, Some(0), "休眠绑定的槽位算空位");
        assert_eq!(store.select_for_device(soft_session("s1")).await.unwrap().id, a);
        assert_eq!(slot("s1").await, Some(2), "原槽位被占就取最小空位");
        // s2 休眠后没人占它的槽位，回来还是 1。
        age_session_binding(&store, "s2", 600).await;
        assert_eq!(store.select_for_device(soft_session("s2")).await.unwrap().id, a);
        assert_eq!(slot("s2").await, Some(1), "原槽位空着就回原位");
        // 解绑 s3，下一个对话复用槽位 0。
        assert!(store.unbind_session(a, "s3").await.unwrap());
        assert_eq!(store.select_for_device(soft_session("s4")).await.unwrap().id, a);
        assert_eq!(slot("s4").await, Some(0));
        // 列表带槽位与派生的会话 id：同槽位同 id、不同槽位不同 id、形态是 uuid。
        let list = store.list_sessions(a).await.unwrap();
        let cred = store.get(a).await.unwrap().unwrap();
        for s in &list {
            assert_eq!(
                s.session_id,
                crate::credentials::sim_slot_session_id(cred.account_uuid.as_deref(), a, s.slot)
            );
            assert_eq!(s.session_id.len(), 36, "{}", s.session_id);
        }
        let mut sids: Vec<&str> = list.iter().map(|s| s.session_id.as_str()).collect();
        sids.sort();
        sids.dedup();
        assert_eq!(sids.len(), list.len(), "各槽位的会话 id 互不相同: {list:?}");
        assert_ne!(
            crate::credentials::sim_slot_session_id(Some("u"), 1, 0),
            crate::credentials::sim_slot_session_id(Some("v"), 1, 0),
            "换账号另一组"
        );
        // 没有 account_uuid 的两个号（刚登录还没拉到 profile、或旧库）也不能算出同一组：
        // 按凭证 id 分；同一个号有没有拉到 uuid 会是两组，那是 uuid 回填那一刻的一次性切换。
        assert_ne!(
            crate::credentials::sim_slot_session_id(None, 1, 0),
            crate::credentials::sim_slot_session_id(None, 2, 0),
            "两个没有 uuid 的号，同槽位不同 id"
        );
        assert_eq!(
            crate::credentials::sim_slot_session_id(None, 1, 0),
            crate::credentials::sim_slot_session_id(Some("  "), 1, 0),
            "空白 uuid 当没有"
        );
        // 后台列表对没有 uuid 的号也与转发路径同一口径（换两个新号：先把 a 删掉）。
        assert_eq!(store.remove(&[a]).await.unwrap(), 1);
        let mut ids2 = Vec::new();
        for l in ["x", "y"] {
            ids2.push(
                store.insert(l, None, l, &format!("r-{l}"), 0, None, None, 1).await.unwrap().id,
            );
        }
        let store2 = &store;
        assert_eq!(store2.select_for_device(soft_session("k")).await.unwrap().id, ids2[0]);
        let listed = store2.list_sessions(ids2[0]).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, crate::credentials::sim_slot_session_id(None, ids2[0], 0));
        assert_ne!(listed[0].session_id, crate::credentials::sim_slot_session_id(None, ids2[1], 0));
    }

    /// 绑定行记下最近一轮的模型：**只给后台列**，不参与键也不参与选号——同一条对话换个模型
    /// 仍是同一条会话、占同一份名额（键里带模型的代价见 `crate::proxy::session_binding_key`）。
    /// 没带模型的那轮（`count_tokens` 之类）保留上一轮的值。
    #[sqlx::test]
    async fn the_session_binding_records_its_latest_model(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a"]).await;
        let a = ids[0];
        let on = |model| Select { model, ..soft_session("s1") };
        store.select_for_device(on(Some("claude-opus-5"))).await.unwrap();
        assert_eq!(
            store.list_sessions(a).await.unwrap()[0].last_model.as_deref(),
            Some("claude-opus-5")
        );
        store.select_for_device(on(Some("claude-fable-5-1"))).await.unwrap();
        let list = store.list_sessions(a).await.unwrap();
        assert_eq!(list.len(), 1, "换模型不另起会话: {list:?}");
        assert_eq!(list[0].last_model.as_deref(), Some("claude-fable-5-1"), "记最近那轮");
        store.select_for_device(on(None)).await.unwrap();
        assert_eq!(
            store.list_sessions(a).await.unwrap()[0].last_model.as_deref(),
            Some("claude-fable-5-1"),
            "没带模型的那轮不抹掉上一轮"
        );
    }

    /// 带设备身份的请求即使也带了会话键，只按设备绑定：一条请求不占两份名额。
    #[sqlx::test]
    async fn device_id_takes_precedence_over_the_session_key(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a"]).await;
        let a = ids[0];
        let sel = Select { device_id: Some("dev-1"), ..soft_session("s1") };
        assert_eq!(store.select_for_device(sel).await.unwrap().id, a);
        assert_eq!(store.device_count(a).await.unwrap(), 1);
        assert_eq!(store.session_count(a).await.unwrap(), 0, "没写会话绑定");
        assert!(store.list_sessions(a).await.unwrap().is_empty());
    }

    /// 会话上限三态同设备上限：`0` 跟随全局、`> 0` 独立上限、`< 0` 明确不限；与设备上限互不
    /// 影响——设备上限 1 时会话照样能开好几条，会话占满也不妨碍设备绑定。
    #[sqlx::test]
    async fn session_limit_tri_state_is_independent_of_the_device_limit(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a"]).await;
        let a = ids[0];
        store.set_setting(DEFAULT_SESSION_LIMIT, "1").await.unwrap();
        store.set_setting(DEFAULT_DEVICE_LIMIT, "1").await.unwrap();
        assert_eq!(store.select_for_device(soft_session("s1")).await.unwrap().id, a);
        assert!(
            store
                .select_for_device(soft_session("s2"))
                .await
                .unwrap_err()
                .downcast_ref::<SessionLimitReached>()
                .is_some()
        );
        // 账号独立上限 3。
        assert!(store.set_session_limit(a, 3).await.unwrap());
        assert_eq!(store.get(a).await.unwrap().unwrap().session_limit, 3);
        assert_eq!(store.select_for_device(soft_session("s2")).await.unwrap().id, a);
        assert_eq!(store.select_for_device(soft_session("s3")).await.unwrap().id, a);
        assert!(
            store
                .select_for_device(soft_session("s4"))
                .await
                .unwrap_err()
                .downcast_ref::<SessionLimitReached>()
                .is_some()
        );
        // 明确不限（批量接口）。
        assert_eq!(store.set_session_limits(&[a], -1).await.unwrap(), 1);
        assert_eq!(store.select_for_device(soft_session("s4")).await.unwrap().id, a);
        assert_eq!(store.select_for_device(soft_session("s5")).await.unwrap().id, a);
        assert_eq!(store.session_count(a).await.unwrap(), 5);
        // 设备名额仍是 0/1：会话一个都没占它。
        assert_eq!(store.select_for_device(soft("dev-1")).await.unwrap().id, a);
        assert!(
            store
                .select_for_device(soft("dev-2"))
                .await
                .unwrap_err()
                .downcast_ref::<DeviceLimitReached>()
                .is_some(),
            "设备上限照旧只管设备"
        );
        assert_eq!(effective_session_limit(0, 7), 7);
        assert_eq!(effective_session_limit(-1, 7), 0);
        assert_eq!(effective_session_limit(2, 7), 2);
        assert_eq!(store.default_session_limit(), 1);
    }

    /// 停用 / 封停 / 删除账号都要连带清掉它的会话绑定，否则会话被钉死在坏号上——与设备绑定
    /// 那条（`banned_credential_releases_its_devices`）同一个道理。
    #[sqlx::test]
    async fn disabling_or_deleting_a_credential_releases_its_sessions(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        assert_eq!(store.select_for_device(soft_session("s1")).await.unwrap().id, a);
        assert!(store.set_disabled(a, true).await.unwrap());
        assert_eq!(store.session_count(a).await.unwrap(), 0, "停用清绑定");
        assert_eq!(store.select_for_device(soft_session("s1")).await.unwrap().id, b, "改选到 b");
        assert!(store.set_disabled(a, false).await.unwrap());
        assert!(store.mark_banned(b, "[401] revoked").await.unwrap());
        assert_eq!(store.session_count(b).await.unwrap(), 0, "封停清绑定");
        assert_eq!(store.select_for_device(soft_session("s1")).await.unwrap().id, a);
        assert!(store.delete(a).await.unwrap());
        let n = scalar(&store, "SELECT COUNT(*) FROM session_bindings").await;
        assert_eq!(n, 0, "删号不留无主的会话绑定");
    }

    /// 休眠的会话软绑定：TTL 过了名额还回去，会话再来仍优先回原号；保留期过了才真删、
    /// 回来就是新会话。
    #[sqlx::test]
    async fn dormant_session_binding_steers_the_session_back(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        assert_eq!(store.select_for_device(soft_session("s1")).await.unwrap().id, a);
        age_session_binding(&store, "s1", 600).await;
        assert_eq!(store.session_count(a).await.unwrap(), 0, "休眠不占名额");
        assert!(store.list_sessions(a).await.unwrap().is_empty(), "明细口径同计数");
        assert_eq!(store.select_for_device(soft_session("s2")).await.unwrap().id, a);
        // 此刻 a 有 1 条活跃、b 一条没有，纯负载均衡会把 s1 判给 b。
        assert_eq!(
            store.select_for_device(soft_session("s1")).await.unwrap().id,
            a,
            "软绑定带回 a"
        );
        assert_eq!(store.session_count(a).await.unwrap(), 2);
        // 超过保留期：行被清掉，回来就是新会话，按负载均衡落到 b。
        age_session_binding(&store, "s1", 7200).await;
        assert_eq!(store.select_for_device(soft_session("s1")).await.unwrap().id, b);
    }

    /// 软绑定：TTL 过了名额就还回去，但设备再来时仍优先回原号——哪怕负载均衡指向别处。
    ///
    /// 这是 thinking 签名能续上的前提：签名跟着账号走，会话隔一小时再续跑要是换了号，
    /// 之后每一轮都要先撞一次 400 再降级重发（见 `crate::proxy::retry_demoted_thinking`）。
    #[sqlx::test]
    async fn dormant_binding_still_steers_the_device_back_to_its_credential(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);

        assert_eq!(store.select_for_device(soft("dev-1")).await.unwrap().id, a);
        age_binding(&store, "dev-1", 600).await;
        assert_eq!(store.device_count(a).await.unwrap(), 0, "休眠绑定不占名额");

        // 休眠期间来了台新设备：名额是空的，照样分给 a（同优先级取 id 小者）。
        assert_eq!(store.select_for_device(soft("dev-2")).await.unwrap().id, a);
        // 此刻 a 有 1 台活跃设备、b 一台都没有，纯负载均衡会把 dev-1 判给 b。
        assert_eq!(store.device_counts().await.unwrap().get(&b).copied().unwrap_or(0), 0);
        assert_eq!(
            store.select_for_device(soft("dev-1")).await.unwrap().id,
            a,
            "软绑定应把它带回 a"
        );
        assert_eq!(store.device_count(a).await.unwrap(), 2, "回来就重新占名额");
    }

    /// 软绑定是「优先」不是「特权」：原号名额已满时照常改选并改绑，否则设备上限就被绕过了。
    #[sqlx::test]
    async fn dormant_binding_gives_way_when_its_credential_is_full(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        assert!(store.set_device_limit(a, 1).await.unwrap());

        assert_eq!(store.select_for_device(soft("dev-1")).await.unwrap().id, a);
        age_binding(&store, "dev-1", 600).await;
        // 休眠腾出的那个名额被 dev-2 占走。
        assert_eq!(store.select_for_device(soft("dev-2")).await.unwrap().id, a);

        // dev-1 回来时 a 已满：改选到 b，并且绑定要真的改过去（而不是留在 a 上）。
        assert_eq!(store.select_for_device(soft("dev-1")).await.unwrap().id, b);
        assert_eq!(store.device_count(a).await.unwrap(), 1);
        let a_devs: Vec<String> =
            store.list_devices(a).await.unwrap().into_iter().map(|d| d.device_id).collect();
        assert_eq!(a_devs, vec!["dev-2".to_string()]);
        assert_eq!(store.select_for_device(soft("dev-1")).await.unwrap().id, b, "改绑后应稳定在 b");
    }

    /// 账号 RPM 上限：还没定下号的请求撞到上限时溢到下一个号，全部发满才 429
    /// （[`RpmLimited`]，且 `sticky` 为假）。账号自己配的上限盖过全局默认。
    ///
    /// 全程 `rate_limited: false`——RPM 与裸请求上限不同，它不看这个标志，每次选号都计。
    #[sqlx::test]
    async fn rpm_limit_spills_to_next_credential_then_rejects(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        store.set_setting(DEFAULT_RPM_LIMIT, "2").await.unwrap();
        let sel = Select { ttl_secs: 0, ..Default::default() };

        // 前两条落在 a（同优先级、设备数都是 0 时 id 小者先中），第 3、4 条 a 已满 → 溢到 b。
        let mut picked: Vec<i64> = Vec::new();
        for _ in 0..4 {
            picked.push(store.select_for_device(sel).await.unwrap().id);
        }
        assert_eq!(picked, vec![a, a, b, b], "发满了应换号而不是直接拒");

        let err = store.select_for_device(sel).await.unwrap_err();
        let rl = err.downcast_ref::<RpmLimited>().expect("应是 RPM 限流错误");
        assert!(!rl.sticky, "没有设备绑定，拒的是整个候选池而不是某个号");
        assert!(
            (1..=RPM_WINDOW_SECS).contains(&rl.retry_after_secs),
            "重试间隔应落在一个窗口之内，实际 {}",
            rl.retry_after_secs
        );

        // 账号独立上限盖过全局默认：a 单独放宽到 5，窗口里已有的 2 条不妨碍它继续接。
        store.set_rpm_limit(a, 5).await.unwrap();
        assert_eq!(store.select_for_device(sel).await.unwrap().id, a);

        // 「明确不限」（-1）同样盖过全局默认：a 收紧到发不出，只剩 b 可选。
        store.set_rpm_limit(a, 1).await.unwrap();
        store.set_rpm_limit(b, -1).await.unwrap();
        assert_eq!(
            store.select_for_device(sel).await.unwrap().id,
            b,
            "-1 即不限，全局默认不再生效"
        );
    }

    /// 粘性命中的号撞到 RPM 上限时**直接拒**，不改选别的号——改绑会让这条会话之后每一轮都
    /// 先撞一次 thinking 签名 400。窗口松了之后这台设备还在原来那个号上。
    #[sqlx::test]
    async fn rpm_limit_rejects_the_bound_credential_instead_of_rebinding(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        store.set_rpm_limit(a, 1).await.unwrap();
        let sel = Select { device_id: Some("dev-1"), ttl_secs: 0, ..Default::default() };

        assert_eq!(store.select_for_device(sel).await.unwrap().id, a, "新设备先落在 a");

        let err = store.select_for_device(sel).await.unwrap_err();
        let rl = err.downcast_ref::<RpmLimited>().expect("应是 RPM 限流错误");
        assert!(rl.sticky, "拒的是这台设备绑定的那个号");
        assert!(rl.retry_after_secs >= 1, "retry-after 不得为 0，否则客户端立刻再撞一次");
        assert_eq!(store.device_count(b).await.unwrap(), 0, "b 空着也不该被改绑过去");

        // 放宽上限即刻恢复，且仍是原来那个号——绑定自始至终没被动过。
        store.set_rpm_limit(a, 10).await.unwrap();
        assert_eq!(store.select_for_device(sel).await.unwrap().id, a);
    }

    /// 窗口滚过去之后名额自己回来（口径同裸请求那条，只是窗口固定 60 秒）。
    #[sqlx::test]
    async fn rpm_window_expires_and_frees_the_slot(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];
        store.set_rpm_limit(a, 1).await.unwrap();
        let sel = Select { ttl_secs: 0, ..Default::default() };

        assert!(store.select_for_device(sel).await.is_ok());
        assert!(store.select_for_device(sel).await.is_err(), "同一窗口内第二条应被拦");

        // 直接把窗口内的那条时间戳推到过期，等价于等了一个窗口。
        {
            let mut hits = store.rpm_rate.hits.lock();
            for q in hits.values_mut() {
                for t in q.iter_mut() {
                    *t -= Duration::from_secs(RPM_WINDOW_SECS as u64 + 1);
                }
            }
        }
        assert!(store.select_for_device(sel).await.is_ok(), "过期后名额应回收");
    }

    /// 上游 429 打过冷却的号在选号时让位；绑定到它的设备**改绑**到新号（这正是 429 换号
    /// 重试要的语义）；冷却结束后不自动回迁——粘性以最后一次选择为准。
    #[sqlx::test]
    async fn cooldown_makes_device_rebind_to_another_credential(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);

        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            a
        );
        store.mark_rate_limited(a, None, Duration::from_secs(300));
        assert!(store.rate_limited_secs(a) > 0, "应处于冷却中");

        // 绑定还在 a 上，但 a 在冷却 → 改选 b，并把绑定迁过去。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            b
        );
        assert_eq!(store.list_devices(b).await.unwrap().len(), 1, "设备应已改绑到 b");
        assert!(store.list_devices(a).await.unwrap().is_empty(), "a 上不该再留着这台设备");
    }

    /// 模型级冷却只挡那一个模型：fable 被容量限制时，同一个号的 sonnet/opus 照常可用。
    /// 这是「窗口没跑满却 429」那种情况的正解——号是好的，赶走整个号纯属自伤。
    #[sqlx::test]
    async fn model_scoped_cooldown_only_blocks_that_model(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let pick = async |model: &str| {
            store
                .select_for_device(Select { model: Some(model), ..Default::default() })
                .await
                .unwrap()
                .id
        };

        store.mark_rate_limited(a, Some("claude-fable-5"), Duration::from_secs(300));
        assert_eq!(pick("claude-fable-5").await, b, "fable 应让位给 b");
        assert_eq!(pick("claude-sonnet-5").await, a, "同一个号的其它模型不该被牵连");
        // 模型级冷却不算「账号被限流」，控制台不该显示成账号出了问题。
        assert_eq!(store.rate_limited_secs(a), 0, "模型级冷却不计入账号级展示");

        // 账号级冷却则对所有模型生效。
        store.mark_rate_limited(a, None, Duration::from_secs(300));
        assert_eq!(pick("claude-sonnet-5").await, b, "账号级冷却应挡下所有模型");
        assert!(store.rate_limited_secs(a) > 0);

        // 连通性测试成功那种「带模型」的解除：清账号级 + 被测模型格，别的模型格不动。
        store.clear_rate_limited(a, Some("claude-sonnet-5"));
        assert_eq!(store.rate_limited_secs(a), 0, "账号级冷却应已解除");
        assert_eq!(pick("claude-sonnet-5").await, a, "被测模型应立即可用");
        assert_eq!(pick("claude-fable-5").await, b, "sonnet 通了证明不了 fable 通，那一格要留着");

        // 手动解除：全部格一起清。
        store.clear_rate_limited(a, None);
        assert_eq!(pick("claude-fable-5").await, a, "手动解除后所有模型都该回来");
    }

    /// fable/mythos 在同一优先级档内 Max 号优先、等级未知其次、Pro 垫底；其它模型不受影响，
    /// 管理员设的优先级始终压过等级。
    #[sqlx::test]
    async fn premium_models_prefer_max_accounts_within_a_priority_tier(pool: PgPool) {
        let (store, ids) = store_with(pool, &["pro", "max", "unknown"]).await;
        let (pro, max, unknown) = (ids[0], ids[1], ids[2]);
        store.set_tier(pro, Some("Pro")).await.unwrap();
        store.set_tier(max, Some("Max 20x")).await.unwrap();
        let pick = async |model: &str| {
            store
                .select_for_device(Select { model: Some(model), ..Default::default() })
                .await
                .unwrap()
                .id
        };
        assert_eq!(pick("claude-fable-5-1").await, max, "fable 先落 Max");
        assert_eq!(pick("claude-sonnet-5").await, pro, "非高档模型不排等级，按 id 兜底");
        store.set_disabled(max, true).await.unwrap();
        assert_eq!(pick("claude-fable-5-1").await, unknown, "Max 不在时等级未知的排在 Pro 前面");
        store.set_disabled(max, false).await.unwrap();
        store.set_priority(pro, 1, PRIORITY_MIN).await.unwrap();
        assert_eq!(pick("claude-fable-5-1").await, pro, "优先级是主键，等级只在同档内排");
    }

    /// 瞬时限速（容量 / 请求速率）也走选号门禁，gate 时长由 ladder 退避值决定（起步 2s）。
    ///
    /// 与额度池满那档的区别只在持续时间：瞬时 gate 很短，避免同一个号被反复轰出 429；
    /// 但不至于像 30s/60s 门禁那样让整池级联封死——ladder 每个号独立从 2s 起步，交错过期。
    #[sqlx::test]
    async fn transient_rate_limit_gates_selection_with_short_cooldown(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let pick = async |model: &str| {
            store
                .select_for_device(Select { model: Some(model), ..Default::default() })
                .await
                .unwrap()
                .id
        };

        // 瞬时限速走 gate：被标记的号不参与选号。
        store.mark_rate_limited(a, Some("claude-opus-5"), Duration::from_secs(2));
        assert_eq!(pick("claude-opus-5").await, b, "瞬时限速的短 gate 也应挡住选号");
        assert_eq!(store.rate_limited_secs(a), 0, "不该冒充账号级限流");

        let models = store.rate_limited_models(a);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].0, "claude-opus-5");
        assert!(models[0].2, "瞬时限速现在也走 gate，gated 应为 true");

        // 额度那档叠上去：长 gate 覆盖短 gate（取较晚的截止时刻）。
        store.mark_rate_limited(a, Some("claude-opus-5"), Duration::from_secs(300));
        assert_eq!(pick("claude-opus-5").await, b, "额度池满那档仍是硬门禁");
        let models = store.rate_limited_models(a);
        assert_eq!(models.len(), 1, "同一个模型只该出现一行");
        assert!(models[0].2, "此刻挂着门禁，gated 应为 true");
        assert!(models[0].1 > 290, "展示的剩余时间应反映较长的那个门禁：{models:?}");
    }

    /// 全部号都因限流暂停时，回的是 429 + 最早恢复时刻，而不是「没有可用凭证，请先登录」——
    /// 后者会把人引去查登录，实际上号都在，只是在等额度回血。
    #[sqlx::test]
    async fn all_paused_reports_rate_limit_not_missing_credentials(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let now = crate::credentials::now_secs();
        store.pause_for_rate_limit(ids[0], "限流", now + 5 * 3600).await.unwrap();
        store.pause_for_rate_limit(ids[1], "限流", now + 2 * 3600).await.unwrap();

        let err = store.select_for_device(Select::default()).await.expect_err("全员暂停应报错");
        let rl = err.downcast_ref::<AllRateLimited>().expect("应是限流错误而非「没有可用凭证」");
        assert!(
            (2 * 3600 - 5..=2 * 3600).contains(&rl.retry_after_secs),
            "应给出最早恢复的那个，实得 {}",
            rl.retry_after_secs
        );
    }

    /// 冷却是**硬门禁**：全部号都在冷却时直接拒（429 + retry-after），不再退回照常选。
    /// 另外「本次已试过的号」（换号重试传进来的排除集）一律出局，重试不会再撞同一个号。
    #[sqlx::test]
    async fn cooldown_is_a_hard_gate_and_exclusions_are_hard(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        store.mark_rate_limited(a, None, Duration::from_secs(300));
        store.mark_rate_limited(b, None, Duration::from_secs(600));

        // 都在冷却 → 拒绝调度，并给出最早解冻那个号（a，300s）的剩余时间。
        let err = store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .await
            .expect_err("全员冷却时不该选出任何号");
        let rl = err.downcast_ref::<AllRateLimited>().expect("应是冷却硬门禁错误");
        assert!(
            (290..=300).contains(&rl.retry_after_secs),
            "retry-after 应取最早解冻的那个号，实得 {}",
            rl.retry_after_secs
        );

        // 逃生口：手动解除 a 的冷却后立刻可用。
        store.clear_rate_limited(a, None);
        assert_eq!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            a
        );
        // 排除集是硬的：a 已试过 → 只能是 b……但 b 还在冷却，硬门禁下同样拒绝。
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[a],
                    ..Default::default()
                })
                .await
                .is_err(),
            "唯一剩下的候选在冷却中 → 拒绝"
        );
        store.clear_rate_limited(b, None);
        assert_eq!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[a],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            b
        );
        // 两个都试过 → 明确报错，让调用方把最初那条 429 透传回去。
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[a, b],
                    ..Default::default()
                })
                .await
                .is_err()
        );
    }

    /// 不计费的路径（`count_tokens` 等，`rate_limited = false`）不占名额：它不产生 usage、
    /// 不消耗额度，拿它占名额只会把真正的请求挤掉，而客户端的 token 预估全靠它。
    #[sqlx::test]
    async fn non_billable_paths_do_not_consume_rate_slots(pool: PgPool) {
        let (store, _) = store_with(pool, &["a"]).await;
        store.set_setting(BARE_RATE_LIMIT, "1").await.unwrap();

        // 不计入的路径打多少条都不占名额。
        for _ in 0..5 {
            assert!(
                store
                    .select_for_device(Select {
                        ttl_secs: 0,
                        rate_limited: false,
                        exclude: &[],
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
        }
        // 名额仍是满的一格：计费路径的第一条照常放行，第二条才被拦。
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .is_ok()
        );
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .is_err(),
            "计费路径应照常受限"
        );
        // 被拦之后，不计费的路径依然畅通。
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: false,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .is_ok()
        );
    }

    /// 窗口过期后名额自动回收；窗口取值非法（0/负数）时退回默认，不会把人永久锁死。
    #[sqlx::test]
    async fn bare_rate_window_expires_and_rejects_bad_config(pool: PgPool) {
        let (store, _) = store_with(pool, &["a"]).await;
        store.set_setting(BARE_RATE_LIMIT, "1").await.unwrap();
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .is_ok()
        );
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .is_err(),
            "同一窗口内第二条应被拦"
        );

        // 直接把窗口内的那条时间戳推到过期，等价于等了一个窗口。
        {
            let mut hits = store.bare_rate.hits.lock();
            for q in hits.values_mut() {
                for t in q.iter_mut() {
                    *t -= Duration::from_secs(DEFAULT_BARE_RATE_WINDOW_SECS as u64 + 1);
                }
            }
        }
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .is_ok(),
            "过期后名额应回收"
        );

        store.set_setting(BARE_RATE_WINDOW_SECS, "0").await.unwrap();
        assert_eq!(
            store.bare_rate_window_secs(),
            DEFAULT_BARE_RATE_WINDOW_SECS,
            "非法窗口退回默认"
        );
    }

    /// 并发选号也守得住名额与槽位：rusqlite 版靠单写连接天然串行，PG 版靠
    /// [`CredentialStore::begin_write`]。十台新设备同时来抢两个各限 1 台的号，只能进来两台；十条新会话
    /// 同时来抢一个号，槽位互不相同、正好是 0..10。
    #[sqlx::test]
    async fn concurrent_selections_respect_limits_and_slots(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        store.set_setting(DEFAULT_DEVICE_LIMIT, "1").await.unwrap();
        let store = std::sync::Arc::new(store);
        let devices: Vec<String> = (0..10).map(|i| format!("dev-{i}")).collect();
        let mut tasks = Vec::new();
        for d in devices.clone() {
            let s = store.clone();
            tasks.push(tokio::spawn(async move { s.select_for_device(soft(&d)).await }));
        }
        let mut won = Vec::new();
        for t in tasks {
            match t.await.unwrap() {
                Ok(c) => won.push(c.id),
                Err(e) => assert!(e.downcast_ref::<DeviceLimitReached>().is_some(), "{e}"),
            }
        }
        won.sort();
        assert_eq!(won, ids, "每个号正好进来一台");
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM device_bindings").await, 2);

        let mut tasks = Vec::new();
        for i in 0..10 {
            let s = store.clone();
            let key = format!("s{i}");
            let only_a = [ids[1]];
            tasks.push(tokio::spawn(async move {
                let sel = Select { exclude: &only_a, ..soft_session(&key) };
                s.select_with_slot(sel).await.map(|(c, slot)| (c.id, slot))
            }));
        }
        let mut slots = Vec::new();
        for t in tasks {
            let (cid, slot) = t.await.unwrap().unwrap();
            assert_eq!(cid, ids[0]);
            slots.push(slot.unwrap());
        }
        slots.sort();
        assert_eq!(slots, (0..10).collect::<Vec<i64>>(), "槽位不重不漏");
    }

    /// 标识与模型名里带 NUL 的来访照常选号（PG 的 TEXT 不收 NUL），去掉 NUL 后按同一个键粘住。
    #[sqlx::test]
    async fn nul_in_client_ids_is_stripped(pool: PgPool) {
        let (store, ids) = soft_store(pool, &["a", "b"]).await;
        let dev = Select {
            device_id: Some("dev\0-1"),
            session_key: None,
            model: Some("claude\0-x"),
            ..soft_session("")
        };
        assert_eq!(store.select_for_device(dev).await.unwrap().id, ids[0]);
        assert!(store.device_is_known("dev-1").await);
        assert!(store.device_is_known("dev\0-1").await);
        assert_eq!(store.select_for_device(soft_session("s\0k")).await.unwrap().id, ids[0]);
        assert!(store.session_slot(ids[0], "sk").await.unwrap().is_some());
    }

    /// 别的连接正在恢复同一个号（控制台列表的惰性恢复）时选号：等它提交后要看得见这个号。
    /// 恢复与读取并成一条语句时，快照里它还停着、CTE 的 `UPDATE` 重判后又不返回它，两路
    /// 都漏掉，池里只有它一个号就报没有可用账号。见 [`candidates_sql`]。
    #[sqlx::test]
    async fn select_sees_a_credential_resumed_by_another_connection(pool: PgPool) {
        let store = std::sync::Arc::new(CredentialStore::for_test(pool.clone()).await);
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap();
        let now = crate::credentials::now_secs();
        store.pause_for_rate_limit(a.id, "到点", now - 1).await.unwrap();

        // 另一条连接开着事务把它恢复了、先不提交，行锁攥在手里。
        let mut other = pool.begin().await.unwrap();
        assert_eq!(CredentialStore::resume_due(&mut other).await.unwrap(), 1);

        let s = store.clone();
        let pick = tokio::spawn(async move {
            s.select_for_device(Select { ttl_secs: 0, ..Default::default() }).await.map(|c| c.id)
        });
        // 让选号先跑到等行锁那一步，再提交恢复。
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!pick.is_finished(), "选号应在等另一条连接的行锁");
        other.commit().await.unwrap();
        assert_eq!(pick.await.unwrap().unwrap(), a.id);
    }
}
