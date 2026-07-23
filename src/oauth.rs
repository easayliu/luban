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

        Self {
            verifier,
            challenge,
            state,
        }
    }

    /// 构造用户需要在浏览器打开的授权 URL。
    pub fn authorize_url(&self) -> String {
        let params = [
            ("code", "true"),
            ("client_id", config::CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", config::REDIRECT_URI),
            ("scope", config::SCOPES),
            ("code_challenge", &self.challenge),
            ("code_challenge_method", "S256"),
            ("state", &self.state),
        ];
        let query: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencode(v)))
            .collect();
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
pub async fn fetch_profile(client: &reqwest::Client, access_token: &str) -> Result<Profile> {
    let resp = client
        .get(config::PROFILE_URL)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .context("请求 profile 端点失败")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("profile 端点返回 {}: {}", status, text);
    }

    let p: ProfileResponse =
        serde_json::from_str(&text).with_context(|| format!("解析 profile 响应失败: {}", text))?;

    let tier = tier_from(
        p.account.as_ref().and_then(|a| a.has_claude_max),
        p.account.as_ref().and_then(|a| a.has_claude_pro),
        p.organization.as_ref().and_then(|o| o.organization_type.as_deref()),
        p.organization.as_ref().and_then(|o| o.rate_limit_tier.as_deref()),
    );
    let email = p.account.as_ref().and_then(|a| a.email.clone());
    let name = p.account.as_ref().and_then(|a| {
        a.full_name
            .clone()
            .or_else(|| a.display_name.clone())
            .filter(|s| !s.trim().is_empty())
    });
    let account_uuid = p
        .account
        .as_ref()
        .and_then(|a| a.uuid.clone())
        .filter(|s| !s.trim().is_empty());

    Ok(Profile { email, name, tier, account_uuid })
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
        let base = humanize_tier(t);
        // 组织类型是 max 时也带上倍数。
        return Some(if base == "Max" { with_mult("Max", mult) } else { base });
    }
    if has_max == Some(false) && has_pro == Some(false) {
        return Some("Free".into());
    }
    None
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
        other => other.to_string(),
    }
}

/// 用授权码换取 token。`pasted` 是用户从回调页粘贴的 `code#state`。
pub async fn exchange_code(
    client: &reqwest::Client,
    pkce: &PkceChallenge,
    pasted: &str,
) -> Result<TokenSet> {
    let (code, returned_state) = split_code_state(pasted)?;
    if returned_state != pkce.state {
        bail!("state 不匹配，可能存在 CSRF 或粘贴错误；请重新登录");
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

/// 用 refresh_token 刷新出新的 access_token。
pub async fn refresh(client: &reqwest::Client, refresh_token: &str) -> Result<TokenSet> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": config::CLIENT_ID,
    });

    post_token(client, body).await
}

/// 向 token 端点 POST，并把响应转换为带过期时间戳的 `TokenSet`。
async fn post_token(client: &reqwest::Client, body: serde_json::Value) -> Result<TokenSet> {
    let resp = client
        .post(config::TOKEN_URL)
        .json(&body)
        .send()
        .await
        .context("请求 token 端点失败")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token 端点返回 {}: {}", status, text);
    }

    let token: TokenResponse =
        serde_json::from_str(&text).with_context(|| format!("解析 token 响应失败: {}", text))?;

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

/// 从 `code#state` 拆出授权码与 state；`#` 后的 fragment 是 state。
fn split_code_state(pasted: &str) -> Result<(String, String)> {
    let trimmed = pasted.trim();
    match trimmed.split_once('#') {
        Some((code, state)) if !code.is_empty() && !state.is_empty() => {
            Ok((code.to_string(), state.to_string()))
        }
        _ => bail!("粘贴内容格式应为 `code#state`"),
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
