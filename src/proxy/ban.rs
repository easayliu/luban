//! 账号封禁探测：从上游错误响应里判定是否应该自动停用凭证，以及第三方应用拒绝的取证日志。

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};

use super::digest::{head, redact_headers, request_digest};

/// 账号被停用时的**状态词**：单独出现不作数，必须与 [`BAN_SUBJECTS`] 中的主语同时出现。
///
/// 这些词曾是裸子串匹配，代价是上游回显请求字段名时会误伤——`"thinking.type.disabled" is
/// not supported for this model` 是一条再普通不过的参数错误，却因字段名里含 `disabled`
/// 被判成封号；客户端只要重试，池子里的号会被逐个扣光。状态词离开主语没有信息量，
/// 「谁 disabled 了」才是判据，故改为共现。
const BAN_STATES: &[&str] =
    &["disabled", "suspended", "banned", "terminated", "deactivated", "violat"];

/// [`BAN_STATES`] 的合法主语：状态词说的是这几样东西时才算账号级错误。
const BAN_SUBJECTS: &[&str] = &["account", "organization", "workspace", "api key", "credential"];

/// 与主语无关、单独出现即判定的特征词：OAuth 刷新失败的报文里没有 account 主语。
const BAN_KEYWORDS: &[&str] = &["invalid_grant", "oauth"];

/// 反向豁免：命中其一则**一定不是**账号级问题，无论状态码与特征词如何都不停用。
/// 用于挡住「特征词碰巧出现在非账号报错里」的误杀，见 [`detect_account_ban`]。
/// 首项不写死 endpoint/model，是因为两者都出现过同款文案。
///
/// 每一项都是**裸子串**匹配，且一旦命中就把 `oauth` 特征词与 401 `authentication_error`
/// 两条判据整个作废，所以写得越像上游原话越好——短到只剩「not allowed for this」这种
/// 片段，任何顺带提到它的真封号文案都会被放过去。「主语 + 状态词」共现不受这里影响，
/// 见 [`detect_account_ban`]。
///
/// 末项取自 `OAuth authentication is currently not allowed for this organization.`：组织
/// 管理员没放开（或关掉了）Claude Code 的 OAuth 登录，是**组织侧的权限配置**、开回来就
/// 恢复，不是账号被封；它带 `oauth` 一词，不豁免会被 [`BAN_KEYWORDS`] 命中而永久停用
/// （保活路径此前正是这么误封的）。只去掉结尾的主语（organization / workspace 都可能），
/// 前半句整句保留。
const NOT_ACCOUNT_PHRASES: &[&str] = &[
    "not supported for this",
    "does not support",
    "unsupported model",
    "oauth authentication is currently not allowed for this",
];

/// 从上游错误响应体解析 `(error.type, error.message)`；解析失败时 message 退化为整段原文。
/// 取一个响应头的文本值；缺失或非 UTF-8 时返回 `"-"`，与日志里其余缺值字段同形。
pub(super) fn header_text(headers: &HeaderMap, name: &str) -> String {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("-").to_string()
}

pub(crate) fn parse_upstream_error(body: &[u8]) -> (Option<String>, String) {
    let text = String::from_utf8_lossy(body);
    let v = serde_json::from_slice::<serde_json::Value>(body).ok();
    let field = |name: &str| {
        v.as_ref().and_then(|v| v.get("error")?.get(name)?.as_str().map(str::to_string))
    };
    (field("type"), field("message").unwrap_or_else(|| text.to_string()))
}

/// 依据状态码与响应体判定是否应自动停用该凭证，命中则返回写入 `ban_reason` 的原因
/// （`[状态码] 类型: 消息`，截断至 200 字符）。
///
/// 三档都要求响应体确实是 Anthropic 的错误 JSON（能取到 `error.type`）或命中特征词，
/// 避免把「非账号问题的 4xx」当成封号，把健康账号打成停用：
/// - 401：`authentication_error` 才停用。裸 401（CDN/网关拦截，无 `error.type`）不停用。
/// - 403：**仅**命中特征词时停用。普通 `permission_error`（如 Pro 账号请求
///   Opus、beta 未开通、区域限制）是能力/权限问题而非封号，原样透传即可。
/// - 400：同 403，仅命中特征词时停用；普通 `invalid_request_error` 是客户端请求错误。
///
/// 「命中特征词」= [`BAN_KEYWORDS`] 之一，或 [`BAN_SUBJECTS`] 与 [`BAN_STATES`] 各中一项。
pub(crate) fn detect_account_ban(status: StatusCode, body: &[u8]) -> Option<String> {
    let (etype, message) = parse_upstream_error(body);
    match ban_verdict(status, etype.as_deref(), &message) {
        BanVerdict::Ban => {
            let head = match &etype {
                Some(t) => format!("[{}] {t}: {message}", status.as_u16()),
                None => format!("[{}] {message}", status.as_u16()),
            };
            Some(head.chars().take(200).collect())
        }
        BanVerdict::Exempt { phrase, overrode_signal: true } => {
            // 豁免改写了结论：没有它这条会被停用。新出现的混合文案（真封号却顺带提到豁免
            // 短语）会先在这里露头，而不是等复盘时才发现漏封。豁免没改写结论的（比如
            // 400 参数错误里回显了字段名）不记，那是每天都有的正常报错。
            tracing::warn!(
                status = status.as_u16(),
                error_type = etype.as_deref().unwrap_or("-"),
                phrase,
                message = %message.chars().take(300).collect::<String>(),
                "account-ban check: an exemption phrase overrode a ban signal; leaving the credential enabled (review if this is a new upstream wording)"
            );
            None
        }
        BanVerdict::Exempt { overrode_signal: false, .. } | BanVerdict::Clear => None,
    }
}

/// [`detect_account_ban`] 的纯判定部分，拆出来是为了能直接测「豁免有没有改写结论」。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BanVerdict {
    /// 账号级错误，停用。
    Ban,
    /// 命中了 [`NOT_ACCOUNT_PHRASES`] 的 `phrase` 而放行；`overrode_signal` 为真表示
    /// 若无豁免本会停用（特征词或 401 `authentication_error` 命中了），值得记一条日志。
    Exempt { phrase: &'static str, overrode_signal: bool },
    /// 什么都没命中，普通 4xx。
    Clear,
}

pub(super) fn ban_verdict(status: StatusCode, etype: Option<&str>, message: &str) -> BanVerdict {
    let hay = format!("{} {}", etype.unwrap_or(""), message).to_lowercase();
    // 「主语 + 状态词」共现（organization disabled / account suspended）是最明确的账号级
    // 信号，**压过下面的豁免**：将来上游若把「organization disabled … OAuth … not allowed
    // for this organization」写在同一句里，不能因为带了豁免短语就漏掉一次真封号。
    let subject_and_state =
        BAN_SUBJECTS.iter().any(|s| hay.contains(s)) && BAN_STATES.iter().any(|s| hay.contains(s));
    // 较弱的两条判据：独立特征词，以及 401 的 `authentication_error` 类型。
    let weak_signal = match status {
        StatusCode::UNAUTHORIZED => {
            etype == Some("authentication_error") || BAN_KEYWORDS.iter().any(|k| hay.contains(k))
        }
        StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST => {
            BAN_KEYWORDS.iter().any(|k| hay.contains(k))
        }
        _ => false,
    };
    if subject_and_state
        && matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST
        )
    {
        return BanVerdict::Ban;
    }
    // 排除「端点/能力不支持」「组织没放开 OAuth」这类与账号状态无关的报错——它们可能带上
    // oauth 等特征词（如 401 `OAuth authentication is currently not supported for this
    // endpoint`），但账号本身是好的，停用了反而白扣一个号。豁免只挡得住上面两条弱判据。
    if let Some(phrase) = NOT_ACCOUNT_PHRASES.iter().find(|p| hay.contains(*p)) {
        return BanVerdict::Exempt { phrase, overrode_signal: weak_signal };
    }
    if weak_signal { BanVerdict::Ban } else { BanVerdict::Clear }
}

/// 「被判成第三方应用」的特征文案。上游原文形如：
/// `Third-party apps now draw from your extra usage, not your plan limits.
/// Add more at claude.ai/settings/usage and keep going`。
///
/// 两条都不带主语，故只能裸子串匹配；但它们只用来决定**要不要多打一条日志**，
/// 误伤的代价仅是一条多余的 info，与 [`BAN_KEYWORDS`] 那种会停用凭证的判据不同。
const THIRD_PARTY_PHRASES: &[&str] = &["third-party app", "extra usage"];

/// 上游是否把这条请求判成了第三方应用（额度改扣超额池而非订阅额度）。
///
/// **注意它不会被 [`detect_account_ban`] 误判成封号**：这段文案里既没有 `oauth`/
/// `invalid_grant`，也凑不出「主语 + 状态词」的共现，故不会停用凭证——账号是好的，
/// 被拒的是请求形态。
pub(super) fn is_third_party_rejection(body: &[u8]) -> bool {
    let (etype, message) = parse_upstream_error(body);
    let hay = format!("{} {}", etype.as_deref().unwrap_or(""), message).to_lowercase();
    THIRD_PARTY_PHRASES.iter().any(|p| hay.contains(p))
}

/// 上游把请求判成第三方应用时，把**我们实际发出去的那份请求**的形态打成一条 info。
///
/// 这类 400 的错误文本本身没有信息量（它只说「你是第三方」，不说凭什么），要查只能看
/// 出站报文长什么样：头的拼写与顺序、`system` 块数与断点、`tools` 里的名字与类型、
/// `metadata` 身份、顶层 key 顺序——判据在这些里面，不在错误文本里。
///
/// **打摘要而不是原文**，两个理由：
///   - 隐私：`messages` 是用户的对话内容，服务端日志不该留。故只记 role + 每块的类型，
///     `text` 只记长度；`tool_use` 记名字（工具名正是要查的那个维度）。
///   - 体积：`system` 在模拟路径下是 10KB 量级的官方基座，原文打出来会把日志刷没。
///     故每块只记长度 + 前 [`super::digest::DUMP_TEXT_HEAD`] 字符 + `cache_control` 原样。
///
/// 其余顶层字段（`model`/`stream`/`metadata`/`tool_choice`/`context_management`…）**原样**
/// 打出——它们既不含用户内容，又都是形态判据。顶层 key 的**顺序**也照抄（本 crate 开了
/// serde_json 的 `preserve_order`），因为顺序本身就是一处判据。
pub(super) fn log_third_party_rejection(
    sent: &Bytes,
    headers: &HeaderMap,
    cred: &crate::credentials::Credential,
    status: StatusCode,
) {
    let hdr = redact_headers(headers);
    let body = match serde_json::from_slice::<serde_json::Value>(sent) {
        Ok(v) => request_digest(&v).to_string(),
        Err(_) => format!(
            "<unparsable {} bytes> {}",
            sent.len(),
            head(&String::from_utf8_lossy(sent), 512)
        ),
    };
    tracing::info!(
        cred_id = cred.id, cred = %cred.label,
        status = status.as_u16(),
        bytes = sent.len(),
        headers = %hdr,
        body = %body,
        "upstream rejected the request as a third-party app; dumping the outbound request shape"
    );
}

#[cfg(test)]
mod tests {
    use crate::proxy::test_support::ROLE_400;
    use crate::proxy::{HeaderValue, StatusCode, detect_account_ban, is_third_party_rejection};

    fn err_body(etype: &str, msg: &str) -> Vec<u8> {
        serde_json::json!({"type": "error", "error": {"type": etype, "message": msg}})
            .to_string()
            .into_bytes()
    }

    /// 账号级错误照旧停用。
    #[test]
    fn bans_on_real_account_errors() {
        let cases = [
            (StatusCode::UNAUTHORIZED, err_body("authentication_error", "invalid bearer token")),
            (StatusCode::FORBIDDEN, err_body("permission_error", "This account has been disabled")),
            (
                StatusCode::BAD_REQUEST,
                err_body("invalid_request_error", "Your account was suspended"),
            ),
            // 主语不止 account：组织级停用同样是封号。
            (
                StatusCode::FORBIDDEN,
                err_body("permission_error", "This organization has been deactivated"),
            ),
            // OAuth 刷新失败没有主语词，靠独立特征词命中。
            (StatusCode::BAD_REQUEST, err_body("invalid_request_error", "invalid_grant")),
            // 豁免短语与明确的「主语 + 状态词」同句：后者优先，仍是封号。
            (
                StatusCode::FORBIDDEN,
                err_body(
                    "permission_error",
                    "This organization has been disabled; OAuth authentication is not allowed for this organization.",
                ),
            ),
            (
                StatusCode::FORBIDDEN,
                err_body(
                    "permission_error",
                    "Account suspended: this feature is not supported for this account.",
                ),
            ),
        ];
        for (status, body) in cases {
            assert!(
                detect_account_ban(status, &body).is_some(),
                "应判定为账号级错误: {status} {}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    /// 豁免有没有改写结论要分得清：改写了的（没有豁免就会停用）记日志，没改写的（普通
    /// 参数错误回显字段名）不记；「主语 + 状态词」共现不受豁免影响。
    #[test]
    fn ban_verdict_tells_overriding_exemptions_apart() {
        use crate::proxy::ban::{BanVerdict, ban_verdict};
        let org_oauth = "OAuth authentication is currently not allowed for this organization.";
        assert_eq!(
            ban_verdict(StatusCode::FORBIDDEN, Some("permission_error"), org_oauth),
            BanVerdict::Exempt {
                phrase: "oauth authentication is currently not allowed for this",
                overrode_signal: true
            },
            "带 oauth 特征词，没有豁免会停用"
        );
        assert_eq!(
            ban_verdict(
                StatusCode::UNAUTHORIZED,
                Some("authentication_error"),
                "OAuth authentication is currently not supported for this endpoint",
            ),
            BanVerdict::Exempt { phrase: "not supported for this", overrode_signal: true },
            "401 authentication_error 本会停用"
        );
        assert_eq!(
            ban_verdict(
                StatusCode::BAD_REQUEST,
                Some("invalid_request_error"),
                "\"thinking.type.disabled\" is not supported for this model.",
            ),
            BanVerdict::Exempt { phrase: "not supported for this", overrode_signal: false },
            "状态词没有主语、没有特征词：豁免没改写什么，不必记"
        );
        assert_eq!(
            ban_verdict(
                StatusCode::FORBIDDEN,
                Some("permission_error"),
                "This organization has been disabled; OAuth authentication is not allowed for this organization.",
            ),
            BanVerdict::Ban,
            "主语 + 状态词压过豁免"
        );
        assert_eq!(
            ban_verdict(
                StatusCode::FORBIDDEN,
                Some("permission_error"),
                "No access to claude-opus-5"
            ),
            BanVerdict::Clear
        );
        assert_eq!(
            ban_verdict(
                StatusCode::TOO_MANY_REQUESTS,
                Some("rate_limit_error"),
                "account suspended"
            ),
            BanVerdict::Clear,
            "只看 400/401/403"
        );
    }

    /// 非账号问题的 4xx 不得停用——这类误杀会把健康账号一个个扣掉。
    #[test]
    fn does_not_ban_on_non_account_errors() {
        let cases = [
            // Pro 账号请求 Opus / beta 未开通：能力问题，不是封号。
            (
                StatusCode::FORBIDDEN,
                err_body("permission_error", "Your account does not have access to claude-opus-5"),
            ),
            // 裸 401（CDN/网关拦截，非 Anthropic 错误 JSON）。
            (StatusCode::UNAUTHORIZED, b"<html>401 Unauthorized</html>".to_vec()),
            // 客户端请求错误。
            (
                StatusCode::BAD_REQUEST,
                err_body("invalid_request_error", "max_tokens: must be <= 64000"),
            ),
            // 特征词碰巧出现在「端点不支持」里：账号是好的。
            (
                StatusCode::UNAUTHORIZED,
                err_body(
                    "authentication_error",
                    "OAuth authentication is currently not supported for this endpoint",
                ),
            ),
            // 上游回显请求字段名，字段名里含状态词。曾在 v0.2.69 把整池账号逐个误禁：
            // 客户端每重试一次就扣掉一个号，而账号本身完全健康。
            (
                StatusCode::BAD_REQUEST,
                err_body(
                    "invalid_request_error",
                    "\"thinking.type.disabled\" is not supported for this model. Thinking defaults to adaptive mode when not specified; use \"thinking.type.enabled\" with \"budget_tokens\" for extended thinking.",
                ),
            ),
            // 有主语没状态词：额度/权限问题，不是封号。
            (
                StatusCode::BAD_REQUEST,
                err_body("invalid_request_error", "Your account has insufficient credits"),
            ),
            // 组织没放开 OAuth：组织侧权限配置，管理员开回来就恢复。带 `oauth` 一词，
            // 不豁免会被特征词命中；401/403 两种状态码都见过同款文案。
            (
                StatusCode::FORBIDDEN,
                err_body(
                    "permission_error",
                    "OAuth authentication is currently not allowed for this organization.",
                ),
            ),
            (
                StatusCode::UNAUTHORIZED,
                err_body(
                    "authentication_error",
                    "OAuth authentication is currently not allowed for this organization.",
                ),
            ),
        ];
        for (status, body) in cases {
            assert!(
                detect_account_ban(status, &body).is_none(),
                "不应停用: {status} {}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    /// 「被判成第三方应用」的那条 400 要认出来，普通 400 不能误认。
    ///
    /// 同一条报文还必须**不**被 [`crate::proxy::detect_account_ban`] 判成封号——账号是好的，
    /// 被拒的是请求形态；误停用等于每撞一次这个 400 就白扣一个号。
    #[test]
    fn detects_third_party_rejection_without_banning() {
        let real = err_body(
            "invalid_request_error",
            "Third-party apps now draw from your extra usage, not your plan limits. Add more at claude.ai/settings/usage and keep going",
        );
        assert!(is_third_party_rejection(&real));
        assert!(
            detect_account_ban(StatusCode::BAD_REQUEST, &real).is_none(),
            "第三方判定不是账号级错误，不得停用凭证"
        );

        // 同一族的另一条文案（额度真的用光）。
        let drained =
            err_body("invalid_request_error", "You're out of extra usage. Add more to keep going");
        assert!(is_third_party_rejection(&drained));

        for other in [
            err_body("invalid_request_error", "max_tokens: must be <= 64000"),
            err_body("authentication_error", "invalid bearer token"),
            b"<html>400 Bad Request</html>".to_vec(),
        ] {
            assert!(
                !is_third_party_rejection(&other),
                "不该误认: {}",
                String::from_utf8_lossy(&other)
            );
        }
    }

    /// 本地拒绝回出去的那份体，形态与上游的错误体一致（客户端只读 `error.message`），
    /// 且不编造 `request_id`——这次请求根本没出去。
    #[test]
    fn local_error_body_matches_the_upstream_shape() {
        let raw = crate::proxy::error_body("invalid_request_error", ROLE_400);
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["message"], ROLE_400);
        assert!(v.get("request_id").is_none());
        // 上游那份也能被 parse_upstream_error 原样读回来，两侧口径一致。
        assert_eq!(crate::proxy::parse_upstream_error(&raw).1, ROLE_400);
    }

    /// 响应头取文本：缺失与非 UTF-8 都落回 `-`，与日志里其余缺值字段同形。
    #[test]
    fn header_text_falls_back_to_a_dash() {
        let mut h = crate::proxy::HeaderMap::new();
        h.insert(
            crate::proxy::HeaderName::from_static("request-id"),
            HeaderValue::from_static("req_011CTt5abcd"),
        );
        h.insert(
            crate::proxy::HeaderName::from_static("x-should-retry"),
            HeaderValue::from_static("true"),
        );
        assert_eq!(crate::proxy::header_text(&h, "request-id"), "req_011CTt5abcd");
        assert_eq!(crate::proxy::header_text(&h, "x-should-retry"), "true");
        assert_eq!(crate::proxy::header_text(&h, "retry-after"), "-", "缺失落回占位");
        h.insert(
            crate::proxy::HeaderName::from_static("x-weird"),
            HeaderValue::from_bytes("中文".as_bytes()).unwrap(),
        );
        assert_eq!(crate::proxy::header_text(&h, "x-weird"), "-", "非 UTF-8 头值不猜");
    }
}
