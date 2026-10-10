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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepaliveResult {
    Ok,
    /// 上游 401/403——token 已吊销、账号被暂停，也可能是组织权限不足或区域限制。到底是
    /// 哪一种只有响应体说得清，所以把状态码、响应体与上游 `request-id` 一并带回，由调用点
    /// 写进封号事件（[`crate::store::CredentialStore::record_ban`]）。此前这是个单元变体，
    /// 落库的原因固定成「upstream 401/403」、来源记成 `manual`，事后无从区分。
    AuthRejected(AuthRejection),
    /// 网络错误或 5xx。
    Failed,
}

/// 保活端点 401/403 时的响应上下文，见 [`KeepaliveResult::AuthRejected`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRejection {
    /// 哪个端点拒的（`event_logging` / `bootstrap` / `policy_limits` / …）。
    pub endpoint: &'static str,
    pub status: u16,
    /// 响应体原文，最多 [`AUTH_REJECTION_BODY_CAP`] 个字符。通常是 Anthropic 的错误 JSON
    /// （`error.type` / `error.message`），也可能是网关的 HTML；解析交给调用点。
    pub body: String,
    /// 上游响应头 `request-id`。
    pub request_id: Option<String>,
}

/// 401/403 响应体最多留这么多字符：错误 JSON 几百字节，网关 HTML 几 KB，再长的没有诊断价值。
pub const AUTH_REJECTION_BODY_CAP: usize = 4096;

/// 从线上最多**读**这么多字节就停：[`AUTH_REJECTION_BODY_CAP`] 是字符数，UTF-8 一个字符最多
/// 4 字节，读到这里必定够截。与「先整体读完再截」的差别在于异常大的网关页面不会整个进内存。
const AUTH_REJECTION_READ_CAP: usize = AUTH_REJECTION_BODY_CAP * 4;

impl KeepaliveResult {
    /// 非 401/403 的状态码直接定档。401/403 必须走 [`Self::from_response`] /
    /// [`Self::from_parts`] 把响应体带上，别在这里丢掉。
    fn from_status(status: u16) -> Self {
        debug_assert!(status != 401 && status != 403, "401/403 must carry the response body");
        if status >= 500 { Self::Failed } else { Self::Ok }
    }

    /// 按响应定档：401/403 时读体、取 `request-id`，其它状态码不读体。
    async fn from_response(r: wreq::Response, endpoint: &'static str) -> Self {
        let status = r.status().as_u16();
        if status != 401 && status != 403 {
            return Self::from_status(status);
        }
        let request_id = upstream_request_id(&r);
        let body = read_body_capped(r, AUTH_REJECTION_READ_CAP).await;
        Self::from_parts(endpoint, status, request_id, &body)
    }

    /// 体已经在手（条件 GET 为算摘要总会读体）时用。
    fn from_parts(
        endpoint: &'static str,
        status: u16,
        request_id: Option<String>,
        body: &[u8],
    ) -> Self {
        if status != 401 && status != 403 {
            return Self::from_status(status);
        }
        let body = String::from_utf8_lossy(body).chars().take(AUTH_REJECTION_BODY_CAP).collect();
        Self::AuthRejected(AuthRejection { endpoint, status, body, request_id })
    }

    pub fn is_ok(&self) -> bool {
        *self == Self::Ok
    }
}

/// 上游响应头 `request-id`（与转发路径记进流水的是同一个头）。
fn upstream_request_id(r: &wreq::Response) -> Option<String> {
    r.headers().get("request-id").and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// 逐块读响应体，攒够 `cap` 字节就停、不再等剩下的；读到一半出错就交出已有的部分。
/// 只用于**要存下来的错误体**：正常 2xx 体（etag 摘要、版本号）仍整体读，它们本来就小且要完整。
async fn read_body_capped(r: wreq::Response, cap: usize) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    let mut stream = r.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        let room = cap.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= cap {
            break;
        }
    }
    out
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
    /// 账号 UUID（交换响应的 `account.uuid`）；重新授权时 profile 拉不到，靠它核对是不是同一个号。
    /// 刷新响应通常没有。
    pub account_uuid: Option<String>,
    /// 组织 UUID（交换响应的 `organization.uuid`）；profile 拉不到时的兜底，
    /// 见 [`crate::credentials::Credential::org_uuid`]。刷新响应通常没有。
    pub organization_uuid: Option<String>,
}

/// 账号 profile：邮箱、姓名、订阅等级、账号 UUID（来自 `/api/oauth/profile`）。
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub email: Option<String>,
    pub name: Option<String>,
    pub tier: Option<String>,
    /// 组织类型原值（`claude_team`/`claude_enterprise`/`claude_max`…），团队号与个人号
    /// 不是一回事（团队号是组织下的一个席位，额度按席位算），故单独留一列供前端标记。
    pub org_type: Option<String>,
    /// 额度档**原值**（`default_claude_max_5x` 之类），来自 `organization.rate_limit_tier`。
    ///
    /// 与 [`Self::tier`] 是两回事：那个是给界面看的（`Max 5x`），这个是 statsig eval 的
    /// `attributes.rateLimitTier` 要发的原串（`cap/2.1.260-2/00003`）。拿展示串顶替，
    /// 发出去的就是一个上游从没见过的取值。
    pub rate_limit_tier: Option<String>,
    /// 账号唯一标识（`account.uuid`）；用于转发时的身份伪装。
    pub account_uuid: Option<String>,
    /// 组织 UUID（`organization.uuid`）。见 [`crate::credentials::Credential::org_uuid`]。
    pub org_uuid: Option<String>,
    /// 订阅创建时刻原串（`organization.subscription_created_at`，ISO 8601）。
    /// 见 [`crate::credentials::Credential::subscription_created_at`]。
    pub subscription_created_at: Option<String>,
    /// 组织名称（`organization.name`）。
    pub org_name: Option<String>,
    /// 席位档原值（`organization.seat_tier`，团队号如 `team_standard` / `team_premium`）；
    /// 个人号没有。团队号的 [`Self::tier`] 由它推导。
    pub seat_tier: Option<String>,
    /// 订阅状态原值（`organization.subscription_status`，如 `active`）。
    pub subscription_status: Option<String>,
    /// 组织是否开了超额用量（`organization.has_extra_usage_enabled`）。
    pub extra_usage_enabled: Option<bool>,
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
    /// 刷新响应里**可以没有**：官方客户端是 `refresh_token: newRefreshToken = refreshToken`
    /// （`services/oauth/client.ts`），缺了就沿用旧的。此前这里是必填，一条合法的「不轮换
    /// refresh_token」的响应会在解析这一步被判成失败，号被白白停掉。
    #[serde(default)]
    refresh_token: Option<String>,
    /// 必填，**刻意不给默认值**：官方客户端也是直接 `Date.now() + expires_in * 1000`，没有
    /// 兜底（缺了就是 NaN）；这里若编一个「默认一小时」就是发明一个上游没说过的过期时间。
    expires_in: u64,
    #[serde(default)]
    account: Option<Account>,
    #[serde(default)]
    organization: Option<Organization>,
}

#[derive(Debug, Deserialize)]
struct Account {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Organization {
    #[serde(default)]
    name: Option<String>,
    /// 官方登录时 profile 拉不到就用这个当组织 id（`tokenAccount.organizationUuid`）。
    #[serde(default)]
    uuid: Option<String>,
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
    #[serde(default)]
    name: Option<String>,
    /// 团队号的席位档，如 `team_standard` / `team_premium`；个人号没有这一项。
    #[serde(default)]
    seat_tier: Option<String>,
    /// 如 `active`。
    #[serde(default)]
    subscription_status: Option<String>,
    #[serde(default)]
    has_extra_usage_enabled: Option<bool>,
    /// 如 `claude_max` / `claude_pro` / `claude_free`。
    #[serde(default)]
    organization_type: Option<String>,
    /// 如 `default_claude_max_5x` / `default_claude_max_20x`，含倍数档位。
    #[serde(default)]
    rate_limit_tier: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    /// ISO 8601 原串（如 `2026-04-15T13:03:55.239Z`）。官方 `storeOAuthAccountInfo` 存的
    /// 就是它，eval 时再换算成毫秒。
    #[serde(default)]
    subscription_created_at: Option<String>,
}

/// 官方 profile 请求的超时（`getOauthProfile.ts` 里 axios 的 `timeout: 10000`）。
const PROFILE_TIMEOUT: Duration = Duration::from_secs(10);

/// 官方 token 请求（交换与刷新）的超时（`services/oauth/client.ts` 里 `timeout: 15000`）。
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);

/// 用 access_token 获取账号 profile（邮箱、姓名、订阅等级）。
///
/// 形态照官方 `getOauthProfile.ts`：axios GET，显式头只有 `Authorization` 与
/// `Content-Type: application/json`，其余（`Accept`、缺省 UA、`Accept-Encoding`、
/// `Connection: close`）都是 axios 自己的。**没有** `anthropic-beta` / `anthropic-version`
/// ——此前多发了这两个、少发了 `Content-Type`，UA 与 `Accept-Encoding` 还是 Messages API
/// 那份 Bun 形态，与 [`post_token`] 一样是每个号必发的两条却与官方对不上。
pub async fn fetch_profile(client: &wreq::Client, access_token: &str) -> Result<Profile> {
    fetch_profile_from(client, config::PROFILE_URL, access_token).await
}

/// [`fetch_profile`] 的实现，端点可换——测试拿本地监听口核对线上形态。
async fn fetch_profile_from(
    client: &wreq::Client,
    url: &str,
    access_token: &str,
) -> Result<Profile> {
    let resp = axios(
        client
            .get(url)
            .header("Accept", config::AXIOS_ACCEPT)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("User-Agent", config::AXIOS_DEFAULT_USER_AGENT)
            .timeout(PROFILE_TIMEOUT),
        "oauth_profile",
    )
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
    // 原样留一份：上游的席位 / 额度档取值没有文档（`team_standard`、`team_tier_1`、`default_raven`…），
    // 新冒出来的写法只能靠这行看。含邮箱姓名，只进本地日志。
    tracing::info!(body = %text, "oauth profile raw response");

    let org_str = |f: fn(&ProfileOrg) -> &Option<String>| {
        p.organization.as_ref().and_then(|o| f(o).clone()).filter(|s| !s.trim().is_empty())
    };
    let seat_tier = org_str(|o| &o.seat_tier);
    let tier = tier_from(
        p.account.as_ref().and_then(|a| a.has_claude_max),
        p.account.as_ref().and_then(|a| a.has_claude_pro),
        p.organization.as_ref().and_then(|o| o.organization_type.as_deref()),
        p.organization.as_ref().and_then(|o| o.rate_limit_tier.as_deref()),
        seat_tier.as_deref(),
    );
    let email = p.account.as_ref().and_then(|a| a.email.clone());
    let name = p.account.as_ref().and_then(|a| {
        a.full_name.clone().or_else(|| a.display_name.clone()).filter(|s| !s.trim().is_empty())
    });
    let account_uuid =
        p.account.as_ref().and_then(|a| a.uuid.clone()).filter(|s| !s.trim().is_empty());

    Ok(Profile {
        email,
        name,
        tier,
        org_type: org_str(|o| &o.organization_type),
        rate_limit_tier: org_str(|o| &o.rate_limit_tier),
        account_uuid,
        org_uuid: org_str(|o| &o.uuid),
        subscription_created_at: org_str(|o| &o.subscription_created_at),
        org_name: org_str(|o| &o.name),
        seat_tier,
        subscription_status: org_str(|o| &o.subscription_status),
        extra_usage_enabled: p.organization.as_ref().and_then(|o| o.has_extra_usage_enabled),
    })
}

/// 由订阅标志推导账号等级：Max > Pro > Free；Max 附带倍数档（如 `Max 5x`）。
/// 组织号有席位档时按席位显示（`team_standard` → `Team Standard`）。
fn tier_from(
    has_max: Option<bool>,
    has_pro: Option<bool>,
    org_type: Option<&str>,
    rate_limit_tier: Option<&str>,
    seat_tier: Option<&str>,
) -> Option<String> {
    let mult = multiplier(rate_limit_tier); // 如 "5x" / "20x"
    let org_type = org_type.map(str::trim).filter(|s| !s.is_empty());

    // 组织号（团队 / 企业）只看组织那边的字段。`has_claude_max`/`has_claude_pro` 是**账号级**的，
    // 跟着人走、不跟组织：同一个人自己买了 Max 又被拉进团队时，登进团队组织那份 profile 里
    // `has_claude_max` 也是 true（实测 `claude_team` + `team_standard` + `default_raven`），
    // 先看它就会把团队席位判成个人 Max。
    if let Some(t) = org_type.filter(|t| !PERSONAL_ORG_TYPES.contains(t)) {
        // 团队号的 `rate_limit_tier` 是 `default_raven` 这类内部代号，读不出档位；
        // 席位档才是买的那一档（实测 `team_standard` / `team_tier_1`）。
        if let Some(seat) = seat_tier.map(str::trim).filter(|s| !s.is_empty()) {
            return Some(humanize_seat(seat));
        }
        // 没有席位档时额度档只能从 `rate_limit_tier` 读——`default_claude_max_5x` 说明这个
        // 组织拿的是 Max 5x 的量。
        if let Some(from_rate) = tier_from_rate_limit(rate_limit_tier) {
            return Some(from_rate);
        }
        return Some(humanize_tier(t));
    }

    if has_max == Some(true) {
        return Some(with_mult("Max", mult));
    }
    if has_pro == Some(true) {
        return Some("Pro".into());
    }
    if let Some(t) = org_type {
        let base = humanize_tier(t);
        // 组织类型是 max 时也带上倍数。
        return Some(if base == "Max" { with_mult("Max", mult) } else { base });
    }
    if has_max == Some(false) && has_pro == Some(false) {
        return Some("Free".into());
    }
    None
}

/// 个人组织的类型；其余（`claude_team`/`claude_enterprise`/没见过的）一律当组织号，
/// 与前端 `isOrgAccount` 同一口径。
const PERSONAL_ORG_TYPES: [&str; 3] = ["claude_max", "claude_pro", "claude_free"];

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

/// 席位档原值美化：`team_standard` → `Team Standard`。
///
/// 中间夹了变体段的写法（实测 `team_labs_premium`）认得出档位词时收成「组织类型 + 档位」
/// （→ `Team Premium`），与普通席位同一口径，徽章、筛选不用再认一种新写法；原值仍在
/// `seat_tier` 里，详情页看得到。认不出档位词的（`team_tier_1`）照原样逐段美化。
fn humanize_seat(raw: &str) -> String {
    let words: Vec<&str> = raw.split('_').filter(|w| !w.is_empty()).collect();
    let level =
        words.iter().skip(1).rev().find(|w| SEAT_LEVELS.contains(&w.to_ascii_lowercase().as_str()));
    let words = match (words.first(), level) {
        (Some(org), Some(level)) => vec![*org, *level],
        _ => words,
    };
    words
        .into_iter()
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 席位档里表示档位的词，见 [`humanize_seat`]。
const SEAT_LEVELS: [&str; 2] = ["standard", "premium"];

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

    // 交换响应缺 refresh_token 没有可回退的旧值，缺了就是缺了。
    post_token(client, exchange_body(&code, &returned_state, &pkce.verifier), None).await
}

/// 授权码交换的请求体。键序照官方 `exchangeCodeForTokens`（`services/oauth/client.ts`）：
/// `grant_type → code → redirect_uri → client_id → code_verifier → state`。serde_json 开着
/// `preserve_order`，这个顺序会原样上线；此前是 `state` 紧跟 `code`、`redirect_uri` 在
/// `client_id` 之后，与官方不同。
fn exchange_body(code: &str, state: &str, verifier: &str) -> serde_json::Value {
    serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": config::REDIRECT_URI,
        "client_id": config::CLIENT_ID,
        "code_verifier": verifier,
        "state": state,
    })
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
    /// 和 [`crate::proxy::classify_account_rejection`] 收紧时是同一个教训。403/429/5xx 以及所有
    /// 网络层错误一律当可重试，不停用。
    ///
    /// 注意：这个端点真实的失败响应形态我们**没有实测样本**，故这里只做保守的字面量匹配，
    /// 而不去猜它的 JSON 结构。刷新失败时无论是否命中都会把响应体原样打进日志
    /// （见 [`crate::store::valid_access_token_for_device`]），线上真出现一次即可据此收紧。
    pub fn is_grant_revoked(&self) -> bool {
        matches!(self.status.as_u16(), 400 | 401)
            && self.body.to_ascii_lowercase().contains("invalid_grant")
    }

    /// 写入 `ban_reason` 的原因（截断至 200 字符，与 `classify_account_rejection` 的口径一致）。
    pub fn ban_reason(&self) -> String {
        format!("[refresh {}] {}", self.status.as_u16(), self.body.trim())
            .chars()
            .take(200)
            .collect()
    }
}

/// 用 refresh_token 刷新出新的 access_token。
pub async fn refresh(client: &wreq::Client, refresh_token: &str) -> Result<TokenSet> {
    post_token(client, refresh_body(refresh_token), Some(refresh_token)).await
}

/// 刷新的请求体。键序照官方 `refreshOAuthToken`：
/// `grant_type → refresh_token → client_id → scope`。`scope` 是固定的
/// [`config::REFRESH_SCOPES`]（为什么不是登录时那组，见那里）；此前根本没发这一项。
fn refresh_body(refresh_token: &str) -> serde_json::Value {
    serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": config::CLIENT_ID,
        "scope": config::REFRESH_SCOPES,
    })
}

/// 向 token 端点 POST，并把响应转换为带过期时间戳的 `TokenSet`。
///
/// 形态照官方 `services/oauth/client.ts`：axios POST，显式头只有 `Content-Type`
/// （由 `.json()` 补上），`Accept` / 缺省 UA / `Accept-Encoding` / `Connection: close`
/// 是 axios 自己的，超时 15s。此前直接用 [`crate::clients::upstream_client`] 的缺省值发，
/// UA 是 `claude-cli/…`、`Accept-Encoding` 是 Bun 那份、没有 `Accept`——也就是把 Messages
/// API 的传输形态套在了一条官方用 axios 发的请求上。
///
/// `fallback_refresh` 是响应里缺 `refresh_token` 时沿用的旧值：刷新传旧 token，交换传
/// `None`（那时没有旧值，缺了只能报错）。
async fn post_token(
    client: &wreq::Client,
    body: serde_json::Value,
    fallback_refresh: Option<&str>,
) -> Result<TokenSet> {
    post_token_to(client, config::TOKEN_URL, body, fallback_refresh).await
}

/// [`post_token`] 的实现，端点可换——测试拿本地监听口核对线上形态。
async fn post_token_to(
    client: &wreq::Client,
    url: &str,
    body: serde_json::Value,
    fallback_refresh: Option<&str>,
) -> Result<TokenSet> {
    let resp = axios(
        client
            .post(url)
            .header("Accept", config::AXIOS_ACCEPT)
            .header("User-Agent", config::AXIOS_DEFAULT_USER_AGENT)
            .json(&body)
            .timeout(TOKEN_TIMEOUT),
        "oauth_token",
    )
    .send()
    .await
    .context("request to the token endpoint failed")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(TokenEndpointError { status, body: text }.into());
    }

    parse_token_set(&text, fallback_refresh)
}

/// 把 token 端点的 2xx 响应体解析成 [`TokenSet`]。
///
/// **绝不**把 `text` 拼进错误：走到这里说明是 2xx，报文里就是 access_token/refresh_token
/// 本身，而这条 error 会被 `valid_access_token_for_device` 打进日志、被 `/api/.../refresh`
/// 原样返回给浏览器——那等于把凭证写进日志文件和 HTTP 响应。非 2xx 的响应体不含凭证，
/// 由 `TokenEndpointError` 原样保留（见其文档）。serde 的报错自带字段名与行列，够定位了。
fn parse_token_set(text: &str, fallback_refresh: Option<&str>) -> Result<TokenSet> {
    let token: TokenResponse = serde_json::from_str(text)
        .with_context(|| format!("failed to parse the token response ({} bytes)", text.len()))?;

    // 官方：`refresh_token: newRefreshToken = refreshToken`——响应没给新的就接着用旧的。
    let refresh_token = match (token.refresh_token, fallback_refresh) {
        (Some(t), _) if !t.is_empty() => t,
        (_, Some(old)) => old.to_string(),
        (_, None) => bail!("the token response has no refresh_token"),
    };

    let organization_uuid =
        token.organization.as_ref().and_then(|o| o.uuid.clone()).filter(|s| !s.trim().is_empty());
    let account_uuid =
        token.account.as_ref().and_then(|a| a.uuid.clone()).filter(|s| !s.trim().is_empty());
    // 优先用账号邮箱作标识，取不到再用组织名。
    let account = token
        .account
        .and_then(|a| a.email_address)
        .or_else(|| token.organization.and_then(|o| o.name))
        .filter(|s| !s.trim().is_empty());

    Ok(TokenSet {
        access_token: token.access_token,
        refresh_token,
        expires_at: now_secs() + token.expires_in,
        account,
        account_uuid,
        organization_uuid,
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

/// 查询串编码，照 JS `URLSearchParams` 的 `application/x-www-form-urlencoded` 序列化：
/// 空格是 `+`，不转义的只有字母数字与 `*-._`（注意 `~` **要**转义、`*` **不**转义，
/// 与 RFC 3986 的 unreserved 集恰好各差一个）。
///
/// 官方授权 URL 是 `authUrl.searchParams.append(...)` 拼出来的（`services/oauth/client.ts`），
/// scope 里的空格上线就是 `+`；此前按 RFC 3986 编成 `%20`，语义等价但逐字不同。
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
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
    /// 组织 id：该凭证最近一次 `/v1/messages` 响应头里的 `anthropic-organization-id`
    /// 优先（见 [`crate::telemetry::Telemetry::org_uuid`]），没有就用凭证上从 profile 存的
    /// `org_uuid`；两处都没有才缺省。
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
    /// `attributes.subscriptionCreatedAt`（毫秒）：profile 的 `organization.subscription_created_at`
    /// 换算而来；凭证上还没存（旧库没刷新过）就 `None`，那一项不发。
    subscription_created_ms: Option<i64>,
}

/// `attributes.subscriptionCreatedAt`（毫秒）：凭证上存的 ISO 8601 原串换算成 Unix 毫秒
/// （官方 `new Date(str).getTime()`，抓包 `cap/2.1.260-2/00003` 里是 `1776258235239`）。
/// 解析不了的串按没有处理——发一个错的时间不如不发。
fn subscription_created_ms(cred: &crate::credentials::Credential) -> Option<i64> {
    let raw = cred.subscription_created_at.as_deref()?.trim();
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(|t| t.timestamp_millis())
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
        // 响应头学到的组织 id 优先，凭证上 profile 存的那份垫底（同一个值，前者更新鲜）。
        let organization_uuid = organization_uuid
            .filter(|o| !o.trim().is_empty())
            .or_else(|| cred.org_uuid.clone().filter(|o| !o.trim().is_empty()));
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
                subscription_created_ms: subscription_created_ms(cred),
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
            subscription_created_ms: subscription_created_ms(cred),
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
            agent_id: None,
            ..Default::default()
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
        // 响应头没学到、凭证上也没存（旧库没刷新过）时才缺省。
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
        // `subscriptionCreatedAt`（订阅创建时刻，毫秒）来自 profile 的
        // `organization.subscription_created_at`——真实客户端登录时把它存进 `~/.claude.json`，
        // eval 时换算成毫秒。`cap/2.1.260-2` 里没有 profile 响应体，之前误判为拿不到。
        // 凭证上还没存（旧库没刷新过）就不发——填个常量会让同一批号全报同一个时间。
        if let Some(ms) = self.subscription_created_ms {
            attrs.insert("subscriptionCreatedAt".into(), ms.into());
        }
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
        Ok(r) => KeepaliveResult::from_response(r, "event_logging").await,
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
    if status == 401 || status == 403 {
        // 拒绝体只为存证，受限读；摘要只记 200 的体（见 remember_etag），这里不用算。
        return KeepaliveResult::from_response(r, endpoint).await;
    }
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
        Ok(r) => KeepaliveResult::from_response(r, "bootstrap").await,
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
        Ok(r) => KeepaliveResult::from_response(r, "penguin_mode").await,
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
        // 成功时读体：里面的服务端开关 `tengu_lantern_spool` 决定模拟请求带不带
        // `anthropic-usage-limit`（[`crate::proxy::note_eval_features`]）。体按 `Content-Encoding`
        // 由客户端解压（官方回的是 br）；读不出来就当没拉到，那个头照旧不写。
        Ok(r) if r.status().is_success() => {
            let status = r.status().as_u16();
            if let Ok(body) = r.bytes().await {
                crate::proxy::note_eval_features(ctx.cred_id, &body);
            }
            KeepaliveResult::from_status(status)
        }
        Ok(r) => KeepaliveResult::from_response(r, "eval").await,
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
        let status = r.status().as_u16();
        if status == 401 || status == 403 {
            return KeepaliveResult::from_response(r, "mcp_registry").await;
        }
        last = KeepaliveResult::from_status(status);
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

/// `GET /v1/mcp_servers?limit=1000&include_additional_installs=true`（claude.ai 侧配置的 MCP
/// 连接器）；`cap/2.1.260-1/00024`。2.1.277 起查询串多了 `include_additional_installs=true`
/// （`cap/2.1.277`、`2.1.280`、`2.1.285` 的 `00021` 等都是），其余头逐字未变。
async fn handshake_mcp_servers(client: &wreq::Client, access_token: &str) -> KeepaliveResult {
    let url = format!(
        "{}/v1/mcp_servers?limit=1000&include_additional_installs=true",
        config::UPSTREAM_BASE_URL
    );
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
        Ok(r) => KeepaliveResult::from_response(r, "mcp_servers").await,
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
        Ok(r) => KeepaliveResult::from_response(r, "code_triggers").await,
        Err(_) => KeepaliveResult::Failed,
    }
}

/// `downloads.claude.ai` 上的版本与插件市场元数据（无鉴权，axios UA）。
async fn handshake_download(client: &wreq::Client, path: &str) -> KeepaliveResult {
    let resp = download_request(client, path).send().await;
    match resp {
        Ok(r) => KeepaliveResult::from_response(r, "download").await,
        Err(_) => KeepaliveResult::Failed,
    }
}

/// `downloads.claude.ai` 请求的超时。这两条无鉴权、与账号无关，卡住了不该拖住保活循环
/// （版本检查排在每张凭证刷新 token 之前）；10s 与 profile 那条同档。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10);

fn download_request(client: &wreq::Client, path: &str) -> wreq::RequestBuilder {
    axios(
        client
            .get(format!("https://downloads.claude.ai{path}"))
            .header("Accept", config::AXIOS_ACCEPT)
            .header("User-Agent", config::DATADOG_USER_AGENT)
            .timeout(DOWNLOAD_TIMEOUT),
        "download",
    )
}

// ---------- 官方最新发布版 ----------

/// 三段版本号 `(主, 次, 修)`。
pub type ReleaseVersion = (u64, u64, u64);

/// 学到新版本时的落库钩子，见 [`ReleaseCache::install_persister`]。返回 `Err` 表示这次没写成，
/// 缓存会记着「欠一次落库」，下次拉完 `latest` 再补（[`ReleaseCache::persist_pending`]）。
type ReleasePersister = Box<
    dyn Fn(ReleaseVersion) -> futures_util::future::BoxFuture<'static, anyhow::Result<()>>
        + Send
        + Sync,
>;

/// 官方最新 Claude Code 发布版的进程内缓存，与 settings 里的 `latest_cc_release` 互为镜像。
///
/// 来源两路：
/// - **网上学**：`downloads.claude.ai/claude-code-releases/latest` 的响应体是一行裸版本串
///   （`cap/2.1.260-2/00060`：`2.1.260`，`text/plain`）。保活每 30min 拉一次，模拟会话的
///   握手也拉。学到的**只升不降**（[`Self::learn`]）：那个端点回了个更旧的数（CDN 缓存、
///   回滚）不能把已知的上限往下拽，网页上手动填的更新版本也不该被它盖掉。
/// - **库里来**：启动、导入设置、网页手动改/删之后，以库为准整体重置（[`Self::sync_from_store`]）。
///
/// 用处：来访 UA 自报的 `claude-cli/<版本>` **不能高于这个数**——一个自称 2.5.0 的客户端，
/// 在官方只发到 2.1.260 的时候不是官方客户端。见 [`crate::proxy`] 对 `from_cc_client` 的判定。
///
/// 落库的两个坑都在这儿堵：
/// - 写库失败不丢：`current != persisted` 就是欠着，每次拉完 `latest` 都补一次；
/// - 并发学到不同的值不会「后写的旧值盖掉新值」：落库串行化，写的永远是**拿到锁那一刻的
///   缓存值**，而缓存只升不降，所以库里最终就是缓存里的。
pub struct ReleaseCache {
    state: parking_lot::Mutex<ReleaseState>,
    /// 串行化落库，写库期间一直拿着（跨 await，故是 tokio 的锁）。它不是 `state` 那把锁，每条
    /// 请求读上限走的是 `state`，不会被写库卡住。
    persist_lock: tokio::sync::Mutex<()>,
    persister: std::sync::OnceLock<ReleasePersister>,
}

#[derive(Default)]
struct ReleaseState {
    /// 当前认的最新版；`None` = 还不知道（调用方退回写死的
    /// [`config::CC_LATEST_KNOWN_RELEASE`]）。
    current: Option<ReleaseVersion>,
    /// 库里已确认写成的值。与 `current` 不同就是还欠一次落库。
    persisted: Option<ReleaseVersion>,
}

/// 进程级那一份。测试用 [`ReleaseCache::new`] 起自己的，别碰这个。
pub static LATEST_RELEASE: ReleaseCache = ReleaseCache::new();

impl Default for ReleaseCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ReleaseCache {
    pub const fn new() -> Self {
        Self {
            state: parking_lot::Mutex::new(ReleaseState { current: None, persisted: None }),
            persist_lock: tokio::sync::Mutex::const_new(()),
            persister: std::sync::OnceLock::new(),
        }
    }

    /// 当前认的官方最新版；还不知道时 `None`。
    pub fn get(&self) -> Option<ReleaseVersion> {
        self.state.lock().current
    }

    /// 装上落库钩子。只装一次，之后的调用忽略。
    pub fn install_persister(
        &self,
        f: impl Fn(ReleaseVersion) -> futures_util::future::BoxFuture<'static, anyhow::Result<()>>
        + Send
        + Sync
        + 'static,
    ) {
        let _ = self.persister.set(Box::new(f));
    }

    /// 以库里的值为准整体重置：启动、导入设置、网页手动改/删之后各调一次。
    ///
    /// **写库必须在 `write` 里做**，它跑在落库串行锁内：后台 [`Self::persist_pending`] 若正拿着锁
    /// 往库里写一个旧值，`write` 会等它写完再写，管理员的值最后落地；反过来管理员先写，后台随后
    /// 拿到锁时核对缓存已经变了，不会再把旧值盖上去。先在锁外写库再进来同步缓存的话，后台那笔
    /// 迟到的写入会把库里刚改的值覆盖回去——进程里认新值、库里是旧值，重启才暴露。
    ///
    /// `write` 返回**库里现在的值**（写成什么就返回什么；只读的场景直接读出来返回）。库是权威：
    /// 这一步之后 `current == persisted == v`，不欠落库。会**降**也会清空：导入了一份旧设置、或
    /// 网页上把值删了，缓存就该跟着回去。`write` 失败则缓存不动，错误原样交回。
    pub async fn sync_from_store<E>(
        &self,
        write: impl std::future::Future<Output = Result<Option<ReleaseVersion>, E>>,
    ) -> Result<(), E> {
        let _serial = self.persist_lock.lock().await;
        let v = write.await?;
        let prev = {
            let mut st = self.state.lock();
            let prev = st.current;
            st.current = v;
            st.persisted = v;
            prev
        };
        if prev != v {
            tracing::info!(
                from = %prev.map(release_string).unwrap_or_else(|| "-".into()),
                to = %v.map(release_string).unwrap_or_else(|| "-".into()),
                "latest Claude Code release synced from settings"
            );
        }
        if let Some(v) = v {
            note_profile_lag(v);
        }
        Ok(())
    }

    /// 从网上学到一个版本。**只升不降**：低于或等于当前值的忽略。返回是否更新了缓存。
    /// 无论更新与否都顺手补一次欠着的落库。
    pub async fn learn(&self, v: ReleaseVersion) -> bool {
        let changed = {
            let mut st = self.state.lock();
            match st.current {
                Some(cur) if cur >= v => {
                    if cur > v {
                        tracing::debug!(
                            known = %release_string(cur),
                            fetched = %release_string(v),
                            "releases/latest returned an older version than the one already known; ignoring"
                        );
                    }
                    false
                }
                prev => {
                    tracing::info!(
                        from = %prev.map(release_string).unwrap_or_else(|| "-".into()),
                        to = %release_string(v),
                        "learned the latest official Claude Code release"
                    );
                    st.current = Some(v);
                    true
                }
            }
        };
        if changed {
            note_profile_lag(v);
        }
        self.persist_pending().await;
        changed
    }

    /// 把欠着的落库补上。每次拉完 `latest`（成功与否）都调一次，上次写失败的这次补回来。
    ///
    /// 写的是**拿到串行锁那一刻的缓存值**，不是调用方手里的那个：两路并发各学到一个版本时，
    /// 缓存只升不降，后写的那个写的一定是较新的。写成之后再核一遍缓存没变才标 `persisted`。
    /// 管理员改/删（[`Self::sync_from_store`]）也在这把锁里写库，所以不会有一笔迟到的后台写入
    /// 盖掉管理员刚落的值。
    pub async fn persist_pending(&self) {
        let Some(persist) = self.persister.get() else { return };
        let _serial = self.persist_lock.lock().await;
        let target = {
            let st = self.state.lock();
            if st.current == st.persisted {
                return;
            }
            st.current
        };
        // `current` 为 `None` 只会来自 `sync_from_store(None)`，那一步已经把 `persisted` 一并
        // 清了，不会走到这儿；防御性地不写「空」。
        let Some(v) = target else { return };
        match persist(v).await {
            Ok(()) => {
                let mut st = self.state.lock();
                if st.current == Some(v) {
                    st.persisted = Some(v);
                }
            }
            Err(e) => tracing::warn!(
                version = %release_string(v),
                error = %e,
                "failed to persist the latest Claude Code release; will retry after the next check"
            ),
        }
    }

    /// 测试用：还欠不欠落库。
    #[cfg(test)]
    fn has_pending(&self) -> bool {
        let st = self.state.lock();
        st.current != st.persisted
    }
}

/// 已学到的官方最新发布版（进程级缓存）；还不知道时 `None`。
pub fn latest_release() -> Option<ReleaseVersion> {
    LATEST_RELEASE.get()
}

/// `主.次.修` 串，settings 里存的和日志里打的都是这个形态。
pub fn release_string((a, b, c): ReleaseVersion) -> String {
    format!("{a}.{b}.{c}")
}

/// 解析 `latest` 端点的响应体（settings 里存的也是同一形态）：一行 `主.次.修`，三段都得是数字。
///
/// 比 [`crate::proxy::parse_version`] 严：那边是给来访 UA 用的宽松解析，这里读的是官方
/// 发布清单，形态不对（HTML 错误页、空体、多了后缀）就当没拿到——把一个错的上限记进去，
/// 会让所有真实客户端都被判成冒充。
pub fn parse_release_body(body: &str) -> Option<ReleaseVersion> {
    let mut it = body.trim().split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// 官方最新版跑到了 luban 模拟基线（[`config::CC_VERSION_BASE`]）前面时提醒一句。
///
/// 上限记住了新版本只解决「别把真实新版客户端判成冒充」；模拟路径发出去的仍是基线那一版的
/// 形态（UA、billing 后缀、build_time、beta 集合），要对齐新版得重新抓包换 profile。这是
/// 代码层面的事，日志里点一下，别让人以为学到版本号就万事大吉。
fn note_profile_lag(v: ReleaseVersion) {
    if let Some(base) = parse_release_body(config::CC_VERSION_BASE)
        && v > base
    {
        tracing::warn!(
            latest = %release_string(v),
            simulated = config::CC_VERSION_BASE,
            "official Claude Code is ahead of luban's simulated profile; simulated traffic still reports the older version until the profile is re-captured"
        );
    }
}

/// 拉 `/claude-code-releases/latest` 并把版本号交给 [`LATEST_RELEASE`]。
///
/// 请求形态与 [`handshake_download`] 完全一样（同一条抓包），只多了读体。2xx 但体解析不出
/// 版本时缓存不动、结果按 `Ok` 报——请求本身没失败，是体不认识，日志里单独记一行。
/// 不论结果如何都补一次欠着的落库。
pub async fn fetch_latest_release(client: &wreq::Client) -> KeepaliveResult {
    let result = fetch_latest_release_inner(client).await;
    LATEST_RELEASE.persist_pending().await;
    result
}

async fn fetch_latest_release_inner(client: &wreq::Client) -> KeepaliveResult {
    let resp = match download_request(client, "/claude-code-releases/latest").send().await {
        Ok(r) => r,
        Err(_) => return KeepaliveResult::Failed,
    };
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        return KeepaliveResult::from_response(resp, "releases_latest").await;
    }
    let result = KeepaliveResult::from_status(status);
    if (200..300).contains(&status) {
        match resp.text().await {
            Ok(body) => match parse_release_body(&body) {
                Some(v) => {
                    LATEST_RELEASE.learn(v).await;
                }
                None => tracing::warn!(
                    body = %body.chars().take(80).collect::<String>(),
                    "releases/latest: body is not a version number, keeping the cached one"
                ),
            },
            Err(e) => {
                tracing::debug!(error = %e, "releases/latest: failed to read body");
            }
        }
    }
    result
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
            fetch_latest_release(client).await
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
mod tests;
