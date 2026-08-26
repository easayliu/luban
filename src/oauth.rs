//! OAuth PKCE 授权流程：生成挑战、构造授权 URL、交换与刷新 token。

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config;
use crate::credentials::now_secs;

/// 一组 OAuth token（交换或刷新得到），交由 [`crate::store`] 落库。
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    /// 过期的 Unix 时间戳（秒）。
    pub expires_at: u64,
    /// 账号邮箱（来自交换响应，用作默认显示名的兜底）。
    pub account: Option<String>,
}

/// 账号 profile：邮箱、姓名、订阅等级、账号 UUID（来自 `/api/oauth/profile`）。
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub email: Option<String>,
    pub name: Option<String>,
    pub tier: Option<String>,
    /// 组织类型原值（`claude_team`/`claude_enterprise`/`claude_max`…），团队号与个人号
    /// 在调度与额度上完全不同（团队额度是整个组织共享的席位额度），故单独留一列供前端标记。
    pub org_type: Option<String>,
    /// 账号唯一标识（`account.uuid`）；用于转发时的身份伪装。
    pub account_uuid: Option<String>,
}

/// 一次登录尝试的 PKCE 上下文，需在交换 token 时回传。
#[derive(Clone)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

impl PkceChallenge {
    /// 生成新的 PKCE 挑战：随机 verifier、S256 challenge、随机 state。
    pub fn generate() -> Self {
        let verifier = random_b64url(32);
        let state = random_b64url(32);

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

        Self { verifier, challenge, state }
    }

    /// 构造用户需要在浏览器打开的授权 URL。
    ///
    /// `scopes` 由调用方从 settings 取（见 [`crate::store::CredentialStore::oauth_scopes`]），
    /// 不在这里读默认值：同一次登录里授权 URL 上的 scope 和用户在同意页上看到的必须是同一份，
    /// 让这个函数自己去兜底只会多一条「配置没生效」的暗路。
    pub fn authorize_url(&self, scopes: &str) -> String {
        let params = [
            ("code", "true"),
            ("client_id", config::CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", config::REDIRECT_URI),
            ("scope", scopes),
            ("code_challenge", &self.challenge),
            ("code_challenge_method", "S256"),
            ("state", &self.state),
        ];
        let query: Vec<String> =
            params.iter().map(|(k, v)| format!("{}={}", k, urlencode(v))).collect();
        format!("{}?{}", config::AUTHORIZE_URL, query.join("&"))
    }
}

/// token 端点的响应结构。授权码交换时通常还带 `account`/`organization`。
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    #[serde(default)]
    account: Option<Account>,
    #[serde(default)]
    organization: Option<Organization>,
}

#[derive(Debug, Deserialize)]
struct Account {
    #[serde(default)]
    email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Organization {
    #[serde(default)]
    name: Option<String>,
}

// ---------- profile ----------

/// `/api/oauth/profile` 响应（只取需要的字段）。
#[derive(Debug, Deserialize)]
struct ProfileResponse {
    #[serde(default)]
    account: Option<ProfileAccount>,
    #[serde(default)]
    organization: Option<ProfileOrg>,
}

#[derive(Debug, Deserialize)]
struct ProfileAccount {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    has_claude_pro: Option<bool>,
    #[serde(default)]
    has_claude_max: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ProfileOrg {
    /// 如 `claude_max` / `claude_pro` / `claude_free`。
    #[serde(default)]
    organization_type: Option<String>,
    /// 如 `default_claude_max_5x` / `default_claude_max_20x`，含倍数档位。
    #[serde(default)]
    rate_limit_tier: Option<String>,
}

/// 用 access_token 获取账号 profile（邮箱、姓名、订阅等级）。
pub async fn fetch_profile(client: &wreq::Client, access_token: &str) -> Result<Profile> {
    let resp = client
        .get(config::PROFILE_URL)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .context("request to the profile endpoint failed")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("profile endpoint returned {}: {}", status, text);
    }

    // 刻意不把 `text` 拼进错误：这是 **2xx** 响应，里面是这个账号的邮箱姓名，而这条 error
    // 会一路走到日志与后台页面。serde 的报错自带字段名与行列，定位足够了。
    let p: ProfileResponse = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse the profile response ({} bytes)", text.len()))?;

    let tier = tier_from(
        p.account.as_ref().and_then(|a| a.has_claude_max),
        p.account.as_ref().and_then(|a| a.has_claude_pro),
        p.organization.as_ref().and_then(|o| o.organization_type.as_deref()),
        p.organization.as_ref().and_then(|o| o.rate_limit_tier.as_deref()),
    );
    let email = p.account.as_ref().and_then(|a| a.email.clone());
    let name = p.account.as_ref().and_then(|a| {
        a.full_name.clone().or_else(|| a.display_name.clone()).filter(|s| !s.trim().is_empty())
    });
    let account_uuid =
        p.account.as_ref().and_then(|a| a.uuid.clone()).filter(|s| !s.trim().is_empty());

    let org_type = p
        .organization
        .as_ref()
        .and_then(|o| o.organization_type.clone())
        .filter(|s| !s.trim().is_empty());
    Ok(Profile { email, name, tier, org_type, account_uuid })
}

/// 由订阅标志推导账号等级：Max > Pro > Free；Max 附带倍数档（如 `Max 5x`）。
fn tier_from(
    has_max: Option<bool>,
    has_pro: Option<bool>,
    org_type: Option<&str>,
    rate_limit_tier: Option<&str>,
) -> Option<String> {
    let mult = multiplier(rate_limit_tier); // 如 "5x" / "20x"
    if has_max == Some(true) {
        return Some(with_mult("Max", mult));
    }
    if has_pro == Some(true) {
        return Some("Pro".into());
    }
    if let Some(t) = org_type.map(str::trim).filter(|s| !s.is_empty()) {
        // 团队/企业号的席位**不体现在 account 的 has_claude_max/pro 上**（实测两个都是
        // false），额度档只能从 `rate_limit_tier` 读——`default_claude_max_5x` 说明这个
        // 组织拿的是 Max 5x 的量。此前这里直接返回 `humanize_tier("claude_team") = "team"`，
        // 既把额度档整个丢了，大小写也和 `Max`/`Pro` 不一致。
        if let Some(from_rate) = tier_from_rate_limit(rate_limit_tier) {
            return Some(from_rate);
        }
        let base = humanize_tier(t);
        // 组织类型是 max 时也带上倍数。
        return Some(if base == "Max" { with_mult("Max", mult) } else { base });
    }
    if has_max == Some(false) && has_pro == Some(false) {
        return Some("Free".into());
    }
    None
}

/// 从 `rate_limit_tier` 读出额度档，如 `default_claude_max_5x` → `Max 5x`。
///
/// 只给**团队/企业号**用：个人号的档位由 `has_claude_max`/`has_claude_pro` 直接给出，
/// 更权威；团队号那两个标志恒为 false，这个字段是唯一的信息来源。
fn tier_from_rate_limit(rate_limit_tier: Option<&str>) -> Option<String> {
    let raw = rate_limit_tier?.to_ascii_lowercase();
    let base = ["max", "pro", "free"].into_iter().find(|k| raw.split('_').any(|seg| seg == *k))?;
    Some(with_mult(&humanize_tier(base), multiplier(rate_limit_tier)))
}

/// 从 `default_claude_max_5x` 提取倍数段 `5x`（形如 `\d+x`）。
fn multiplier(rate_limit_tier: Option<&str>) -> Option<String> {
    rate_limit_tier?.split('_').find_map(|seg| {
        let is_mult = seg.len() >= 2
            && seg.ends_with('x')
            && seg[..seg.len() - 1].chars().all(|c| c.is_ascii_digit());
        is_mult.then(|| seg.to_string())
    })
}

fn with_mult(base: &str, mult: Option<String>) -> String {
    match mult {
        Some(m) => format!("{} {}", base, m),
        None => base.to_string(),
    }
}

/// 把 `claude_max` 之类的原始类型美化成 `Max`。
fn humanize_tier(raw: &str) -> String {
    match raw.trim_start_matches("claude_") {
        "max" => "Max".into(),
        "pro" => "Pro".into(),
        "free" => "Free".into(),
        // 没见过的类型（`team`/`enterprise`…）至少把首字母大写，别在界面上混着
        // `Max`/`Pro` 显示一个小写词。
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => other.to_string(),
            }
        }
    }
}

/// 用授权码换取 token。`pasted` 是用户从回调页粘贴的 `code#state`。
pub async fn exchange_code(
    client: &wreq::Client,
    pkce: &PkceChallenge,
    pasted: &str,
) -> Result<TokenSet> {
    let (code, returned_state) = split_code_state(pasted)?;
    if returned_state != pkce.state {
        bail!("state mismatch, possibly CSRF or a bad paste; please log in again");
    }

    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "state": returned_state,
        "client_id": config::CLIENT_ID,
        "redirect_uri": config::REDIRECT_URI,
        "code_verifier": pkce.verifier,
    });

    post_token(client, body).await
}

/// token 端点返回的非 2xx 响应。作为 typed error 抛出（而不是拍平成字符串），
/// 让调用方能区分「refresh_token 已被上游作废」与「网络/服务端抖动」——前者只能换号，
/// 后者重试即可。见 [`Self::is_grant_revoked`] 与
/// [`crate::store::valid_access_token_for_device`]。
#[derive(Debug)]
pub struct TokenEndpointError {
    pub status: wreq::StatusCode,
    pub body: String,
}

impl std::fmt::Display for TokenEndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "token endpoint returned {}: {}", self.status, self.body)
    }
}

impl std::error::Error for TokenEndpointError {}

impl TokenEndpointError {
    /// 该 refresh_token 是否已被上游**永久**作废（重试没有意义，只能停用换号）。
    ///
    /// 判据取 OAuth 2.0 的 `invalid_grant`——refresh_token 被吊销/过期/已轮换作废时的标准
    /// 错误码。刻意**只**认这一个、且只在 400/401 上认：误判会把健康账号停用掉，
    /// 和 [`crate::proxy::detect_account_ban`] 收紧时是同一个教训。403/429/5xx 以及所有
    /// 网络层错误一律当可重试，不停用。
    ///
    /// 注意：这个端点真实的失败响应形态我们**没有实测样本**，故这里只做保守的字面量匹配，
    /// 而不去猜它的 JSON 结构。刷新失败时无论是否命中都会把响应体原样打进日志
    /// （见 [`crate::store::valid_access_token_for_device`]），线上真出现一次即可据此收紧。
    pub fn is_grant_revoked(&self) -> bool {
        matches!(self.status.as_u16(), 400 | 401)
            && self.body.to_ascii_lowercase().contains("invalid_grant")
    }

    /// 写入 `ban_reason` 的原因（截断至 200 字符，与 `detect_account_ban` 的口径一致）。
    pub fn ban_reason(&self) -> String {
        format!("[refresh {}] {}", self.status.as_u16(), self.body.trim())
            .chars()
            .take(200)
            .collect()
    }
}

/// 用 refresh_token 刷新出新的 access_token。
pub async fn refresh(client: &wreq::Client, refresh_token: &str) -> Result<TokenSet> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": config::CLIENT_ID,
    });

    post_token(client, body).await
}

/// 向 token 端点 POST，并把响应转换为带过期时间戳的 `TokenSet`。
async fn post_token(client: &wreq::Client, body: serde_json::Value) -> Result<TokenSet> {
    let resp = client
        .post(config::TOKEN_URL)
        .json(&body)
        .send()
        .await
        .context("request to the token endpoint failed")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(TokenEndpointError { status, body: text }.into());
    }

    // **绝不**把 `text` 拼进错误：走到这里说明是 2xx，报文里就是 access_token/refresh_token
    // 本身，而这条 error 会被 `valid_access_token_for_device` 打进日志、被 `/api/.../refresh`
    // 原样返回给浏览器——那等于把凭证写进日志文件和 HTTP 响应。非 2xx 的响应体不含凭证，
    // 由 `TokenEndpointError` 原样保留（见其文档）。serde 的报错自带字段名与行列，够定位了。
    let token: TokenResponse = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse the token response ({} bytes)", text.len()))?;

    // 优先用账号邮箱作标识，取不到再用组织名。
    let account = token
        .account
        .and_then(|a| a.email_address)
        .or_else(|| token.organization.and_then(|o| o.name))
        .filter(|s| !s.trim().is_empty());

    Ok(TokenSet {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: now_secs() + token.expires_in,
        account,
    })
}

/// 只取粘贴内容里的 `state` 段，用于在进行中的多次登录里找出这一次对应的 PKCE 挑战
/// （见 [`crate::web::AppState`] 的 `pkce`）。格式不对时给出与 [`exchange_code`] 一致的报错。
pub fn state_of(pasted: &str) -> Result<String> {
    Ok(split_code_state(pasted)?.1)
}

/// 从 `code#state` 拆出授权码与 state；`#` 后的 fragment 是 state。
fn split_code_state(pasted: &str) -> Result<(String, String)> {
    let trimmed = pasted.trim();
    match trimmed.split_once('#') {
        Some((code, state)) if !code.is_empty() && !state.is_empty() => {
            Ok((code.to_string(), state.to_string()))
        }
        _ => bail!("the pasted value must look like `code#state`"),
    }
}

/// 生成 `n` 字节随机数据并做 base64url(no-pad) 编码。
fn random_b64url(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 最小 URL 百分号编码，仅保留 RFC 3986 unreserved 字符。
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------- session keepalive ----------

/// 向上游发一次会话保活请求（`event_logging` + `metrics`）。
///
/// 抓包显示官方客户端在整个会话期间持续上报：`/api/event_logging/v2/batch`（~2-3 分钟）
/// 与 `/api/claude_code/metrics`（~5 分钟）。两者都带 OAuth access_token，luban 不发这些
/// 请求，上游可能因此判定会话已废弃并吊销 refresh_token。
///
/// 两个端点独立发，任一失败不影响另一个。返回 `(event_logging_ok, metrics_ok)`。
pub async fn keepalive(client: &wreq::Client, access_token: &str) -> (bool, bool) {
    let ev = keepalive_event_logging(client, access_token).await;
    let mt = keepalive_metrics(client, access_token).await;
    (ev, mt)
}

/// `POST /api/event_logging/v2/batch` — 空事件批次，只为让上游看到 token 活跃。
async fn keepalive_event_logging(client: &wreq::Client, access_token: &str) -> bool {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_EVENT_LOGGING);
    let body = serde_json::json!({"events": []});
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", config::KEEPALIVE_USER_AGENT)
        .header("x-service-name", "claude-code")
        .header("Accept", "application/json, text/plain, */*")
        .json(&body)
        .send()
        .await;
    match resp {
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

/// `POST /api/claude_code/metrics` — 空指标批次，同样只为保活。
async fn keepalive_metrics(client: &wreq::Client, access_token: &str) -> bool {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_METRICS);
    let body = serde_json::json!({
        "resource_attributes": {
            "service.name": "claude-code",
            "service.version": config::KEEPALIVE_USER_AGENT.strip_prefix("claude-code/").unwrap_or("2.1.246"),
        },
        "metrics": []
    });
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", config::KEEPALIVE_USER_AGENT)
        .header("Accept", "application/json, text/plain, */*")
        .json(&body)
        .send()
        .await;
    match resp {
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{TokenEndpointError, tier_from};
    use wreq::StatusCode;

    /// 团队号的档位只能从 `rate_limit_tier` 读——实测 `cred_id=9`（`claude_team`）的
    /// `account.has_claude_max`/`has_claude_pro` **都是 false**，而
    /// `organization.rate_limit_tier` 是 `default_claude_max_5x`。
    /// 此前这条路返回的是 `"team"`：额度档整个丢了，大小写也和 `Max`/`Pro` 不一致。
    #[test]
    fn team_tier_comes_from_the_rate_limit_field() {
        assert_eq!(
            tier_from(Some(false), Some(false), Some("claude_team"), Some("default_claude_max_5x")),
            Some("Max 5x".into())
        );
        // 企业号同理。
        assert_eq!(
            tier_from(None, None, Some("claude_enterprise"), Some("default_claude_max_20x")),
            Some("Max 20x".into())
        );
        // 读不出档位时退回组织类型，但首字母大写，别在界面上混着小写词。
        assert_eq!(tier_from(None, None, Some("claude_team"), None), Some("Team".into()));
        assert_eq!(tier_from(None, None, Some("claude_team"), Some("weird")), Some("Team".into()));
    }

    /// 个人号的判定顺序不变：`has_claude_max`/`has_claude_pro` 比组织字段更权威。
    #[test]
    fn personal_tier_still_wins_over_the_org_fields() {
        assert_eq!(
            tier_from(Some(true), None, Some("claude_team"), Some("default_claude_max_20x")),
            Some("Max 20x".into())
        );
        assert_eq!(tier_from(None, Some(true), Some("claude_team"), None), Some("Pro".into()));
        assert_eq!(tier_from(Some(false), Some(false), None, None), Some("Free".into()));
        assert_eq!(tier_from(None, None, None, None), None);
    }

    fn err(status: StatusCode, body: &str) -> TokenEndpointError {
        TokenEndpointError { status, body: body.into() }
    }

    /// refresh_token 被吊销/轮换作废：判定为永久失效，触发停用并换号。
    #[test]
    fn detects_revoked_grant() {
        let cases = [
            (StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#),
            (
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid_grant","error_description":"Refresh token not found"}"#,
            ),
            (StatusCode::UNAUTHORIZED, r#"{"error":"invalid_grant"}"#),
            // 大小写不敏感。
            (StatusCode::BAD_REQUEST, r#"{"error":"INVALID_GRANT"}"#),
        ];
        for (status, body) in cases {
            assert!(err(status, body).is_grant_revoked(), "应判定为永久失效: {status} {body}");
        }
    }

    /// 其余一律当可重试——误判会把健康账号停用掉，宁可多 503 一次也不停错号。
    #[test]
    fn does_not_revoke_on_retryable_errors() {
        let cases = [
            // 服务端抖动。
            (StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"server_error"}"#),
            (StatusCode::BAD_GATEWAY, "<html>502</html>"),
            // 限流：等一会儿就好，账号是好的。
            (StatusCode::TOO_MANY_REQUESTS, r#"{"error":"rate_limited"}"#),
            // 非 invalid_grant 的 4xx：多半是我们自己请求构造错了，不该记到账号头上。
            (StatusCode::BAD_REQUEST, r#"{"error":"invalid_request"}"#),
            (StatusCode::BAD_REQUEST, r#"{"error":"invalid_client"}"#),
            (StatusCode::FORBIDDEN, r#"{"error":"access_denied"}"#),
            // 状态码对但内容无关：不认。
            (StatusCode::BAD_REQUEST, "Bad Request"),
            // 内容命中但状态码不对：同样不认，两个条件都要满足。
            (StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"invalid_grant"}"#),
        ];
        for (status, body) in cases {
            assert!(!err(status, body).is_grant_revoked(), "不应停用: {status} {body}");
        }
    }

    /// ban_reason 带上状态码、去掉首尾空白、截断至 200 字符。
    #[test]
    fn ban_reason_is_bounded() {
        let e = err(StatusCode::BAD_REQUEST, "  {\"error\":\"invalid_grant\"}  ");
        assert_eq!(e.ban_reason(), r#"[refresh 400] {"error":"invalid_grant"}"#);

        let long = err(StatusCode::BAD_REQUEST, &"x".repeat(500));
        assert_eq!(long.ban_reason().chars().count(), 200);
    }
}
