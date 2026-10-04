//! 凭证记录模型与刷新判定。持久化在 SQLite，见 [`crate::store`]。

use std::collections::{BTreeSet, HashMap};
use std::ops::Bound;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config;

/// 调度优先级的取值范围：P0..=P4，数值小者优先，P0 最先调度。档位少而各有含义
/// （见 admin-ui 的档位名）：同档内按设备数负载均衡，跨档按先后顺序用尽。
pub const PRIORITY_MIN: i64 = 0;
pub const PRIORITY_MAX: i64 = 4;
/// 新账号落在中间档 P2（常规）：要优先就往小调、要垫底就往大调，都不用动其余账号。
/// 同档内按设备数负载均衡，新号加进来立刻参与分摊。
pub const PRIORITY_DEFAULT: i64 = 2;

/// 把旧口径的一组优先级按名次压到 P0..=P4，返回「旧值 → 新档」。`mid` 是旧口径的默认档
/// （最早是 0，后来 P1..=P100 时是 50）：等于它的落 P2；比它小的里最靠近的一个值落 P1、
/// 其余落 P0；比它大的里最靠近的一个值落 P3、其余落 P4。先后顺序不变，档位多于 5 个时
/// 两头的会并档。
pub fn priority_tiers_by_rank(
    values: impl IntoIterator<Item = i64>,
    mid: i64,
) -> HashMap<i64, i64> {
    let distinct: BTreeSet<i64> = values.into_iter().collect();
    let mut map = HashMap::from([(mid, PRIORITY_DEFAULT)]);
    for (i, &p) in distinct.range(..mid).rev().enumerate() {
        map.insert(p, if i == 0 { PRIORITY_DEFAULT - 1 } else { PRIORITY_MIN });
    }
    for (i, &p) in distinct.range((Bound::Excluded(mid), Bound::Unbounded)).enumerate() {
        map.insert(p, if i == 0 { PRIORITY_DEFAULT + 1 } else { PRIORITY_MAX });
    }
    map
}

/// 一条 Claude OAuth 凭证（对应 SQLite 一行）。
#[derive(Debug, Clone)]
pub struct Credential {
    pub id: i64,
    /// 用户可编辑的显示名（如账号备注）。
    pub label: String,
    /// 账号等级（Max / Pro / Free 等），可能未知。团队号取的是组织的额度档
    /// （`rate_limit_tier`），见 [`crate::oauth::tier_from_rate_limit`]。
    pub tier: Option<String>,
    /// 组织类型原值（`claude_team`/`claude_enterprise`/`claude_max`…），来自
    /// `/api/oauth/profile` 的 `organization.organization_type`；`None` 表示没拉到。
    ///
    /// 单独存一列而不是并进 [`Self::tier`]：团队号是组织下的一个席位（额度按席位算），
    /// 与个人号不是一回事，界面上得能一眼分开。
    pub org_type: Option<String>,
    /// 额度档**原值**（`default_claude_max_5x` 之类），来自 `/api/oauth/profile` 的
    /// `organization.rate_limit_tier`；`None` 表示没拉到（旧库、或 profile 里就没有）。
    ///
    /// 与 [`Self::tier`] 分开存：那个是展示串（`Max 5x`），这个是 statsig eval 的
    /// `attributes.rateLimitTier` 要原样发出去的值，见 [`crate::oauth::KeepaliveCtx`]。
    pub rate_limit_tier: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    /// access_token 过期的 Unix 时间戳（秒）。
    pub expires_at: u64,
    /// 调度优先级：数值小者优先，取值 [`PRIORITY_MIN`]..=[`PRIORITY_MAX`]。
    pub priority: i64,
    /// 是否停用（停用的凭证不参与转发）。
    pub disabled: bool,
    /// 允许绑定的设备数上限；`<= 0` 表示不限。见 [`crate::store`] 的粘性绑定选择。
    pub device_limit: i64,
    /// 允许同时活跃的**模拟会话**数上限，三态同 [`Self::device_limit`]：`> 0` 独立上限；`0`
    /// 跟随全局默认 [`crate::store::DEFAULT_SESSION_LIMIT`]；`< 0` 明确不限。只管模拟路径上
    /// 没有设备身份的来访，见 `crate::store::Select::session_key`。
    pub session_limit: i64,
    /// 该账号每分钟最多转发多少条请求（RPM 上限）。三态同 [`Self::device_limit`]：
    /// `> 0` 本账号独立上限；`0` 跟随全局默认；`< 0` 本账号明确不限。
    /// 生效值见 [`crate::store::effective_rpm_limit`]，计数窗口见 `crate::store` 里的选号。
    pub rpm_limit: i64,
    /// 该账号自己的「额度用到多少就提前停调度」阈值（**5h 窗口**，百分比）。
    /// `None` 跟随全局 [`crate::store::QUOTA_PAUSE_PCT`]；`Some(0)` 本账号这一档不停；
    /// `Some(1..=100)` 本账号独立阈值。生效值见 [`crate::store::effective_quota_pause_pct`]。
    ///
    /// 与 [`Self::rpm_limit`] 的三态编码不同（那边 `0` 是「跟随」）：这里 `0` 本身就是一个
    /// 有意义的取值（关），「跟随」只能用 NULL 表达。
    pub quota_pause_pct: Option<i64>,
    /// 同上，**7d 窗口**那一档；`None` 跟随全局 [`crate::store::QUOTA_PAUSE_PCT_7D`]。
    pub quota_pause_pct_7d: Option<i64>,
    /// 自动检测到的上游账号级错误原因（如封号）；`None` 表示未被自动停用
    /// （手动停用或未停用皆为 `None`）。见 [`crate::store::CredentialStore::record_ban`]。
    pub ban_reason: Option<String>,
    /// 账号 UUID（来自 `/api/oauth/profile` 的 `account.uuid`）；转发时用于身份伪装。
    pub account_uuid: Option<String>,
    /// 组织 UUID（profile 的 `organization.uuid`，交换响应的 `organization.uuid` 兜底）。
    ///
    /// 遥测 `auth` 块与 eval 的 `organizationUUID` 要它。此前唯一来源是这个号最近一次
    /// `/v1/messages` 响应头里的 `anthropic-organization-id`——刚登录、或久没转发过请求时
    /// 就缺省，而官方每条都带（345/345）。真实客户端也是登录时从 profile 存下来的
    /// （`storeOAuthAccountInfo` 的 `organizationUuid`）。响应头学到的值仍优先。
    pub org_uuid: Option<String>,
    /// 订阅创建时刻，profile 的 `organization.subscription_created_at` **原串**（ISO 8601）。
    /// eval 的 `subscriptionCreatedAt` 发它换算成的毫秒数；`None` 表示还没拉到，那一项不发。
    pub subscription_created_at: Option<String>,
    /// 组织名称（profile 的 `organization.name`）。只给后台看。
    pub org_name: Option<String>,
    /// 席位档原值（`organization.seat_tier`，团队号如 `team_standard`）；个人号为空。
    pub seat_tier: Option<String>,
    /// 订阅状态原值（`organization.subscription_status`，如 `active`）。只给后台看。
    pub subscription_status: Option<String>,
    /// 组织是否开了超额用量（`organization.has_extra_usage_enabled`）；`None` 为还没拉到。
    pub extra_usage_enabled: Option<bool>,
    /// 被上游限流自动停用后，**到点自动重新启用**的 Unix 时间戳（秒）；`None` 表示不自动
    /// 恢复（人工停用、封号，或压根没停用）。
    ///
    /// 这一列是「限流停用」与「人工/封号停用」的唯一区分点：
    /// [`crate::store::CredentialStore::select_for_device`] 惰性把到点的号启用回来，
    /// 连通性测试通过也只会自动启用 `resume_at` 非空的号——人工关掉的不该被一次测试打开。
    /// 见 [`crate::store::CredentialStore::pause_for_rate_limit`]。
    pub resume_at: Option<u64>,
    /// 该账号专用的出站代理（`socks5://`/`socks5h://`/`http://` 等）；`None` 或空串表示直连。
    ///
    /// 配了之后这个号的**全部**出站流量都走它——转发、token 刷新、profile、连通性测试。
    /// 漏掉任何一条都会让那条请求带着真实出口 IP 打到上游，逐账号隔离当场失效，
    /// 且日志上看不出来。故取客户端只有 [`crate::clients::ClientPool::for_credential`]
    /// 一个入口，且建不出客户端时报错而不是退回直连。
    pub proxy: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Credential {
    /// profile 那几列还有没拉到的：账号 UUID、额度档原值、组织 UUID、订阅创建时刻、组织名称。
    /// [`crate::store::ensure_fresh_token`] 据此决定刷新后要不要顺手拉一次 profile。
    /// `tier` / `org_type` 不算——它们在 profile 里也可能就是空的（免费号），拿它们判会让
    /// 那类号每次刷新都多一次往返。
    pub fn profile_incomplete(&self) -> bool {
        let missing = |v: &Option<String>| v.as_deref().map(str::trim).is_none_or(str::is_empty);
        missing(&self.account_uuid)
            || missing(&self.rate_limit_tier)
            || missing(&self.org_uuid)
            || missing(&self.subscription_created_at)
            || missing(&self.org_name)
    }

    /// 距离过期的剩余秒数（已过期返回 0）。
    pub fn expires_in_secs(&self) -> u64 {
        self.expires_at.saturating_sub(now_secs())
    }

    /// 该凭证对上游呈现的稳定伪装 device_id：`sha256(account_uuid ⊕ 设备指纹)` 的 64 位
    /// 小写 hex，与原生 Claude Code 的 device_id 格式一致。
    ///
    /// 叠加 `fingerprint`（客户端原始 device_id + 平台 arch/os）后：
    /// - 同一真实设备恒定不变；
    /// - 不同设备（如 arm mac / windows）得到不同 device_id，避免同一 id 在上游出现
    ///   自相矛盾的平台头。
    ///
    /// `fingerprint` 为空则退化为仅按账号派生（等价单设备）。
    /// 无 `account_uuid` 时返回 `None`（转发时退化为透传客户端原值）。
    pub fn spoof_device_id(&self, fingerprint: &str) -> Option<String> {
        use sha2::{Digest, Sha256};
        let uuid = self.account_uuid.as_deref()?.trim();
        if uuid.is_empty() {
            return None;
        }
        let mut hasher = Sha256::new();
        hasher.update(uuid.as_bytes());
        if !fingerprint.is_empty() {
            hasher.update([0u8]); // 分隔符，避免拼接歧义
            hasher.update(fingerprint.as_bytes());
        }
        Some(hex_lower(&hasher.finalize()))
    }

    /// 是否被系统封禁（上游 401/403、refresh_token 撤销、代理异常等）。
    /// 区别于手动停用（`ban_reason` 为 `None`）、限速暂停（`resume_at` 非空）和订阅未生效暂停
    /// （[`Self::is_subscription_paused`]）。
    pub fn is_banned(&self) -> bool {
        self.disabled
            && self.ban_reason.is_some()
            && self.resume_at.is_none()
            && !self.is_subscription_paused()
    }

    /// 是否因订阅未生效而暂停（见 [`crate::store::CredentialStore::suspend_for_inactive_subscription`]）。
    /// 与封号同形（disabled + ban_reason、`resume_at` 空），只能按原因开头的固定格式认
    /// （[`crate::store::is_subscription_pause_reason`]）。
    ///
    /// **不能算进 [`Self::is_banned`]**：保活会整个跳过封禁的号，而这一档往往一停几周
    /// （等续费）——保活不跑，refresh_token 就在闲置里过期，续费后第一次连通性测试刷新失败，
    /// 号被当成 token 作废永久封掉。保活对它只刷 token、不发别的端点（那些每一发都是 403），
    /// 遥测也不发，见 `web.rs` 的保活循环与 `telemetry` 的 flush。
    pub fn is_subscription_paused(&self) -> bool {
        self.disabled
            && self.resume_at.is_none()
            && self.ban_reason.as_deref().is_some_and(crate::store::is_subscription_pause_reason)
    }

    /// 是否已过期或即将过期（进入刷新窗口）。
    pub fn needs_refresh(&self) -> bool {
        self.expires_in_secs() <= config::REFRESH_LEEWAY_SECS
    }
}

/// 模拟路径的会话 id：`sha256("luban-session" ‖ 账号键 ‖ seed)` 取前 16 字节按 uuid v4
/// 形态格式化。同一账号同一 seed 恒定，换账号或换 seed 即不同。账号键见
/// [`session_account_key`]：有 `account_uuid` 用它，没有就用凭证 id——两个都没有 uuid 的号
/// 不能算出同一组会话 id，否则换号后上游会在两个组织下看到同一个会话 uuid。
///
/// seed 正常是**槽位**（[`slot_session_seed`]）：每个账号的模拟会话 id 是一组固定的槽位，
/// 数量就是会话上限，对话占哪个槽位就用哪个 id，槽位释放后下一个对话**复用**同一个 id——
/// 上游看到的每个账号只在这几个会话 id 之间轮转，与设备 id 恒定的做法一致，而不是每个对话
/// 一个新 uuid、时间一长无限增多。没有槽位可占的（带设备身份、不写会话绑定的模拟请求，或
/// luban 自己发的探测）退回按缓存前缀或固定 seed 派生。
///
/// 前缀是为了和 [`Credential::spoof_device_id`] 分开取值——同样的输入派生出两个字段，不加
/// 区分前缀就会得到「device_id 与 session_id 的高位相同」这种真实客户端不产生的相关性。
pub fn derive_session_id(account_key: &str, seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"luban-session\0");
    h.update(account_key.as_bytes());
    h.update([0u8]);
    h.update(seed.as_bytes());
    let digest = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    crate::proxy::uuid_from_bytes(b)
}

/// 派生会话 id 时代表「哪个账号」的键：`account_uuid`（去空白、非空）优先，没有就用
/// `cred:<凭证 id>`。凭证 id 不复用（AUTOINCREMENT），删号重登也是新的一组会话 id。
/// 转发路径与后台列表都走这里，两边不会漂开。
pub fn session_account_key(account_uuid: Option<&str>, cred_id: i64) -> String {
    match account_uuid.map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => u.to_string(),
        None => format!("cred:{cred_id}"),
    }
}

/// 第 `slot` 个会话槽位的 seed（`slot:<n>`），见 [`derive_session_id`]。
pub fn slot_session_seed(slot: i64) -> String {
    format!("slot:{slot}")
}

/// 第 `slot` 个会话槽位的会话 id，见 [`derive_session_id`]。后台列会话时用它算出上游看到的
/// 那个 uuid，与转发路径同一个函数，两边不会漂开。
pub fn sim_slot_session_id(account_uuid: Option<&str>, cred_id: i64, slot: i64) -> String {
    derive_session_id(&session_account_key(account_uuid, cred_id), &slot_session_seed(slot))
}

/// 把字节切片编码为小写十六进制字符串。
pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 当前 Unix 时间戳（秒）。
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod priority_tests {
    use super::*;

    /// 按名次压档：默认档落 P2，两侧最靠近的各占 P1/P3，再往外的并进 P0/P4。
    #[test]
    fn tiers_by_rank_keeps_order_and_merges_extremes() {
        let map = priority_tiers_by_rank([1, 30, 49, 50, 50, 51, 77, 100], 50);
        let got: Vec<i64> = [1, 30, 49, 50, 51, 77, 100].iter().map(|p| map[p]).collect();
        assert_eq!(got, vec![0, 0, 1, 2, 3, 4, 4]);

        let legacy = priority_tiers_by_rank([-3, 0, 2], 0);
        assert_eq!((legacy[&-3], legacy[&0], legacy[&2]), (1, 2, 3), "最早的口径默认 P0");
        assert_eq!(priority_tiers_by_rank([], 50)[&50], PRIORITY_DEFAULT);
    }
}
