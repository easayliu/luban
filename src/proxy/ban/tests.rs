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
        (StatusCode::BAD_REQUEST, err_body("invalid_request_error", "Your account was suspended")),
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
        ban_verdict(StatusCode::FORBIDDEN, Some("permission_error"), "No access to claude-opus-5"),
        BanVerdict::Clear
    );
    assert_eq!(
        ban_verdict(StatusCode::TOO_MANY_REQUESTS, Some("rate_limit_error"), "account suspended"),
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
        // 订阅未生效（付费档需续费 / Free 档需订阅）：续费 / 订阅后就恢复。带 `oauth` 一词，
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

/// 「订阅未生效」（上游原话是组织不允许 OAuth）要单独认出来（去暂停调度），401/403 都认；别的状态码、别的
/// 权限报错不认；与「组织被停用」同句时封号判定优先。
#[test]
fn detects_org_oauth_disallowed() {
    use crate::proxy::{AccountRejection, classify_account_rejection};
    let sub = |status, body: &[u8]| {
        classify_account_rejection(status, body) == AccountRejection::SubscriptionInactive
    };
    let msg = "OAuth authentication is currently not allowed for this organization.";
    assert!(sub(StatusCode::FORBIDDEN, &err_body("permission_error", msg)));
    assert!(sub(StatusCode::UNAUTHORIZED, &err_body("authentication_error", msg)));
    // 非 JSON 体（原文就是那句话）也认。
    assert!(sub(StatusCode::FORBIDDEN, msg.as_bytes()));
    // 400 上不认（没人接手暂停），也不封号。
    assert_eq!(
        classify_account_rejection(
            StatusCode::BAD_REQUEST,
            &err_body("invalid_request_error", msg)
        ),
        AccountRejection::Other
    );
    for other in [
        err_body("permission_error", "Your account does not have access to claude-opus-5"),
        err_body(
            "authentication_error",
            "OAuth authentication is currently not supported for this endpoint",
        ),
        b"<html>403 Forbidden</html>".to_vec(),
    ] {
        assert_eq!(
            classify_account_rejection(StatusCode::FORBIDDEN, &other),
            AccountRejection::Other,
            "不该误认: {}",
            String::from_utf8_lossy(&other)
        );
    }
    // 同句带「组织被停用」：封号优先，顺序收在 classify_account_rejection 里。
    let mixed = err_body(
        "permission_error",
        "This organization has been disabled; OAuth authentication is currently not allowed for this organization.",
    );
    assert!(matches!(
        classify_account_rejection(StatusCode::FORBIDDEN, &mixed),
        AccountRejection::Ban(_)
    ));
}

/// 暂停写进库：号出池、不带恢复时刻、原因可读且带固定片段，不算封号；连通性测试
/// 那条恢复认得出它。
#[sqlx::test]
async fn park_org_oauth_disallowed_pauses_until_resumed(pool: sqlx::PgPool) {
    use crate::proxy::park_org_oauth_disallowed;
    let store = std::sync::Arc::new(crate::store::CredentialStore::for_test(pool.clone()).await);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap();
    assert!(park_org_oauth_disallowed(&store, &a, 403, "forward").await);
    let got = store.get(a.id).await.unwrap().unwrap();
    assert!(got.disabled && got.resume_at.is_none());
    assert_eq!(
        got.ban_reason.as_deref(),
        Some(
            "[subscription-inactive 403] organization does not allow OAuth authentication (subscription lapsed, or a Free plan without one); paused until enabled manually or a connectivity test passes"
        )
    );
    assert!(got.is_subscription_paused() && !got.is_banned());
    assert!(!park_org_oauth_disallowed(&store, &a, 403, "keepalive").await, "已暂停的不重写");
    assert!(store.resume_if_subscription_suspended(a.id).await.unwrap());
}

/// 「被判成第三方应用」的那条 400 要认出来，普通 400 不能误认。
///
/// 同一条报文还必须**不**被 [`detect_account_ban`] 判成封号——账号是好的，
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
        assert!(!is_third_party_rejection(&other), "不该误认: {}", String::from_utf8_lossy(&other));
    }
}

/// 本地拒绝回出去的那份体，形态与上游的错误体一致（客户端只读 `error.message`），
/// 且不编造 `request_id`——这次请求根本没出去，与官方没有 id 时一样回 `null`。
#[test]
fn local_error_body_matches_the_upstream_shape() {
    let raw = crate::proxy::error_body("invalid_request_error", ROLE_400);
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["message"], ROLE_400);
    assert_eq!(v.get("request_id"), Some(&serde_json::Value::Null));
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
