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
    /// 进程已运行秒数。
    uptime_secs: f64,
}

/// 进程级种子，每次启动随机一次；用于从 account_uuid 派生出每次启动不同的 session_id。
static PROCESS_SEED: std::sync::LazyLock<[u8; 16]> = std::sync::LazyLock::new(|| {
    let mut buf = [0u8; 16];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut buf);
    buf
});

impl KeepaliveCtx {
    pub fn new(cred: &crate::credentials::Credential, uptime_secs: f64) -> Self {
        let id_str = cred.id.to_string();
        let basis = cred.account_uuid.as_deref().unwrap_or(&id_str);
        let seed: &[u8] = &*PROCESS_SEED;
        let session_id = derive_uuid(basis, seed, b"session");
        let prompt_id = derive_uuid(basis, seed, b"prompt");
        let device_id = derive_hex64(basis, seed, b"device");

        let subscription_type = match cred.org_type.as_deref() {
            Some(t) if t.contains("team") => "team",
            Some(t) if t.contains("enterprise") => "enterprise",
            _ => "individual",
        }
        .to_string();

        Self {
            session_id,
            device_id,
            prompt_id,
            account_uuid: basis.to_string(),
            subscription_type,
            uptime_secs,
        }
    }

    /// 所有事件共用的 `env` 块。
    fn env_block(&self) -> serde_json::Value {
        serde_json::json!({
            "platform": "darwin",
            "node_version": "v26.3.0",
            "terminal": "vscode",
            "package_managers": "npm,pnpm",
            "runtimes": "bun,node",
            "is_running_with_bun": true,
            "is_ci": false,
            "is_claubbit": false,
            "is_github_action": false,
            "is_claude_code_action": false,
            "is_claude_ai_auth": true,
            "version": keepalive_version(),
            "arch": "arm64",
            "is_claude_code_remote": false,
            "deployment_environment": "unknown-darwin",
            "is_conductor": false,
            "version_base": keepalive_version(),
            "build_time": "2026-08-25T18:33:51Z",
            "is_local_agent_mode": false,
            "platform_raw": "darwin",
            "shell": "zsh"
        })
    }

    /// base64 编码的 `process` 运行时指标。
    fn process_b64(&self) -> String {
        let rss = 180_000_000.0 + self.uptime_secs * 6.0;
        let heap = 44_000_000.0 + self.uptime_secs * 5.0;
        let user_cpu = (self.uptime_secs * 6300.0) as u64;
        let sys_cpu = (self.uptime_secs * 1200.0) as u64;
        let val = serde_json::json!({
            "uptime": self.uptime_secs,
            "rss": rss as u64,
            "heapTotal": (heap * 0.98) as u64,
            "heapUsed": heap as u64,
            "external": 16_420_226_u64,
            "arrayBuffers": 14335_u64,
            "constrainedMemory": 34_359_738_368_u64,
            "cpuUsage": { "user": user_cpu, "system": sys_cpu },
            "cpuPercent": 0.42,
            "cpuWindowMs": (self.uptime_secs * 1000.0).min(1_800_000.0) as u64
        });
        URL_SAFE_NO_PAD.encode(serde_json::to_string(&val).unwrap_or_default())
    }

    /// base64 编码的 `additional_metadata`。
    fn metadata_b64(&self, extra: serde_json::Value) -> String {
        let mut m = serde_json::json!({
            "renderer_mode": "default",
            "subscription_type": &self.subscription_type,
            "cc_prompt_id": &self.prompt_id
        });
        if let (Some(obj), Some(base)) = (extra.as_object(), m.as_object_mut()) {
            for (k, v) in obj {
                base.insert(k.clone(), v.clone());
            }
        }
        URL_SAFE_NO_PAD.encode(serde_json::to_string(&m).unwrap_or_default())
    }

    /// `auth` 块。
    fn auth_block(&self) -> serde_json::Value {
        serde_json::json!({ "account_uuid": &self.account_uuid })
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
        serde_json::json!({
            "event_type": "ClaudeCodeInternalEvent",
            "event_data": {
                "event_name": name,
                "client_timestamp": ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "model": "claude-sonnet-5",
                "session_id": &self.session_id,
                "user_type": "external",
                "betas": config::KEEPALIVE_EVENT_BETAS,
                "env": self.env_block(),
                "entrypoint": "cli",
                "is_interactive": true,
                "client_type": "cli",
                "process": self.process_b64(),
                "additional_metadata": self.metadata_b64(extra_meta),
                "auth": self.auth_block(),
                "event_id": uuid_v4(),
                "device_id": &self.device_id
            }
        })
    }

    /// Datadog 日志条目的公共字段（flat 形态，取自 `cap/2.1.145/00066`）。
    ///
    /// 拆成手工构建避免 `json!` 宏在字段过多时撞 recursion_limit。
    fn dd_entry(&self, message: &str, extra: serde_json::Value) -> serde_json::Value {
        use serde_json::{Map, Value, json};
        let ver = keepalive_version();
        let mut m = Map::new();
        let s = |v: &str| Value::String(v.to_string());

        m.insert("ddsource".into(), s("nodejs"));
        m.insert(
            "ddtags".into(),
            s(&format!(
                "event:{message},arch:arm64,client_type:cli,entrypoint:cli,\
                 model:claude-sonnet-5,platform:darwin,subscription_type:{},\
                 user_bucket:15,user_type:external,version:{ver},version_base:{ver}",
                self.subscription_type
            )),
        );
        m.insert("message".into(), s(message));
        m.insert("service".into(), s("claude-code"));
        m.insert("hostname".into(), s("claude-code"));
        m.insert("env".into(), s("external"));
        m.insert("model".into(), s("claude-sonnet-5"));
        m.insert("session_id".into(), s(&self.session_id));
        m.insert("user_type".into(), s("external"));
        m.insert("betas".into(), s(config::KEEPALIVE_EVENT_BETAS));
        m.insert("entrypoint".into(), s("cli"));
        m.insert("is_interactive".into(), s("true"));
        m.insert("client_type".into(), s("cli"));
        m.insert(
            "process_metrics".into(),
            json!({
                "uptime": self.uptime_secs,
                "rss": (180_000_000.0 + self.uptime_secs * 6.0) as u64,
                "heapTotal": (44_000_000.0 + self.uptime_secs * 4.8) as u64,
                "heapUsed": (44_000_000.0 + self.uptime_secs * 5.0) as u64,
                "external": 16_420_226_u64,
                "arrayBuffers": 14335_u64,
                "constrainedMemory": 34_359_738_368_u64,
                "cpuUsage": {
                    "user": (self.uptime_secs * 6300.0) as u64,
                    "system": (self.uptime_secs * 1200.0) as u64
                },
                "cpuPercent": 0.42,
                "cpuWindowMs": 32_u64
            }),
        );
        for (k, v) in
            [("swe_bench_run_id", ""), ("swe_bench_instance_id", ""), ("swe_bench_task_id", "")]
        {
            m.insert(k.into(), s(v));
        }
        m.insert("subscription_type".into(), s(&self.subscription_type));
        m.insert("renderer_mode".into(), s("default"));
        m.insert("prompt_id".into(), s(&self.prompt_id));
        m.insert("platform".into(), s("darwin"));
        m.insert("platform_raw".into(), s("darwin"));
        m.insert("arch".into(), s("arm64"));
        m.insert("node_version".into(), s("v26.3.0"));
        m.insert("terminal".into(), s("vscode"));
        m.insert("shell".into(), s("zsh"));
        m.insert("package_managers".into(), s("npm,pnpm"));
        m.insert("runtimes".into(), s("bun,node"));
        for (k, v) in [
            ("is_running_with_bun", true),
            ("is_ci", false),
            ("is_claubbit", false),
            ("is_claude_code_remote", false),
            ("is_local_agent_mode", false),
            ("is_conductor", false),
            ("is_github_action", false),
            ("is_claude_code_action", false),
            ("is_claude_ai_auth", true),
        ] {
            m.insert(k.into(), Value::Bool(v));
        }
        m.insert("version".into(), s(ver));
        m.insert("version_base".into(), s(ver));
        m.insert("build_time".into(), s("2026-08-25T18:33:51Z"));
        m.insert("deployment_environment".into(), s("unknown-darwin"));
        m.insert("user_bucket".into(), Value::Number(15.into()));

        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
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
        serde_json::json!({
            "attributes": {
                "id": &self.device_id,
                "sessionId": &self.session_id,
                "deviceID": &self.device_id,
                "platform": "darwin",
                "accountUUID": &self.account_uuid,
                "userType": "external",
                "subscriptionType": &self.subscription_type,
                "organizationRole": "user",
                "appVersion": keepalive_version(),
                "entrypoint": "cli"
            },
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

/// 伪 UUID v4。
fn uuid_v4() -> String {
    let mut buf = [0u8; 16];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut buf);
    buf[6] = (buf[6] & 0x0F) | 0x40;
    buf[8] = (buf[8] & 0x3F) | 0x80;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        u16::from_be_bytes([buf[4], buf[5]]),
        u16::from_be_bytes([buf[6], buf[7]]),
        u16::from_be_bytes([buf[8], buf[9]]),
        u64::from_be_bytes([0, 0, buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]),
    )
}

/// 每 tick（30min）发一次 `event_logging`。
///
/// 真实客户端每次都带上版本检查等活动产生的事件；空批次是指纹。
/// 这里模拟 idle 周期产生的 7 个事件（`tengu_*`），结构取自 `cap/2.1.145/00086`。
pub async fn keepalive_event_logging(
    client: &wreq::Client,
    access_token: &str,
    ctx: &KeepaliveCtx,
) -> bool {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_EVENT_LOGGING);
    let body = serde_json::json!({ "events": ctx.idle_events() });
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

/// 首 tick 发一次 `metrics`。
///
/// 真实客户端带 4 项指标（session.count / cost.usage / token.usage / active_time.total），
/// 空 metrics 数组是指纹。这里填合理的小值，结构取自 `cap/2.1.145/00061`。
pub async fn keepalive_metrics(
    client: &wreq::Client,
    access_token: &str,
    ctx: &KeepaliveCtx,
) -> bool {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_METRICS);
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let body = serde_json::json!({
        "resource_attributes": {
            "service.name": "claude-code",
            "service.version": keepalive_version(),
            "os.type": "darwin",
            "os.version": "27.0.0",
            "host.arch": "arm64",
            "aggregation.temporality": "delta",
            "user.customer_type": "claude_ai",
            "user.subscription_type": &ctx.subscription_type
        },
        "metrics": [
            {
                "name": "claude_code.session.count",
                "description": "Count of CLI sessions started",
                "unit": "",
                "data_points": [{
                    "attributes": {
                        "user.id": &ctx.device_id,
                        "session.id": &ctx.session_id,
                        "terminal.type": "vscode",
                        "start_type": "fresh"
                    },
                    "value": 1,
                    "timestamp": &ts
                }]
            },
            {
                "name": "claude_code.cost.usage",
                "description": "Cost of the Claude Code session",
                "unit": "USD",
                "data_points": [{
                    "attributes": {
                        "user.id": &ctx.device_id,
                        "session.id": &ctx.session_id,
                        "terminal.type": "vscode",
                        "model": "claude-sonnet-5",
                        "query_source": "main",
                        "effort": "high"
                    },
                    "value": 0.042,
                    "timestamp": &ts
                }]
            },
            {
                "name": "claude_code.token.usage",
                "description": "Number of tokens used",
                "unit": "tokens",
                "data_points": [
                    { "attributes": { "user.id": &ctx.device_id, "session.id": &ctx.session_id, "terminal.type": "vscode", "model": "claude-sonnet-5", "query_source": "main", "effort": "high", "type": "input" }, "value": 5, "timestamp": &ts },
                    { "attributes": { "user.id": &ctx.device_id, "session.id": &ctx.session_id, "terminal.type": "vscode", "model": "claude-sonnet-5", "query_source": "main", "effort": "high", "type": "output" }, "value": 18, "timestamp": &ts },
                    { "attributes": { "user.id": &ctx.device_id, "session.id": &ctx.session_id, "terminal.type": "vscode", "model": "claude-sonnet-5", "query_source": "main", "effort": "high", "type": "cacheRead" }, "value": 22000, "timestamp": &ts },
                    { "attributes": { "user.id": &ctx.device_id, "session.id": &ctx.session_id, "terminal.type": "vscode", "model": "claude-sonnet-5", "query_source": "main", "effort": "high", "type": "cacheCreation" }, "value": 6500, "timestamp": &ts }
                ]
            },
            {
                "name": "claude_code.active_time.total",
                "description": "Total active time in seconds",
                "unit": "s",
                "data_points": [
                    { "attributes": { "user.id": &ctx.device_id, "session.id": &ctx.session_id, "terminal.type": "vscode", "type": "user" }, "value": 3.8, "timestamp": &ts },
                    { "attributes": { "user.id": &ctx.device_id, "session.id": &ctx.session_id, "terminal.type": "vscode", "type": "cli" }, "value": 2.4, "timestamp": &ts }
                ]
            }
        ]
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

/// 每小时发一次 `GET /api/claude_code/policy_limits`。
pub async fn keepalive_policy_limits(client: &wreq::Client, access_token: &str) -> bool {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_POLICY_LIMITS);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", config::KEEPALIVE_USER_AGENT)
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await;
    match resp {
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

/// 每小时发一次 `GET /api/claude_code/settings`。
pub async fn keepalive_settings(client: &wreq::Client, access_token: &str) -> bool {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_SETTINGS);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", config::KEEPALIVE_USER_AGENT)
        .header("Cache-Control", "no-cache")
        .header("Pragma", "no-cache")
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await;
    match resp {
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

// ---------- startup bootstrap + 周期端点 ----------

/// 启动握手：`GET /api/claude_cli/bootstrap`。
///
/// 取自 `cap/2.1.145/00043`（UA = `claude-code/2.1.246`）。
pub async fn keepalive_bootstrap(client: &wreq::Client, access_token: &str) -> bool {
    let url = format!(
        "{}{}?entrypoint=cli&model=claude-sonnet-5",
        config::UPSTREAM_BASE_URL,
        config::KEEPALIVE_BOOTSTRAP
    );
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("User-Agent", config::KEEPALIVE_USER_AGENT)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await;
    match resp {
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

/// 启动握手：`GET /api/claude_code_penguin_mode`。
///
/// 取自 `cap/2.1.145/00044`（UA = `axios/1.15.2`，不带 Content-Type）。
pub async fn keepalive_penguin_mode(client: &wreq::Client, access_token: &str) -> bool {
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
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

/// Statsig 特性标志评估：启动 + 每 6h。
///
/// 取自 `cap/2.1.145/00039`（UA = `Bun/1.4.1`，Accept = `*/*`）。
pub async fn keepalive_eval(client: &wreq::Client, access_token: &str, ctx: &KeepaliveCtx) -> bool {
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
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
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
