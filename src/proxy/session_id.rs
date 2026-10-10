//! 会话 id 的取值、冲突检测与按账号钉住：来访 `X-Claude-Code-Session-Id` 头 /
//! `metadata.user_id` 两处该读哪个、出站两处该写哪个、以及缺失时怎么派生。

use axum::http::HeaderMap;

use crate::store;

use super::body::{anon_session_key, extract_session_id, session_binding_key};
#[cfg(doc)]
use super::body::{sim_device_fingerprint, sim_session_key};
use super::simulation::{Simulation, looks_like_uuid};

/// 来访自己带的会话 id，**头和体都看**，且必须是合法的 uuid 形态。
///
/// 两个来源都要看：官方两处逐字相同，但第三方客户端常常只带其中一处——只认头，一个
/// 在 `metadata.user_id` 里带了自己会话 id 的客户端就会被 luban 换成派生值，它的多轮
/// 对话在上游看来成了「每一轮各自一个会话」。
///
/// **必须校验形态**（[`looks_like_uuid`]）。官方那个恒为 uuid v4，而这个值会同时写进
/// `X-Claude-Code-Session-Id` 头和 `metadata.user_id`：
///
/// - 一个 `sess-42` 之类的短串本身就是判据——上游那边这个字段从来只有 uuid；
/// - 更硬的是带控制字符/超长的值：`HeaderValue::from_str` 会失败，于是头上没有、体里
///   却有，拼出「两处不一致」这个官方绝不产生的组合（[`official_headers`] 里那个
///   `if let Ok(v)` 就是这么漏的）。
///
/// 校验不过就当没带，退回派生值——那至少是个自洽的 uuid。
pub(super) fn incoming_session_id(
    headers: &HeaderMap,
    body: Option<&serde_json::Value>,
) -> Option<String> {
    // **两个来源各自校验**，不是「先取头、再拿结果去过校验」。后者会让一个非法的头
    // **遮住**体里那个合法 uuid：`.or(from_body)` 在头存在时根本不看体，然后校验一挂，
    // 整条退回派生值——客户端明明给了一个能用的会话 id。
    let from_header = headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| looks_like_uuid(v))
        .map(str::to_string);
    // 体那侧走 [`extract_session_id`]——**两种 `metadata.user_id` 格式都认**。
    let from_body = extract_session_id(body).filter(|s| looks_like_uuid(s));

    match (from_header, from_body) {
        // 两处都有且不同：官方这两处**逐字相同**，不同值说明来访自己就不自洽。
        // 拒不拒由 [`store::ForwardFlags::reject_session_conflict`] 拨（默认拒，判在
        // [`session_id_conflict`]）；关掉时退到这里，取头那个并留一行日志——不静默。
        (Some(h), Some(b)) if h != b => {
            tracing::warn!(
                header = %h,
                body = %b,
                "inbound session id differs between the header and metadata.user_id;                  using the header (official CC sends the same value in both)"
            );
            Some(h)
        }
        (Some(h), _) => Some(h),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// 来访的会话 id 头体不一致时返回 `(头那个, 体那个)`；一致、或某一处没有/不合法时 `None`。
///
/// 官方 CC 的 `X-Claude-Code-Session-Id` 与 `metadata.user_id` 里那个 `session_id`
/// **逐字相同**。两处给出两个**都合法却不同**的 uuid，是官方从不产生的形态，而 luban 拿
/// 会话 id 当会话链（`cc_prompt_id` / `cc_prev_req` / `diagnostics`）的键——选错一个就是把
/// 两条链接到了一起，且没有任何办法在事后发现。
///
/// 只在两处**都是合法 uuid** 时才算冲突：一处非法时 [`incoming_session_id`] 本来就只认另
/// 一处，那不是冲突，是客户端只给对了一个。
pub(super) fn session_id_conflict(
    headers: &HeaderMap,
    body: Option<&serde_json::Value>,
) -> Option<(String, String)> {
    let h = headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| looks_like_uuid(v))?
        .to_string();
    // 与 [`incoming_session_id`] 同一个解析器：扁平串（Windows 那种）也算「体里有」，
    // 否则那一类客户端的头体冲突永远检测不到。
    let b = extract_session_id(body).filter(|s| looks_like_uuid(s))?;
    (h != b).then_some((h, b))
}

/// 选号时这条请求的会话那一侧，见 [`session_plan`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionPlan {
    /// 这条请求的会话键（`session_bindings` 与流水的 `session_key` 列），取法见
    /// [`session_binding_key`]。
    pub key: Option<String>,
    /// 选号时按 `key` 写会话绑定、占会话名额；额度探测为假（见下）。
    pub binds: bool,
    /// 带设备身份也按会话占名额，见 [`store::Select::per_session`]。
    pub per_session: bool,
    /// 出站沿用来访会话 id（不走模拟），不占派生槽位，见 [`store::Select::passthrough_session`]。
    pub passthrough: bool,
    /// 匿名侧查询：按 `key` 只**跟随**已有的活跃绑定、跟不上就分散，不写绑定、不占名额，见
    /// [`store::Select::follow_only`]。
    pub follow_only: bool,
    /// 流水上记的键（`usage_logs.session_key`）：一般就是 `key`，匿名侧查询换成带类别的
    /// [`anon_session_key`]。
    pub log_key: Option<String>,
}

/// 这条请求按什么占名额、会话键是什么。
///
/// **会话键**：来访带合法会话 id 就用它，没带用缓存前缀 + 首条用户消息的指纹（`prefix_key`，
/// [`sim_session_key`]）。模拟路径一律要键；不走模拟的只在下面按会话占名额时才要。
///
/// **带设备身份的按会话占名额**（`per_session`）的前提是出站 device_id 确实被改写成收敛后的值：
/// [`store::ForwardFlags::devices_by_session`]（设备指纹归一化、身份伪装、改写设备 ID 三项都开）。
/// 那时一个号在上游只呈现「平台 × 客户端版本」那几台设备，绑了几台真实机器上游看不见，看得见的
/// 是同时活跃几条会话，名额就该按它算。模拟路径也一样——身份伪装关着时模拟请求同样保留客户端
/// 原始 device_id，不能只凭「走模拟」就当成已收敛。任一开关关着，每台真实设备在上游都是一台独立
/// 设备，仍按设备占名额。
///
/// 这里只看开关、不看会落到哪个号：派生 device_id 还要账号有 `account_uuid`
/// （[`crate::credentials::Credential::spoof_device_id`]），而正常的号都有——加号时就拉了 profile，
/// 缺了的（[`crate::credentials::Credential::profile_incomplete`]）下次刷新 token 时顺手补上。
/// 为这个过渡态在选号里逐号切换「按设备 / 按会话」两套名额，代价远大于收益，故不做。
///
/// 按会话占名额时**没有退回按设备那条路**：来访没带会话 id 就按前缀指纹分会话（device_id 取自
/// 体，有设备就一定解析得出体，指纹总算得出来）。否则这类请求会悄悄受设备上限管，而后台在这个
/// 模式下根本不展示设备上限。
///
/// 抓包（`cap/auto-2.1.285-20260930` 的 C 段）：`--continue` 恢复的对话沿用原会话 id（`00314`
/// 与退出前的 `00277` 同一个），回来仍落在原号上；`/clear` 才换新 id、算一条新会话。
///
/// **额度探测与不计费路径不占会话名额**（`no_bind`，`binds` 为假）：CC 每次启动都发一条额度
/// 探测，`--continue` 时它带的是加载历史之前的临时会话 id（同上抓包的 `00292`），之后再也不
/// 出现，占了就要白白空占一个名额到 TTL；`count_tokens` 不产生对话，只有 `/v1/messages` 才算
/// 一条会话。它们只按设备亲和选号。
///
/// **匿名侧查询不占会话名额**（`follow_only`）：没有设备身份的来访（模拟路径），标题生成、安全
/// 分类、预热、额度探测、WebSearch / WebFetch 辅助调用这些一次性请求（`side_class`，见
/// [`super::CcRequestKind::is_side_query`]）没有设备亲和可依，主线程落在哪个号上只能凭会话键
/// 去找：同一个键有活跃绑定就跟过去（官方同一会话本来就在同一个号上），找不到（来访没带会话
/// id、前缀指纹与主线程不同，或主线程那条已过期）就分散到负载最低的号，**不新建绑定**——否则每条
/// 侧查询各占一份名额、白占到 TTL。流水上的键换成带类别的 [`anon_session_key`]，后台据此把这类
/// 用量与真正的对话分开。
///
/// **没有设备身份、不走模拟、但带了合法会话 id 的**（`anon_sid`）也按会话键：真 CC 的
/// `count_tokens` 恒不带 `metadata`（`cap/auto-2.1.285-20260930` 34 条全是，会话头都在），
/// 不按键就只能裸请求按负载选号，同一会话的 token 计数散到一圈号上；按键之后它是侧查询，跟着
/// 主线程走。带会话头却不带 `metadata` 的 `/v1/messages`（v0.3.12 记过 CC Desktop 有这种）也
/// 走这里：多轮对话会话内粘住，不再每轮换号。出站会话 id 沿用来访那个（[`bare_session_id`]
/// 按账号钉住），不占派生槽位（`passthrough`）。
pub(super) fn session_plan(
    flags: store::ForwardFlags,
    simulating: bool,
    has_device: bool,
    inbound_session: Option<&str>,
    prefix_key: Option<&str>,
    no_bind: bool,
    side_class: Option<&str>,
) -> SessionPlan {
    let device_by_session = has_device && flags.devices_by_session();
    let anon_sid = !has_device && !simulating && inbound_session.is_some();
    let key = if simulating || device_by_session || anon_sid {
        match (inbound_session, prefix_key) {
            (Some(sid), _) => Some(session_binding_key(Some(sid), "")),
            (None, Some(k)) => Some(session_binding_key(None, k)),
            (None, None) => None,
        }
    } else {
        None
    };
    let per_session = device_by_session && key.is_some();
    let anon_side = if has_device { None } else { side_class };
    let log_key = match (&key, anon_side) {
        (Some(k), Some(class)) => Some(anon_session_key(k, class)),
        _ => key.clone(),
    };
    SessionPlan {
        binds: key.is_some() && !(per_session && no_bind),
        follow_only: key.is_some() && anon_side.is_some(),
        log_key,
        key,
        per_session,
        passthrough: (per_session || anon_sid) && !simulating,
    }
}

/// 不走模拟、按会话占名额的来访要不要算前缀指纹（[`session_plan`] 的 `prefix_key`）：只有没带
/// 会话 id 的才要——带了就以它为准，指纹是整份 tools + system 的哈希，能省则省。模拟路径另算
/// （它派生出站会话 id 也要用指纹）。
pub(super) fn needs_prefix_key(
    flags: store::ForwardFlags,
    simulating: bool,
    has_device: bool,
    inbound_session: Option<&str>,
) -> bool {
    simulating || (has_device && flags.devices_by_session() && inbound_session.is_none())
}

/// 这条请求**出站**时该落在 `X-Claude-Code-Session-Id` 头与 `metadata.user_id` 两处的
/// 同一个会话 id（非模拟路径；模拟那条的在 [`Simulation::session_id`]）。`None` 即无从
/// 决定，两处都保持来访原样。
///
/// 取值顺序就是 luban 内部已经在用的那套，只是把结论**送到出站**：
///
/// 1. `bare_session`（[`bare_session_id`]）——来访没有 `metadata.user_id`、由 luban 补一份
///    的那条路。它自己已经是「来访头优先（按账号钉住）、否则按账号+设备派生」；
/// 2. 否则 [`incoming_session_id`]——头体各自校验后选出来的那个合法值，`pin`
///    （[`store::ForwardFlags::spoof_identity`]）开着时按账号钉住（[`account_session_id`]），
///    关着时原值照发。
///
/// 两者都取不到时返回 `None`：这时来访要么两处都没有会话 id，要么带的两处都不是 uuid，
/// 没有任何依据凭空造一个（`bare_session` 那条路才有派生的前提，见它的六个条件）。
pub(super) fn outbound_session_id(
    headers: &HeaderMap,
    body: Option<&serde_json::Value>,
    bare_session: Option<&str>,
    cred: &crate::credentials::Credential,
    pin: bool,
) -> Option<String> {
    match bare_session {
        Some(sid) => Some(sid.to_string()),
        None => incoming_session_id(headers, body).map(|sid| pin_session_id(cred, sid, pin)),
    }
}

/// 来访没带 `metadata.user_id` 时用来补一份的 session_id；不需要补时为 `None`。
/// 语义与各项前提见 [`Upstream::bare_session`]。
///
/// 六个前提缺一不可：
/// - `sim.is_none()`：模拟那条路自己带 session_id，不走这里；
/// - 补不补的开关：平时看本功能自己的 `flags.fill_metadata`（网页可关）；billing-only 下改看
///   `flags.real_billing_keep_user_id`（「带 user_id」：带了改写、没带补上，关掉即不带），
///   `fill_metadata` 那项不再起作用；
/// - `flags.spoof_identity`：身份伪装总开关——补出来的那份身份正是它管的东西，
///   它关着还补，等于绕过总开关；
/// - `billable`：非计费路径（count_tokens）出站体一律原样透传，补了也发不出去；
/// - `!has_user_id`：字段已经在就交给 [`spoof_identity`] 原格式改写，两条路只能有一条动它；
/// - `spoof_device_id` 有值：这是 [`ensure_cc_metadata`] 造身份的前提（无 `account_uuid`
///   就造不出自洽身份）。不满足时连头也不补——否则会补出一个「头上有会话 id、体里没
///   metadata」的新破绽，比两处都缺更显眼。
///
/// **不再排除真 CC 客户端**：实测 CC Desktop 等客户端有时不带 `metadata.user_id`，
/// 上游对无 metadata 的请求走更严的限流通道，触发裸 429。补上后同一条请求立即 200。
pub(super) fn bare_session_id(
    headers: &HeaderMap,
    flags: store::ForwardFlags,
    sim: Option<&Simulation>,
    billable: bool,
    has_user_id: bool,
    cred: &crate::credentials::Credential,
    device_fp: &str,
) -> Option<String> {
    let fill = match flags.billing_only() {
        true => flags.real_billing_keep_user_id,
        false => flags.fill_metadata,
    };
    if sim.is_some()
        || !fill
        || !flags.spoof_identity
        || !billable
        || has_user_id
        || cred.spoof_device_id(device_fp).is_none()
    {
        return None;
    }
    // 走到这里必然没有 `metadata.user_id`（上面刚判过），体里也就没有会话 id 可取。来访头上
    // 那个按账号钉住（[`account_session_id`]，本函数已要求 `spoof_identity` 开着），没带才派生。
    Some(
        incoming_session_id(headers, None)
            .map(|sid| pin_session_id(cred, sid, true))
            .unwrap_or_else(|| session_id_for(cred, device_fp)),
    )
}

/// 模拟用的 session_id：按「账号 + seed」派生，见 [`crate::credentials::derive_session_id`]。
///
/// seed 正常是**槽位**（[`crate::credentials::slot_session_seed`]）：这条会话在选中的号上占的
/// 那个槽位，由选号时的会话绑定分配（`session_bindings.slot`），槽位释放后下一个对话复用同一个
/// 会话 id，上游看到的每个账号只在会话上限那么多个 id 之间轮转。没有槽位的（带设备身份、不写
/// 会话绑定的模拟请求）退回按缓存前缀（[`sim_session_key`]）派生，与 v0.3.126 相同。
pub(super) fn session_id_for(cred: &crate::credentials::Credential, seed: &str) -> String {
    crate::credentials::derive_session_id(
        &crate::credentials::session_account_key(cred.account_uuid.as_deref(), cred.id),
        seed,
    )
}

/// 来访自带会话 id 时，出站两处（`X-Claude-Code-Session-Id` 与 `metadata.user_id`）落的那个
/// **按账号钉住**的会话 id：`sha256("luban-session-pin" ‖ account_uuid ‖ 来访会话 id)` 取前
/// 16 字节按 uuid v4 格式化。同一条来访会话在同一个账号上恒定（多轮对话在上游仍是一条
/// 会话、缓存照常接上），换到另一个账号即是另一个 uuid。
///
/// 为什么不能把来访那个原样透传：设备绑定的账号被停用 / 冷却时这台设备会被改绑到别的号，
/// 而客户端那条会话还在继续。`ban/luban-ban-13`、`ban-14` 里两条会话就是这样在 30 秒内先后
/// 出现在两个组织下——device_id 按账号派生（[`crate::credentials::Credential::spoof_device_id`]）
/// 所以是两台设备，会话 uuid 却是同一个。官方客户端一条会话只属于一个账号、一台设备，「同一
/// 个 session uuid 跨两个 org、配两个 device_id」是它永远不产生的形态。钉住之后上游在新号上
/// 看到的是一条全新的会话配一台全新的设备，自洽。
///
/// 三处都走这一份（[`outbound_session_id`]、[`bare_session_id`]、[`Simulation::detect`]），
/// 与 [`spoof_identity`] 同一道闸：这是在改客户端写的身份字段，身份伪装关着时来访原值照发。
/// 前缀与 [`session_id_for`] 不同、输入也不同（那边是设备指纹，这边是来访会话 id），不会撞
/// 出同值。没有 `account_uuid` 的凭证派生不出来（返回 `None`，调用方沿用来访原值）——那种
/// 号的身份本来就补不出来（`spoof_device_id` 同样为 `None`）。
pub(super) fn account_session_id(
    cred: &crate::credentials::Credential,
    client_session: &str,
) -> Option<String> {
    crate::credentials::pinned_session_id(cred.account_uuid.as_deref(), client_session)
}

/// [`account_session_id`] 的开关版：`pin` 为真（[`store::ForwardFlags::spoof_identity`]）时按账号
/// 钉住，派生不出来或开关关着都沿用来访原值。
pub(super) fn pin_session_id(
    cred: &crate::credentials::Credential,
    client_session: String,
    pin: bool,
) -> String {
    if !pin {
        return client_session;
    }
    account_session_id(cred, &client_session).unwrap_or(client_session)
}

#[cfg(test)]
mod tests;
