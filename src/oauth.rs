//! OAuth PKCE 授权流程：生成挑战、构造授权 URL、交换与刷新 token。

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};

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

/// 每次保活循环构建一份，携带该凭证在当前进程里的"会话"身份。
///
/// 所有 id 由 `account_uuid` 确定性派生——同一凭证在同一进程里恒定；进程重启后变。
/// 没有 `account_uuid` 时用凭证 id 兜底（id 是自增整数，不会碰撞但也没有任何含义）。
pub struct KeepaliveCtx {
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
            };
        }
        let id_str = cred.id.to_string();
        let basis = cred.account_uuid.as_deref().unwrap_or(&id_str);
        let seed: &[u8] = &*PROCESS_SEED;
        let session_id = derive_uuid(basis, seed, b"session");
        let prompt_id = derive_uuid(basis, seed, b"prompt");
        let device_id = derive_hex64(basis, seed, b"device");

        Self {
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
        attrs.insert("organizationRole".into(), "user".into());
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
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", ctx.ua())
        .header("x-service-name", "claude-code")
        .header("Accept", "application/json, text/plain, */*")
        .json(&body)
        .send()
        .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// 每小时发一次 `GET /api/claude_code/policy_limits`；会话启动时也发一次。
pub async fn keepalive_policy_limits(
    client: &wreq::Client,
    access_token: &str,
    ctx: &KeepaliveCtx,
) -> KeepaliveResult {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_POLICY_LIMITS);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", ctx.ua())
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// 每小时发一次 `GET /api/claude_code/settings`；会话启动时也发一次。
pub async fn keepalive_settings(
    client: &wreq::Client,
    access_token: &str,
    ctx: &KeepaliveCtx,
) -> KeepaliveResult {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_SETTINGS);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", ctx.ua())
        .header("Cache-Control", "no-cache")
        .header("Pragma", "no-cache")
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
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
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", ctx.ua())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// 启动握手：`GET /api/claude_code_penguin_mode`。
///
/// 取自 `cap/2.1.145/00044`（UA = `axios/1.15.2`，不带 Content-Type）。
pub async fn keepalive_penguin_mode(client: &wreq::Client, access_token: &str) -> KeepaliveResult {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_PENGUIN_MODE);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", config::DATADOG_USER_AGENT)
        .header("Accept", "application/json, text/plain, */*")
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
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", config::KEEPALIVE_UA_BUN)
        .header("Accept", "*/*")
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
        let resp = client
            .get(&url)
            .header("User-Agent", ctx.cli_ua())
            .header("Accept", "application/json, text/plain, */*")
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
    let resp = client
        .get(&url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", "mcp-servers-2025-12-04")
        .header("anthropic-version", "2023-06-01")
        .header(
            "anthropic-mcp-client-capabilities",
            "eyJyb290cyI6eyJsaXN0Q2hhbmdlZCI6dHJ1ZX0sImVsaWNpdGF0aW9uIjp7fX0=",
        )
        .header("MCP-Protocol-Version", "2025-11-25")
        .header("User-Agent", config::DATADOG_USER_AGENT)
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
    let resp = client
        .get(&url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header("User-Agent", ctx.cli_ua())
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-client-platform", "claude_code_cli")
        .header("x-organization-uuid", org)
        .header("anthropic-beta", "ccr-triggers-2026-01-30")
        .send()
        .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// `downloads.claude.ai` 上的版本与插件市场元数据（无鉴权，axios UA）。
async fn handshake_download(client: &wreq::Client, path: &str) -> KeepaliveResult {
    let resp = client
        .get(format!("https://downloads.claude.ai{path}"))
        .header("Accept", "application/json, text/plain, */*")
        .header("User-Agent", config::DATADOG_USER_AGENT)
        .send()
        .await;
    match resp {
        Ok(r) => KeepaliveResult::from_status(r.status().as_u16()),
        Err(_) => KeepaliveResult::Failed,
    }
}

/// 替一个新会话做完整的启动握手，顺序照 `cap/2.1.260-1`（17:14:56–17:15:05）：
/// policy_limits、settings、eval、penguin_mode、mcp-registry（翻页）、mcp_servers、
/// code/triggers、mcp_servers、bootstrap、releases/latest、plugins latest。额度探测那条
/// `/v1/messages` 是客户端自己发的，经 luban 转发，不在这里。
///
/// 失败只记日志：握手是形态补齐，不影响转发。401/403 也不在这里封号——转发路径与保活
/// 各有自己的判定。
pub async fn session_handshake(
    client: &wreq::Client,
    access_token: &str,
    cred: &crate::credentials::Credential,
    h: crate::telemetry::Handshake,
    organization_uuid: Option<String>,
) {
    let ctx = KeepaliveCtx::new(cred, 0.0, organization_uuid, Some(h.snapshot));
    let mut results: Vec<(&str, KeepaliveResult)> = Vec::with_capacity(11);
    results.push(("policy_limits", keepalive_policy_limits(client, access_token, &ctx).await));
    results.push(("settings", keepalive_settings(client, access_token, &ctx).await));
    results.push(("eval", keepalive_eval(client, access_token, &ctx).await));
    results.push(("penguin_mode", keepalive_penguin_mode(client, access_token).await));
    results.push(("mcp_registry", handshake_mcp_registry(client, &ctx).await));
    results.push(("mcp_servers", handshake_mcp_servers(client, access_token).await));
    results.push(("code_triggers", handshake_code_triggers(client, access_token, &ctx).await));
    results.push(("mcp_servers", handshake_mcp_servers(client, access_token).await));
    results.push(("bootstrap", keepalive_bootstrap(client, access_token, &ctx, &h.model).await));
    results.push((
        "releases_latest",
        handshake_download(client, "/claude-code-releases/latest").await,
    ));
    results.push((
        "plugins_latest",
        handshake_download(client, "/claude-code-releases/plugins/claude-plugins-official/latest")
            .await,
    ));
    let failed: Vec<&str> = results.iter().filter(|(_, r)| !r.is_ok()).map(|(n, _)| *n).collect();
    let session: String = ctx.session_id.chars().take(8).collect();
    if failed.is_empty() {
        tracing::debug!(cred_id = cred.id, cred = %cred.label, session, "session handshake done");
    } else {
        tracing::warn!(cred_id = cred.id, cred = %cred.label, session, failed = ?failed, "session handshake: some endpoints failed");
    }
}

/// Datadog 遥测日志：每 tick（30min）发往 `http-intake.logs.us5.datadoghq.com`。
///
/// 取自 `cap/2.1.145/00066`（idle 周期，2 条 flat 格式日志）。
/// `dd_client` 应为直连客户端（不走凭证代理——真实客户端的 Datadog 也是直连）。
pub async fn keepalive_datadog_logs(dd_client: &wreq::Client, ctx: &KeepaliveCtx) -> bool {
    let resp = dd_client
        .post(config::DATADOG_INTAKE_URL)
        .header("DD-API-KEY", config::DATADOG_API_KEY)
        .header("User-Agent", config::DATADOG_USER_AGENT)
        .header("Accept", "application/json, text/plain, */*")
        .header("Accept-Encoding", "gzip, compress, deflate, br")
        .json(&ctx.dd_idle_entries())
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
    use wreq::StatusCode;

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
