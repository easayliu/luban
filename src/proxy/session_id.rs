//! 会话 id 的取值、冲突检测与按账号钉住：来访 `X-Claude-Code-Session-Id` 头 /
//! `metadata.user_id` 两处该读哪个、出站两处该写哪个、以及缺失时怎么派生。

use axum::http::HeaderMap;

use crate::store;

use super::body::extract_session_id;
#[cfg(doc)]
use super::body::{sim_device_fingerprint, sim_session_key};
use super::simulation::{Simulation, looks_like_uuid};
use super::uuid_from_bytes;

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
/// - `flags.fill_metadata`：本功能自己的开关（网页可关）；
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
    if sim.is_some()
        || !flags.fill_metadata
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

/// 模拟用的 session_id：`sha256("luban-session" ‖ account_uuid ‖ 会话键)` 取前 16 字节，
/// 按 uuid v4 形态格式化。会话键是这条请求的**缓存前缀加对话起点**的指纹（[`sim_session_key`]：
/// tools、system 正文与首条用户消息），同一账号下同一条对话恒定，换了对话或换了账号即不同。
///
/// 此前第二段是设备指纹：同一账号经模拟路径的所有裸请求共用一个会话 id，不同应用、不同
/// 工作区的对话在上游看来是一条会话打了全部请求。改按前缀派生后，一条会话里 tools 与 system
/// 稳定、消息逐轮增长，与官方一条会话的形态一致；设备仍是同一台（[`sim_device_fingerprint`]）。
///
/// 前缀是为了和 [`crate::credentials::Credential::spoof_device_id`] 分开取值——同样的输入
/// 派生出两个字段，不加区分前缀就会得到「device_id 与 session_id 的高位相同」这种真实
/// 客户端不产生的相关性。
pub(super) fn session_id_for(cred: &crate::credentials::Credential, session_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"luban-session\0");
    h.update(cred.account_uuid.as_deref().unwrap_or("").as_bytes());
    h.update([0u8]);
    h.update(session_key.as_bytes());
    let digest = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    uuid_from_bytes(b)
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
    use sha2::{Digest, Sha256};
    let account = cred.account_uuid.as_deref().map(str::trim).filter(|u| !u.is_empty())?;
    let mut h = Sha256::new();
    h.update(b"luban-session-pin\0");
    h.update(account.as_bytes());
    h.update([0u8]);
    h.update(client_session.as_bytes());
    let digest = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    Some(uuid_from_bytes(b))
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
mod tests {
    use crate::proxy::test_support::{
        PLAIN_BODY, all_on, detect_for, detect_with, rewrite_body_with_session, sim_for, test_cred,
    };
    use crate::proxy::{Bytes, HeaderValue, store};

    /// 来访没带会话 id 时，模拟路径的会话 id 按**缓存前缀 + 对话起点**派生：tools、system、
    /// 首条用户消息相同的请求是同一条会话（后续轮次追加消息不换），任一处变了就是另一条；
    /// 与设备指纹无关；换账号即另一条。
    #[test]
    fn simulated_session_id_follows_the_cache_prefix() {
        let body = |sys: &str, msg: &str| {
            format!(
                r#"{{"model":"claude-opus-5","max_tokens":8,"system":"{sys}","messages":[{{"role":"user","content":"{msg}"}}]}}"#
            )
        };
        let a1 = sim_for(&body("S", "hi")).session_id;
        let a2 = sim_for(
            r#"{"model":"claude-opus-5","max_tokens":8,"system":"S","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"ok"},{"role":"user","content":"more"}]}"#,
        )
        .session_id;
        let b = sim_for(&body("T", "hi")).session_id;
        let c = sim_for(&body("S", "bye")).session_id;
        assert_eq!(a1, a2, "同一对话追加消息仍是同一条会话");
        assert_ne!(a1, b, "system 变了是另一条会话");
        assert_ne!(a1, c, "同一应用的另一个对话（首条用户消息不同）是另一条会话");
        assert!(super::looks_like_uuid(&a1), "{a1}");

        let v: serde_json::Value = serde_json::from_str(&body("S", "hi")).unwrap();
        let key = crate::proxy::sim_session_key(&v);
        let h = crate::proxy::HeaderMap::new();
        // 设备指纹不参与：同一账号换个指纹，会话 id 不变（设备与会话各自派生）。
        let other_fp =
            super::Simulation::detect(Some(&v), &h, false, all_on(), &test_cred(), "other", &key)
                .unwrap();
        assert_eq!(other_fp.session_id, a1);
        // 换账号即另一条会话。
        let mut cred2 = test_cred();
        cred2.account_uuid = Some("11111111-2222-4333-8444-555555555555".into());
        let other_acct =
            super::Simulation::detect(Some(&v), &h, false, all_on(), &cred2, "fp", &key).unwrap();
        assert_ne!(other_acct.session_id, a1);
        // 直接对上派生函数。
        assert_eq!(a1, super::session_id_for(&test_cred(), &key));
    }

    /// 模拟路径的会话 id **优先取来访自己那个**：几个客户端各开各的会话，折叠成一个按设备
    /// 派生的 id，在上游看来就是「一台设备上一个会话打了所有请求」。
    ///
    /// 头和体**两处都看**，且都要过 uuid 形态校验。
    #[test]
    fn simulation_keeps_the_inbound_session_id() {
        const SID1: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
        const SID2: &str = "4dc73702-d904-4887-809d-17b93cc5357c";
        let body = Bytes::from(PLAIN_BODY.to_string());
        let with_header = |sid: &'static str| {
            let mut h = crate::proxy::HeaderMap::new();
            h.insert("x-claude-code-session-id", HeaderValue::from_static(sid));
            h
        };
        // 沿用的是**按账号钉住**的派生值（[`crate::proxy::account_session_id`]）：来访会话之间仍然
        // 一一对应，只是换了账号就是另一个 uuid；身份伪装关着时才是来访原值。
        let pin = |sid: &str| crate::proxy::account_session_id(&test_cred(), sid).unwrap();
        let sim = detect_with(&body, &with_header(SID1), all_on()).expect("该请求应走模拟路径");
        assert_eq!(sim.session_id, pin(SID1), "来访自己带了就用它（按账号钉住）");
        assert_ne!(sim.session_id, SID1);
        let no_spoof = store::ForwardFlags { spoof_identity: false, ..all_on() };
        assert_eq!(
            detect_with(&body, &with_header(SID1), no_spoof).unwrap().session_id,
            SID1,
            "身份伪装关着时原值照用"
        );

        let sim2 = detect_with(&body, &with_header(SID2), all_on()).unwrap();
        assert_ne!(sim.session_id, sim2.session_id, "两个客户端会话不该被折叠成一个");

        // 没带的才按「账号 + 设备指纹」派生。
        let bare = detect_for(&body, all_on()).unwrap();
        assert_ne!(bare.session_id, SID1);
        assert_eq!(bare.session_id, detect_for(&body, all_on()).unwrap().session_id, "派生值稳定");

        // 头上没有、**体里有**：官方两处逐字相同，只认头的话，一个只在 metadata 里带了
        // 会话 id 的客户端，它的多轮对话在上游看来就成了「每轮各自一个会话」。
        let in_body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"metadata":{{"user_id":"{{\"device_id\":\"d\",\"session_id\":\"{SID2}\"}}"}}}}"#
        ));
        let from_body = detect_for(&in_body, all_on()).expect("该请求应走模拟路径");
        assert_eq!(from_body.session_id, pin(SID2), "体里那个也要认");

        // **形态不对就不认**：官方那个恒为 uuid。`sess-42` 这种短串本身就是判据，而带控制
        // 字符的值会让 `HeaderValue::from_str` 失败——于是头上没有、体里却有，拼出「两处
        // 不一致」这个官方绝不产生的组合。一律退回派生值。
        for bad in ["sess-42", "", "not-a-uuid-at-all", "D0C1FB05-9B19-4576-9465-E2B8A206DABF"] {
            let mut h = crate::proxy::HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(bad) {
                h.insert("x-claude-code-session-id", v);
            }
            let s = detect_with(&body, &h, all_on()).unwrap();
            assert_eq!(s.session_id, bare.session_id, "{bad:?} 该退回派生值");
        }
    }

    /// 头与体**各自校验**：非法的头不能遮住体里那个合法 uuid。
    #[test]
    fn a_bad_header_does_not_mask_a_valid_body_session_id() {
        const GOOD: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
        const OTHER: &str = "4dc73702-d904-4887-809d-17b93cc5357c";
        let body: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"metadata":{{"user_id":"{{\"session_id\":\"{GOOD}\"}}"}}}}"#
        ))
        .unwrap();
        let with = |h: &'static str| {
            let mut m = crate::proxy::HeaderMap::new();
            m.insert("x-claude-code-session-id", HeaderValue::from_static(h));
            m
        };

        // 头非法：**不能**因此把体里那个合法的一起丢掉。原先 `.or(from_body)` 在头存在时
        // 根本不看体，校验一挂整条退回派生值。
        assert_eq!(
            crate::proxy::incoming_session_id(&with("sess-42"), Some(&body)).as_deref(),
            Some(GOOD),
            "非法的头不该遮住体里那个"
        );
        // 头合法：用头（官方两处相同，这里也是绝大多数情形）。
        assert_eq!(
            crate::proxy::incoming_session_id(&with(OTHER), Some(&body)).as_deref(),
            Some(OTHER),
            "两处都合法但不同值时取头，并打一行 warn"
        );
        // 只有头。
        assert_eq!(crate::proxy::incoming_session_id(&with(OTHER), None).as_deref(), Some(OTHER));
        // 两处都没有/都不合法。
        assert!(crate::proxy::incoming_session_id(&crate::proxy::HeaderMap::new(), None).is_none());
        assert!(crate::proxy::incoming_session_id(&with("sess-42"), None).is_none());
    }

    /// 会话 id 头体冲突的判据：两处**都合法却不同**才算，其余一律不算。
    ///
    /// 拒绝本身由 `reject_session_conflict` 拨（默认开），判据在这里。只有一处合法时不算
    /// 冲突——那是客户端只给对了一个，[`crate::proxy::incoming_session_id`] 会取合法的那个。
    #[test]
    fn session_id_conflict_needs_two_valid_different_uuids() {
        const A: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
        const B: &str = "4dc73702-d904-4887-809d-17b93cc5357c";
        let body = |sid: &str| -> serde_json::Value {
            serde_json::from_str(&format!(
                r#"{{"metadata":{{"user_id":"{{\"session_id\":\"{sid}\"}}"}}}}"#
            ))
            .unwrap()
        };
        let with = |h: &'static str| {
            let mut m = crate::proxy::HeaderMap::new();
            m.insert("x-claude-code-session-id", HeaderValue::from_static(h));
            m
        };

        // 两处都合法且不同 → 冲突。
        assert_eq!(
            crate::proxy::session_id_conflict(&with(A), Some(&body(B))),
            Some((A.to_string(), B.to_string()))
        );
        // 相同 → 不冲突（官方就是这个形态）。
        assert!(crate::proxy::session_id_conflict(&with(A), Some(&body(A))).is_none());
        // 只有一处 → 不冲突。
        assert!(crate::proxy::session_id_conflict(&with(A), None).is_none());
        assert!(
            crate::proxy::session_id_conflict(&crate::proxy::HeaderMap::new(), Some(&body(B)))
                .is_none()
        );
        // 一处非法 → 不算冲突，是「只给对了一个」。
        assert!(crate::proxy::session_id_conflict(&with("sess-42"), Some(&body(B))).is_none());
        assert_eq!(
            crate::proxy::incoming_session_id(&with("sess-42"), Some(&body(B))).as_deref(),
            Some(B),
            "那种情况下取合法的那个"
        );
    }

    /// 扁平 `metadata.user_id`（Windows 那类客户端）里的会话 id 必须和内嵌 JSON 同等对待。
    ///
    /// 曾经体那侧有两份解析器：转发主路用认两种格式的 [`crate::proxy::extract_session_id`]，而
    /// [`crate::proxy::incoming_session_id`] 与 [`crate::proxy::session_id_conflict`] 用的那份只认内嵌
    /// JSON。于是扁平串客户端在冲突检测眼里「体里没有会话 id」——头体不一致对整整一类
    /// 客户端形同虚设，默认拒的开关也拦不住。
    #[test]
    fn the_flat_user_id_carries_a_session_id_too() {
        const A: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
        const B: &str = "4dc73702-d904-4887-809d-17b93cc5357c";
        let flat = |sid: &str| -> serde_json::Value {
            serde_json::json!({
                "metadata": { "user_id": format!("user_deadbeef_account_acct-1_session_{sid}") }
            })
        };
        let with = |h: &'static str| {
            let mut m = crate::proxy::HeaderMap::new();
            m.insert("x-claude-code-session-id", HeaderValue::from_static(h));
            m
        };

        // 体那侧读得出来（三个入口同一个解析器）。
        assert_eq!(crate::proxy::extract_session_id(Some(&flat(B))).as_deref(), Some(B));
        // 头 A、扁平体 B：两处都合法且不同 → 冲突，默认开关会本地拒。
        assert_eq!(
            crate::proxy::session_id_conflict(&with(A), Some(&flat(B))),
            Some((A.to_string(), B.to_string())),
            "扁平格式的头体冲突也要检测得到"
        );
        // 相同 → 不冲突。
        assert!(crate::proxy::session_id_conflict(&with(A), Some(&flat(A))).is_none());
        // 头非法、扁平体合法 → 不是冲突，是「只给对了一个」，选体里那个。
        assert!(crate::proxy::session_id_conflict(&with("sess-42"), Some(&flat(B))).is_none());
        assert_eq!(
            crate::proxy::incoming_session_id(&with("sess-42"), Some(&flat(B))).as_deref(),
            Some(B)
        );
        // 头缺失、扁平体合法 → 选体里那个。
        assert_eq!(
            crate::proxy::incoming_session_id(&crate::proxy::HeaderMap::new(), Some(&flat(B)))
                .as_deref(),
            Some(B)
        );
        // 扁平串里 session 段不是 uuid → 当没带（同内嵌 JSON 那侧的口径）。
        assert!(
            crate::proxy::incoming_session_id(
                &crate::proxy::HeaderMap::new(),
                Some(&flat("sess-9"))
            )
            .is_none()
        );
    }

    /// **出站两处必须同值**：`X-Claude-Code-Session-Id` 头与 `metadata.user_id` 里那个
    /// 会话 id，官方逐字相同。此前只验了「luban 内部选中了哪个值」，没验最终发出去的那份，
    /// 于是两种情形一直漏着：非法的头原样转发、体里有合法值而头是缺的。
    #[test]
    fn the_outbound_session_id_is_the_same_in_the_header_and_the_body() {
        const GOOD: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
        const OTHER: &str = "4dc73702-d904-4887-809d-17b93cc5357c";
        let cred = test_cred();
        let device_fp = "fp";

        // 一条 CC 形态的请求（走「补身份 / 定点改写」那条路，不走模拟）。
        let body_with = |user_id: Option<&str>| -> Bytes {
            let mut v = serde_json::json!({
                "model": "claude-opus-5",
                "messages": [{"role": "user", "content": "hi"}],
                "system": [{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}]
            });
            if let Some(u) = user_id {
                v["metadata"] = serde_json::json!({ "user_id": u });
            }
            Bytes::from(serde_json::to_vec(&v).unwrap())
        };
        let headers_with = |h: Option<&str>| {
            let mut m = crate::proxy::HeaderMap::new();
            if let Some(h) = h {
                m.insert("x-claude-code-session-id", HeaderValue::from_str(h).unwrap());
            }
            m
        };

        // 走一遍转发路径上那三步：选 bare_session → 选出站会话 id → 落到头与体。
        let outbound_with = |flags: store::ForwardFlags,
                             h: Option<&str>,
                             user_id: Option<&str>|
         -> (Option<String>, Option<String>) {
            let headers = headers_with(h);
            let body = body_with(user_id);
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let bare = crate::proxy::bare_session_id(
                &headers,
                flags,
                None,
                true,
                crate::proxy::body_has_user_id(Some(&parsed)),
                &cred,
                device_fp,
            );
            let session_out = crate::proxy::outbound_session_id(
                &headers,
                Some(&parsed),
                bare.as_deref(),
                &cred,
                flags.spoof_identity,
            );
            let out_headers = crate::proxy::build_forward_headers(
                &headers,
                "tok",
                flags,
                None,
                session_out.as_deref(),
            );
            let out_body = rewrite_body_with_session(
                &body,
                &cred,
                device_fp,
                flags,
                None,
                bare.as_deref(),
                session_out.as_deref(),
            );
            let out_json: serde_json::Value = serde_json::from_slice(&out_body).unwrap();
            (
                out_headers
                    .get("x-claude-code-session-id")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
                crate::proxy::extract_session_id(Some(&out_json)),
            )
        };
        let outbound = |h: Option<&str>, user_id: Option<&str>| outbound_with(all_on(), h, user_id);
        let json_id =
            |sid: &str| format!(r#"{{"device_id":"d","account_uuid":"a","session_id":"{sid}"}}"#);
        let flat_id = |sid: &str| format!("user_deadbeef_account_acct-1_session_{sid}");
        // 身份伪装开着时出站落的是**按账号钉住**的派生值（[`crate::proxy::account_session_id`]），
        // 不是来访原值：同一条会话换号后不该带着同一个 uuid 出现在另一个组织下。
        let pin = |sid: &str| crate::proxy::account_session_id(&cred, sid).unwrap();
        let good = pin(GOOD);
        let other = pin(OTHER);
        assert_ne!(good, GOOD, "钉住的值与来访原值不同");
        assert_ne!(good, other, "两条来访会话钉出来的是两个值");
        assert!(crate::proxy::looks_like_uuid(&good), "钉住的值是 uuid: {good}");

        // 1) 头是 `sess-42`、体里是合法 uuid：选体里那个，**头也得换成它**。
        //    此前非法的头原样发了出去，出站两处对不上。
        let (h, b) = outbound(Some("sess-42"), Some(&json_id(GOOD)));
        assert_eq!(h.as_deref(), Some(good.as_str()), "非法的头要被选中的那个顶掉");
        assert_eq!(b.as_deref(), Some(good.as_str()));
        assert_eq!(h, b, "出站两处必须同值");

        // 2) 头缺失、体里是合法 uuid：**头要补上**。此前 `bare_session` 只在「体里没有
        //    metadata」时才有值，这条路上头一直是缺的。
        let (h, b) = outbound(None, Some(&json_id(GOOD)));
        assert_eq!(h.as_deref(), Some(good.as_str()), "头缺失时按体里那个补上");
        assert_eq!(h, b);

        // 3) 扁平格式同样成立（Windows 那类客户端）。
        let (h, b) = outbound(None, Some(&flat_id(GOOD)));
        assert_eq!(h.as_deref(), Some(good.as_str()));
        assert_eq!(h, b);
        let (h, b) = outbound(Some("sess-42"), Some(&flat_id(GOOD)));
        assert_eq!(h.as_deref(), Some(good.as_str()));
        assert_eq!(h, b);

        // 4) 两处都合法却不同（默认会被 `reject_session_conflict` 本地拒；关掉后走到这里）：
        //    [`crate::proxy::incoming_session_id`] 取头那个，体里那份就得跟着改成它——否则发出去的
        //    仍是一份官方不产生的请求。
        let (h, b) = outbound(Some(OTHER), Some(&json_id(GOOD)));
        assert_eq!(h.as_deref(), Some(other.as_str()), "两处都合法时取头");
        assert_eq!(b.as_deref(), Some(other.as_str()), "体里那份要同步过去");
        let (h, b) = outbound(Some(OTHER), Some(&flat_id(GOOD)));
        assert_eq!(
            (h.as_deref(), b.as_deref()),
            (Some(other.as_str()), Some(other.as_str())),
            "扁平串同理"
        );

        // 4b) 头合法、体里有 user_id 但**没有会话段**：`bare_session` 不接（有 user_id），
        //     [`crate::proxy::ensure_cc_metadata`] 也不接（同理），此前这条路上体里一直缺着——
        //     头有会话、体没会话，官方绝不产生。现在按各自格式补一段。
        let (h, b) = outbound(Some(GOOD), Some(r#"{"device_id":"d","account_uuid":"a"}"#));
        assert_eq!(h.as_deref(), Some(good.as_str()));
        assert_eq!(b.as_deref(), Some(good.as_str()), "内嵌 JSON 缺会话段要补上");
        let (h, b) = outbound(Some(GOOD), Some("user_deadbeef_account_acct-1"));
        assert_eq!(h.as_deref(), Some(good.as_str()));
        assert_eq!(b.as_deref(), Some(good.as_str()), "扁平串缺会话段要补上");

        // 5) 头合法、体里没有 metadata：走补身份那条路，两处都是头里那个钉住后的值（回归）。
        let (h, b) = outbound(Some(GOOD), None);
        assert_eq!(h.as_deref(), Some(good.as_str()));
        assert_eq!(h, b);

        // 6) 头非法、体里也没有 metadata：`bare_session` 派生一个，头上那个非法值同样要被
        //    顶掉——否则「头 sess-42 + 体里派生 uuid」又是一处对不上。
        let (h, b) = outbound(Some("sess-42"), None);
        assert!(h.as_deref().is_some_and(crate::proxy::looks_like_uuid), "派生值是 uuid: {h:?}");
        assert_eq!(h, b);

        // 7) 正常形态（两处同值）：钉住后两处仍同值；**身份伪装关着**时一个字节都不动——
        //    头不重写、体不改写，来访原值照发。
        let same = json_id(GOOD);
        let (h, b) = outbound(Some(GOOD), Some(&same));
        assert_eq!((h.as_deref(), b.as_deref()), (Some(good.as_str()), Some(good.as_str())));
        let no_spoof = store::ForwardFlags { spoof_identity: false, ..all_on() };
        let before = body_with(Some(&same));
        let (h, b) = outbound_with(no_spoof, Some(GOOD), Some(&same));
        assert_eq!((h.as_deref(), b.as_deref()), (Some(GOOD), Some(GOOD)), "关着时原值照发");
        let after: serde_json::Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(crate::proxy::extract_session_id(Some(&after)).as_deref(), Some(GOOD));
        // 关着时头体不一致的来访照样被归一到选中的那个原值（归一不受这道闸影响）。
        let (h, b) = outbound_with(no_spoof, Some(OTHER), Some(&json_id(GOOD)));
        assert_eq!(
            (h.as_deref(), b.as_deref()),
            (Some(OTHER), Some(GOOD)),
            "关着时体里的身份字段不动、头归一到选中的那个"
        );
    }

    /// [`crate::proxy::account_session_id`]：同一条来访会话在同一个账号上恒定、换账号即不同；没有
    /// `account_uuid` 的凭证派生不出来；与设备派生那份（[`crate::proxy::session_id_for`]）不撞。
    /// 这是 `ban/luban-ban-13/14` 里「同一个 session uuid 30 秒内出现在两个组织下」的修法。
    #[test]
    fn the_outbound_session_id_is_pinned_per_account() {
        const SID: &str = "8d80a214-b800-4073-a976-c98058fa0eef";
        const SID2: &str = "47904256-0f00-4445-a5da-e660c674ffe1";
        let a = test_cred();
        let mut b = test_cred();
        b.id = 2;
        b.account_uuid = Some("f54bd3ef-9040-4d35-a50f-3caa75ce4b97".into());
        let on_a = crate::proxy::account_session_id(&a, SID).unwrap();
        let on_b = crate::proxy::account_session_id(&b, SID).unwrap();
        assert!(crate::proxy::looks_like_uuid(&on_a), "{on_a}");
        assert_ne!(on_a, SID, "不是来访原值");
        assert_ne!(on_a, on_b, "换账号即是另一条会话");
        assert_eq!(on_a, crate::proxy::account_session_id(&a, SID).unwrap(), "同账号同会话恒定");
        assert_ne!(
            on_a,
            crate::proxy::account_session_id(&a, SID2).unwrap(),
            "同账号两条会话不折叠"
        );
        assert_ne!(
            on_a,
            crate::proxy::session_id_for(&a, SID),
            "与设备派生那份前缀不同，同输入也不撞"
        );
        // 没有 account_uuid：派生不出来，调用方沿用原值。
        let mut bare = test_cred();
        bare.account_uuid = None;
        assert_eq!(crate::proxy::account_session_id(&bare, SID), None);
        assert_eq!(crate::proxy::pin_session_id(&bare, SID.into(), true), SID);
        bare.account_uuid = Some("  ".into());
        assert_eq!(crate::proxy::account_session_id(&bare, SID), None);
        // 开关关着：原值。
        assert_eq!(crate::proxy::pin_session_id(&a, SID.into(), false), SID);
        assert_eq!(crate::proxy::pin_session_id(&a, SID.into(), true), on_a);
    }

    /// 不该补身份的四种情形——补错了都会造出「官方不产生的形态」，比不补更糟。
    #[test]
    fn bare_session_skipped_when_it_would_break_shape() {
        let bare = crate::proxy::HeaderMap::new();
        let call = |flags, sim, billable, has_user_id, cred: &crate::credentials::Credential| {
            crate::proxy::bare_session_id(&bare, flags, sim, billable, has_user_id, cred, "fp")
        };
        let sim = sim_for(PLAIN_BODY);

        // 1) 走模拟那条路：session_id 在 Simulation 里，不能再派生一个。
        assert!(call(all_on(), Some(&sim), true, false, &test_cred()).is_none());
        // 2) 身份伪装关着：这是总开关。
        let no_spoof = store::ForwardFlags { spoof_identity: false, ..all_on() };
        assert!(call(no_spoof, None, true, false, &test_cred()).is_none());
        // 3) 非计费路径（count_tokens）：出站体原样透传，补了也发不出去，只剩个孤头。
        assert!(call(all_on(), None, false, false, &test_cred()).is_none());
        // 4) 来访已经有 user_id：交给 spoof_identity 原格式改写，两条路只能有一条动它。
        assert!(call(all_on(), None, true, true, &test_cred()).is_none());
        // 5) 凭证没有 account_uuid：造不出自洽身份，连头也不补（否则头有体无）。
        let no_uuid = crate::credentials::Credential { account_uuid: None, ..test_cred() };
        assert!(call(all_on(), None, true, false, &no_uuid).is_none());
        // 6) 本功能自己的开关关着。
        let no_fill = store::ForwardFlags { fill_metadata: false, ..all_on() };
        assert!(call(no_fill, None, true, false, &test_cred()).is_none());
    }
}
