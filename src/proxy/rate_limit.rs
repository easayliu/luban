//! 上游 429 / 额度耗尽的判定与调度冷却：LimitScope 分档、RateLimitInfo 限流头快照、
//! 按额度阈值提前挪出调度池。

use axum::http::HeaderMap;

use crate::store;

/// 一次 429 该冷却到什么范围，见 [`rate_limit_scope`]。
#[derive(Debug, Clone, PartialEq)]
pub(super) enum LimitScope {
    /// 基础额度窗口真的耗尽：该账号所有模型一起让位。
    Account,
    /// 这个号的某个额度池满了（它专用的超额/回补池）：只让这个模型让位，其余模型照常。
    Model(String),
    /// **谁的额度都没满**，上游只是这一刻不让发：模型容量限制、或请求速率（RPM）限制。
    ///
    /// 与 [`Self::Model`] 分开的理由是「换号有没有意义」完全相反：额度池是**跟着账号走**的，
    /// 换个号确实可能还有余量；而容量/速率限制是**跟着模型或出口走**的，换号重发只会在下一个
    /// 号上撞同一发 429，并把同一个模型的冷却挨个盖满整池——线上症状就是「一个号被限流，所有
    /// 号的卡片上都显示这个模型在冷却，新请求全被冷却硬门禁挡在门外」。
    ///
    /// 故这一档两条都不做：**不换号重试**（一条请求内不会走号），**冷却也不进选号门禁**
    /// （跨请求也不会靠冷却把号一个个点掉，见 [`park_rate_limited`]）。429 连同 `retry-after`
    /// 原样交回客户端，让它按上游给的节奏退避——上游要退避的是发请求这个动作本身，不是某个号。
    ///
    /// 冷却时长也另算，见 [`RateLimitInfo::transient_cooldown`]：这是几秒到几十秒的事，
    /// 拿额度那套（可以睡满几十小时）去算它，等于因为一次瞬时拥堵把号锁掉半天。
    Transient(String),
    /// **这个号的套餐不含这个模型**：响应里一个额度窗口都没有，只说 usage credits 被组织关掉了
    /// （`overage-disabled-reason`）。不是限流——等到什么时候都不会有额度长出来，除非换套餐或
    /// 开 extra usage。故它既不打冷却也不停号，而是落一条准入记录
    /// （[`store::CredentialStore::deny_model`]）并**换号重发**，之后的选号直接绕开这一格。
    ///
    /// **只给「fable/mythos 且账号等级不是 Max」**（[`store::premium_model`] + [`is_max_plan`]）：
    /// 这是唯一能把这形态解读成「套餐不含」的组合。其余组合见 [`Self::OverageDisabled`]。
    Unsupported(String),
    /// 形态与 [`Self::Unsupported`] 相同（无窗口、只说 credits 被组织关了），但**不能**解读成
    /// 「套餐不含」：请求的是所有付费套餐都含的基础模型（线上 sonnet-4-6 撞出过），或账号本就是
    /// Max。它也不代表撞了上限——头里没有任何窗口说
    /// 满了。上游到底为什么把这一发记到 credits 上不可知，所以只做最小动作：该号该模型短冷却
    /// （同瞬时那档：吃 `retry-after` 但不超 60 秒，没给就 30 秒），本条请求换号重发。
    /// 不落库、不停号、不记准入。
    OverageDisabled(String),
}

impl LimitScope {
    pub(super) fn account_level(&self) -> bool {
        matches!(self, Self::Account)
    }

    /// 这一发 429 是不是「换个号就可能发得出去」——只有额度是跟着账号走的，
    /// 容量/速率限制换号无益，见 [`Self::Transient`]。
    pub(super) fn worth_swapping(&self) -> bool {
        !matches!(self, Self::Transient(_))
    }

    /// 传给 [`store::CredentialStore::mark_rate_limited`] 的模型维度。
    pub(super) fn model(&self) -> Option<&str> {
        match self {
            Self::Account => None,
            Self::Model(m)
            | Self::Transient(m)
            | Self::Unsupported(m)
            | Self::OverageDisabled(m) => Some(m),
        }
    }

    pub(super) fn label(&self) -> &str {
        match self {
            Self::Account => "account",
            Self::Model(_) => "model",
            Self::Transient(_) => "transient",
            Self::Unsupported(_) => "unsupported",
            Self::OverageDisabled(_) => "overage-disabled",
        }
    }
}

/// 判定一次 429 是「这个账号没额度了」还是「只有这一个模型没路可走」。
///
/// **规则是实测倒逼出来的**，两次真实的 fable-5 429 头长这样（形态一致）：
///
/// ```text
/// unified-status: rejected                            ← 不是 "rate_limited"
/// representative-claim: seven_day_overage_included     ← 指向 7d_oi
/// 5h:    allowed,         utilization=0.20             ← 基础窗口很空
/// 7d:    allowed,         utilization=0.70
/// 7d_oi: rejected,        utilization=1.02             ← 满掉的只有「7 天含超额」
/// overage-status: rejected（org_level_disabled）
/// retry-after: 304802
/// ```
///
/// 演化了三版，每版的教训都写在规则里：
///
/// 1. **状态词不止一个**：`rejected` 与 `rate_limited` 都算被拒（`allowed`/`allowed_warning`
///    才是放行）。第一版只认 `rate_limited`，漏判。
/// 2. **窗口名不能写死**：被拒的是 `7d_oi`（7 天含超额），第一版根本没解析它。故扫**所有**
///    `unified-<窗口>-status/utilization`，不必维护 `representative-claim` 到窗口名的映射
///    （`seven_day_overage_included` → `7d_oi` 这种对应关系纯属猜谜）。
/// 3. **超额族窗口不算账号额度**：第二版把「任一窗口被拒/打满」一律判账号级，于是上面那条
///    把整个账号冷却了 24 小时——可它满掉的只是**超额/回补池**（`7d_oi` 比基础 7d 的
///    利用率还高，说明两边记的不是同一笔账；fable 走的正是这个池子，见
///    [`config::CC_BETA_FALLBACK_CREDIT`] 的注）。实测在 7d_oi 仍
///    rejected 期间，同一账号的 sonnet/opus 连通性测试照常 200——账号好好的，只有 fable
///    没路。故 `_oi`/`overage` 窗口被拒只判**模型级**，账号级只看基础窗口。
///
/// `unified-status` 是**本次请求**的判决：fable 被超额池拒掉时它同样是 `rejected`，说明
/// 不了账号整体，故只在没有任何逐窗口明细时才拿它兜底（保守判账号级）。模型级的冷却时长
/// 优先吃 `retry-after`（两次实测都给了，直指池子重置时刻），不会重蹈「30 秒放出去反复撞」
/// 的循环——那是早年时长不认 `retry-after` 的锅，不是作用域的。请求体里读不出模型名时
/// 退回账号级——没有模型可挂，宁可保守。
///
/// 生产路径都走 [`rate_limit_scope_for`]（要带账号等级）；这个两参数的形态只剩测试在用，
/// 按「一个非 Max 号」解读。
#[cfg(test)]
pub(super) fn rate_limit_scope(info: &RateLimitInfo, model: Option<&str>) -> LimitScope {
    rate_limit_scope_for(info, model, false)
}

/// 同 [`rate_limit_scope`]，多一个「这个号是 Max 档吗」（见 [`is_max_plan`]）：只影响「无窗口 +
/// overage 被关」那一档的解读——Max 号的 fable 不可能是「套餐不含」，见
/// [`LimitScope::OverageDisabled`]。
pub(super) fn rate_limit_scope_for(
    info: &RateLimitInfo,
    model: Option<&str>,
    max_plan: bool,
) -> LimitScope {
    let Some(model) = model else { return LimitScope::Account };
    let rejected = |s: &str| s.contains("rate_limited") || s.contains("rejected");
    let base_gone = info.window_status.iter().any(|(w, s)| !is_overage_window(w) && rejected(s))
        || info.window_utilization.iter().any(|(w, u)| !is_overage_window(w) && *u >= 1.0);
    let no_detail = info.window_status.is_empty() && info.window_utilization.is_empty();
    // 一个额度窗口都没报、只说 credits 被组织关了：这个模型不在该套餐的任何窗口里（线上实测
    // Pro 号打 fable：`overage-disabled-reason=org_level_disabled` + `credits-*` + 一个月后的
    // `unified-reset`，仅此而已）。它不是限流，见 [`LimitScope::Unsupported`]。**排在最前**：
    // 这形态下即便带了 `unified-status=rejected`，说的也是这次请求，不是账号。
    //
    // 但只有「高档套餐专属模型 + 账号不是 Max」才能这么解读。同一形态落在 sonnet/opus/haiku 上
    // （线上 sonnet-4-6 撞出过）或落在 Max 号上，既不是「不含」也不是「撞上限」——头里没有任何
    // 窗口说满了——只能当一次说不清的拒绝：短冷却 + 换号，见 [`LimitScope::OverageDisabled`]。
    if no_detail && info.overage_disabled_reason.is_some() {
        return if store::premium_model(model) && !max_plan {
            LimitScope::Unsupported(model.to_string())
        } else {
            LimitScope::OverageDisabled(model.to_string())
        };
    }
    let unified_gone = no_detail && info.unified_status.as_deref().is_some_and(rejected);
    // 超额/回补池被拒或打满：额度是跟着账号走的，换个号可能还有余量，故仍判 [`LimitScope::Model`]。
    let overage_gone = info.window_status.iter().any(|(w, s)| is_overage_window(w) && rejected(s))
        || info.window_utilization.iter().any(|(w, u)| is_overage_window(w) && *u >= 1.0);
    if base_gone || unified_gone {
        LimitScope::Account
    } else if overage_gone {
        LimitScope::Model(model.to_string())
    } else {
        // 走到这里的 429 里，**没有一个窗口是满的**（也可能一个限流头都没带）：那就不是「这个
        // 号没额度了」，而是容量或请求速率限制。它不跟着账号走，见 [`LimitScope::Transient`]。
        LimitScope::Transient(model.to_string())
    }
}

/// 一次确认的上游 429 该怎么把这个号挪出调度池，两档分开处理：
///
/// - **账号级**（基础窗口真耗尽）：走 [`store::CredentialStore::pause_for_rate_limit`]，
///   把**调度开关关掉并落库**，同时记下到点自动恢复的时刻。落库是关键——额度耗尽动辄几小时
///   到几天，只记内存的话一次进程重启就忘了，重启后又拿这个号去撞一发 429；而且后台看不到
///   这个号为什么不干活。恢复有三条路：到点惰性自动恢复、连通性测试通过自动恢复、
///   控制台手动打开。
/// - **模型级**（超额池满，账号本身好着）：走进程内的
///   [`store::CredentialStore::mark_rate_limited`]，挡住这个号的这一个模型。这一档默认才
///   30 秒，落库既不值得、也会在卡片上把一个健康账号显示成「已停用」——它的 sonnet/opus
///   明明还在正常服务。
/// - **瞬时级**（容量与请求速率限制）：同样走
///   [`store::CredentialStore::mark_rate_limited`]，但 cooldown 用 ladder 退避值（2s 起步），
///   远短于额度那一档。短 gate 阻止同一个号被立刻再选中、反复打出 429，同时因为持续时间短，
///   不会像长 gate 那样级联封死整池。
pub(super) fn park_rate_limited(
    store: &store::CredentialStore,
    cred: &crate::credentials::Credential,
    scope: &LimitScope,
    cooldown: std::time::Duration,
    // 瞬时限流已经在这条路线上连撞到 [`TRANSIENT_MAX_ATTEMPTS`]：升级为完整冷却，
    // 照常挪出调度池，让后续请求改走别的号。
    transient_exhausted: bool,
) {
    let Some(model) = scope.model() else {
        let resume_at = crate::credentials::now_secs() + cooldown.as_secs();
        let reason = format!(
            "upstream rate limit: account quota exhausted, scheduling resumes automatically in about {}",
            human_secs(cooldown)
        );
        match store.pause_for_rate_limit(cred.id, &reason, resume_at) {
            Ok(true) => tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                resume_at,
                "account-level rate limit: taken out of the pool, resumes automatically when it expires (or enable it manually / run a connectivity test from the console)"
            ),
            // 号已经被封 / 人工停用 / 按订阅停了（同一个号并发在飞的另一条先回来了）：不改写。
            Ok(false) => {}
            // 落库失败不该把这条请求也搭进去：至少退回进程内冷却，本进程内仍不会再选它。
            Err(e) => {
                tracing::error!(
                    cred_id = cred.id, cred = %cred.label,
                    error = %e,
                    "persisting the rate-limit pause failed, falling back to an in-process cooldown"
                );
                store.mark_rate_limited(cred.id, None, cooldown);
            }
        }
        return;
    };
    // 瞬时限流（容量 / 请求速率）：用 ladder 退避值做**短时硬门禁**。
    //
    // 之前这一档只打 soft mark（不阻塞调度），理由是「上游限的是出口，换谁都一样，不该封号」。
    // 问题是：soft mark 不拦选号，新请求立刻又选到同一个号 → 上游容量还没恢复 → 再次 429，
    // 形成无意义的循环轰炸。
    //
    // 改成 gate：cooldown 已经是 max(upstream retry-after, ladder) 的较大值，ladder 从 2s 起步
    // （2→4→8→16→32→60s），短到不会把整池封死——全池级联需要所有号同时在 gate，而 2s 的窗口
    // 在客户端按 retry-after 退避的周期内早就放开了。跨请求的设备改绑确实会让其他号各自独立
    // 起一把 ladder，但每个号的第一档都只 gate 2s，交错过期，同时压满的概率极低。
    // 真连撞到 TRANSIENT_MAX_ATTEMPTS 档就升级为完整冷却（下面那条路），逻辑闭环。
    if matches!(scope, LimitScope::Transient(_)) && !transient_exhausted {
        store.mark_rate_limited(cred.id, Some(model), cooldown);
        return;
    }
    store.mark_rate_limited(cred.id, Some(model), cooldown);
}

/// 额度快用尽时**提前**把这个号挪出调度池，不必等真撞上一发 429。
///
/// 「收到 429 才停」是纯被动的：触发它的那条请求必然失败，而客户端那头看到的就是一次报错。
/// 可上游在**每一条**响应里都报着基础额度窗口的使用率
/// （`anthropic-ratelimit-unified-<窗口>-utilization`，0~1），越过阈值时这个号剩下的额度
/// 已经不够再跑完一轮对话，继续调度只是把那发 429 推迟到下一条请求上。阈值由
/// [`store::CredentialStore::quota_pause_pct`] 配（默认 90%，配 `0` 即关掉本机制、退回
/// 「收到 429 才停」的老行为）。
///
/// 只看**基础**窗口，与 [`rate_limit_scope`] 共用 [`is_overage_window`] 口径：超额池
/// （`7d_oi`/`overage`）快满了不代表账号额度耗尽——实测那期间同一账号的 sonnet/opus 照常
/// 200，按它停整个号是误伤。同理这里也不做模型级那一档：使用率讲的是账号额度，不是某个
/// 模型此刻有没有容量。
///
/// **5h 与 7d 各用各的阈值**（[`QuotaPauseThresholds`]），默认只按 5h 停、7d 那档是关的。
/// 两档都可以**逐账号覆盖**（`Credential::quota_pause_pct` / `quota_pause_pct_7d`，控制台
/// 账号菜单里配）：账号配了的那档用账号的，配 0 即这个号这一档不停，没配的跟随全局。
/// 混用一个数字的老口径会让一个周用量偏高的号被整段停掉——5h 明明还空着、这会儿完全能干活，
/// 却要等到下个 7d 重置才回池。7d 真满了不需要我们提前动手：那时上游自己回 429，账号级冷却
/// 接手，睡到 7d 重置为止。要开天级那档见 [`store::QUOTA_PAUSE_PCT_7D`]。
///
/// 停到哪：越过阈值的那些基础窗口中**最晚**的一个 `*-reset`（取 max 的理由同
/// [`RateLimitInfo::exhausted_base_reset`]：5h 到点了 7d 照样拦着）。落库、恢复路径与账号级
/// 429 完全一致——到点惰性自动恢复、连通性测试通过自动恢复、控制台手动打开。
///
/// 返回是否已经把号停在池外，调用方据此决定要不要再走「测试通过就恢复」那条路——否则一次
/// 手动探活会把刚按阈值停掉的号放回去，下一条请求再停一次，来回拉锯。
pub(super) fn park_if_quota_nearly_exhausted(
    store: &store::CredentialStore,
    cred: &crate::credentials::Credential,
    info: &RateLimitInfo,
) -> bool {
    // 与 429 那一档同受「限流冷却/换号重试」这个总开关：关掉它的人要的是**完全**不干预调度、
    // 原样把上游的判决交给客户端，那时按使用率自动停号只会是个惊吓。重试次数配 0 同样视为
    // 关闭（与转发、连通性测试口径一致）。要单独关本机制，把阈值配成 0 即可。
    let thresholds = QuotaPauseThresholds::for_credential(store, cred);
    if thresholds.all_off()
        || !store.forward_flags().rate_limit_retry
        || store.rate_limit_retry_max() == 0
    {
        return false;
    }
    let Some((window, used)) = info.saturated_base_window(&thresholds) else {
        return false;
    };
    let pct = thresholds.pct_for(window);
    // 同一批限流头会被这个号所有在途请求各看一遍：已经停在池外的就别再写库、也别再刷屏。
    // 读一次库的代价只在真越阈值时付，正常流量走不到这里。
    if matches!(store.get(cred.id), Ok(Some(c)) if c.disabled) {
        return true;
    }
    let cooldown = info.quota_pause_cooldown(&thresholds);
    let resume_at = crate::credentials::now_secs() + cooldown.as_secs();
    let reason = format!(
        "quota nearly exhausted: window {window} is at {:.1}% (pause threshold {pct}%), scheduling resumes automatically in about {}",
        used * 100.0,
        human_secs(cooldown)
    );
    match store.pause_for_rate_limit(cred.id, &reason, resume_at) {
        // 没写入 = 号已经被封 / 人工停用 / 按订阅停了：同样已在池外，只是不必再记一遍。
        Ok(false) => true,
        Ok(true) => {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                window,
                utilization = used,
                threshold_pct = pct,
                resume_at,
                ratelimit = %info.raw,
                "quota nearly exhausted: taken out of the pool before hitting a 429, resumes automatically when the window resets (or enable it manually / run a connectivity test from the console)"
            );
            true
        }
        // 落库失败就当没停：这一档是「提前量」，为它把请求也搭进去不值得，真到 429 时
        // 账号级那条路还会再停一次。
        Err(e) => {
            tracing::error!(
                cred_id = cred.id, cred = %cred.label,
                error = %e,
                "persisting the quota-threshold pause failed, this credential stays in the pool until it actually gets a 429"
            );
            false
        }
    }
}

/// 把秒数写成人话（`3h 12m` / `45m` / `30s`），写进 `ban_reason` 给人看。
///
/// 只保留两级、且不做四舍五入：这行字是给人快速判断「还要等多久」的，`2d 3h` 足够，
/// 精确到秒反而更难读。真要精确时刻的话，`resume_at` 是原样落库的，前端自己格式化即可。
fn human_secs(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let (days, hours, mins) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (days, hours, mins) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// 该窗口是否属于**超额/回补池**而非账号基础额度（实测形态：`7d_oi`、`overage`）。
///
/// 它满了只说明「这条超额通道走不通」，不代表账号额度耗尽——同一时刻别的模型照常 200，
/// 故既不判账号级（[`rate_limit_scope`]），也不拿它的 reset 当账号冷却
/// （[`RateLimitInfo::exhausted_base_reset`]）。两处必须同一口径，故抽成一个函数。
fn is_overage_window(w: &str) -> bool {
    w.ends_with("_oi") || w.contains("overage")
}

/// 这个窗口是不是**天级**的（`7d`，将来若有 `30d` 同理）。
///
/// 按名字的时间单位分，而不是照着 `7d` 写死一个等号：上游加一个新窗口时，`3d` 该跟着天级那档
/// 走、`1h` 该跟着小时级那档走，这是唯一不用改代码也不会错档的分法。超额族在调用点已经先被
/// [`is_overage_window`] 滤掉了，`7d_oi` 落不到这里。
fn is_long_window(w: &str) -> bool {
    w.ends_with('d')
}

/// 提前停调度的两档阈值：小时级窗口（`5h`）一档、天级窗口（`7d`）另一档，各自 `0` = 该档不停。
///
/// **不共用一个数**：同一个 90% 在两个窗口上的后果差着数量级——5h 停号最多歇几小时就自己回来，
/// 7d 停号是歇到下个周重置。原来两档混用一个阈值，结果一个周用量偏高的号会被整段挪出池子，
/// 哪怕它这 5 小时一点没用、还能正常干活。天级那档默认关，见
/// [`store::QUOTA_PAUSE_PCT_7D`]。
#[derive(Clone, Copy, Debug)]
pub(super) struct QuotaPauseThresholds {
    /// 小时级窗口的阈值（百分比，`0` = 关）。
    short_pct: i64,
    /// 天级窗口的阈值（百分比，`0` = 关）。
    long_pct: i64,
}

impl QuotaPauseThresholds {
    /// 这个账号生效的两档阈值：账号自己配了的那档用账号的
    /// （[`crate::credentials::Credential::quota_pause_pct`] / `quota_pause_pct_7d`），
    /// 没配的跟随全局。两档各自取，见 [`store::effective_quota_pause_pct`]。
    fn for_credential(
        store: &store::CredentialStore,
        cred: &crate::credentials::Credential,
    ) -> Self {
        Self {
            short_pct: store::effective_quota_pause_pct(
                cred.quota_pause_pct,
                store.quota_pause_pct(),
            ),
            long_pct: store::effective_quota_pause_pct(
                cred.quota_pause_pct_7d,
                store.quota_pause_pct_7d(),
            ),
        }
    }

    /// 该窗口适用的阈值（百分比）。
    fn pct_for(&self, window: &str) -> i64 {
        if is_long_window(window) { self.long_pct } else { self.short_pct }
    }

    /// 该窗口的使用率算不算越过了它自己那档阈值。那档配成 `0`（关）时恒为 false——
    /// 「关」必须真的什么都不做，不能让 `used >= 0.0` 把每条响应都判成越阈值。
    fn crossed(&self, window: &str, used: f64) -> bool {
        let pct = self.pct_for(window);
        pct > 0 && used >= pct as f64 / 100.0
    }

    /// 两档都关着 = 本机制整个不启用。
    fn all_off(&self) -> bool {
        self.short_pct <= 0 && self.long_pct <= 0
    }
}

/// 上游订阅账号限流快照，从 `anthropic-ratelimit-unified-*` 响应头解析。
///
/// 5h/7d 两个窗口各有 status/reset(unix 秒)/utilization(0~1)；`representative` 指明
/// 当前起约束作用的窗口（如 `five_hour`）。`raw` 保留全部匹配头，字段变化时兜底回看。
#[derive(Default, Clone)]
pub(crate) struct RateLimitInfo {
    pub(super) unified_status: Option<String>,
    pub(super) five_h_status: Option<String>,
    pub(super) five_h_reset: Option<i64>,
    pub(super) five_h_utilization: Option<f64>,
    pub(super) seven_d_status: Option<String>,
    pub(super) seven_d_reset: Option<i64>,
    pub(super) seven_d_utilization: Option<f64>,
    pub(super) representative: Option<String>,
    /// `retry-after`（秒）。429 时上游一般会给，是冷却时长最直接的来源，见
    /// [`RateLimitInfo::cooldown`]。
    pub(super) retry_after: Option<i64>,
    /// 不带窗口名的 `anthropic-ratelimit-unified-reset`（unix 秒）：上游给的「整体什么时候
    /// 恢复」，比按 `representative-claim` 反查窗口更直接。
    pub(super) unified_reset: Option<i64>,
    /// `anthropic-ratelimit-unified-overage-in-use`：本次请求是否动用了 **usage credits**
    /// （Anthropic 官方术语，旧称 extra usage：套餐包含的用量用完后不拦你，切成按标准
    /// API 价的按量计费继续跑）。别把它叫「超额计费」——那不是官方说法。
    /// 这是「额度满了但不 429」的关键标记——基础窗口 rejected、请求却 200 成功，
    /// 烧的是按量计费的钱；把它落进快照，前端才能把这种号和真正健康的号区分开。
    pub(super) overage_in_use: Option<bool>,
    /// `anthropic-ratelimit-unified-overage-disabled-reason`（实测值 `org_level_disabled`）：
    /// 这次请求本该走 usage credits，但组织没开。它单独出现、**不带任何额度窗口头**时，说明
    /// 这个模型压根不在该套餐的额度窗口里——Pro 号打 fable 就是这个形态，见 [`rate_limit_scope`]。
    pub(super) overage_disabled_reason: Option<String>,
    /// **所有** `anthropic-ratelimit-unified-<窗口>-status` 的取值（窗口名原样保留）。
    ///
    /// 刻意不写死窗口名：实测除了 `5h`/`7d`，还有 `7d_oi`（7 天含超额），而**真正被拒的
    /// 正是它**——只解析 5h/7d 会看到「两个窗口都没满」，从而把一次账号级限流误判成模型
    /// 容量限制。窗口种类是上游说了算的，只能全收，见 [`rate_limit_scope`]。
    pub(super) window_status: Vec<(String, String)>,
    /// 所有 `…-<窗口>-utilization` 的取值。同上，全收。
    pub(super) window_utilization: Vec<(String, f64)>,
    /// 所有 `…-<窗口>-reset` 的取值（unix 秒，窗口名原样保留）。同上，全收——冷却要睡到
    /// **被拒的那个窗口**自己的重置时刻，而它未必是 5h/7d 中的一个。
    pub(super) window_reset: Vec<(String, i64)>,
    /// 全部匹配到的限流/anthropic- 头，`k=v` 以 `, ` 连接。
    pub(super) raw: String,
}

impl RateLimitInfo {
    pub(super) fn from_headers(headers: &HeaderMap) -> Self {
        let mut info = RateLimitInfo::default();
        let mut pairs: Vec<String> = Vec::new();
        for (k, v) in headers.iter() {
            let name = k.as_str().to_ascii_lowercase();
            if !(name.contains("ratelimit")
                || name == "retry-after"
                || name.starts_with("anthropic-"))
            {
                continue;
            }
            let val = v.to_str().unwrap_or("<non-utf8>");
            pairs.push(format!("{name}={val}"));
            match name.as_str() {
                "anthropic-ratelimit-unified-status" => info.unified_status = Some(val.to_string()),
                "anthropic-ratelimit-unified-5h-status" => {
                    info.five_h_status = Some(val.to_string())
                }
                "anthropic-ratelimit-unified-5h-reset" => info.five_h_reset = val.parse().ok(),
                "anthropic-ratelimit-unified-5h-utilization" => {
                    info.five_h_utilization = val.parse().ok()
                }
                "anthropic-ratelimit-unified-7d-status" => {
                    info.seven_d_status = Some(val.to_string())
                }
                "anthropic-ratelimit-unified-7d-reset" => info.seven_d_reset = val.parse().ok(),
                "anthropic-ratelimit-unified-7d-utilization" => {
                    info.seven_d_utilization = val.parse().ok()
                }
                "anthropic-ratelimit-unified-representative-claim" => {
                    info.representative = Some(val.to_string())
                }
                "retry-after" => info.retry_after = val.trim().parse().ok(),
                "anthropic-ratelimit-unified-reset" => info.unified_reset = val.parse().ok(),
                "anthropic-ratelimit-unified-overage-in-use" => {
                    info.overage_in_use = Some(val.trim() == "true")
                }
                "anthropic-ratelimit-unified-overage-disabled-reason" => {
                    info.overage_disabled_reason = Some(val.trim().to_string())
                }
                _ => {}
            }
            // 通用收集：`anthropic-ratelimit-unified-<窗口>-status|utilization`，窗口名不限。
            if let Some(rest) = name.strip_prefix("anthropic-ratelimit-unified-") {
                if let Some(win) = rest.strip_suffix("-status") {
                    info.window_status.push((win.to_string(), val.to_string()));
                } else if let Some(win) = rest.strip_suffix("-utilization")
                    && let Ok(u) = val.parse::<f64>()
                {
                    info.window_utilization.push((win.to_string(), u));
                } else if let Some(win) = rest.strip_suffix("-reset")
                    && let Ok(ts) = val.parse::<i64>()
                {
                    // 不带窗口名的 `…-unified-reset` 不会命中：那时 rest 是 `reset`，
                    // 剥不掉 `-reset` 前缀那一横，不会造出一个名字为空的假窗口。
                    info.window_reset.push((win.to_string(), ts));
                }
            }
        }
        info.raw = pairs.join(", ");
        info
    }

    /// 这发响应**一个限流头都没带**：`anthropic-ratelimit-*` 与 `retry-after` 全缺。
    ///
    /// 不能拿 [`Self::raw`] 是否为空当判据——`raw` 连 `anthropic-organization-id`、
    /// `anthropic-workspace-id` 这类与限流无关的头也一并收着（见 [`Self::from_headers`]
    /// 的过滤条件里那条 `starts_with("anthropic-")`）。实测的裸 429 里就只有这两条，
    /// `raw` 非空而限流信息为零。
    pub(super) fn no_limit_headers(&self) -> bool {
        self.unified_status.is_none()
            && self.unified_reset.is_none()
            && self.retry_after.is_none()
            && self.window_status.is_empty()
            && self.window_utilization.is_empty()
            && self.window_reset.is_empty()
    }

    /// 该凭证被上游 429 之后应冷却多久。
    ///
    /// **冷却时长一律由上游给的重置时刻算出，没有任何写死的窗口长度**——「5h 窗口」指的是
    /// 它的统计口径，不是「睡 5 小时」：账号是在自己那个窗口的 `*-reset` 时刻回血的，那才是
    /// 该醒的点。只有上游一个时间都没给时才落到默认值。
    ///
    /// **账号级**（额度真耗尽）取值优先级：
    /// 1. **被拒/打满的那个基础窗口**自己的 `*-reset`（见 [`Self::exhausted_base_reset`]）
    ///    ——判账号级正是因为它满了，它什么时候重置，账号就什么时候能用；
    /// 2. 不带窗口名的 `anthropic-ratelimit-unified-reset`——上游给的「整体什么时候恢复」，
    ///    逐窗口明细缺失时的兜底；
    /// 3. `retry-after`（秒）——连一个 reset 时刻都没给时才用它；
    /// 4. 各窗口 `*-reset` 里**最早**的那个，连哪个满了都不知道时，宁可早醒也不要多睡；
    /// 5. 都没有 → [`DEFAULT_RATE_LIMIT_COOLDOWN_SECS`]。
    ///
    /// **为什么 `retry-after` 在账号级被降到第三位**（它曾经排第一）：它是个**相对秒数**，
    /// 要重新锚回我们自己的时钟才能变成时刻，而 `*-reset` 本身就是绝对时刻。两者口径不同，
    /// 于是漂移有三处叠加——上游把剩余时间向下取整成整秒、本地时钟与上游未必一致、界面显示
    /// 又是截断到分钟（不进位）。线上实测的症状：同一张卡片一边写「12:20 重置」（读的是
    /// `5h-reset`），一边写「12:19 自动恢复」（`now + retry-after`），真实差距不到一秒，
    /// 显示出来却整整差一分钟，且那一分钟里发出去的请求必然再撞 429。
    /// 改成直接吃窗口的 `*-reset` 之后，恢复时刻与卡片上的重置时刻**是同一个数**，
    /// 不存在对不上的可能。`retry-after` 仍是没有 reset 时的兜底。
    ///
    /// **模型级不看任何 reset**：窗口都没跑满，reset 说的是「这个窗口什么时候重置」，跟
    /// 「这个模型什么时候有容量」是两码事，拿它当冷却会让一个好账号的某个模型白白闲置几小时。
    /// 那一档只认 `retry-after`，没有就用 [`DEFAULT_MODEL_COOLDOWN_SECS`]。
    ///
    /// 结果夹在 `[1s, `[`MAX_RATE_LIMIT_COOLDOWN_SECS`]`]`：**睡满上游说的那个 reset**，
    /// 到点自动回到调度池里参与正常选号，不做定时探活、也不提前放出去撞。上限经历过
    /// 6h → 24h → 7d：夹得比真实窗口短，等于每到上限就把这个号放出去白撞一次 429
    /// （上游实测给过 63 小时的 `retry-after`，7d 窗口耗尽时还会更长）。
    /// 冷却现在是硬门禁（见 [`store::CredentialStore::select_for_device`]），
    /// 睡过头的代价由「连通性测试成功自动解除」和控制台的手动解除兜底。
    pub(super) fn cooldown(&self, account_level: bool) -> std::time::Duration {
        let now = crate::credentials::now_secs() as i64;
        let earliest_window_reset = self
            .window_reset_candidates()
            .into_iter()
            .filter(|reset| *reset > now)
            .min()
            .map(|reset| reset - now);
        let future_secs = |reset: i64| Some(reset - now).filter(|d| *d > 0);
        let secs = if account_level {
            // 绝对时刻优先，相对秒数兜底——两者口径不同，混用会让恢复时刻和卡片上的重置
            // 时刻差出一分钟（见上面的方法文档）。
            self.exhausted_base_reset()
                .and_then(future_secs)
                .or_else(|| self.unified_reset.and_then(future_secs))
                .or(self.retry_after)
                .or(earliest_window_reset)
                .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN_SECS)
        } else {
            self.retry_after.unwrap_or(DEFAULT_MODEL_COOLDOWN_SECS)
        }
        .clamp(1, MAX_RATE_LIMIT_COOLDOWN_SECS);
        std::time::Duration::from_secs(secs as u64)
    }

    /// 按这一发 429 的判定档位算冷却时长，见 [`LimitScope`]。三档各有各的口径，故由 scope
    /// 分派，调用方不必自己记「哪一档该传什么」。
    pub(super) fn cooldown_for(&self, scope: &LimitScope) -> std::time::Duration {
        match scope {
            // 说不清的拒绝同瞬时那档：吃 `retry-after` 但夹在 60 秒以内——这形态不代表撞上限，
            // 上游若顺手给了个按额度窗口算的大 retry-after，照单全收就把一个健康号的这个模型
            // 锁掉几天。
            LimitScope::Transient(_) | LimitScope::OverageDisabled(_) => self.transient_cooldown(),
            // 套餐不含的模型不走冷却（落的是准入记录），这里没有可睡的时长。
            LimitScope::Unsupported(_) => std::time::Duration::ZERO,
            _ => self.cooldown(scope.account_level()),
        }
    }

    /// 写进准入记录的依据：把这发 429 里说明「为什么没额度」的那几个头摘出来。
    pub(super) fn plan_denial_reason(&self) -> String {
        let reason = self.overage_disabled_reason.as_deref().unwrap_or("-");
        format!(
            "upstream 429 without any quota window for this model (overage-disabled-reason={reason}): the plan does not include it"
        )
    }

    /// 瞬时限流（容量/请求速率）的冷却：**吃 `retry-after`，但夹在
    /// [`MAX_TRANSIENT_COOLDOWN_SECS`] 以内**。
    ///
    /// 这一档谁的额度都没满（见 [`LimitScope::Transient`]），等的只是「这一刻别发」，几秒到
    /// 几十秒就过去了。而冷却是选号的硬门禁，长冷却在这一档纯属误伤：上游偶尔会在这种 429 上
    /// 带一个按额度窗口算出来的大 `retry-after`（实测给过 63 小时），照单全收就等于因为一次
    /// 瞬时拥堵把这个号的这个模型锁掉两天多。
    pub(super) fn transient_cooldown(&self) -> std::time::Duration {
        let secs = self
            .retry_after
            .unwrap_or(DEFAULT_MODEL_COOLDOWN_SECS)
            .clamp(1, MAX_TRANSIENT_COOLDOWN_SECS);
        std::time::Duration::from_secs(secs as u64)
    }

    /// 把逐项收集的三张表（status / utilization / reset）按窗口名合并成一份结构化快照，
    /// 供落库展示（见 [`store::QuotaWindow`]）。
    ///
    /// 顺序按**首次出现**的窗口名排，即上游响应头里的顺序——前端照着渲染就是上游的原序，
    /// 不必自己定一套排法。三张表是分开收的（解析时一个头只落一处），故这里以 status 打头、
    /// 再把只出现在另外两张表里的窗口补上，避免漏掉「只报了 utilization 没报 status」的窗口。
    pub(super) fn windows(&self) -> Vec<store::QuotaWindow> {
        let mut names: Vec<&str> = Vec::new();
        for name in self
            .window_status
            .iter()
            .map(|(w, _)| w.as_str())
            .chain(self.window_utilization.iter().map(|(w, _)| w.as_str()))
            .chain(self.window_reset.iter().map(|(w, _)| w.as_str()))
        {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
            .into_iter()
            .map(|name| store::QuotaWindow {
                name: name.to_string(),
                status: self.window_status.iter().find(|(w, _)| w == name).map(|(_, s)| s.clone()),
                utilization: self
                    .window_utilization
                    .iter()
                    .find(|(w, _)| w == name)
                    .map(|(_, u)| *u),
                reset: self.window_reset.iter().find(|(w, _)| w == name).map(|(_, t)| *t),
            })
            .collect()
    }

    /// 各窗口的 `*-reset`（unix 秒），全窗口通收后再并上 5h/7d 专用字段（重复无所谓，
    /// 调用方只取 min/max）。
    pub(super) fn window_reset_candidates(&self) -> Vec<i64> {
        self.window_reset
            .iter()
            .map(|(_, ts)| *ts)
            .chain([self.five_h_reset, self.seven_d_reset].into_iter().flatten())
            .collect()
    }

    /// 已越过**自己那档**阈值的**基础**窗口里，用得最狠的那个 `(窗口名, 使用率)`。
    ///
    /// 供 [`park_if_quota_nearly_exhausted`] 判定与写原因文案。取使用率最高的那个纯粹是为了
    /// 让文案指向最有说服力的那一个——停多久另算，见 [`Self::quota_pause_cooldown`]。
    /// 超额族窗口不算（[`is_overage_window`]），口径与 [`rate_limit_scope`] 一致。
    ///
    /// 阈值按窗口分档取（[`QuotaPauseThresholds`]）：5h 用 5h 那档、7d 用 7d 那档，某档关着
    /// 时那个窗口再满也不参与判定。
    pub(super) fn saturated_base_window(&self, t: &QuotaPauseThresholds) -> Option<(&str, f64)> {
        self.window_utilization
            .iter()
            .filter(|(w, u)| !is_overage_window(w) && t.crossed(w, *u))
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(w, u)| (w.as_str(), *u))
    }

    /// 按阈值提前停调度时该睡多久：越过**自己那档**阈值的基础窗口中**最晚**的那个 `*-reset`。
    ///
    /// 取 max 而不是 min，理由同 [`Self::exhausted_base_reset`]——两个窗口都越过各自阈值时，
    /// 5h 到点了 7d 那档照样拦着，早醒只会立刻再被停一次。反过来，7d 那档关着（默认）时它
    /// 压根不参与，一次 5h 触发的停号就只睡到 5h 重置，不会被一个用了 95% 的 7d 拖成几天。没有逐窗口 reset 时退到不带窗口名的
    /// `unified-reset`，再退到所有窗口里最早的 reset，最后才是
    /// [`DEFAULT_RATE_LIMIT_COOLDOWN_SECS`]。
    ///
    /// **不看 `retry-after`**：这一档判定发生在一条**正常响应**上，那个头压根不会出现。
    pub(super) fn quota_pause_cooldown(&self, t: &QuotaPauseThresholds) -> std::time::Duration {
        let now = crate::credentials::now_secs() as i64;
        let future_secs = |reset: i64| Some(reset - now).filter(|d| *d > 0);
        let over =
            |w: &str| self.window_utilization.iter().any(|(name, u)| name == w && t.crossed(w, *u));
        let saturated_reset = self
            .window_reset
            .iter()
            .filter(|(w, _)| !is_overage_window(w) && over(w))
            .map(|(_, ts)| *ts)
            .max();
        let earliest_window_reset =
            self.window_reset_candidates().into_iter().filter_map(future_secs).min();
        let secs = saturated_reset
            .and_then(future_secs)
            .or_else(|| self.unified_reset.and_then(future_secs))
            .or(earliest_window_reset)
            .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN_SECS)
            .clamp(1, MAX_RATE_LIMIT_COOLDOWN_SECS);
        std::time::Duration::from_secs(secs as u64)
    }

    /// 已被拒/已打满的**基础窗口**（排除超额族）中最晚的那个 `*-reset`——账号级冷却该睡到的
    /// 时刻，也是 [`rate_limit_scope`] 判账号级的依据本身。
    ///
    /// 取**最晚**而不是最早：5h 和 7d 同时耗尽时，5h 到点了 7d 照样拦着，早醒只是白撞一发
    /// 429 再重新睡回去。而只有一个窗口满时 max 退化成它自己，正是要的答案。
    ///
    /// 只看基础窗口，与 [`rate_limit_scope`] 用同一个 [`is_overage_window`] 口径：超额池
    /// （`7d_oi`/`overage`）满不是账号额度耗尽，它压根走不到账号级这一档。
    pub(super) fn exhausted_base_reset(&self) -> Option<i64> {
        let rejected = |w: &str| {
            self.window_status.iter().any(|(name, s)| {
                name == w && (s.contains("rate_limited") || s.contains("rejected"))
            }) || self.window_utilization.iter().any(|(name, u)| name == w && *u >= 1.0)
        };
        self.window_reset
            .iter()
            .filter(|(w, _)| !is_overage_window(w) && rejected(w))
            .map(|(_, ts)| *ts)
            .max()
    }
}

/// 模型级冷却在没有 `retry-after` 时的时长。
///
/// 取 30 秒：容量限制是「这一阵挤」，不是「这个号没额度了」，躲一小会儿就该让它回来试；
/// 押太久等于把一个健康账号的这个模型白白闲置。
pub(super) const DEFAULT_MODEL_COOLDOWN_SECS: i64 = 30;

/// 冷却时长的上限：7 天窗口 + 1 小时余量。
///
/// 账号的基础窗口最长就是 7d，睡满它即可；留 1 小时余量是因为 `retry-after` 是相对本次
/// 请求算的，而 reset 时刻本身还可能被上游微调。上限存在的意义只剩「挡住明显异常的头」
/// （比如 reset 落在几年后），不再是「每隔 N 小时放出去试一次」——那种试探每次都要白撞
/// 一发 429，而额度没到点是不会自己长回来的。
pub(super) const MAX_RATE_LIMIT_COOLDOWN_SECS: i64 = 7 * 24 * 3600 + 3600;

/// 瞬时限流（容量/请求速率）那一档的冷却上限：60 秒。见
/// [`RateLimitInfo::transient_cooldown`]。
///
/// 这一档没有任何窗口是满的，等的只是这一阵拥堵；上游在这种 429 上给出的 `retry-after`
/// 未必按同一口径算（实测见过直接给额度窗口重置时刻的），照单全收会把一个额度充足的号
/// 按几十小时锁住。夹到一分钟：真需要等更久时，客户端下一条请求会再撞一发、再冷却一分钟，
/// 代价是一次往返；夹错方向（该等 1 小时却只等 1 分钟）远比反过来便宜。
pub(super) const MAX_TRANSIENT_COOLDOWN_SECS: i64 = 60;

/// 上游 429 但没给任何可用的等待时间时，凭证的默认冷却时长。
///
/// 取一分钟：这种情况多半是突发/并发限流（额度耗尽那种上游会明确给 reset），躲过这一阵即可；
/// 冷却太长会让一个其实还能用的号长时间闲置。
pub(super) const DEFAULT_RATE_LIMIT_COOLDOWN_SECS: i64 = 60;

#[cfg(test)]
mod tests;
