//! 控制台登录鉴权：用户名 + 密码登录，换一个会话 token，之后每个请求带
//! `Authorization: Bearer <token>`。转发代理 `/v1/*` 不走这里（那边认接入 Key）。
//!
//! 四种角色见 [`UserRole`]：唯一的 admin、唯一的只读访客、代理、用户。密码用 argon2id 存在
//! `users.password_hash`；旧版存在 settings 里的无盐 sha256 迁移时带上前缀搬过来，登录校验
//! 通过即换成 argon2（见 [`store::migrate_users`]）。admin 与访客的密码仍可由
//! `LUBAN_ADMIN_PASSWORD` / `LUBAN_VIEWER_PASSWORD` 接管，接管时库里的哈希不起作用。
//!
//! **未设管理密码时管理接口一律拒绝**，本机也不例外。默认监听 `0.0.0.0`、Docker 也把端口发布
//! 到所有网卡，未设密码等于管理面对网络匿名敞开。设密码得带启动日志里打印的初始化口令
//! （[`AppState::setup_token`]），证明来人能看到这台服务的日志。不给本机开免密口子：服务端分不出
//! 「本机浏览器」和「同机反代转进来的外部请求」。
//!
//! **权限按路由默认拒绝**：中间件给每条路由定一个访问级别（[`access_of`]），没列出来的一律只给
//! admin。往后新加的接口忘了标，代理和用户只是用不了，不会越权。代理和用户能打的接口里，
//! 落在某个号 / 某条出口代理上的（路径里的 id、或批量接口 body 里的 `ids`），中间件先核对
//! 是不是本人名下的，不是一律回 404（不回 403，免得试出别人的号是否存在）。列表类接口由
//! handler 按 [`Actor::scope`] 过滤。
//!
//! **只读访客**只放行 `GET`/`HEAD`，再去掉几条读也不该给的（见 [`VIEWER_DENIED`]），响应体里
//! 的代理密码统一打码（[`redact_url_credentials`]）。

use axum::{
    Extension, Json,
    extract::{ConnectInfo, MatchedPath, Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::{self, LEGACY_SHA256_PREFIX, Scope, User, UserRole};
use crate::web::AppState;

type ApiError = (StatusCode, String);

/// sha256 十六进制。
pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

/// 密码最短长度（去掉首尾空白后）。
const MIN_PASSWORD_LEN: usize = 4;

/// 当前登录的人。中间件鉴权通过后放进请求扩展，handler 用 `Extension<Actor>` 取。
#[derive(Debug, Clone)]
pub struct Actor {
    pub id: i64,
    pub username: String,
    pub role: UserRole,
}

impl Actor {
    /// 数据可见范围：admin 与访客看全部，代理和用户只看自己名下的。
    pub fn scope(&self) -> Scope {
        match self.role {
            UserRole::Admin | UserRole::Viewer => Scope::All,
            UserRole::Agent | UserRole::User => Scope::Owner(self.id),
        }
    }

    pub fn is_admin(&self) -> bool {
        self.role == UserRole::Admin
    }

    pub fn is_viewer(&self) -> bool {
        self.role == UserRole::Viewer
    }
}

impl From<User> for Actor {
    fn from(u: User) -> Self {
        Actor { id: u.id, username: u.username, role: u.role }
    }
}

// ---------- 密码 ----------

/// argon2id 哈希（PHC 串，盐随机）。几十毫秒的 CPU 活，调用方放在 `spawn_blocking` 里。
fn hash_password_blocking(pw: &str) -> anyhow::Result<String> {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    let mut salt = [0u8; 16];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|e| anyhow::anyhow!("{e}"))?;
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// 校验密码与存着的哈希：argon2 PHC 串，或带 [`LEGACY_SHA256_PREFIX`] 的旧版无盐 sha256。
fn verify_hash_blocking(stored: &str, pw: &str) -> bool {
    if let Some(hex) = stored.strip_prefix(LEGACY_SHA256_PREFIX) {
        return constant_time_eq(sha256_hex(pw).as_bytes(), hex.as_bytes());
    }
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    PasswordHash::new(stored)
        .is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

/// 同时在跑的 argon2 计算（登录校验、算新密码哈希）最多几个。每个占 ~19MB 内存、几十毫秒
/// CPU，不设上限的话一波并发登录就能把阻塞线程池和内存一起打满；超出的排队等。
///
/// **许可要移进阻塞闭包里**、算完才放：客户端断开时异步的那一半会被取消，留在 async 侧的
/// 许可随之归还，而 `spawn_blocking` 里的计算照跑不误——断连重试几轮，在跑的计算就远超上限。
static ARGON2_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(crate) async fn hash_password(pw: &str) -> Result<String, ApiError> {
    let pw = pw.to_owned();
    let permit = ARGON2_PERMITS.acquire().await.map_err(internal)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        hash_password_blocking(&pw)
    })
    .await
    .map_err(internal)?
    .map_err(internal)
}

/// 新密码的基本校验：去掉首尾空白后不短于 [`MIN_PASSWORD_LEN`]。回去掉空白后的密码。
pub(crate) fn check_new_password(pw: &str) -> Result<&str, ApiError> {
    let pw = pw.trim();
    if pw.chars().count() < MIN_PASSWORD_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("password must be at least {MIN_PASSWORD_LEN} characters"),
        ));
    }
    Ok(pw)
}

/// 环境接管的那份密码（admin / 访客各一份），没接管为 None。
fn env_password(state: &AppState, role: UserRole) -> Option<&str> {
    match role {
        UserRole::Admin => state.admin_env.as_deref().map(|s| s.trim()),
        UserRole::Viewer => state.viewer_env.as_deref().map(|s| s.trim()),
        UserRole::Agent | UserRole::User => None,
    }
}

/// 会话的密码指纹：签发时记下，每次认会话都与当前算出的比对，对不上即失效。
///
/// 环境变量接管的 admin / 访客取环境密码的**版本号**（见 [`sync_env_accounts`]），其余取库里
/// 存的哈希——于是改密码、重置密码、换掉或撤掉 `LUBAN_ADMIN_PASSWORD` /
/// `LUBAN_VIEWER_PASSWORD`，旧会话都会随之作废，包括改密码前已经在路上、按旧密码校验通过的
/// 那次登录。
///
/// 环境密码不拿它的哈希当指纹：指纹随会话落库，无盐 sha256 一旦库泄露就能飞快地离线猜出来。
/// 库里存的 argon2 哈希本身是加盐慢哈希，对它取 sha256 再存没有这个问题。
pub(crate) fn password_tag(state: &AppState, role: UserRole, stored_hash: &str) -> String {
    match env_password(state, role) {
        Some(_) => {
            let key = match role {
                UserRole::Viewer => store::VIEWER_ENV_PASSWORD_VERSION,
                _ => store::ADMIN_ENV_PASSWORD_VERSION,
            };
            let version = state.store.get_setting(key).ok().flatten().unwrap_or_default();
            format!("env:v{version}")
        }
        None if stored_hash.is_empty() => String::new(),
        None => format!("db:{}", sha256_hex(stored_hash)),
    }
}

/// 密码校验通过后签发会话要用的：指纹，以及签发时须核对的库内哈希（环境接管时为 None）。
struct Verified {
    tag: String,
    expected_hash: Option<String>,
}

/// 校验某个账号的密码，通过回 [`Verified`]。环境接管的 admin / 访客比环境里那份；其余比库里
/// 的哈希，旧版哈希校验通过后换成 argon2 存回去（指纹按换过的那份算）。
async fn verify_user_password(
    state: &AppState,
    user: &User,
    pw: &str,
) -> Result<Option<Verified>, ApiError> {
    if let Some(env) = env_password(state, user.role) {
        return Ok(constant_time_eq(env.as_bytes(), pw.as_bytes())
            .then(|| Verified { tag: password_tag(state, user.role, ""), expected_hash: None }));
    }
    let stored = state.store.user_password_hash(user.id).map_err(internal)?.unwrap_or_default();
    if stored.is_empty() {
        return Ok(None);
    }
    let ok = {
        let (stored, pw) = (stored.clone(), pw.to_owned());
        let permit = ARGON2_PERMITS.acquire().await.map_err(internal)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            verify_hash_blocking(&stored, &pw)
        })
        .await
        .map_err(internal)?
    };
    if !ok {
        return Ok(None);
    }
    let mut current = stored.clone();
    if stored.starts_with(LEGACY_SHA256_PREFIX) {
        let upgraded = hash_password(pw).await?;
        // 只在库里还是这份旧哈希时才换：与并发的改密码交错时不把新密码盖回旧的。
        match state.store.set_user_password_hash(user.id, &upgraded, Some(&stored)) {
            Ok(true) => current = upgraded,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(user_id = user.id, error = %e, "failed to upgrade a legacy password hash")
            }
        }
    }
    Ok(Some(Verified {
        tag: password_tag(state, user.role, &current),
        expected_hash: Some(current),
    }))
}

/// 是否已设管理密码（环境接管或库里存了哈希）。
///
/// 给那些「开着鉴权才允许」的接口用。未设密码时 [`require_login`] 已经一律拒绝，这层是
/// 兜底：个别接口给出去的东西比「能改配置」更重（如导出含明文 token 的迁移文件），不把
/// 安全全押在中间件的装配上，它们自己再确认一次这道门锁着。
pub fn admin_configured(state: &AppState) -> bool {
    state.admin_env.is_some() || state.store.admin_user().is_ok_and(|u| u.password_set)
}

/// 访客能不能登录：环境接管了访客密码，或库里有设过密码的访客行。
fn viewer_enabled(state: &AppState) -> bool {
    state.viewer_env.is_some()
        || state.store.viewer_user().ok().flatten().is_some_and(|u| u.password_set)
}

/// 启动时对齐环境接管的账号：
/// - `LUBAN_VIEWER_PASSWORD` 设了而库里还没有访客行时补一行（密码哈希留空，校验走环境那份），
///   会话才有地方挂；
/// - 两个环境密码各与上次启动时记下的 argon2 哈希比对，换了、撤了或新设了，版本号加一
///   （[`password_tag`] 只记版本号），按旧密码签的会话随即失效。
pub(crate) fn sync_env_accounts(state: &AppState) {
    if state.viewer_env.is_some()
        && matches!(state.store.viewer_user(), Ok(None))
        && let Err(e) = state.store.upsert_viewer("")
    {
        tracing::warn!(error = %format!("{e:#}"), "failed to create the viewer account for LUBAN_VIEWER_PASSWORD");
    }
    for (role, hash_key, version_key) in [
        (UserRole::Admin, store::ADMIN_ENV_PASSWORD_HASH, store::ADMIN_ENV_PASSWORD_VERSION),
        (UserRole::Viewer, store::VIEWER_ENV_PASSWORD_HASH, store::VIEWER_ENV_PASSWORD_VERSION),
    ] {
        if let Err(e) = sync_env_password(state, env_password(state, role), hash_key, version_key) {
            tracing::warn!(error = %format!("{e:#}"), ?role, "failed to record the environment password version");
        }
    }
}

/// 比对一个环境密码与上次记下的哈希，变了就把版本号加一、换上新哈希（撤了就删掉哈希）。
fn sync_env_password(
    state: &AppState,
    env: Option<&str>,
    hash_key: &str,
    version_key: &str,
) -> anyhow::Result<()> {
    let stored = state.store.get_setting(hash_key)?.filter(|h| !h.is_empty());
    let unchanged = match (env, stored.as_deref()) {
        (None, None) => true,
        (Some(pw), Some(h)) => verify_hash_blocking(h, pw),
        _ => false,
    };
    if unchanged {
        return Ok(());
    }
    let version: u64 =
        state.store.get_setting(version_key)?.and_then(|v| v.parse().ok()).unwrap_or(0);
    state.store.set_setting(version_key, &(version + 1).to_string())?;
    match env {
        Some(pw) => state.store.set_setting(hash_key, &hash_password_blocking(pw)?)?,
        None => state.store.delete_setting(hash_key)?,
    }
    Ok(())
}

// ---------- 会话 ----------

fn bearer(headers: &header::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// 签发一个会话 token（32 字节随机数的十六进制），库里只存它的 sha256 与密码指纹。
/// 库里的密码在校验之后被改掉了（`expected_hash` 对不上）回 None，不签。
fn issue_session(
    state: &AppState,
    user_id: i64,
    verified: &Verified,
) -> Result<Option<String>, ApiError> {
    let mut bytes = [0u8; 32];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let created = state
        .store
        .create_session(
            &sha256_hex(&token),
            user_id,
            &verified.tag,
            verified.expected_hash.as_deref(),
        )
        .map_err(internal)?;
    Ok(created.then_some(token))
}

/// 登录尝试计数：同一用户名 [`LOGIN_FAIL_WINDOW`] 内最多尝试 [`LOGIN_FAIL_MAX`] 次，超出的一律
/// 429、不再校验密码；登录成功清零。按用户名不按来源 IP：IP 取自可伪造的转发头，拿它限流
/// 换个头就绕过了。代价是有人故意把某个用户名试锁，那个人要等窗口过去才能登录。
///
/// **校验之前预占**（[`reserve_login_attempt`]），而不是失败之后再记：后者挡不住并发——同时
/// 涌进来的一批请求都还没失败，全都会进 argon2。预占之后第 11 个并发请求当场就被拒。
///
/// **两张表**：存在的账号一张，条数不会超过账号数，从不淘汰——锁定记录被挤掉就等于提前解锁；
/// 不存在的用户名也得计数（否则按「锁不锁」就能试出谁存在），但用户名是来访随便填的，这张
/// 设容量上限 [`LOGIN_UNKNOWN_CAPACITY`]，满了只淘汰没锁住的；全是锁住的就对新来的不存在
/// 用户名直接拒（它们本来就登不进来）。超长的用户名在入口就拒，不计。
struct LoginAttempts {
    count: u32,
    since: std::time::Instant,
}

#[derive(Default)]
struct LoginGuard {
    known: std::collections::HashMap<String, LoginAttempts>,
    unknown: std::collections::HashMap<String, LoginAttempts>,
}

static LOGIN_GUARD: std::sync::LazyLock<parking_lot::Mutex<LoginGuard>> =
    std::sync::LazyLock::new(Default::default);
const LOGIN_FAIL_MAX: u32 = 10;
const LOGIN_FAIL_WINDOW: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const LOGIN_UNKNOWN_CAPACITY: usize = 1024;
/// 登录时接受的用户名最长多少字节。建账号时上限是 32 个字符，留够中文等多字节字符的余量。
const MAX_LOGIN_USERNAME_LEN: usize = 128;

/// 预占一次登录尝试：还有名额回 true（并记上这一次），已锁回 false。`known` 为这个用户名
/// 是否确有账号。
fn reserve_login_attempt(key: &str, known: bool) -> bool {
    let mut guard = LOGIN_GUARD.lock();
    let now = std::time::Instant::now();
    let live = |a: &LoginAttempts| now.duration_since(a.since) < LOGIN_FAIL_WINDOW;
    guard.known.retain(|_, a| live(a));
    guard.unknown.retain(|_, a| live(a));
    let table = if known { &mut guard.known } else { &mut guard.unknown };
    if let Some(a) = table.get_mut(key) {
        if a.count >= LOGIN_FAIL_MAX {
            return false;
        }
        a.count += 1;
        return true;
    }
    if !known && table.len() >= LOGIN_UNKNOWN_CAPACITY {
        let victim = table
            .iter()
            .filter(|(_, a)| a.count < LOGIN_FAIL_MAX)
            .min_by_key(|(_, a)| a.since)
            .map(|(k, _)| k.clone());
        match victim {
            Some(k) => {
                table.remove(&k);
            }
            None => return false,
        }
    }
    table.insert(key.to_owned(), LoginAttempts { count: 1, since: now });
    true
}

/// 登录成功：清掉这个用户名的尝试计数。
fn clear_login_attempts(key: &str) {
    LOGIN_GUARD.lock().known.remove(key);
}

// ---------- 中间件 ----------

/// 一条路由要求的身份（不含访客，访客另按方法判，见 [`viewer_allowed`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    /// 只有 admin。没列出来的路由都是这一档。
    Admin,
    /// admin 与代理（用户管理），handler 里再按范围收窄。
    Manager,
    /// 登录的人都能打，按 [`Owned`] 核对落在谁的东西上。
    Member(Owned),
}

/// 代理和用户打 [`Access::Member`] 路由时，中间件要先核对归属的东西。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owned {
    /// 不落在某个具体的号或代理上，由 handler 按范围过滤（列表、新增、登录相关）。
    Nothing,
    /// 路径里的号 id（`/credentials/{id}/…`）。
    CredentialPath,
    /// 路径里的出口代理 id（`/proxies/{id}`）。
    ProxyPath,
    /// body 里的 `ids`：一批号。
    CredentialIds,
    /// body 里的 `ids`：一批出口代理。
    ProxyIds,
    /// 路径里的封号事件 id（`/ban-events/{id}/…`）：看它落在哪个号上。号已删掉的事件
    /// 无从判断归属，只给 admin / 访客。
    BanEventPath,
}

/// 代理和用户能打的路由（路径不带 `/api` 前缀，与 `MatchedPath` 对应）。
/// `/credentials/{id}` 开头的另由 [`access_of`] 统一放行并按路径 id 核对。
const MEMBER_ROUTES: &[(&str, Owned)] = &[
    ("/auth/me", Owned::Nothing),
    ("/auth/password", Owned::Nothing),
    ("/auth/logout", Owned::Nothing),
    ("/authorize", Owned::Nothing),
    ("/exchange", Owned::Nothing),
    ("/models", Owned::Nothing),
    ("/credentials", Owned::Nothing),
    ("/credentials/priority", Owned::CredentialIds),
    ("/credentials/device-limit", Owned::CredentialIds),
    ("/credentials/session-limit", Owned::CredentialIds),
    ("/credentials/rpm-limit", Owned::CredentialIds),
    ("/credentials/quota-pause-pct", Owned::CredentialIds),
    ("/credentials/disabled", Owned::CredentialIds),
    ("/credentials/delete", Owned::CredentialIds),
    ("/credentials/proxy", Owned::CredentialIds),
    ("/credentials/groups", Owned::CredentialIds),
    // 分组列表：handler 按身份收窄；增删改在 handler 里再要求 admin。
    ("/groups", Owned::Nothing),
    ("/proxies", Owned::Nothing),
    ("/proxies/test", Owned::Nothing),
    ("/proxies/batch", Owned::Nothing),
    ("/proxies/delete", Owned::ProxyIds),
    ("/proxies/{id}", Owned::ProxyPath),
    // 封号记录：列表接口由 handler 要求带上本人名下的 `cred_id`。
    ("/ban-events", Owned::Nothing),
    ("/ban-events/{id}/logs", Owned::BanEventPath),
    // 请求流水：handler 把结果强制限定在本人名下的号上。
    ("/usage", Owned::Nothing),
    // 分层账单：handler 按身份收窄可见的号主与拆分维度。
    ("/billing", Owned::Nothing),
    // 上号 Key：handler 按身份收窄到本人名下的（admin 看全部）。
    ("/provision-keys", Owned::Nothing),
    ("/provision-keys/{id}", Owned::Nothing),
];

/// 路由的访问级别，默认只给 admin。
fn access_of(path: &str) -> Access {
    if let Some((_, owned)) = MEMBER_ROUTES.iter().find(|(p, _)| *p == path) {
        return Access::Member(*owned);
    }
    // 某一个号底下的所有操作都只关乎这个号，核对路径里的 id 是本人的就够了。
    if path == "/credentials/{id}" || path.starts_with("/credentials/{id}/") {
        return Access::Member(Owned::CredentialPath);
    }
    if path == "/users" || path.starts_with("/users/") {
        return Access::Manager;
    }
    Access::Admin
}

/// 访客连 `GET` 也不给的接口（路径不带 `/api` 前缀）：
/// - `/authorize`：生成 PKCE 存进内存，是「添加账号」的第一步，算写；
/// - `/export`：迁移文件带全部账号的明文 token 和接入 key，看到就等于能用；
/// - `/settings`、`/learned-rejections`：只有系统设置页用，而系统设置整页不对访客开放
///   （`/settings` 还带着明文接入 key）。`/proxies` 不在此列：账号页要靠它显示代理名称，
///   地址里的密码由 [`redact_url_credentials`] 遮掉；
/// - `/users`：访客是来看号池的，用不着控制台账号名单；
/// - `/api-keys`：带着查看明文的接口，看到就等于能用。
/// - `/provision-keys`：访客建不了 Key，列表也没必要看。
const VIEWER_DENIED: &[&str] =
    &["/authorize", "/export", "/settings", "/learned-rejections", "/users", "/api-keys", "/provision-keys"];

/// 访客能不能打这个请求：只读方法，且不在 [`VIEWER_DENIED`] 里。退出登录例外：它是
/// `POST`，但只作废访客自己的会话，不放行的话访客点「退出」只清得掉本地 token。
fn viewer_allowed(method: &Method, path: &str) -> bool {
    if path == "/auth/logout" && *method == Method::POST {
        return true;
    }
    matches!(*method, Method::GET | Method::HEAD)
        && !VIEWER_DENIED.iter().any(|d| path == *d || path.starts_with(&format!("{d}/")))
}

/// 未设密码时管理接口回的文案，前端据此切到「初始化管理密码」页。
const SETUP_REQUIRED: &str =
    "set an admin password first, using the setup token from the server log";

const LOGIN_REQUIRED: &str = "login required";

/// 请求路径里第 `index` 段（去掉 `/api` 前缀后按 `/` 切，0 是第一段）解析成 id。
fn path_id(req: &Request, index: usize) -> Option<i64> {
    let path = req.uri().path();
    let path = path.strip_prefix("/api").unwrap_or(path);
    path.trim_start_matches('/').split('/').nth(index)?.parse().ok()
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, format!("{what} not found")).into_response()
}

/// 代理和用户打 [`Access::Member`] 路由时核对归属；body 读出来了要原样装回去，所以回的是
/// 装好的请求。
// Err 就是要原样回给客户端的响应，只在中间件里走一次，没必要装箱。
#[allow(clippy::result_large_err)]
async fn check_owned(
    state: &AppState,
    actor: &Actor,
    owned: Owned,
    req: Request,
) -> Result<Request, Response> {
    let owner = actor.id;
    match owned {
        Owned::Nothing => Ok(req),
        Owned::CredentialPath => {
            let id = path_id(&req, 1).ok_or_else(|| not_found("credential"))?;
            match state.store.credential_owner(id) {
                Ok(Some(o)) if o == owner => Ok(req),
                Ok(_) => Err(not_found("credential")),
                Err(e) => Err(internal(e).into_response()),
            }
        }
        Owned::ProxyPath => {
            let id = path_id(&req, 1).ok_or_else(|| not_found("proxy"))?;
            match state.store.proxy_owner(id) {
                Ok(Some(o)) if o == owner => Ok(req),
                Ok(_) => Err(not_found("proxy")),
                Err(e) => Err(internal(e).into_response()),
            }
        }
        Owned::BanEventPath => {
            let id = path_id(&req, 1).ok_or_else(|| not_found("ban event"))?;
            let owner_of = |id| -> anyhow::Result<Option<i64>> {
                match state.store.ban_event_credential(id)? {
                    Some(cred) => state.store.credential_owner(cred),
                    None => Ok(None),
                }
            };
            match owner_of(id) {
                Ok(Some(o)) if o == owner => Ok(req),
                Ok(_) => Err(not_found("ban event")),
                Err(e) => Err(internal(e).into_response()),
            }
        }
        Owned::CredentialIds | Owned::ProxyIds => {
            let (parts, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, 1024 * 1024)
                .await
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()).into_response())?;
            #[derive(Deserialize)]
            struct Ids {
                #[serde(default)]
                ids: Vec<i64>,
            }
            // 解不出来的交给 handler 去回它自己的 400，这里只管「解得出的 id 是不是本人的」。
            let ids = serde_json::from_slice::<Ids>(&bytes).map(|b| b.ids).unwrap_or_default();
            let all_owned = if owned == Owned::CredentialIds {
                state.store.credentials_owned_by(&ids, owner)
            } else {
                state.store.proxies_owned_by(&ids, owner)
            };
            match all_owned {
                Ok(true) => Ok(Request::from_parts(parts, axum::body::Body::from(bytes))),
                Ok(false) => Err(not_found(if owned == Owned::CredentialIds {
                    "credential"
                } else {
                    "proxy"
                })),
                Err(e) => Err(internal(e).into_response()),
            }
        }
    }
}

/// 中间件：认会话、判访问级别、核对归属，把 [`Actor`] 放进请求扩展。
///
/// 未设管理密码一律 401（本机也不放行，原因见模块说明）。回 401 而不是 403：前端的拦截器
/// 见 401 就重新拉一次鉴权状态，未设密码的来访由此落到初始化页。登录了但权限不够回 403：
/// 回 401 会让前端以为登录失效、清掉会话踢回登录页。
pub async fn require_login(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if !admin_configured(&state) {
        return (StatusCode::UNAUTHORIZED, SETUP_REQUIRED).into_response();
    }
    let Some(token) = bearer(req.headers()) else {
        return (StatusCode::UNAUTHORIZED, LOGIN_REQUIRED).into_response();
    };
    if token.starts_with(store::PROVISION_KEY_PREFIX) {
        return provision_key_request(&state, token.to_owned(), req, next).await;
    }
    let token_hash = sha256_hex(token);
    let row = match state.store.session_lookup(&token_hash) {
        Ok(Some(row)) => row,
        Ok(None) => return (StatusCode::UNAUTHORIZED, LOGIN_REQUIRED).into_response(),
        Err(e) => return internal(e).into_response(),
    };
    // 密码指纹对不上（改过密码、环境变量里的密码换了或撤了）：这条会话作废。
    let current_tag = password_tag(&state, row.user.role, &row.password_hash);
    if current_tag.is_empty() || !constant_time_eq(current_tag.as_bytes(), row.pw_tag.as_bytes()) {
        let _ = state.store.delete_session(&token_hash);
        return (StatusCode::UNAUTHORIZED, LOGIN_REQUIRED).into_response();
    }
    if row.user.effectively_disabled() {
        return (StatusCode::UNAUTHORIZED, LOGIN_REQUIRED).into_response();
    }
    if let Err(e) = state.store.renew_session_if_due(&token_hash, row.remaining_secs) {
        tracing::warn!(error = %e, "failed to renew a console session");
    }
    let actor = Actor::from(row.user);
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());
    let path = path.strip_prefix("/api").unwrap_or(&path).to_owned();

    let mut req = match actor.role {
        UserRole::Admin => req,
        UserRole::Viewer => {
            if !viewer_allowed(req.method(), &path) {
                return (StatusCode::FORBIDDEN, "read-only viewer: this action is not allowed")
                    .into_response();
            }
            req
        }
        UserRole::Agent | UserRole::User => match access_of(&path) {
            Access::Admin => {
                return (StatusCode::FORBIDDEN, "this action requires the admin account")
                    .into_response();
            }
            Access::Manager if actor.role != UserRole::Agent => {
                return (StatusCode::FORBIDDEN, "this action requires an agent or the admin")
                    .into_response();
            }
            Access::Manager => req,
            Access::Member(owned) => match check_owned(&state, &actor, owned, req).await {
                Ok(req) => req,
                Err(resp) => return resp,
            },
        },
    };
    let viewer = actor.is_viewer();
    req.extensions_mut().insert(actor);
    let resp = next.run(req).await;
    if viewer { redact_for_viewer(resp).await } else { resp }
}

/// 请求是拿上号 Key 来的（放进请求扩展）。上号时没指定代理就自动从代理池分配，见
/// `web::login::exchange`。
#[derive(Debug, Clone, Copy)]
pub struct ViaProvisionKey;

/// 上号 Key 能打的接口（路径不带 `/api` 前缀）：取授权链接、交授权码、列出能选的分组与
/// 本人的代理池（按 id 选代理用）。其余一律 403——Key 是放在脚本里的，泄露了也只能往这个人
/// 名下添号。回给 Key 的响应里代理密码一律打码。
const PROVISION_ROUTES: &[(Method, &str)] = &[
    (Method::GET, "/authorize"),
    (Method::POST, "/exchange"),
    (Method::GET, "/groups"),
    (Method::GET, "/proxies"),
];

/// 上号 Key 建时记下的密码指纹是否仍与所属账号此刻的一致（与会话同一套判据）。
pub(crate) fn provision_key_current(
    state: &AppState,
    role: UserRole,
    stored_hash: &str,
    key_tag: &str,
) -> bool {
    let current = password_tag(state, role, stored_hash);
    !current.is_empty() && constant_time_eq(current.as_bytes(), key_tag.as_bytes())
}

/// 带上号 Key 的请求：认出所属账号，只放行 [`PROVISION_ROUTES`]，以这个人的身份进 handler。
/// 访客名下不该有 Key（建不出来），万一有也不认。
async fn provision_key_request(
    state: &AppState,
    token: String,
    mut req: Request,
    next: Next,
) -> Response {
    let invalid = || (StatusCode::UNAUTHORIZED, "invalid provision key").into_response();
    let hit = match state.store.provision_key_lookup(&token) {
        Ok(Some(hit)) => hit,
        Ok(None) => return invalid(),
        Err(e) => return internal(e).into_response(),
    };
    // 密码指纹对不上（改过密码、被重置、环境变量里的密码换了或撤了）：这把 Key 作废、删掉。
    if !provision_key_current(state, hit.user.role, &hit.password_hash, &hit.pw_tag) {
        if let Err(e) = state.store.delete_provision_key(hit.id, None) {
            tracing::warn!(error = %e, key_id = hit.id, "failed to delete a revoked provision key");
        }
        return invalid();
    }
    let user = hit.user;
    if user.effectively_disabled() || user.role == UserRole::Viewer {
        return invalid();
    }
    if let Err(e) = state.store.touch_provision_key(hit.id) {
        tracing::warn!(error = %e, "failed to record provision key use");
    }
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());
    let path = path.strip_prefix("/api").unwrap_or(&path);
    if !PROVISION_ROUTES.iter().any(|(m, p)| m == req.method() && *p == path) {
        return (StatusCode::FORBIDDEN, "a provision key can only add accounts").into_response();
    }
    req.extensions_mut().insert(Actor::from(user));
    req.extensions_mut().insert(ViaProvisionKey);
    redact_for_viewer(next.run(req).await).await
}

// ---------- 接口 ----------

#[derive(Serialize)]
pub struct StateResp {
    /// 是否已设置管理密码（true = 需登录）。
    configured: bool,
    /// 管理密码是否由环境变量接管（true = 网页不可改）。
    env_managed: bool,
    /// 未设密码：要先用初始化口令设密码才能进控制台。恒等于 `!configured`，单列一个字段
    /// 是让前端不必自己推这层语义。
    setup_required: bool,
    /// 能否用访客账号登录。只透露「开没开」，不涉及密码本身。
    viewer_enabled: bool,
}

/// 鉴权状态（公开）。
pub async fn state(State(state): State<AppState>) -> Json<StateResp> {
    let configured = admin_configured(&state);
    Json(StateResp {
        configured,
        env_managed: state.admin_env.is_some(),
        setup_required: !configured,
        viewer_enabled: configured && viewer_enabled(&state),
    })
}

#[derive(Deserialize)]
pub struct LoginReq {
    username: String,
    password: String,
}

#[derive(Serialize)]
pub struct LoginResp {
    ok: bool,
    token: String,
    role: UserRole,
    username: String,
}

/// 用户名 + 密码登录（公开），成功回会话 token。
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<LoginReq>,
) -> Result<Json<LoginResp>, ApiError> {
    // 这是全项目唯一能看出「有人在猜密码」的地方，不带来源等于记了个寂寞。
    let ip = client_ip(&headers, peer);
    if !admin_configured(&state) {
        return Err((StatusCode::BAD_REQUEST, "no admin password has been set yet".into()));
    }
    let username = req.username.trim();
    if username.len() > MAX_LOGIN_USERNAME_LEN {
        return Err((StatusCode::UNAUTHORIZED, "wrong username or password".into()));
    }
    let key = username.to_lowercase();
    let user = state.store.user_by_username(username).map_err(internal)?;
    if !reserve_login_attempt(&key, user.is_some()) {
        tracing::warn!(%ip, username, "console login rejected: too many failed attempts");
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed sign-in attempts; try again in a few minutes".into(),
        ));
    }
    let verified = match &user {
        Some(u) => verify_user_password(&state, u, req.password.trim()).await?,
        None => None,
    };
    let (Some(user), Some(verified)) = (user, verified) else {
        tracing::warn!(%ip, username, "console login failed: wrong username or password");
        return Err((StatusCode::UNAUTHORIZED, "wrong username or password".into()));
    };
    clear_login_attempts(&key);
    if user.effectively_disabled() {
        tracing::warn!(%ip, username, "console login rejected: account disabled");
        return Err((StatusCode::FORBIDDEN, "this account is disabled".into()));
    }
    // 校验到签发之间密码被重置了：按旧密码校验通过的这次登录不签会话。
    let Some(token) = issue_session(&state, user.id, &verified)? else {
        tracing::warn!(%ip, username, "console login rejected: the password changed during sign-in");
        return Err((StatusCode::UNAUTHORIZED, "wrong username or password".into()));
    };
    tracing::info!(%ip, username = %user.username, role = ?user.role, "console login succeeded");
    Ok(Json(LoginResp { ok: true, token, role: user.role, username: user.username }))
}

/// 退出登录：作废当前会话（已鉴权）。
pub async fn logout(
    State(state): State<AppState>,
    headers: header::HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    if let Some(token) = bearer(&headers) {
        state.store.delete_session(&sha256_hex(token)).map_err(internal)?;
    }
    Ok(ok_json())
}

#[derive(Deserialize)]
pub struct PwReq {
    password: String,
}

#[derive(Deserialize)]
pub struct SetupReq {
    password: String,
    /// 启动日志里的初始化口令。
    #[serde(default)]
    token: Option<String>,
}

/// 等长逐字节异或比较：不因第一个不同字节提前返回，免得按响应时间一位一位试出口令。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// [`constant_time_eq`] 给别的模块用（转发入口比对环境变量里的接入 Key）。
pub(crate) fn secrets_equal(a: &[u8], b: &[u8]) -> bool {
    constant_time_eq(a, b)
}

/// 生成初始化口令：16 字节随机数的十六进制（32 位）。
pub(crate) fn new_setup_token() -> String {
    let mut bytes = [0u8; 16];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 未设密码时把初始化口令打进日志：设密码只能靠它。启动时与清除密码后各打一次。
///
/// 口令每次启动随机生成、重启前不变（不是用一次就作废）：它只在未设密码时起作用，设上密码
/// 之后 `setup` 直接拒绝，用过的口令也就没有用处了。
pub(crate) fn log_setup_token(token: &str) {
    tracing::warn!(
        setup_token = %token,
        "no admin password is set: the console rejects every request until one is set; \
         open the console and enter this setup token to set it"
    );
}

/// 串行化首次设置管理密码：两条并发的 setup 不能都判成「还没设」、后写的那条悄悄把先设的
/// 密码盖掉。
static SETUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 首次设置管理密码（仅未配置时，公开），成功即登录、回会话 token。须带启动日志里的初始化
/// 口令，否则谁先连上端口谁就能把密码定下来、接管整个管理面。
///
/// 先判「已设过」再验口令：设过密码之后来的请求该看到的是「已设置」，而不是一句口令不对。
pub async fn setup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<SetupReq>,
) -> Result<Json<LoginResp>, ApiError> {
    let ip = client_ip(&headers, peer);
    let _guard = SETUP_LOCK.lock().await;
    if admin_configured(&state) {
        return Err((StatusCode::BAD_REQUEST, "an admin password is already set".into()));
    }
    let given = req.token.as_deref().map(str::trim).unwrap_or("");
    if !constant_time_eq(given.as_bytes(), state.setup_token.as_bytes()) {
        tracing::warn!(%ip, "admin password setup rejected: invalid setup token");
        return Err((StatusCode::FORBIDDEN, "invalid setup token".into()));
    }
    let pw = check_new_password(&req.password)?;
    let hash = hash_password(pw).await?;
    let admin = state.store.admin_user().map_err(internal)?;
    state.store.set_user_password_hash(admin.id, &hash, None).map_err(internal)?;
    // 谁在什么时候把密码定下来的，比任何一项设置变更都更该留痕。
    tracing::info!(%ip, "admin password set for the first time");
    let verified =
        Verified { tag: password_tag(&state, admin.role, &hash), expected_hash: Some(hash) };
    let token = issue_session(&state, admin.id, &verified)?
        .ok_or_else(|| internal("the admin password changed while it was being set"))?;
    Ok(Json(LoginResp { ok: true, token, role: admin.role, username: admin.username }))
}

/// 改自己的密码（已鉴权，访客不行）。admin 传空串 = 清除管理密码，控制台回到初始化状态
/// （访客跟着清掉、所有会话作废）；环境接管的 admin 不能在网页上改。
///
/// 改成功后作废本人的其它会话，当前这个保留。
pub async fn change_password(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<PwReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if actor.is_viewer() {
        return Err((StatusCode::FORBIDDEN, "the viewer password is set by the admin".into()));
    }
    if actor.is_admin() && state.admin_env.is_some() {
        return Err((StatusCode::BAD_REQUEST, "the admin password is managed by an environment variable and cannot be changed from the web UI".into()));
    }
    let cleared = req.password.trim().is_empty();
    if cleared && !actor.is_admin() {
        check_new_password("")?;
    }
    // 先记下此刻的哈希与当前会话，算完新哈希再按它们条件写入（见
    // `store::CredentialStore::change_own_password`）：算哈希期间被管理员重置了密码、撤了会话
    // 的话，这条在途请求写不进去。
    let token_hash = bearer(&headers)
        .map(sha256_hex)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, LOGIN_REQUIRED.to_string()))?;
    let old_hash = state.store.user_password_hash(actor.id).map_err(internal)?.unwrap_or_default();
    let new_hash = if cleared {
        String::new()
    } else {
        hash_password(check_new_password(&req.password)?).await?
    };
    let new_tag = password_tag(&state, actor.role, &new_hash);
    let written = state
        .store
        .change_own_password(actor.id, &token_hash, &old_hash, &new_hash, &new_tag)
        .map_err(internal)?;
    if !written {
        return Err((StatusCode::UNAUTHORIZED, LOGIN_REQUIRED.into()));
    }
    if cleared {
        // 访客跟着清：否则下回重设管理密码时，早先发出去的访客密码悄悄又能用了。
        state.store.delete_viewer().map_err(internal)?;
        state.store.delete_all_sessions().map_err(internal)?;
    }
    tracing::info!(
        ip = %client_ip(&headers, peer),
        username = %actor.username,
        cleared,
        "console password changed"
    );
    if cleared {
        // 清掉之后又得靠初始化口令才能进来，把它重新打出来，免得还要翻启动时那一行。
        log_setup_token(&state.setup_token);
    }
    Ok(ok_json())
}

#[derive(Serialize)]
pub struct MeResp {
    id: i64,
    username: String,
    role: UserRole,
    /// 管理密码是否由环境变量接管（仅 admin 有意义）。
    admin_env_managed: bool,
    /// 是否已设访客密码。仅 admin 可见，其他人恒为 false。
    viewer_configured: bool,
    /// 访客密码是否由环境变量接管（true = 网页不可改）。仅 admin 可见。
    viewer_env_managed: bool,
}

/// 当前登录身份（已鉴权）。前端据此决定显示哪些页面、藏掉哪些按钮。
pub async fn me(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> Json<MeResp> {
    let admin = actor.is_admin();
    Json(MeResp {
        id: actor.id,
        username: actor.username,
        role: actor.role,
        admin_env_managed: admin && state.admin_env.is_some(),
        viewer_configured: admin && viewer_enabled(&state),
        viewer_env_managed: admin && state.viewer_env.is_some(),
    })
}

/// 设置 / 清除访客密码（仅 admin，中间件按路由拦了其他人；环境接管时禁止）。空串 = 清除，
/// 清除后已登录的访客下一次请求即 401、被踢回登录页。访客的用户名固定是 `viewer`。
pub async fn set_viewer_password(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<PwReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.viewer_env.is_some() {
        return Err((StatusCode::BAD_REQUEST, "the viewer password is managed by an environment variable and cannot be changed from the web UI".into()));
    }
    let cleared = req.password.trim().is_empty();
    if cleared {
        state.store.delete_viewer().map_err(internal)?;
    } else {
        let hash = hash_password(check_new_password(&req.password)?).await?;
        state.store.upsert_viewer(&hash).map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
        // 换了密码，拿旧密码登进来的访客全部下线。
        if let Some(v) = state.store.viewer_user().map_err(internal)? {
            state.store.delete_user_sessions(v.id, None).map_err(internal)?;
        }
    }
    tracing::info!(ip = %client_ip(&headers, peer), cleared, "viewer password changed");
    Ok(ok_json())
}

fn ok_json() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

pub(crate) fn internal(e: impl std::fmt::Display) -> ApiError {
    // 同 `web::internal`：错误详情只回给客户端、服务端不留痕的话，500 在日志里查不到。
    let msg = e.to_string();
    tracing::error!(error = %msg, "auth endpoint internal error");
    (StatusCode::INTERNAL_SERVER_ERROR, msg)
}

/// 把文本里所有 URL 的 userinfo 打码：`http://user:secret@h` → `http://user:***@h`，
/// 只有用户名（`http://token@h`，常见于把凭据放在用户名里的代理商）整段换成 `***`。
///
/// 给访客的响应体整体过一遍，而不是只盯着 `proxy` 这类字段：代理串会顺着报错文案流到别处
/// （`invalid proxy URL: http://u:p@h:0` 进了 `ban_reason`、封号事件的 reason、流水的
/// error_message……），按字段打码总会漏一处，往后新加的字段也不会有人记得补。
///
/// 一段 authority 从 `://` 后数到**未转义的双引号或换行**为止，别的字符一概不当结束符：
/// 密码里什么都可能有（`'`、`/`、空格、`<`……，报错文案还会原样回显用户填的非法串），
/// 在哪个字符处断，含那个字符的密码就漏半截。响应体是 JSON，字符串只会在未转义的 `"`
/// 处结束（密码里的 `"` 写成 `\"`，反斜杠后面那个字符跳过），纯文本的报错按行算。代价是
/// 同一个字符串里 URL 后面若还有 `@`（另一个 URL、邮箱），中间那段会被一并遮掉——给访客看
/// 的东西宁可多遮。
pub fn redact_url_credentials(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("://") {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let mut escaped = false;
        let end = tail
            .char_indices()
            .find(|&(_, c)| {
                if escaped {
                    escaped = false;
                    return false;
                }
                if c == '\\' {
                    escaped = true;
                    return false;
                }
                matches!(c, '"' | '\n' | '\r')
            })
            .map_or(tail.len(), |(j, _)| j);
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                // 只在用户名看得出就是用户名时才留：`http://token@host:8080/path@tail` 里
                // 最后那个 `@` 在路径上，按第一个 `:` 切出来的「用户名」是 `token@host`，留下
                // 就把 token 漏了。用户名里有 `@` `/` `?` `#` 的，整段一起遮。
                let user = authority[..at]
                    .find(':')
                    .map(|colon| &authority[..colon])
                    .filter(|u| !u.contains(['@', '/', '?', '#']));
                if let Some(user) = user {
                    out.push_str(user);
                    out.push(':');
                }
                out.push_str("***");
                out.push_str(&authority[at..]);
                changed = true;
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    if changed { std::borrow::Cow::Owned(out) } else { std::borrow::Cow::Borrowed(text) }
}

/// 访客的响应体过一遍 [`redact_url_credentials`]。管理接口没有流式响应，整段读进来无妨。
async fn redact_for_viewer(resp: Response) -> Response {
    let (mut parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => return internal(e).into_response(),
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    };
    match redact_url_credentials(text) {
        std::borrow::Cow::Borrowed(_) => Response::from_parts(parts, axum::body::Body::from(bytes)),
        std::borrow::Cow::Owned(redacted) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, axum::body::Body::from(redacted))
        }
    }
}

/// 记进日志的来源标识：优先前置层给的 `x-forwarded-for` 首段（luban 常挂在反代后面，
/// 直接取对端只会得到反代自己），退回 `x-real-ip`，都没有才用 TCP 对端地址。
///
/// 这两个头是客户端可伪造的，所以只当**线索**用，不作任何判决依据；真要防爆破得在前置层做。
///
/// `peer` 由 `ConnectInfo` 提取，取决于 [`crate::web::run`] 里那句
/// `into_make_service_with_connect_info`——换掉它这三个接口会一律 500，改动服务装配时留意。
fn client_ip(headers: &header::HeaderMap, peer: std::net::SocketAddr) -> String {
    let from_header = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match from_header {
        Some(ip) => ip.to_owned(),
        None => peer.ip().to_string(),
    }
}

#[cfg(test)]
mod tests;
