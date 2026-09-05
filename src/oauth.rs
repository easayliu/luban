//! OAuth PKCE 授权流程：生成挑战、构造授权 URL、交换与刷新 token。

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::config;
use crate::credentials::now_secs;

/// 保活端点的返回状态。调用点根据 `AuthRejected` 标 banned 并跳过后续端点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepaliveResult {
    Ok,
    /// 上游 401/403——token 已吊销或账号被暂停。
    AuthRejected,
    /// 网络错误或 5xx。
    Failed,
}

impl KeepaliveResult {
    fn from_status(status: u16) -> Self {
        if status == 401 || status == 403 {
            Self::AuthRejected
        } else if status >= 500 {
            Self::Failed
        } else {
            Self::Ok
        }
    }

    pub fn is_ok(self) -> bool {
        self == Self::Ok
    }
}

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
    /// 额度档**原值**（`default_claude_max_5x` 之类），来自 `organization.rate_limit_tier`。
    ///
    /// 与 [`Self::tier`] 是两回事：那个是给界面看的（`Max 5x`），这个是 statsig eval 的
    /// `attributes.rateLimitTier` 要发的原串（`cap/2.1.260-2/00003`）。拿展示串顶替，
    /// 发出去的就是一个上游从没见过的取值。
    pub rate_limit_tier: Option<String>,
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
    let rate_limit_tier = p
        .organization
        .as_ref()
        .and_then(|o| o.rate_limit_tier.clone())
        .filter(|s| !s.trim().is_empty());
    Ok(Profile { email, name, tier, org_type, rate_limit_tier, account_uuid })
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

/// 每次保活循环构建一份，携带该凭证在当前进程里的"会话"身份。
///
/// 所有 id 由 `account_uuid` 确定性派生——同一凭证在同一进程里恒定；进程重启后变。
/// 没有 `account_uuid` 时用凭证 id 兜底（id 是自增整数，不会碰撞但也没有任何含义）。
pub struct KeepaliveCtx {
    /// 这份上下文属于哪张凭证。只用来给 [`AXIOS_ETAGS`] 分桶——条件请求的缓存键得跟着
    /// 账号走，不同账号的 policy_limits 本来就不是同一份。
    pub cred_id: i64,
    /// 模拟的 session UUID。
    pub session_id: String,
    /// 模拟的 device_id（sha256 hex，64 字符）。
    pub device_id: String,
    /// 模拟的 prompt UUID。
    prompt_id: String,
    /// account_uuid（直传）。
    account_uuid: String,
    /// subscription_type（team / individual）。
    subscription_type: String,
    /// 该凭证最近一次 `/v1/messages` 响应头里的 `anthropic-organization-id`；一次都没见过
    /// 时缺省（见 [`crate::telemetry::Telemetry::org_uuid`]）。
    organization_uuid: Option<String>,
    /// 进程已运行秒数。
    uptime_secs: f64,
    /// 事件顶层 `model`、会话级 `betas`、客户端版本。挂到真实会话上时取该会话的，
    /// 否则用写死的保活默认值（sonnet-5 / [`config::KEEPALIVE_EVENT_BETAS`] / 保活 UA 版本）。
    model: String,
    betas: String,
    version: String,
    /// 额度档原值，statsig eval 的 `attributes.rateLimitTier`。
    /// 见 [`crate::credentials::Credential::rate_limit_tier`]。
    rate_limit_tier: Option<String>,
    /// `attributes.firstTokenTime`（毫秒）：这张凭证**第一次拿到 token** 的时刻。
    ///
    /// 真实客户端记的是这台机器第一次登录成功的时间，luban 这边最接近的就是凭证入库那一刻
    /// （`created_at`）——同一个语义，不是编出来的常量。
    first_token_ms: Option<i64>,
}

/// `attributes.firstTokenTime`（毫秒）：这张凭证第一次拿到 token 的时刻。
///
/// 取凭证入库那一刻（`created_at`，秒）×1000。真实客户端记的是本机第一次登录成功的时间，
/// 语义是同一个；`created_at` 为 0（旧库里没有这一列时的默认值）就返回 `None`——发一个
/// 1970 年的时间戳比不发更显眼。
fn first_token_ms(cred: &crate::credentials::Credential) -> Option<i64> {
    (cred.created_at > 0).then(|| cred.created_at as i64 * 1000)
}

/// 进程级种子，每次启动随机一次；用于从 account_uuid 派生出每次启动不同的 session_id。
static PROCESS_SEED: std::sync::LazyLock<[u8; 16]> = std::sync::LazyLock::new(|| {
    let mut buf = [0u8; 16];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut buf);
    buf
});

impl KeepaliveCtx {
    /// `session` 有值时，空闲事件挂到这个真实会话上：session_id / device_id / prompt_id /
    /// 版本 / 模型 / beta 全取它的，`uptime` 从该会话起点算——真实 CC 进程开着没人说话时，
    /// 版本检查事件就是从同一个会话发出的。没有近期会话才退回按账号派生的那套身份。
    pub fn new(
        cred: &crate::credentials::Credential,
        uptime_secs: f64,
        organization_uuid: Option<String>,
        session: Option<crate::telemetry::SessionSnapshot>,
    ) -> Self {
        let subscription_type =
            crate::telemetry::subscription_type(cred.org_type.as_deref()).to_string();
        if let Some(s) = session {
            let uptime_secs = std::time::SystemTime::now()
                .duration_since(s.started_wall)
                .map(|d| d.as_secs_f64())
                .unwrap_or(uptime_secs);
            return Self {
                cred_id: cred.id,
                session_id: s.session_id,
                device_id: s.device_id,
                prompt_id: s.prompt_id,
                account_uuid: s.account_uuid,
                subscription_type,
                organization_uuid,
                uptime_secs,
                model: s.model,
                betas: s.betas,
                version: s.version,
                rate_limit_tier: cred.rate_limit_tier.clone(),
                first_token_ms: first_token_ms(cred),
            };
        }
        let id_str = cred.id.to_string();
        let basis = cred.account_uuid.as_deref().unwrap_or(&id_str);
        let seed: &[u8] = &*PROCESS_SEED;
        let session_id = derive_uuid(basis, seed, b"session");
        let prompt_id = derive_uuid(basis, seed, b"prompt");
        let device_id = derive_hex64(basis, seed, b"device");

        Self {
            cred_id: cred.id,
            session_id,
            device_id,
            prompt_id,
            account_uuid: basis.to_string(),
            subscription_type,
            organization_uuid,
            uptime_secs,
            model: "claude-sonnet-5".to_string(),
            betas: config::KEEPALIVE_EVENT_BETAS.to_string(),
            version: keepalive_version().to_string(),
            rate_limit_tier: cred.rate_limit_tier.clone(),
            first_token_ms: first_token_ms(cred),
        }
    }

    /// 事件/日志的公共字段都由 [`crate::telemetry::Identity`] 生成，与逐请求遥测同一套
    /// （`env` 块、`auth` 块、base64 编码方式、`build_time` 查表）。
    fn identity(&self) -> crate::telemetry::Identity {
        crate::telemetry::Identity {
            session_id: self.session_id.clone(),
            device_id: self.device_id.clone(),
            account_uuid: self.account_uuid.clone(),
            organization_uuid: self.organization_uuid.clone(),
            subscription_type: self.subscription_type.clone(),
            version: self.version.clone(),
        }
    }

    /// axios 那几类端点的 UA：`claude-code/<版本>`，版本跟会话（挂到真实会话时是它的）。
    pub fn ua(&self) -> String {
        format!("claude-code/{}", self.version)
    }

    /// `claude-cli/<版本> (external, cli)`：mcp-registry 与 code/triggers 用的是 SDK 那份 UA。
    pub fn cli_ua(&self) -> String {
        format!("claude-cli/{} (external, cli)", self.version)
    }

    /// 规范模型名（去掉展示名里的 `[1m]`），bootstrap 的 `model=` 用。
    pub fn model_normalized(&self) -> String {
        self.model.trim_end_matches("[1m]").to_string()
    }

    /// 保活事件共用的那几项。
    fn event_ctx(&self) -> crate::telemetry::EventCtx<'_> {
        crate::telemetry::EventCtx {
            model: &self.model,
            betas: &self.betas,
            prompt_id: &self.prompt_id,
            uptime_secs: self.uptime_secs,
        }
    }

    /// 生成单条 `ClaudeCodeInternalEvent`，时间戳往回偏移 `ago_ms` 毫秒。
    fn event(
        &self,
        name: &str,
        now: &chrono::DateTime<chrono::Utc>,
        ago_ms: i64,
        extra_meta: serde_json::Value,
    ) -> serde_json::Value {
        let ts = *now - chrono::Duration::milliseconds(ago_ms);
        self.identity().event(name, ts, &self.event_ctx(), extra_meta)
    }

    /// Datadog 日志条目（flat 形态，取自 `cap/2.1.145/00066`）。
    fn dd_entry(&self, message: &str, extra: serde_json::Value) -> serde_json::Value {
        // Datadog 那份 `model` 是规范名：把展示名的 `[1m]` 去掉。
        let model = self.model.trim_end_matches("[1m]");
        self.identity().dd_entry(message, &self.event_ctx(), model, extra)
    }

    /// 模拟 idle 周期的 Datadog 日志条目（2 条，与 `cap/2.1.145/00066` 对齐）。
    fn dd_idle_entries(&self) -> Vec<serde_json::Value> {
        vec![
            self.dd_entry(
                "tengu_feature_ok",
                serde_json::json!({"feature_name": "job_sweep_drafts"}),
            ),
            self.dd_entry("tengu_feature_ok", serde_json::json!({"feature_name": "update_check"})),
        ]
    }

    /// eval 端点的请求体（Statsig 特性标志评估）。
    fn eval_body(&self) -> serde_json::Value {
        // 官方在 `platform` 与 `accountUUID` 之间带 `organizationUUID`（`cap/2.1.258/00003`）；
        // 还没从响应头里学到组织 id 时只能缺省。
        let mut attrs = serde_json::Map::new();
        attrs.insert("id".into(), self.device_id.clone().into());
        attrs.insert("sessionId".into(), self.session_id.clone().into());
        attrs.insert("deviceID".into(), self.device_id.clone().into());
        attrs.insert("platform".into(), "darwin".into());
        if let Some(org) = &self.organization_uuid {
            attrs.insert("organizationUUID".into(), org.clone().into());
        }
        attrs.insert("accountUUID".into(), self.account_uuid.clone().into());
        attrs.insert("userType".into(), "external".into());
        attrs.insert("subscriptionType".into(), self.subscription_type.clone().into());
        // 额度档**原值**（`default_claude_max_5x`），不是界面上那个 `Max 5x`。旧库里的号
        // 还没回填过就没有这一项——比发一个上游从没见过的取值强。
        if let Some(raw) = &self.rate_limit_tier {
            attrs.insert("rateLimitTier".into(), raw.clone().into());
        }
        attrs.insert("organizationRole".into(), "user".into());
        // `subscriptionCreatedAt`（订阅创建时刻，毫秒）**拿不到**：它不在 profile、bootstrap
        // 或 policy_limits 的响应里（`cap/2.1.260-2` 全部响应体搜过），真实客户端是从本地
        // `~/.claude.json` 读的。按 §8.2 的口径记为未对齐——不填伪造常量。
        if let Some(ms) = self.first_token_ms {
            attrs.insert("firstTokenTime".into(), ms.into());
        }
        attrs.insert("appVersion".into(), self.version.clone().into());
        attrs.insert("entrypoint".into(), "cli".into());
        serde_json::json!({
            "attributes": attrs,
            "forcedVariations": {},
            "forcedFeatures": [],
            "url": ""
        })
    }

    /// 模拟 idle 版本检查周期产生的 7 个事件（与 `cap/2.1.145/00086` 对齐）。
    fn idle_events(&self) -> Vec<serde_json::Value> {
        let now = chrono::Utc::now();
        vec![
            self.event(
                "tengu_native_auto_updater_start",
                &now,
                2200,
                serde_json::json!({}),
            ),
            self.event(
                "tengu_version_check_success",
                &now,
                800,
                serde_json::json!({"latency_ms": 1430, "attempt": 1}),
            ),
            self.event(
                "tengu_feature_ok",
                &now,
                790,
                serde_json::json!({"feature_name": "update_check"}),
            ),
            self.event(
                "tengu_native_update_complete",
                &now,
                780,
                serde_json::json!({"latency_ms": 1433, "was_new_install": false, "was_force_reinstall": false}),
            ),
            self.event(
                "tengu_native_auto_updater_up_to_date",
                &now,
                770,
                serde_json::json!({"latency_ms": 1434}),
            ),
            self.event(
                "tengu_native_version_cleanup",
                &now,
                760,
                serde_json::json!({"total_count": 4, "deleted_count": 0, "protected_count": 2, "retained_count": 2}),
            ),
            self.event(
                "tengu_feature_ok",
                &now,
                750,
                serde_json::json!({"feature_name": "native_cleanup_versions"}),
            ),
        ]
    }
}

fn keepalive_version() -> &'static str {
    config::KEEPALIVE_USER_AGENT.strip_prefix("claude-code/").unwrap_or("2.1.246")
}

/// 从 basis+seed+tag 派生 UUID v4 格式的字符串。
fn derive_uuid(basis: &str, seed: &[u8], tag: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(basis.as_bytes());
    h.update(seed);
    h.update(tag);
    let d = h.finalize();
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        u32::from_be_bytes([d[0], d[1], d[2], d[3]]),
        u16::from_be_bytes([d[4], d[5]]),
        u16::from_be_bytes([d[6], d[7]]) & 0x0FFF,
        (u16::from_be_bytes([d[8], d[9]]) & 0x3FFF) | 0x8000,
        u64::from_be_bytes([0, 0, d[10], d[11], d[12], d[13], d[14], d[15]]),
    )
}

/// 从 basis+seed+tag 派生 64 字符的小写 hex（与真实 device_id 格式一致）。
fn derive_hex64(basis: &str, seed: &[u8], tag: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(basis.as_bytes());
    h.update(seed);
    h.update(tag);
    crate::credentials::hex_lower(&h.finalize())
}

// ---------- Axios 形态 ----------

/// 给一条辅助请求套上 axios 的传输形态：`Accept-Encoding` / `Connection` 两个头 + 该端点的
/// 线上头序（[`config::axios_shape`]）。
///
/// **必须每条都套**：这些端点原先复用的是 Messages API 客户端的 `default_headers`，于是
/// 发出去的 `Accept-Encoding` 是 Bun 那份 `gzip, deflate, br, zstd`，`Connection` 缺省成
/// `keep-alive`，头名还全是小写、`Host`/`Content-Length` 钉在队尾——十来个端点整整齐齐地
/// 与官方对不上，而它们恰好是**每个会话都会发一遍**的那批。
///
/// 头**值**由各调用点自己 `.header(...)`，本函数只管形态；漏加一个头不会凭空发出，
/// 多加的排在队尾（见 [`crate::proxy::orig_header_case`] 记的同一套 `OrigHeaderMap` 语义）。
pub(crate) fn axios(req: wreq::RequestBuilder, shape: &str) -> wreq::RequestBuilder {
    req.header("Accept-Encoding", config::AXIOS_ACCEPT_ENCODING)
        .header("Connection", config::AXIOS_CONNECTION)
        .orig_headers(orig_headers(config::axios_shape(shape)))
}

/// 按给定的头名表构造 `OrigHeaderMap`（决定线上的拼写与顺序）。
pub(crate) fn orig_headers(order: &[&'static str]) -> wreq::header::OrigHeaderMap {
    let mut orig = wreq::header::OrigHeaderMap::new();
    for name in order {
        orig.insert(*name);
    }
    orig
}

/// `policy_limits` / `settings` 的条件请求缓存：`(凭证 id, 端点) → 上次响应体的 sha256`。
///
/// 官方发的是 `If-None-Match: "sha256:<hex>"`，而那个 hex 就是**客户端自己算的响应体
/// 摘要**——`cap/2.1.260-2/00002` 那条 settings 的值是
/// `44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a`，正是 `{}` 的
/// sha256。所以这不是服务端给的 ETag，是客户端的本地缓存键。
///
/// **只在进程内存里**：真实客户端把它写在 `~/.claude.json` 里，跨进程留存；luban 重启后
/// 第一发不带这个头——那与「一台刚装好的机器第一次跑 CC」是同一个形态，能接受。落库
/// 反而要为一个纯缓存字段加一列。
static AXIOS_ETAGS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<(i64, &'static str), String>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// 每张凭证最近一次跑完启动握手的时刻。保活循环据此跳过自己那份重复的启动串，
/// 见 [`handshake_recent`]。
static LAST_HANDSHAKE: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<i64, std::time::Instant>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// 记一次启动握手。
fn note_handshake(cred_id: i64) {
    LAST_HANDSHAKE.lock().insert(cred_id, std::time::Instant::now());
}

/// 这张凭证在 `within` 之内跑过启动握手吗。
///
/// 保活的首 tick 会发 `bootstrap` + `penguin_mode`、每小时发 `policy_limits` + `settings`，
/// 而新会话的启动握手把这四条**全都**发了一遍。两边触发条件不同（一个按时间、一个按新
/// 会话），撞在一起时上游看到的就是同一个账号几秒内把同一批端点打了两遍——真实客户端
/// 一次进程启动只打一遍。故保活那边先问一句这个。
pub fn handshake_recent(cred_id: i64, within: std::time::Duration) -> bool {
    LAST_HANDSHAKE
        .lock()
        .get(&cred_id)
        .is_some_and(|t| std::time::Instant::now().duration_since(*t) < within)
}

/// 取该凭证该端点上次缓存的 `If-None-Match` 值（`"sha256:…"`，含引号）。
fn etag_of(cred_id: i64, endpoint: &'static str) -> Option<String> {
    AXIOS_ETAGS.lock().get(&(cred_id, endpoint)).map(|h| format!("\"sha256:{h}\""))
}

/// 收到 200 时记下响应体的 sha256；304 时保留原值（上游说的就是「没变」）。
fn remember_etag(cred_id: i64, endpoint: &'static str, status: u16, body: &[u8]) {
    if status != 200 {
        return;
    }
    use sha2::{Digest, Sha256};
    let hex = crate::credentials::hex_lower(&Sha256::digest(body));
    AXIOS_ETAGS.lock().insert((cred_id, endpoint), hex);
}

/// 每 tick（30min）发一次 `event_logging`。
///
/// 真实客户端每次都带上版本检查等活动产生的事件；空批次是指纹。
/// 这里模拟 idle 周期产生的 7 个事件（`tengu_*`），结构取自 `cap/2.1.145/00086`。
pub async fn keepalive_event_logging(
    client: &wreq::Client,
    access_token: &str,
    ctx: &KeepaliveCtx,
) -> KeepaliveResult {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_EVENT_LOGGING);
    let body = serde_json::json!({ "events": ctx.idle_events() });
    let resp = axios(
        client
            .post(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("anthropic-beta", config::OAUTH_BETA_HEADER)
            .header("User-Agent", ctx.ua())
            .header("x-service-name", "claude-code")
            .header("Accept", config::AXIOS_ACCEPT)
            .json(&body),
        "event_logging",
    )
    .send()
    .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// 每小时发一次 `GET /api/claude_code/policy_limits`；会话启动时也发一次。
///
/// 带 `If-None-Match`（见 [`etag_of`]）：官方每次都带，缺了它就是「一个从不缓存的客户端
/// 每小时把同一份策略重拉一遍」。
pub async fn keepalive_policy_limits(
    client: &wreq::Client,
    access_token: &str,
    ctx: &KeepaliveCtx,
) -> KeepaliveResult {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_POLICY_LIMITS);
    let mut req = axios(
        client
            .get(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("anthropic-beta", config::OAUTH_BETA_HEADER)
            .header("User-Agent", ctx.ua())
            .header("Accept", config::AXIOS_ACCEPT),
        "policy_limits",
    );
    if let Some(etag) = etag_of(ctx.cred_id, "policy_limits") {
        req = req.header("If-None-Match", etag);
    }
    conditional_get(req, ctx.cred_id, "policy_limits").await
}

/// 每小时发一次 `GET /api/claude_code/settings`；会话启动时也发一次。
pub async fn keepalive_settings(
    client: &wreq::Client,
    access_token: &str,
    ctx: &KeepaliveCtx,
) -> KeepaliveResult {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_SETTINGS);
    let mut req = axios(
        client
            .get(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("anthropic-beta", config::OAUTH_BETA_HEADER)
            .header("User-Agent", ctx.ua())
            .header("Cache-Control", "no-cache")
            .header("Pragma", "no-cache")
            .header("Accept", config::AXIOS_ACCEPT),
        "settings",
    );
    if let Some(etag) = etag_of(ctx.cred_id, "settings") {
        req = req.header("If-None-Match", etag);
    }
    conditional_get(req, ctx.cred_id, "settings").await
}

/// 发一条条件 GET，并把 200 的响应体摘要记进 [`AXIOS_ETAGS`] 供下次带 `If-None-Match`。
///
/// 304 视为成功：那正是缓存命中时官方拿到的状态。
async fn conditional_get(
    req: wreq::RequestBuilder,
    cred_id: i64,
    endpoint: &'static str,
) -> KeepaliveResult {
    let Ok(r) = req.send().await else { return KeepaliveResult::Failed };
    let status = r.status().as_u16();
    // 读 body 只为算摘要；这两个端点的响应都是几百字节量级。
    let body = r.bytes().await.unwrap_or_default();
    remember_etag(cred_id, endpoint, status, &body);
    if status == 304 { KeepaliveResult::Ok } else { KeepaliveResult::from_status(status) }
}

// ---------- startup bootstrap + 周期端点 ----------

/// 启动握手：`GET /api/claude_cli/bootstrap?entrypoint=cli&model=<规范名>`。
///
/// 头形态取自 `cap/2.1.260-1/00029`（UA = `claude-code/<版本>`）。
pub async fn keepalive_bootstrap(
    client: &wreq::Client,
    access_token: &str,
    ctx: &KeepaliveCtx,
    model: &str,
) -> KeepaliveResult {
    let url = format!(
        "{}{}?entrypoint=cli&model={model}",
        config::UPSTREAM_BASE_URL,
        config::KEEPALIVE_BOOTSTRAP
    );
    let resp = axios(
        client
            .get(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("anthropic-beta", config::OAUTH_BETA_HEADER)
            .header("User-Agent", ctx.ua())
            .header("Content-Type", "application/json")
            .header("Accept", config::AXIOS_ACCEPT),
        "bootstrap",
    )
    .send()
    .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// 启动握手：`GET /api/claude_code_penguin_mode`。
///
/// 取自 `cap/2.1.260-2/00005`（UA = `axios/1.15.2`，不带 Content-Type）。
pub async fn keepalive_penguin_mode(client: &wreq::Client, access_token: &str) -> KeepaliveResult {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_PENGUIN_MODE);
    let resp = axios(
        client
            .get(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("anthropic-beta", config::OAUTH_BETA_HEADER)
            .header("User-Agent", config::DATADOG_USER_AGENT)
            .header("Accept", config::AXIOS_ACCEPT),
        "penguin_mode",
    )
    .send()
    .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// Statsig 特性标志评估：启动 + 每 6h。
///
/// 取自 `cap/2.1.145/00039`（UA = `Bun/1.4.1`，Accept = `*/*`）。
pub async fn keepalive_eval(
    client: &wreq::Client,
    access_token: &str,
    ctx: &KeepaliveCtx,
) -> KeepaliveResult {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_EVAL);
    // **不套 axios 形态**：eval 走的是 Bun 自带的 fetch，`Connection: keep-alive`、
    // `Accept: */*`、`Accept-Encoding` 跟 Messages API 那份一样，见 [`config::AXIOS_SHAPE_EVAL`]。
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("Connection", "keep-alive")
        .header("User-Agent", config::KEEPALIVE_UA_BUN)
        .header("Accept", "*/*")
        .header("Accept-Encoding", config::CC_ACCEPT_ENCODING)
        .orig_headers(orig_headers(config::AXIOS_SHAPE_EVAL.order))
        .json(&ctx.eval_body())
        .send()
        .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

// ---------- 会话启动握手（每个新会话一次） ----------

/// `GET /mcp-registry/v0/servers?…`：无鉴权，UA 是 SDK 那份；官方按 `metadata.next_cursor`
/// 翻页（抓包里翻了 4 页）。返回最后一页的状态。
async fn handshake_mcp_registry(client: &wreq::Client, ctx: &KeepaliveCtx) -> KeepaliveResult {
    let base = format!(
        "{}/mcp-registry/v0/servers?version=latest&limit=100&visibility=commercial%2Cgsuite%2Centerprise%2Chealth",
        config::UPSTREAM_BASE_URL
    );
    let mut cursor: Option<String> = None;
    let mut last = KeepaliveResult::Failed;
    for _ in 0..4 {
        let url = match &cursor {
            Some(c) => format!("{base}&cursor={}", urlencode(c)),
            None => base.clone(),
        };
        let resp = axios(
            client
                .get(&url)
                .header("User-Agent", ctx.cli_ua())
                .header("Accept", config::AXIOS_ACCEPT),
            "mcp_registry",
        )
        .send()
        .await;
        let Ok(r) = resp else { return KeepaliveResult::Failed };
        last = KeepaliveResult::from_status(r.status().as_u16());
        let next = r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| {
                v.get("metadata")
                    .and_then(|m| m.get("next_cursor"))
                    .or_else(|| v.get("next_cursor"))
                    .and_then(|c| c.as_str())
                    .map(str::to_string)
            })
            .filter(|c| !c.is_empty());
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    last
}

/// `GET /v1/mcp_servers?limit=1000`（claude.ai 侧配置的 MCP 连接器）；`cap/2.1.260-1/00024`。
async fn handshake_mcp_servers(client: &wreq::Client, access_token: &str) -> KeepaliveResult {
    let url = format!("{}/v1/mcp_servers?limit=1000", config::UPSTREAM_BASE_URL);
    let resp = axios(
        client
            .get(&url)
            .header("Accept", config::AXIOS_ACCEPT)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("anthropic-beta", "mcp-servers-2025-12-04")
            .header("anthropic-version", "2023-06-01")
            .header(
                "anthropic-mcp-client-capabilities",
                "eyJyb290cyI6eyJsaXN0Q2hhbmdlZCI6dHJ1ZX0sImVsaWNpdGF0aW9uIjp7fX0=",
            )
            .header("MCP-Protocol-Version", "2025-11-25")
            .header("User-Agent", config::DATADOG_USER_AGENT),
        "mcp_servers",
    )
    .send()
    .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// `GET /v1/code/triggers`（Claude Code Remote 的触发器列表），带 `x-organization-uuid`；
/// 没拿到组织 id 就不发——缺了那个头的形态官方不产生。`cap/2.1.260-1/00027`。
async fn handshake_code_triggers(
    client: &wreq::Client,
    access_token: &str,
    ctx: &KeepaliveCtx,
) -> KeepaliveResult {
    let Some(org) = ctx.organization_uuid.as_deref() else { return KeepaliveResult::Ok };
    let url = format!("{}/v1/code/triggers", config::UPSTREAM_BASE_URL);
    let resp = axios(
        client
            .get(&url)
            .header("Accept", config::AXIOS_ACCEPT)
            .header("Content-Type", "application/json")
            .header("User-Agent", ctx.cli_ua())
            .header("Authorization", format!("Bearer {}", access_token))
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-client-platform", "claude_code_cli")
            .header("x-organization-uuid", org)
            .header("anthropic-beta", "ccr-triggers-2026-01-30"),
        "code_triggers",
    )
    .send()
    .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// `downloads.claude.ai` 上的版本与插件市场元数据（无鉴权，axios UA）。
async fn handshake_download(client: &wreq::Client, path: &str) -> KeepaliveResult {
    let resp = axios(
        client
            .get(format!("https://downloads.claude.ai{path}"))
            .header("Accept", config::AXIOS_ACCEPT)
            .header("User-Agent", config::DATADOG_USER_AGENT),
        "download",
    )
    .send()
    .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// 一个新会话的启动握手，**分两段**跑。
///
/// 抓包里的时序（`cap/2.1.260-2`，同一个会话；末列是相对第一条的偏移）：
///
/// ```text
/// 17:10:17.207  policy_limits ┐ 同时发                     +0ms   ┐
/// 17:10:17.208  settings      ┘                            +1ms   │ 领跑段
/// 17:10:18.649  eval                ← 等前面那组回来        +1442ms│ 都在首条
/// 17:10:18.855  quota probe (messages)                     +1648ms┘ messages 之前
/// 17:10:18.909  penguin_mode  ┐                            +1702ms ┐
/// 17:10:18.915  mcp_servers   │                            +1708ms │
/// 17:10:18.919  mcp-registry  │ 同时发（17ms 内五条）        +1712ms │ 收尾段
/// 17:10:18.921  bootstrap     │                            +1714ms │ 与主请求
/// 17:10:18.926  code/triggers ┘                            +1719ms │ 重叠
/// 17:10:19.355  mcp-registry?cursor=…  ← 翻页只能串行       +2148ms │
/// 17:10:19.697  mcp-registry?cursor=…                      +2490ms │
/// 17:10:19.699  **第一条 /v1/messages**                     +2492ms │
/// 17:10:20.072  mcp-registry?cursor=…                      +2865ms ┘
/// ```
///
/// 也就是说：**领跑那四条确实排在首条 messages 之前**（政策/设置/特性开关/额度探测），
/// 而收尾那批与主请求是重叠的、甚至排在它后面。故 [`Self::lead`] 由转发路径 `await`
/// （带上限，见 [`config::HANDSHAKE_LEAD_TIMEOUT_MS`]），[`Self::rest`] 照旧 spawn。
///
/// 失败只记日志：握手是形态补齐，不影响转发。401/403 也不在这里封号——转发路径与保活
/// 各有自己的判定。
pub struct HandshakeRunner {
    ctx: KeepaliveCtx,
    /// bootstrap 的 `model=` 参数：规范名。
    model: String,
    cred_id: i64,
    cred_label: String,
    /// 要不要补**无鉴权的公共请求**（mcp-registry、downloads）。
    ///
    /// 这几条不带 `Authorization`，与「哪个账号」无关，补不补都不影响凭证形态；但真实
    /// CC 客户端**自己也在发**（`cap/2.1.258-api` 里 mcp-registry 与 releases/latest 都
    /// 是客户端直连打的，不经 luban）。同一台机器上再补一遍，从上游看就是同一个客户端把
    /// 同一批公共端点打了两遍。
    ///
    /// 故只给**模拟客户端**补：那种客户端根本不是 CC，不会自己发这些。
    public_traffic: bool,
}

impl HandshakeRunner {
    /// `public_traffic` 见 [`Self::public_traffic`]：模拟客户端传 `true`，真实 CC 传 `false`。
    pub fn new(
        cred: &crate::credentials::Credential,
        h: crate::telemetry::Handshake,
        organization_uuid: Option<String>,
        public_traffic: bool,
    ) -> Self {
        note_handshake(cred.id);
        Self {
            ctx: KeepaliveCtx::new(cred, 0.0, organization_uuid, Some(h.snapshot)),
            model: h.model,
            cred_id: cred.id,
            cred_label: cred.label.clone(),
            public_traffic,
        }
    }

    /// 领跑段：`policy_limits` + `settings`（并发）→ `eval`。**调用方应当在发出这个会话的
    /// 首条 `/v1/messages` 之前 `await` 它**，额度探测紧随其后由调用方发。
    pub async fn lead(&self, client: &wreq::Client, access_token: &str) {
        let (policy_limits, settings) = tokio::join!(
            keepalive_policy_limits(client, access_token, &self.ctx),
            keepalive_settings(client, access_token, &self.ctx),
        );
        let eval = keepalive_eval(client, access_token, &self.ctx).await;
        self.report(
            "lead",
            &[("policy_limits", policy_limits), ("settings", settings), ("eval", eval)],
        );
    }

    /// 收尾段：penguin / mcp_servers ×2 / mcp-registry（翻页）/ bootstrap / code triggers /
    /// downloads。抓包里这批与首条 messages 重叠，故 spawn 即可，不必挡着主请求。
    pub async fn rest(&self, client: &wreq::Client, access_token: &str) {
        let (penguin_mode, mcp_servers, mcp_registry, bootstrap, code_triggers, mcp_servers_2) = tokio::join!(
            keepalive_penguin_mode(client, access_token),
            handshake_mcp_servers(client, access_token),
            self.maybe_registry(client),
            keepalive_bootstrap(client, access_token, &self.ctx, &self.model),
            handshake_code_triggers(client, access_token, &self.ctx),
            handshake_mcp_servers(client, access_token),
        );
        self.report(
            "rest",
            &[
                ("penguin_mode", penguin_mode),
                ("mcp_servers", mcp_servers),
                ("mcp_registry", mcp_registry),
                ("bootstrap", bootstrap),
                ("code_triggers", code_triggers),
                ("mcp_servers", mcp_servers_2),
            ],
        );
    }

    /// `downloads.claude.ai` 上那两条：**不跟 rest 一起发，各自延迟**。
    ///
    /// 抓包里它们离会话起点很远，且节奏完全不同（`cap/2.1.260-2`）：
    ///
    /// ```text
    /// 17:14:56.354  policy_limits（会话起点）
    /// 17:15:05.957  releases/latest                      +9.6s
    /// 17:17:01.380  plugins/claude-plugins-official      +2min5s
    /// ```
    ///
    /// 另一个会话（17:43:01 起）的 releases 也在 +9.8s，plugins 那一整段窗口里干脆没有。
    /// 跟 rest 一起在 +2s 内发完，就是把两条本该稀稀拉拉的后台请求挤成了启动风暴的一部分。
    ///
    /// 这两条**无鉴权**，与账号无关，晚发几秒/几分钟不影响任何功能。
    pub async fn downloads(&self, client: &wreq::Client) {
        let releases = {
            tokio::time::sleep(Duration::from_millis(config::DOWNLOAD_RELEASES_DELAY_MS)).await;
            handshake_download(client, "/claude-code-releases/latest").await
        };
        let plugins = {
            tokio::time::sleep(Duration::from_millis(
                config::DOWNLOAD_PLUGINS_DELAY_MS - config::DOWNLOAD_RELEASES_DELAY_MS,
            ))
            .await;
            handshake_download(
                client,
                "/claude-code-releases/plugins/claude-plugins-official/latest",
            )
            .await
        };
        self.report("downloads", &[("releases_latest", releases), ("plugins_latest", plugins)]);
    }

    /// mcp-registry 翻页：**无鉴权的公共请求**，真实 CC 自己也在发，故只给模拟客户端补。
    /// 见 [`Self::public_traffic`]。
    async fn maybe_registry(&self, client: &wreq::Client) -> KeepaliveResult {
        if !self.public_traffic {
            return KeepaliveResult::Ok;
        }
        handshake_mcp_registry(client, &self.ctx).await
    }

    fn report(&self, stage: &str, results: &[(&str, KeepaliveResult)]) {
        let failed: Vec<&str> =
            results.iter().filter(|(_, r)| !r.is_ok()).map(|(n, _)| *n).collect();
        let session: String = self.ctx.session_id.chars().take(8).collect();
        if failed.is_empty() {
            tracing::debug!(cred_id = self.cred_id, cred = %self.cred_label, session, stage, "session handshake done");
        } else {
            tracing::warn!(cred_id = self.cred_id, cred = %self.cred_label, session, stage, failed = ?failed, "session handshake: some endpoints failed");
        }
    }
}

/// Datadog 遥测日志：每 tick（30min）发往 `http-intake.logs.us5.datadoghq.com`。
///
/// 取自 `cap/2.1.145/00066`（idle 周期，2 条 flat 格式日志）。
/// `dd_client` 应为直连客户端（不走凭证代理——真实客户端的 Datadog 也是直连）。
pub async fn keepalive_datadog_logs(dd_client: &wreq::Client, ctx: &KeepaliveCtx) -> bool {
    let resp = axios(
        dd_client
            .post(config::DATADOG_INTAKE_URL)
            .header("Accept", config::AXIOS_ACCEPT)
            .header("DD-API-KEY", config::DATADOG_API_KEY)
            .header("User-Agent", config::DATADOG_USER_AGENT)
            .json(&ctx.dd_idle_entries()),
        "datadog",
    )
    .send()
    .await;
    match resp {
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{KeepaliveCtx, TokenEndpointError, tier_from};
    use crate::config;
    use wreq::StatusCode;

    /// axios 那套辅助端点的头序表逐条对上抓包（`cap/2.1.260-2`）。
    ///
    /// 这是 [`config::AXIOS_SHAPES`] 唯一的正确性依据。钉住它是因为「头序」这种东西改错了
    /// 不会有任何运行时症状——请求照样 200，只是每个会话十来条请求整整齐齐地与官方不一样。
    #[test]
    fn axios_header_orders_match_the_captures() {
        // (端点, 抓包编号, 逐字头序)
        let cases: &[(&str, &str, &[&str])] = &[
            (
                "policy_limits",
                "00001",
                &[
                    "Accept",
                    "Authorization",
                    "anthropic-beta",
                    "User-Agent",
                    "If-None-Match",
                    "Accept-Encoding",
                    "Host",
                    "Connection",
                ],
            ),
            (
                "settings",
                "00002",
                &[
                    "Accept",
                    "Authorization",
                    "anthropic-beta",
                    "User-Agent",
                    "Cache-Control",
                    "Pragma",
                    "If-None-Match",
                    "Accept-Encoding",
                    "Host",
                    "Connection",
                ],
            ),
            (
                "penguin_mode",
                "00005",
                &[
                    "Accept",
                    "Authorization",
                    "anthropic-beta",
                    "User-Agent",
                    "Accept-Encoding",
                    "Host",
                    "Connection",
                ],
            ),
            (
                "mcp_registry",
                "00007",
                &["Accept", "User-Agent", "Accept-Encoding", "Host", "Connection"],
            ),
            (
                "bootstrap",
                "00008",
                &[
                    "Accept",
                    "Content-Type",
                    "User-Agent",
                    "Authorization",
                    "anthropic-beta",
                    "Accept-Encoding",
                    "Host",
                    "Connection",
                ],
            ),
            (
                "event_logging",
                "00016",
                &[
                    "Accept",
                    "Content-Type",
                    "User-Agent",
                    "x-service-name",
                    "Authorization",
                    "anthropic-beta",
                    "Content-Length",
                    "Accept-Encoding",
                    "Host",
                    "Connection",
                ],
            ),
            (
                "datadog",
                "00017",
                &[
                    "Accept",
                    "Content-Type",
                    "DD-API-KEY",
                    "User-Agent",
                    "Content-Length",
                    "Accept-Encoding",
                    "Host",
                    "Connection",
                ],
            ),
        ];
        for (name, cap, order) in cases {
            assert_eq!(config::axios_shape(name), *order, "{name}（cap/2.1.260-2/{cap}）");
        }

        // `Authorization` 与 `User-Agent` 的先后在两个端点上正好相反——这正是「不能合并成
        // 一张总表」的证据，别哪天又想着统一。
        let pos = |ep: &str, h: &str| config::axios_shape(ep).iter().position(|x| *x == h).unwrap();
        assert!(pos("policy_limits", "Authorization") < pos("policy_limits", "User-Agent"));
        assert!(pos("bootstrap", "User-Agent") < pos("bootstrap", "Authorization"));

        // 尾部三件套 11 类一致。
        for shape in config::AXIOS_SHAPES {
            assert_eq!(
                &shape.order[shape.order.len() - 3..],
                &["Accept-Encoding", "Host", "Connection"],
                "{} 的尾部",
                shape.name
            );
        }

        // eval 不是 axios：Bun 的 fetch，`Connection` 在串中间而不是队尾。
        let eval = config::AXIOS_SHAPE_EVAL.order;
        assert_eq!(eval.last(), Some(&"Content-Length"), "eval 队尾是 Content-Length");
        assert!(eval.contains(&"Connection"));
        assert_ne!(eval.last(), Some(&"Connection"));
    }

    /// `If-None-Match` 里那串 `sha256:…` 是**客户端自己算的响应体摘要**，不是服务端 ETag。
    ///
    /// 依据：`cap/2.1.260-2/00002` 那条 settings 发的是
    /// `"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"`，
    /// 而那正是 `{}` 的 sha256。
    #[test]
    fn conditional_get_hashes_the_cached_body() {
        const EMPTY_OBJECT_SHA: &str =
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
        // 用一个本测试专属的凭证 id，免得和别的用例共用那张进程级表。
        let id = 90_001;

        // 没缓存过就不带这个头——一台刚装好的机器第一次跑就是这个形态。
        assert!(super::etag_of(id, "settings").is_none());

        super::remember_etag(id, "settings", 200, b"{}");
        assert_eq!(
            super::etag_of(id, "settings").as_deref(),
            Some(format!("\"sha256:{EMPTY_OBJECT_SHA}\"").as_str()),
            "与 cap/2.1.260-2/00002 逐字相同"
        );

        // 304 不动缓存：上游说的就是「没变」，拿一个空 body 覆盖会把下次的条件请求打歪。
        super::remember_etag(id, "settings", 304, b"");
        assert_eq!(
            super::etag_of(id, "settings").as_deref(),
            Some(format!("\"sha256:{EMPTY_OBJECT_SHA}\"").as_str()),
            "304 保留原值"
        );

        // 换了内容就换摘要；不同端点各记各的。
        super::remember_etag(id, "settings", 200, b"{\"a\":1}");
        assert_ne!(
            super::etag_of(id, "settings").as_deref(),
            Some(format!("\"sha256:{EMPTY_OBJECT_SHA}\"").as_str())
        );
        assert!(super::etag_of(id, "policy_limits").is_none(), "两个端点不共用一个键");
    }

    /// 保活循环靠这个标记跳过自己那份重复的启动串：新会话的握手已经把
    /// bootstrap / penguin / policy_limits / settings 全打过一遍了。
    #[test]
    fn a_recent_handshake_is_visible_to_the_keepalive_loop() {
        use std::time::Duration;
        let id = 90_002;
        assert!(!super::handshake_recent(id, Duration::from_secs(1800)), "没握过手就没有标记");
        super::note_handshake(id);
        assert!(super::handshake_recent(id, Duration::from_secs(1800)), "刚握过");
        // 窗口足够短时又算「不近」——保活下一跳照常发自己那份。
        assert!(!super::handshake_recent(id, Duration::ZERO));
        assert!(!super::handshake_recent(90_003, Duration::from_secs(1800)), "别的号不受影响");
    }

    /// 有近期真实会话时，保活事件与 Datadog 日志挂在那个会话的身份上：同一个 session_id /
    /// device_id / 版本 / 模型 / beta 串，`auth` 块带组织 id。没有时退回按账号派生的身份。
    #[test]
    fn keepalive_attaches_to_the_real_session_when_there_is_one() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let store = crate::store::CredentialStore::open_in_memory().unwrap();
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        let snapshot = crate::telemetry::SessionSnapshot {
            session_id: "4dc73702-d904-4887-809d-17b93cc5357c".into(),
            device_id: "b9".repeat(32),
            account_uuid: "9922ef8e-7945-4f5a-ab4f-cf5f521531df".into(),
            version: "2.1.260".into(),
            model: "claude-opus-5[1m]".into(),
            betas: "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07".into(),
            prompt_id: "6c079143-0c53-4c48-817d-105460b3f622".into(),
            started_wall: std::time::SystemTime::now() - std::time::Duration::from_secs(600),
        };
        let org = Some("09520b85-f6b6-432f-97e2-6ecb804a083f".to_string());
        let ctx = KeepaliveCtx::new(&cred, 5.0, org.clone(), Some(snapshot));
        assert_eq!(ctx.session_id, "4dc73702-d904-4887-809d-17b93cc5357c");
        assert_eq!(ctx.device_id, "b9".repeat(32));
        assert!(ctx.uptime_secs >= 600.0, "uptime 从真实会话起点算，而不是 luban 的");

        let ev = &ctx.idle_events()[0]["event_data"];
        assert_eq!(ev["session_id"], "4dc73702-d904-4887-809d-17b93cc5357c");
        assert_eq!(ev["device_id"], "b9".repeat(32));
        assert_eq!(ev["model"], "claude-opus-5[1m]");
        assert_eq!(ev["betas"], "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07");
        assert_eq!(ev["env"]["version"], "2.1.260");
        assert_eq!(ev["auth"]["organization_uuid"], "09520b85-f6b6-432f-97e2-6ecb804a083f");
        let meta: serde_json::Value = serde_json::from_slice(
            &STANDARD.decode(ev["additional_metadata"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622");

        let dd = &ctx.dd_idle_entries()[0];
        assert_eq!(dd["model"], "claude-opus-5", "Datadog 那份是规范名");
        assert_eq!(dd["session_id"], "4dc73702-d904-4887-809d-17b93cc5357c");
        assert_eq!(dd["version"], "2.1.260");
        assert_eq!(
            ctx.eval_body()["attributes"]["organizationUUID"],
            "09520b85-f6b6-432f-97e2-6ecb804a083f"
        );
        assert_eq!(ctx.eval_body()["attributes"]["appVersion"], "2.1.260");

        // 没有近期会话：退回派生身份，模型/版本用保活默认值，auth 只有账号。
        let idle = KeepaliveCtx::new(&cred, 5.0, None, None);
        assert_ne!(idle.session_id, ctx.session_id);
        let ev = &idle.idle_events()[0]["event_data"];
        assert_eq!(ev["model"], "claude-sonnet-5");
        assert_eq!(ev["env"]["version"], super::keepalive_version());
        assert!(ev["auth"].get("organization_uuid").is_none());
        assert!(idle.eval_body()["attributes"].get("organizationUUID").is_none());
    }

    /// statsig eval 的 `attributes`：`rateLimitTier` 发**原值**、`firstTokenTime` 取凭证
    /// 入库那一刻，`subscriptionCreatedAt` **不发**（拿不到，见 [`super::KeepaliveCtx::eval_body`]）。
    ///
    /// 官方那份的键序（`cap/2.1.260-2/00003`）：
    /// `… userType, subscriptionType, rateLimitTier, organizationRole,
    ///    subscriptionCreatedAt, firstTokenTime, appVersion, entrypoint`。
    #[test]
    fn eval_attributes_carry_the_raw_rate_limit_tier() {
        let store = crate::store::CredentialStore::open_in_memory().unwrap();
        let cred = store.insert("t", None, "a", "r", 0, None, Some("claude_team")).unwrap();
        store.set_rate_limit_tier(cred.id, Some("default_claude_max_5x")).unwrap();
        let cred = store.get(cred.id).unwrap().unwrap();

        let attrs = KeepaliveCtx::new(&cred, 1.0, None, None).eval_body();
        let attrs = &attrs["attributes"];
        assert_eq!(attrs["rateLimitTier"], "default_claude_max_5x", "发原值，不是界面上的 Max 5x");
        assert_eq!(attrs["subscriptionType"], "team");
        // 入库那一刻的毫秒时间戳，不是 0、也不是编出来的常量。
        let first = attrs["firstTokenTime"].as_i64().expect("凭证有 created_at 就该有这一项");
        assert_eq!(first, cred.created_at as i64 * 1000);
        assert!(first > 1_700_000_000_000, "毫秒量级: {first}");
        // 拿不到的那个明确不发——填个常量会让同一批号全都是同一个订阅创建时间。
        assert!(
            attrs.get("subscriptionCreatedAt").is_none(),
            "拿不到就不发，别填伪造常量: {attrs}"
        );

        // 键序照抓包：rateLimitTier 夹在 subscriptionType 与 organizationRole 之间。
        let keys: Vec<&str> = attrs.as_object().unwrap().keys().map(String::as_str).collect();
        let at = |k: &str| keys.iter().position(|x| *x == k).unwrap();
        assert!(at("subscriptionType") < at("rateLimitTier"));
        assert!(at("rateLimitTier") < at("organizationRole"));
        assert!(at("organizationRole") < at("firstTokenTime"));
        assert!(at("firstTokenTime") < at("appVersion"));

        // 旧库里没回填过的号：这一项整个不发，而不是发一个空串。
        let bare = store.insert("t2", None, "a2", "r2", 0, None, None).unwrap();
        let bare = KeepaliveCtx::new(&bare, 1.0, None, None).eval_body();
        assert!(bare["attributes"].get("rateLimitTier").is_none());
    }

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
