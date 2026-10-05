//! 凭证视图：控制台展示用的 [`CredentialView`] 与默认上限。

use super::*;

// ---------- 视图与错误 ----------

/// 一个模型当前的冷却剩余时间，见 [`CredentialView::rate_limited_models`]。
#[derive(Serialize)]
struct ModelCooldown {
    model: String,
    secs: i64,
    /// 这条冷却是否**挡着选号**。现在额度池满和瞬时限速两档都走门禁（总为 `true`），
    /// 区别在于持续时间：瞬时限速从 2s 起步（ladder 退避），远短于额度池满那一档。
    gated: bool,
}

/// 构造凭证视图时要用到的两个全局默认上限。
///
/// 包成结构体而不是并列两个 `i64` 参数：位置写反了照样编译得过，而那是一个「把设备上限当成
/// RPM 上限算」的静默错误——同 [`store::Select`] 的理由。
#[derive(Clone, Copy)]
pub(super) struct DefaultLimits {
    /// 全局默认设备数上限，见 [`store::CredentialStore::default_device_limit`]。
    device: i64,
    /// 全局默认模拟会话数上限，见 [`store::CredentialStore::default_session_limit`]。
    session: i64,
    /// 全局默认账号 RPM 上限，见 [`store::CredentialStore::default_rpm_limit`]。
    rpm: i64,
    /// 全局的提前停调度阈值（5h 档），见 [`store::CredentialStore::quota_pause_pct`]。
    quota_pct: i64,
    /// 带设备身份的来访按会话占名额、设备上限不生效，见 [`store::ForwardFlags::devices_by_session`]。
    devices_by_session: bool,
    /// 同上，7d 档，见 [`store::CredentialStore::quota_pause_pct_7d`]。
    quota_pct_7d: i64,
}

impl DefaultLimits {
    pub(super) fn of(store: &store::CredentialStore) -> Self {
        Self {
            device: store.default_device_limit(),
            session: store.default_session_limit(),
            rpm: store.default_rpm_limit(),
            quota_pct: store.quota_pause_pct(),
            devices_by_session: store.forward_flags().devices_by_session(),
            quota_pct_7d: store.quota_pause_pct_7d(),
        }
    }
}

/// 对外暴露的凭证视图（不返回明文 token）。
#[derive(Serialize)]
pub(super) struct CredentialView {
    pub(super) id: i64,
    label: String,
    tier: Option<String>,
    /// 组织类型原值（`claude_team`/`claude_enterprise`/…）。前端据此给团队号单独打标——
    /// 团队号是组织下的一个席位，跟个人号不是一回事。
    org_type: Option<String>,
    /// 额度档原值（`default_claude_max_5x`/`default_raven`…）。团队号的徽章颜色看它。
    rate_limit_tier: Option<String>,
    /// 订阅创建时刻原串（`organization.subscription_created_at`，ISO 8601），徽章提示里显示。
    subscription_created_at: Option<String>,
    /// profile 里只给后台看的几项：组织名称、席位档原值、订阅状态原值、超额用量开关。
    org_name: Option<String>,
    seat_tier: Option<String>,
    subscription_status: Option<String>,
    extra_usage_enabled: Option<bool>,
    priority: i64,
    disabled: bool,
    expires_in: u64,
    /// 过期时刻（Unix 秒）。前端展示用它而非 `expires_in`：倒计时要么静止要么得自己走，
    /// 而绝对时刻渲染多少次都是同一个值，也不受浏览器时钟偏差影响。
    expires_at: u64,
    expired: bool,
    created_at: u64,
    updated_at: u64,
    /// 账号自身的设备上限设置：`> 0` 独立上限；`0` 跟随全局默认；`< 0` 明确不限。
    device_limit: i64,
    /// 实际生效的设备上限（已套用全局默认）；0 表示不限。设备按会话占名额时
    /// （[`store::ForwardFlags::devices_by_session`]）设备不再受限，恒为 0。
    device_limit_effective: i64,
    /// 设备上限是否生效：设备按会话占名额时为假，前端据此隐藏设备上限的配置与「占满」提示。
    device_limit_applies: bool,
    /// 当前已绑定的设备数。
    device_count: i64,
    /// 账号自身的会话上限设置：`> 0` 独立上限；`0` 跟随全局默认；`< 0` 明确不限。
    /// 管模拟路径上没有设备身份的来访，以及按会话占名额时的带设备来访，见
    /// `store::Select::per_session`。
    session_limit: i64,
    /// 实际生效的会话上限（已套用全局默认）；0 表示不限。
    session_limit_effective: i64,
    /// 当前活跃的会话绑定数（TTL 内），口径同 `device_count`。
    session_count: i64,
    /// 账号自身的 RPM 上限设置：`> 0` 独立上限；`0` 跟随全局默认；`< 0` 明确不限。
    rpm_limit: i64,
    /// 实际生效的 RPM 上限（已套用全局默认）；0 表示不限。前端拿它和 `rpm` 一起显示成
    /// 「12 / 30」，两个数同一个窗口（最近 60 秒），可以直接比。
    rpm_limit_effective: i64,
    /// 账号自身的提前停调度阈值（5h 档，百分比）：`null` 跟随全局；`0` 本账号这一档不停；
    /// `1..=100` 独立阈值。见 [`crate::credentials::Credential::quota_pause_pct`]。
    quota_pause_pct: Option<i64>,
    /// 同上，7d 档。
    quota_pause_pct_7d: Option<i64>,
    /// 5h 档实际生效的阈值（已套用全局）；`0` = 这一档不停。
    quota_pause_pct_effective: i64,
    /// 7d 档实际生效的阈值（已套用全局）；`0` = 这一档不停。
    quota_pause_pct_7d_effective: i64,
    /// 自动检测到的上游账号级错误原因（如封号）；`None` 表示未被自动停用。
    ban_reason: Option<String>,
    /// 该账号专用的出站代理；`None` 表示直连。**原样返回、不脱敏**：代理串里可能带账号密码，
    /// 但这是个已经过管理鉴权的接口，而把它打码会让人没法确认自己配的到底是哪一条。
    /// 访客看到的由鉴权中间件统一去掉密码，见 [`auth::redact_url_credentials`]。
    proxy: Option<String>,
    /// `proxy` 在代理池里对应那条的 id（按 URL 全等），不在池里或直连为 `None`。前端按它查
    /// 代理名称：访客拿到的 URL 已去掉密码，只差密码的两条代理打码后长得一样，按 URL 查会串位。
    pub(super) proxy_id: Option<i64>,
    /// 脱敏后的 refresh_token（前缀 + 尾 4 位），仅用于界面区分。
    token_hint: String,
    /// 最新一次的订阅额度快照（无请求记录时为 None）。
    quota: Option<store::QuotaSnapshot>,
    /// 最近一次被使用（转发请求）的时间戳（Unix 秒）；从未使用为 None。
    last_used: Option<i64>,
    /// 累计等价 API 费用（USD）。
    cost_total: f64,
    /// 当前 RPM：最近 60 秒经该账号转发的请求数（见 [`store::CredentialStore::recent_rpm`]）。
    ///
    /// 与 `quota.requests_5h/7d` 不同，它不依赖上游限流头——那两个要等一条带头的响应才刷新，
    /// 且窗口起点由 `reset` 反推，看不出「此刻压了多少」。
    rpm: i64,
    /// **账号级**进程内冷却的剩余秒数；`0` 表示不在冷却中。
    ///
    /// 正常路径上这一项几乎恒为 0：账号级 429 走的是落库的 `resume_at`（见下），只有落库
    /// 失败的兜底分支才会退回进程内冷却。留着它是为了让那个兜底状态在后台也能看见。
    /// 模型级冷却在 `rate_limited_models` 里，两者不可混用——见 `crate::store::RateLimitCooldown`。
    rate_limited_secs: i64,
    /// **模型级**冷却明细（超额池满或瞬时限速，记在进程内）。
    ///
    /// 这一档**不代表账号有问题**：即便是挡选号的那种，也只有列出的这些模型让位，该号的其余
    /// 模型照常服务，所以前端不能拿它把账号显示成「不可调度」。此前它压根没被透出来，于是
    /// fable 撞超额池被冷却时后台一片正常，选号侧却已经跳过它了。
    ///
    /// 每条的 `gated` 进一步分开两种情形，前端的措辞必须跟着分——见 [`ModelCooldown::gated`]。
    rate_limited_models: Vec<ModelCooldown>,
    /// 上游判过「这个号的套餐不含这些模型」（Pro 号打 fable 那类），见
    /// [`store::CredentialStore::deny_model`]。落库、长期有效，选号时这些模型绕开该号，
    /// 其余模型照常——同样**不代表账号有问题**。与 `rate_limited_models` 的区别是它不会
    /// 几十秒就过去：解除靠连通性测试通过、等级刷新变了，或手动「解除冷却」。
    denied_models: Vec<store::ModelDenial>,
    /// 被上游账号级限流而**自动停用**时，到点自动恢复调度的时刻（Unix 秒）；`None` 表示
    /// 不自动恢复（正常在用、人工停用、或封号）。
    ///
    /// 前端据此把「被限流暂停」和「已停用/已封号」分开显示：两者 `disabled` 都是 `true`，
    /// 区别只在这一项。展示绝对时刻而非倒计时的理由同 `expires_at`。恢复有三条路：到点自动、
    /// 连通性测试通过、手动打开启用开关。
    resume_at: Option<u64>,
    /// 该号被自动封停过几次（`ban_events` 条数，解封不清零）。列表接口才填，其余返回单个
    /// 视图的接口为 0——前端拿列表的那份。
    ban_count: i64,
}

impl CredentialView {
    /// 由凭证 + 已绑定设备数 + 活跃模拟会话数 + 全局默认上限构造视图。
    pub(super) fn new(
        c: &Credential,
        device_count: i64,
        session_count: i64,
        defaults: DefaultLimits,
    ) -> Self {
        let secs = c.expires_in_secs();
        Self {
            id: c.id,
            label: c.label.clone(),
            tier: c.tier.clone(),
            org_type: c.org_type.clone(),
            rate_limit_tier: c.rate_limit_tier.clone(),
            subscription_created_at: c.subscription_created_at.clone(),
            org_name: c.org_name.clone(),
            seat_tier: c.seat_tier.clone(),
            subscription_status: c.subscription_status.clone(),
            extra_usage_enabled: c.extra_usage_enabled,
            priority: c.priority,
            disabled: c.disabled,
            expires_in: secs,
            expires_at: c.expires_at,
            expired: secs == 0,
            created_at: c.created_at,
            updated_at: c.updated_at,
            device_limit: c.device_limit,
            device_limit_effective: if defaults.devices_by_session {
                0
            } else {
                store::effective_device_limit(c.device_limit, defaults.device)
            },
            device_limit_applies: !defaults.devices_by_session,
            device_count,
            session_limit: c.session_limit,
            session_limit_effective: store::effective_session_limit(
                c.session_limit,
                defaults.session,
            ),
            session_count,
            rpm_limit: c.rpm_limit,
            rpm_limit_effective: store::effective_rpm_limit(c.rpm_limit, defaults.rpm),
            quota_pause_pct: c.quota_pause_pct,
            quota_pause_pct_7d: c.quota_pause_pct_7d,
            quota_pause_pct_effective: store::effective_quota_pause_pct(
                c.quota_pause_pct,
                defaults.quota_pct,
            ),
            quota_pause_pct_7d_effective: store::effective_quota_pause_pct(
                c.quota_pause_pct_7d,
                defaults.quota_pct_7d,
            ),
            ban_reason: c.ban_reason.clone(),
            proxy: c.proxy.clone(),
            proxy_id: None,
            token_hint: mask_token(&c.refresh_token),
            quota: None,
            last_used: None,
            cost_total: 0.0,
            rpm: 0,
            rate_limited_secs: 0,
            rate_limited_models: Vec::new(),
            denied_models: Vec::new(),
            resume_at: c.resume_at,
            ban_count: 0,
        }
    }

    pub(super) fn with_ban_count(mut self, n: i64) -> Self {
        self.ban_count = n;
        self
    }

    /// 附加「套餐不含」的模型记录（落库的，没有就是空）。
    /// 附加代理池 id，见 [`Self::proxy_id`]。
    pub(super) fn with_proxy_ids(mut self, ids: &std::collections::HashMap<String, i64>) -> Self {
        self.proxy_id = self.proxy.as_ref().and_then(|url| ids.get(url).copied());
        self
    }

    pub(super) fn with_denials(mut self, denials: Vec<store::ModelDenial>) -> Self {
        self.denied_models = denials;
        self
    }

    /// 附加冷却状态：账号级剩余秒数 + 模型级明细（都在内存里，没有就是 0 / 空）。
    pub(super) fn with_cooldown(mut self, secs: i64, models: Vec<(String, i64, bool)>) -> Self {
        self.rate_limited_secs = secs;
        self.rate_limited_models = models
            .into_iter()
            .map(|(model, secs, gated)| ModelCooldown { model, secs, gated })
            .collect();
        self
    }

    /// 链式附加额度快照、最近使用时间、累计费用与当前 RPM。
    pub(super) fn with_stats(
        mut self,
        quota: Option<store::QuotaSnapshot>,
        last_used: Option<i64>,
        cost_total: f64,
        rpm: i64,
    ) -> Self {
        self.quota = quota;
        self.last_used = last_used;
        self.cost_total = cost_total;
        self.rpm = rpm;
        self
    }
}

/// 脱敏：保留前缀（到第三个 `-`）与尾 4 位，中间用 `…` 省略。
fn mask_token(token: &str) -> String {
    let tail: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let prefix: String = token.splitn(4, '-').take(3).collect::<Vec<_>>().join("-");
    if prefix.is_empty() { format!("…{}", tail) } else { format!("{}-…{}", prefix, tail) }
}
