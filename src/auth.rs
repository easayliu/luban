//! 管理界面登录鉴权。
//!
//! 密码以 sha256 存于 SQLite（`admin_password_sha256`），或由 `LUBAN_ADMIN_PASSWORD`
//! 环境接管。设置后 `/api/*` 管理接口需带 `Authorization: Bearer <password>`。
//! 转发代理 `/v1/*` 不走这里。
//!
//! **未设密码时管理接口一律拒绝**，本机也不例外。默认监听 `0.0.0.0`、Docker 也把端口发布到
//! 所有网卡，此前未设密码等于管理面对网络匿名敞开：谁先连上谁就能设密码、导出全部明文 token。
//! 现在要先设密码，而设密码得带启动日志里打印的初始化口令（[`AppState::setup_token`]），
//! 证明来人能看到这台服务的日志。
//!
//! 不给本机开免密口子：服务端分不出「本机浏览器」和「同机反代转进来的外部请求」。nginx 默认
//! 把 `Host` 改写成 `proxy_pass` 的地址（`$proxy_host`），`proxy_pass http://127.0.0.1:4600`
//! 转进来的请求对端与 `Host` 都是回环，按对端 + `Host` 判本机的话，公网上的人经反代就能免密
//! 进来、不带口令抢先设密码。本机用起来的补偿：`--open` 打开浏览器时把口令带在地址的
//! `#setup_token=` 里，初始化页自动填好。
//!
//! **只读访客**：另设一个访客密码（`viewer_password_sha256` 或 `LUBAN_VIEWER_PASSWORD`），
//! 用它登录的人能看控制台的全部页面、什么都改不了。只在管理密码已设时生效。权限由中间件
//! 统一判：访客只放行 `GET`/`HEAD`，再去掉几条读也不该给的（见 [`VIEWER_DENIED`]）。没按
//! 「逐个接口声明要什么权限」做，是因为写接口全是 `POST`/`DELETE`，按方法默认拒绝，往后新加
//! 的写接口不必记得补一道检查也拦得住。

use axum::{
    Extension, Json,
    extract::{ConnectInfo, MatchedPath, Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store;
use crate::web::AppState;

type ApiError = (StatusCode, String);

/// sha256 十六进制。
pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

/// 生效的管理密码哈希：环境接管优先，否则用库中存的哈希；都无则 None（未启用鉴权）。
fn admin_hash(state: &AppState) -> Option<String> {
    if let Some(pw) = &state.admin_env {
        return Some(sha256_hex(pw));
    }
    state.store.get_setting(store::ADMIN_PASSWORD).ok().flatten().filter(|s| !s.is_empty())
}

/// 配置了的访客密码哈希（不管它眼下能不能用）：环境接管优先，否则库中存的哈希。
fn configured_viewer_hash(state: &AppState) -> Option<String> {
    if let Some(pw) = &state.viewer_env {
        return Some(sha256_hex(pw));
    }
    state.store.get_setting(store::VIEWER_PASSWORD).ok().flatten().filter(|s| !s.is_empty())
}

/// **生效的**访客密码哈希：配置了，且确认与管理密码不会被认混（两者规范形都已知且不同）。
///
/// 规范形缺一个就不认访客，宁可访客暂时进不来，也不冒「访客构造出管理密码」的险。缺的只会是
/// 升级前就存在库里的管理密码（那时没存规范形），管理员下一次带密码访问就补上，见
/// [`backfill_admin_canonical`]。
fn viewer_hash(state: &AppState) -> Option<String> {
    let h = configured_viewer_hash(state)?;
    match (admin_canonical(state), viewer_canonical(state)) {
        (Some(a), Some(v)) if a != v => Some(h),
        _ => None,
    }
}

/// 密码的规范形：反复百分号解码到解不动为止（每层去首尾空白）。
///
/// 中间件对请求头里的密码原文、解码各认一次，访客又能随意构造请求头，所以只要两个密码能经
/// 编码 / 解码互相得到（`pass word` 与 `pass%20word`，或再套几层的 `pass%2520word`），
/// 知道其中一个就能拼出另一个。它们的规范形必然相同；规范形不同的，就不存在这种推导关系。
/// 判「认混」比规范形，两个方向、任意层数一并排除。
///
/// **不设层数上限**：设了上限，把密码编码超过上限层数就能让两边规范形对不上、绕过判定。
/// 循环必然终止——[`percent_decode`] 只在至少有一个合法 `%XX` 时才回 `Some`，每解一层串至少
/// 短 2 字节，层数不超过长度的三分之一。
fn canonical(pw: &str) -> String {
    let mut cur = pw.trim().to_owned();
    while let Some(d) = percent_decode(&cur) {
        cur = d.trim().to_owned();
    }
    cur
}

fn canonical_hash(pw: &str) -> String {
    sha256_hex(&canonical(pw))
}

fn stored(state: &AppState, key: &str) -> Option<String> {
    state.store.get_setting(key).ok().flatten().filter(|s| !s.is_empty())
}

/// 管理密码规范形的哈希：环境接管时现算，否则读库（升级前设的密码可能还没有）。
fn admin_canonical(state: &AppState) -> Option<String> {
    match &state.admin_env {
        Some(pw) => Some(canonical_hash(pw)),
        None => stored(state, store::ADMIN_PASSWORD_CANONICAL),
    }
}

fn viewer_canonical(state: &AppState) -> Option<String> {
    match &state.viewer_env {
        Some(pw) => Some(canonical_hash(pw)),
        None => stored(state, store::VIEWER_PASSWORD_CANONICAL),
    }
}

/// 拿到管理密码明文的时候（登录、带密码访问），把库里缺的规范形补上。
///
/// 先不加锁看一眼：绝大多数请求规范形早就在，不该每个请求都去抢 [`AUTH_WRITE_LOCK`]。
fn backfill_admin_canonical(state: &AppState, admin_pw: &str) {
    if state.admin_env.is_some() || stored(state, store::ADMIN_PASSWORD_CANONICAL).is_some() {
        return;
    }
    let _guard = AUTH_WRITE_LOCK.lock();
    backfill_admin_canonical_locked(state, admin_pw);
}

/// [`backfill_admin_canonical`] 的持锁部分，调用方须已持有 [`AUTH_WRITE_LOCK`]。
///
/// 锁里**重新核对**库里的哈希还是不是这个明文的：请求在改密码之前通过了鉴权、补存却落在
/// 改密码之后的话，写进去的就是旧密码的规范形，和新密码配不上对。
fn backfill_admin_canonical_locked(state: &AppState, admin_pw: &str) {
    if state.admin_env.is_some()
        || stored(state, store::ADMIN_PASSWORD_CANONICAL).is_some()
        || stored(state, store::ADMIN_PASSWORD) != Some(sha256_hex(admin_pw))
    {
        return;
    }
    if let Err(e) =
        state.store.set_setting(store::ADMIN_PASSWORD_CANONICAL, &canonical_hash(admin_pw))
    {
        tracing::warn!(error = %e, "failed to store the admin password canonical hash");
    }
}

/// 请求头里的密码按哪种读法对上了管理密码，就回那种读法的明文；对不上为 None。
fn admin_plaintext(admin: &str, bearer: &str) -> Option<String> {
    if sha256_hex(bearer) == admin {
        return Some(bearer.to_owned());
    }
    percent_decode(bearer).map(|d| d.trim().to_owned()).filter(|d| sha256_hex(d) == admin)
}

fn bearer(headers: &header::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
}

/// 配了访客密码却没生效时在启动日志里说一声，免得访客怎么都登不进来还查不到原因。
pub(crate) fn warn_if_viewer_inactive(state: &AppState) {
    if configured_viewer_hash(state).is_none() || viewer_hash(state).is_some() {
        return;
    }
    let reason = match (admin_canonical(state), viewer_canonical(state)) {
        (Some(a), Some(v)) if a == v => "it equals the admin password after URL decoding",
        _ => "the admin password has not been verified since upgrading; sign in as admin once",
    };
    tracing::warn!(reason, "viewer password is configured but inactive");
}

/// 登录身份。中间件鉴权通过后放进请求扩展，handler 用 `Extension<Role>` 取。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 管理员：什么都能做。
    Admin,
    /// 只读访客：只能看。响应体里的代理密码由中间件统一遮掉，见 [`redact_url_credentials`]。
    Viewer,
}

impl Role {
    pub fn is_viewer(self) -> bool {
        self == Role::Viewer
    }
}

/// 访客连 `GET` 也不给的接口（路径不带 `/api` 前缀）：
/// - `/authorize`：生成 PKCE 存进内存，是「添加账号」的第一步，算写；
/// - `/export`：迁移文件带全部账号的明文 token 和接入 key，看到就等于能用；
/// - `/settings`、`/learned-rejections`：只有系统设置页用，而系统设置整页不对访客开放
///   （`/settings` 还带着明文接入 key）。`/proxies` 不在此列：账号页要靠它显示代理名称，
///   地址里的密码由 [`redact_url_credentials`] 遮掉。
const VIEWER_DENIED: &[&str] = &["/authorize", "/export", "/settings", "/learned-rejections"];

/// 访客能不能打这个请求：只读方法，且不在 [`VIEWER_DENIED`] 里。
fn viewer_allowed(method: &Method, path: &str) -> bool {
    let path = path.strip_prefix("/api").unwrap_or(path);
    matches!(*method, Method::GET | Method::HEAD) && !VIEWER_DENIED.contains(&path)
}

/// 按密码认身份：先比管理密码，再比访客密码；都不对为 None。
///
/// 访客密码只在管理密码已设时才认（`admin` 为 None 时调用方已直接拒绝），且要与管理密码
/// 规范形不同才生效（见 [`viewer_hash`]）。请求头里的密码有原文、编码两种读法，要走
/// [`role_of_bearer`]，不要直接调这个。
fn role_of(state: &AppState, admin: &str, pw: &str) -> Option<Role> {
    let h = sha256_hex(pw);
    if h == admin {
        return Some(Role::Admin);
    }
    viewer_hash(state).filter(|v| *v == h).map(|_| Role::Viewer)
}

/// 按请求头里的密码认身份：原文、百分号解码各认一次，两种读法认出不同身份时取访客。
///
/// 网页端发的是 `encodeURIComponent(密码)`，脚本发的是原文，同一个头两种读法都得试。
/// 「两种读法各认出一种身份」只会发生在两个密码互为编码时，而那种组合已由 [`viewer_hash`]
/// 按规范形排除；这里取低的那个只是第二道闸。
fn role_of_bearer(state: &AppState, admin: &str, pw: &str) -> Option<Role> {
    let raw = role_of(state, admin, pw);
    let decoded = percent_decode(pw).and_then(|d| role_of(state, admin, d.trim()));
    match (raw, decoded) {
        (Some(Role::Viewer), _) | (_, Some(Role::Viewer)) => Some(Role::Viewer),
        (raw, decoded) => raw.or(decoded),
    }
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

/// 是否已启用管理鉴权（环境接管或库里存了哈希）。
///
/// 给那些「开着鉴权才允许」的接口用。未设密码时 [`require_admin`] 已经一律拒绝，这层是
/// 兜底：个别接口给出去的东西比「能改配置」更重（如导出含明文 token 的迁移文件），不把
/// 安全全押在中间件的装配上，它们自己再确认一次这道门锁着。
pub fn admin_configured(state: &AppState) -> bool {
    admin_hash(state).is_some()
}

/// 百分号解码（对应前端的 `encodeURIComponent`）；不含 `%`、编码不合法或解出非 UTF-8 时返回 None。
fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 未设密码时管理接口回的文案，前端据此切到「初始化管理密码」页。
const SETUP_REQUIRED: &str =
    "set an admin password first, using the setup token from the server log";

/// 中间件：已设密码时校验 `Authorization: Bearer <password>`；未设时一律 401（本机也不
/// 放行，原因见模块说明）。
///
/// 回 401 而不是 403：前端的拦截器见 401 就重新拉一次鉴权状态，未设密码的来访由此落到
/// 初始化页，不必再为这一种情况单写一条分支。
///
/// 认出身份后放进请求扩展（[`Role`]）。访客打写接口回 403 而不是 401：密码是对的，回 401
/// 会让前端以为登录失效、清掉密码踢回登录页。
pub async fn require_admin(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(hash) = admin_hash(&state) else {
        return (StatusCode::UNAUTHORIZED, SETUP_REQUIRED).into_response();
    };
    let given = bearer(req.headers());
    // 原文（脚本直接带明文）与百分号解码（网页端为支持非 ASCII 密码会编码）都认。
    let Some(role) = given.and_then(|pw| role_of_bearer(&state, &hash, pw)) else {
        return (StatusCode::UNAUTHORIZED, "admin password required").into_response();
    };
    if role == Role::Admin
        && let Some(pw) = given.and_then(|pw| admin_plaintext(&hash, pw))
    {
        backfill_admin_canonical(&state, &pw);
    }
    if role.is_viewer() {
        let path = req
            .extensions()
            .get::<MatchedPath>()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_else(|| req.uri().path().to_owned());
        if !viewer_allowed(req.method(), &path) {
            return (
                StatusCode::FORBIDDEN,
                "read-only viewer: this action requires the admin password",
            )
                .into_response();
        }
    }
    req.extensions_mut().insert(role);
    let resp = next.run(req).await;
    if role.is_viewer() { redact_for_viewer(resp).await } else { resp }
}

#[derive(Serialize)]
pub struct StateResp {
    /// 是否已设置管理密码（true = 需登录）。
    configured: bool,
    /// 是否由环境变量接管（true = 网页不可改）。
    env_managed: bool,
    /// 未设密码：要先用初始化口令设密码才能进控制台。恒等于 `!configured`，单列一个字段
    /// 是让前端不必自己推这层语义。
    setup_required: bool,
    /// 能否用访客密码登录（已设且生效）。登录页据此把文案写成「管理密码或访客密码」，否则设了
    /// 访客密码的人看到满页「管理登录」，以为自己进不去。只透露「开没开」，不涉及密码本身。
    viewer_enabled: bool,
}

/// 鉴权状态（公开）。
pub async fn state(State(state): State<AppState>) -> Json<StateResp> {
    let configured = admin_hash(&state).is_some();
    Json(StateResp {
        configured,
        env_managed: state.admin_env.is_some(),
        setup_required: !configured,
        viewer_enabled: configured && viewer_hash(&state).is_some(),
    })
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

/// 串行化控制台密码的所有写入（首次设置、改管理密码、改访客密码、补存规范形），连同写之前
/// 的检查一起：
/// - 两条并发的 setup 不能都判成「还没设」、后写的那条悄悄把先设的密码盖掉；
/// - 密码哈希与规范形是两个键、分两次写，两次更新交错成「哈希 A → 哈希 B → 规范形 B →
///   规范形 A」时，库里的密码是 B、规范形却是 A，之后的冲突检查就拿错的规范形放行；
/// - 冲突检查读的是另一个密码的规范形，检查到写入之间另一个密码不能变。
///
/// 读的一方不拿锁，靠写入顺序兜：先删旧规范形、再写哈希、最后写新规范形（见
/// [`write_password`]），中途任何时刻读到的要么一致、要么缺规范形——缺了访客就不生效。
static AUTH_WRITE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn ok_json() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}
fn internal(e: impl std::fmt::Display) -> ApiError {
    // 同 `web::internal`：错误详情只回给客户端、服务端不留痕的话，500 在日志里查不到。
    let msg = e.to_string();
    tracing::error!(error = %msg, "auth endpoint internal error");
    (StatusCode::INTERNAL_SERVER_ERROR, msg)
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

/// 校验密码（供前端登录确认，公开）。
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<PwReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // 这是全项目唯一能看出「有人在猜管理密码」的地方，不带来源等于记了个寂寞。
    let ip = client_ip(&headers, peer);
    match admin_hash(&state) {
        None => Err((StatusCode::BAD_REQUEST, "no admin password has been set yet".into())),
        Some(h) => match role_of(&state, &h, req.password.trim()) {
            Some(role) => {
                if role == Role::Admin {
                    backfill_admin_canonical(&state, req.password.trim());
                }
                tracing::info!(%ip, ?role, "admin login succeeded");
                Ok(Json(serde_json::json!({ "ok": true, "role": role })))
            }
            None => {
                tracing::warn!(%ip, "admin login failed: wrong password");
                Err((StatusCode::UNAUTHORIZED, "wrong password".into()))
            }
        },
    }
}

/// 首次设置密码（仅未配置时，公开）。须带启动日志里的初始化口令，否则谁先连上端口谁就能
/// 把密码定下来、接管整个管理面。
///
/// 先判「已设过」再验口令：设过密码之后来的请求该看到的是「已设置」，而不是一句口令不对。
pub async fn setup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<SetupReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ip = client_ip(&headers, peer);
    let _guard = AUTH_WRITE_LOCK.lock();
    if admin_hash(&state).is_some() {
        return Err((StatusCode::BAD_REQUEST, "an admin password is already set".into()));
    }
    let given = req.token.as_deref().map(str::trim).unwrap_or("");
    if !constant_time_eq(given.as_bytes(), state.setup_token.as_bytes()) {
        tracing::warn!(%ip, "admin password setup rejected: invalid setup token");
        return Err((StatusCode::FORBIDDEN, "invalid setup token".into()));
    }
    let pw = req.password.trim();
    if pw.len() < 4 {
        return Err((StatusCode::BAD_REQUEST, "password must be at least 4 characters".into()));
    }
    if viewer_canonical(&state).is_some_and(|v| v == canonical_hash(pw)) {
        return Err((StatusCode::BAD_REQUEST, ADMIN_COLLIDES.into()));
    }
    set_admin_password(&state, pw)?;
    // 谁在什么时候把密码定下来的，比任何一项设置变更都更该留痕。
    tracing::info!(%ip, "admin password set for the first time");
    Ok(ok_json())
}

/// 修改/清除密码（已鉴权；环境接管时禁止）。空串=清除。
pub async fn change_password(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<PwReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.admin_env.is_some() {
        return Err((StatusCode::BAD_REQUEST, "the admin password is managed by an environment variable and cannot be changed from the web UI".into()));
    }
    let pw = req.password.trim();
    let cleared = pw.is_empty();
    let _guard = AUTH_WRITE_LOCK.lock();
    if cleared {
        // 访客密码跟着清：否则下回重设管理密码时，早先发出去的访客密码悄悄又能用了。
        // 环境接管的那份清不掉，也无妨——它本来就随部署配置走。
        for key in store::CONSOLE_AUTH_KEYS {
            state.store.delete_setting(key).map_err(internal)?;
        }
    } else {
        if pw.len() < 4 {
            return Err((StatusCode::BAD_REQUEST, "password must be at least 4 characters".into()));
        }
        if viewer_canonical(&state).is_some_and(|v| v == canonical_hash(pw)) {
            return Err((StatusCode::BAD_REQUEST, ADMIN_COLLIDES.into()));
        }
        set_admin_password(&state, pw)?;
    }
    tracing::info!(
        ip = %client_ip(&headers, peer),
        cleared,
        "admin password changed"
    );
    if cleared {
        // 清掉之后又得靠初始化口令才能进来，把它重新打出来，免得还要翻启动时那一行。
        log_setup_token(&state.setup_token);
    }
    Ok(ok_json())
}

const ADMIN_COLLIDES: &str =
    "the admin password must not equal the viewer password, even after URL encoding or decoding";
const VIEWER_COLLIDES: &str =
    "the viewer password must not equal the admin password, even after URL encoding or decoding";

/// 写一个密码的哈希与规范形，调用方须已持有 [`AUTH_WRITE_LOCK`]。
///
/// 顺序要紧：先删旧规范形、再写哈希、最后写新规范形。不拿锁的读方（中间件认身份）在中途
/// 读到的是「新哈希 + 无规范形」，访客不生效；反过来先写规范形的话，会有一瞬间是「旧哈希 +
/// 新规范形」，冲突判定拿的不是真密码的规范形。
fn write_password(
    state: &AppState,
    hash_key: &str,
    canonical_key: &str,
    pw: &str,
) -> Result<(), ApiError> {
    state.store.delete_setting(canonical_key).map_err(internal)?;
    state.store.set_setting(hash_key, &sha256_hex(pw)).map_err(internal)?;
    state.store.set_setting(canonical_key, &canonical_hash(pw)).map_err(internal)
}

/// 写管理密码，调用方须已持有 [`AUTH_WRITE_LOCK`]。
fn set_admin_password(state: &AppState, pw: &str) -> Result<(), ApiError> {
    write_password(state, store::ADMIN_PASSWORD, store::ADMIN_PASSWORD_CANONICAL, pw)
}

#[derive(Serialize)]
pub struct MeResp {
    role: Role,
    /// 是否已设访客密码。仅管理员可见，访客看到恒为 false。
    viewer_configured: bool,
    /// 设了却没生效（与管理密码互为编码，或升级前的管理密码还没校验过规范形）。
    viewer_inactive: bool,
    /// 访客密码是否由环境变量接管（true = 网页不可改）。
    viewer_env_managed: bool,
}

/// 当前登录身份（已鉴权）。前端据此决定要不要藏掉改动类的按钮。
pub async fn me(State(state): State<AppState>, Extension(role): Extension<Role>) -> Json<MeResp> {
    let admin = !role.is_viewer();
    Json(MeResp {
        role,
        viewer_configured: admin && configured_viewer_hash(&state).is_some(),
        viewer_inactive: admin
            && configured_viewer_hash(&state).is_some()
            && viewer_hash(&state).is_none(),
        viewer_env_managed: admin && state.viewer_env.is_some(),
    })
}

/// 设置/清除访客密码（仅管理员，访客被中间件按方法拦下；环境接管时禁止）。空串=清除，
/// 清除后已登录的访客下一次请求即 401、被踢回登录页。
pub async fn set_viewer_password(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<PwReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.viewer_env.is_some() {
        return Err((StatusCode::BAD_REQUEST, "the viewer password is managed by an environment variable and cannot be changed from the web UI".into()));
    }
    let pw = req.password.trim();
    let cleared = pw.is_empty();
    let _guard = AUTH_WRITE_LOCK.lock();
    if cleared {
        // 先删哈希：中途读到的是「无访客密码」，而不是「有哈希、没规范形」以外的什么。
        state.store.delete_setting(store::VIEWER_PASSWORD).map_err(internal)?;
        state.store.delete_setting(store::VIEWER_PASSWORD_CANONICAL).map_err(internal)?;
    } else {
        if pw.len() < 4 {
            return Err((StatusCode::BAD_REQUEST, "password must be at least 4 characters".into()));
        }
        // 走到这里的一定是管理员（中间件拦了访客），请求头里就有管理密码明文：比规范形
        // 不依赖库里有没有存过它的规范形。
        let admin =
            admin_hash(&state).zip(bearer(&headers)).and_then(|(h, pw)| admin_plaintext(&h, pw));
        let Some(admin) = admin else {
            return Err((StatusCode::UNAUTHORIZED, "admin password required".into()));
        };
        backfill_admin_canonical_locked(&state, &admin);
        if canonical(&admin) == canonical(pw) {
            return Err((StatusCode::BAD_REQUEST, VIEWER_COLLIDES.into()));
        }
        write_password(&state, store::VIEWER_PASSWORD, store::VIEWER_PASSWORD_CANONICAL, pw)?;
    }
    tracing::info!(
        ip = %client_ip(&headers, peer),
        cleared,
        "viewer password changed"
    );
    Ok(ok_json())
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, percent_decode};
    use axum::http::{HeaderMap, HeaderValue, header};

    fn with_host(host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        h
    }

    fn test_state() -> crate::web::AppState {
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        crate::web::AppState::for_test(store)
    }

    /// 走真实中间件：`ConnectInfo` 手动塞进扩展，对应 `into_make_service_with_connect_info`。
    async fn protected_status(
        state: &crate::web::AppState,
        peer: &str,
        host: &str,
        bearer: Option<&str>,
    ) -> axum::http::StatusCode {
        use axum::{Router, body::Body, extract::ConnectInfo, http::Request, routing::get};
        use tower::ServiceExt;
        let app = Router::new()
            .route("/x", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn_with_state(state.clone(), super::require_admin))
            .with_state(state.clone());
        let mut req = Request::builder().uri("/x").header(header::HOST, host);
        if let Some(pw) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {pw}"));
        }
        let mut req = req.body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
        app.oneshot(req).await.unwrap().status()
    }

    /// 未设密码一律拒，本机也不放行——同机 nginx 默认把 `Host` 改写成 `127.0.0.1:4600`，
    /// 转进来的外部请求与本机直连长得一模一样。
    #[tokio::test]
    async fn unset_password_rejects_every_request() {
        use axum::http::StatusCode;
        let state = test_state();
        for (peer, host) in [
            ("127.0.0.1:5000", "127.0.0.1:4600"),
            ("[::1]:5000", "localhost:4600"),
            ("172.17.0.1:5000", "luban.example"),
        ] {
            let status = protected_status(&state, peer, host, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{peer} / {host}");
        }

        // 设了密码之后带对了才能进，本机远程一个口径。
        state
            .store
            .set_setting(crate::store::ADMIN_PASSWORD, &super::sha256_hex("pw1234"))
            .unwrap();
        let local = protected_status(&state, "127.0.0.1:5000", "127.0.0.1:4600", None).await;
        assert_eq!(local, StatusCode::UNAUTHORIZED);
        let local =
            protected_status(&state, "127.0.0.1:5000", "127.0.0.1:4600", Some("pw1234")).await;
        assert_eq!(local, StatusCode::OK);
        let remote =
            protected_status(&state, "172.17.0.1:5000", "luban.example", Some("pw1234")).await;
        assert_eq!(remote, StatusCode::OK);
    }

    /// 访客只能打 GET，且 `/export`、`/authorize` 也不给；挂在 `/api` 下走 `MatchedPath`，
    /// 与真实装配一致。
    #[tokio::test]
    async fn viewer_is_read_only() {
        use axum::{
            Extension, Router,
            body::Body,
            extract::ConnectInfo,
            http::{Request, StatusCode},
            routing::get,
        };
        use tower::ServiceExt;
        let state = test_state();
        set_passwords(&state, "admin-pw", "viewer-pw");
        let role = |Extension(r): Extension<super::Role>| async move { format!("{r:?}") };
        let app = Router::new().nest(
            "/api",
            Router::new()
                .route("/credentials", get(role).post(role))
                .route("/credentials/{id}", get(role).delete(role))
                .route("/export", get(role))
                .route("/authorize", get(role))
                .route("/settings", get(role))
                .route(
                    "/leak",
                    get(|| async { r#"{"ban_reason":"invalid proxy URL: http://u:secret@h:0"}"# }),
                )
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    super::require_admin,
                ))
                .with_state(state.clone()),
        );
        let call = |method: &str, uri: &str, pw: &str| {
            let mut req = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {pw}"))
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo("127.0.0.1:5000".parse::<std::net::SocketAddr>().unwrap()));
            let app = app.clone();
            async move { app.oneshot(req).await.unwrap().status() }
        };
        for (method, uri) in [
            ("GET", "/api/credentials"),
            ("POST", "/api/credentials"),
            ("DELETE", "/api/credentials/1"),
            ("GET", "/api/export"),
            ("GET", "/api/authorize"),
        ] {
            assert_eq!(call(method, uri, "admin-pw").await, StatusCode::OK, "admin {method} {uri}");
        }
        assert_eq!(call("GET", "/api/credentials", "viewer-pw").await, StatusCode::OK);
        assert_eq!(call("GET", "/api/credentials/1", "viewer-pw").await, StatusCode::OK);
        for (method, uri) in [
            ("POST", "/api/credentials"),
            ("DELETE", "/api/credentials/1"),
            ("GET", "/api/export"),
            ("GET", "/api/authorize"),
            ("GET", "/api/settings"),
        ] {
            assert_eq!(
                call(method, uri, "viewer-pw").await,
                StatusCode::FORBIDDEN,
                "viewer {method} {uri}"
            );
        }
        // 网页端会对密码做 encodeURIComponent，访客也要认得出。
        assert_eq!(call("GET", "/api/credentials", "viewer%2Dpw").await, StatusCode::OK);
        assert_eq!(call("GET", "/api/credentials", "nope").await, StatusCode::UNAUTHORIZED);

        // 响应体里不论哪个字段带了代理密码，访客拿到的都是打过码的，管理员原样。
        let body = |pw: &str| {
            let mut req = Request::builder()
                .uri("/api/leak")
                .header(header::AUTHORIZATION, format!("Bearer {pw}"))
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo("127.0.0.1:5000".parse::<std::net::SocketAddr>().unwrap()));
            let app = app.clone();
            async move {
                let resp = app.oneshot(req).await.unwrap();
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                String::from_utf8(bytes.to_vec()).unwrap()
            }
        };
        assert!(body("viewer-pw").await.contains("http://u:***@h:0"));
        assert!(body("admin-pw").await.contains("http://u:secret@h:0"));

        // 管理密码清掉后访客密码不再起作用：未设管理密码时一律 401。
        state.store.delete_setting(crate::store::ADMIN_PASSWORD).unwrap();
        assert_eq!(call("GET", "/api/credentials", "viewer-pw").await, StatusCode::UNAUTHORIZED);
    }

    /// 按网页端的写法落两个密码：哈希与规范形都写上。
    fn set_passwords(state: &crate::web::AppState, admin: &str, viewer: &str) {
        use crate::store::{
            ADMIN_PASSWORD, ADMIN_PASSWORD_CANONICAL, VIEWER_PASSWORD, VIEWER_PASSWORD_CANONICAL,
        };
        for (k, v) in [
            (ADMIN_PASSWORD, super::sha256_hex(admin)),
            (ADMIN_PASSWORD_CANONICAL, super::canonical_hash(admin)),
            (VIEWER_PASSWORD, super::sha256_hex(viewer)),
            (VIEWER_PASSWORD_CANONICAL, super::canonical_hash(viewer)),
        ] {
            state.store.set_setting(k, &v).unwrap();
        }
    }

    fn bearer_headers(pw: &str) -> HeaderMap {
        let mut h = with_host("127.0.0.1:4600");
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {pw}")).unwrap());
        h
    }

    /// 两个密码能经编码 / 解码互相得到时，知道访客密码就能拼出管理密码（`pass word` →
    /// `pass%20word` → 请求头 `pass%2520word` 解码一次即管理密码）。两个方向、多层编码，
    /// 设置时一律拒。
    #[tokio::test]
    async fn encoded_collisions_are_rejected_in_both_directions() {
        use axum::{
            Json,
            extract::{ConnectInfo, State},
        };
        let state = test_state();
        let peer: std::net::SocketAddr = "127.0.0.1:5000".parse().unwrap();
        super::set_admin_password(&state, "pass%20word").unwrap();
        // 管理员网页端带的是 encodeURIComponent('pass%20word')。
        let admin_header = bearer_headers("pass%2520word");
        let set_viewer = |pw: &str| {
            super::set_viewer_password(
                State(state.clone()),
                ConnectInfo(peer),
                admin_header.clone(),
                Json(super::PwReq { password: pw.into() }),
            )
        };
        for pw in ["pass word", "pass%20word", "pass%2520word", "pass%252520word", "pass%20%77ord"]
        {
            let err = set_viewer(pw).await.unwrap_err();
            assert_eq!(err.0, axum::http::StatusCode::BAD_REQUEST, "viewer {pw:?} 应被拒");
        }
        set_viewer("pass words").await.expect("规范形不同的可以设");

        // 反过来：访客密码先在，改管理密码时同样两个方向都拒。
        let change = |pw: &str| {
            super::change_password(
                State(state.clone()),
                ConnectInfo(peer),
                admin_header.clone(),
                Json(super::PwReq { password: pw.into() }),
            )
        };
        for pw in ["pass words", "pass%20words", "pass%2520words"] {
            let err = change(pw).await.unwrap_err();
            assert_eq!(err.0, axum::http::StatusCode::BAD_REQUEST, "admin {pw:?} 应被拒");
        }
    }

    /// 绕过设置校验硬塞进库的冲突组合（或环境变量配成这样）：访客密码不生效，访客构造的
    /// 请求头既认不成访客、更认不成管理员。
    #[tokio::test]
    async fn colliding_viewer_password_is_inactive() {
        use super::{Role, role_of_bearer};
        let state = test_state();
        set_passwords(&state, "pass%20word", "pass word");
        let admin = super::sha256_hex("pass%20word");
        assert_eq!(
            role_of_bearer(&state, &admin, "pass%20word"),
            Some(Role::Admin),
            "管理员脚本带原文"
        );
        assert_eq!(
            role_of_bearer(&state, &admin, "pass%2520word"),
            Some(Role::Admin),
            "管理员网页端"
        );
        assert_eq!(role_of_bearer(&state, &admin, "pass word"), None, "访客密码不生效");
        assert!(super::viewer_hash(&state).is_none());

        // 管理密码是访客密码编码 9 层：照样判成冲突、访客不生效，再套一层的请求头只认得出管理员。
        let mut admin9 = "pass word".to_owned();
        for _ in 0..9 {
            admin9 = admin9.replace('%', "%25").replace(' ', "%20");
        }
        set_passwords(&state, &admin9, "pass word");
        assert!(super::viewer_hash(&state).is_none());
        let h = super::sha256_hex(&admin9);
        assert_eq!(role_of_bearer(&state, &h, "pass word"), None);
    }

    /// 升级前存进库的管理密码没有规范形：访客密码先不生效，管理员带密码访问一次补上之后才生效。
    #[tokio::test]
    async fn legacy_admin_hash_gets_its_canonical_form_backfilled() {
        use axum::{
            Router,
            body::Body,
            extract::ConnectInfo,
            http::{Request, StatusCode},
            routing::get,
        };
        use tower::ServiceExt;
        let mut state = test_state();
        state.viewer_env = Some(std::sync::Arc::new("viewer-pw".into()));
        state
            .store
            .set_setting(crate::store::ADMIN_PASSWORD, &super::sha256_hex("admin-pw"))
            .unwrap();
        let app = Router::new()
            .route("/x", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn_with_state(state.clone(), super::require_admin))
            .with_state(state.clone());
        let call = |pw: &str| {
            let mut req = Request::builder()
                .uri("/x")
                .header(header::AUTHORIZATION, format!("Bearer {pw}"))
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo("127.0.0.1:5000".parse::<std::net::SocketAddr>().unwrap()));
            let app = app.clone();
            async move { app.oneshot(req).await.unwrap().status() }
        };
        assert_eq!(call("viewer-pw").await, StatusCode::UNAUTHORIZED, "规范形未知，访客先不认");
        assert_eq!(call("admin-pw").await, StatusCode::OK);
        assert_eq!(call("viewer-pw").await, StatusCode::OK, "管理员访问过一次后访客生效");
    }

    /// 请求用旧密码通过了鉴权、补存规范形却落在改密码之后：不能拿旧密码的规范形盖掉新的。
    #[test]
    fn stale_backfill_never_overwrites_the_new_canonical() {
        let state = test_state();
        state
            .store
            .set_setting(crate::store::ADMIN_PASSWORD, &super::sha256_hex("old-pw"))
            .unwrap();
        super::set_admin_password(&state, "new-pw").unwrap();
        super::backfill_admin_canonical(&state, "old-pw");
        assert_eq!(super::admin_canonical(&state), Some(super::canonical_hash("new-pw")));
        // 规范形缺着、但哈希已不是这个明文：同样不补。
        state.store.delete_setting(crate::store::ADMIN_PASSWORD_CANONICAL).unwrap();
        super::backfill_admin_canonical(&state, "old-pw");
        assert_eq!(super::admin_canonical(&state), None);
        super::backfill_admin_canonical(&state, "new-pw");
        assert_eq!(super::admin_canonical(&state), Some(super::canonical_hash("new-pw")));
    }

    /// 两个访客密码来回并发地写：无论怎么交错，最后库里的哈希与规范形属于同一个密码。
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_viewer_updates_keep_hash_and_canonical_paired() {
        use axum::{
            Json,
            extract::{ConnectInfo, State},
        };
        let state = test_state();
        super::set_admin_password(&state, "admin-pw").unwrap();
        let peer: std::net::SocketAddr = "127.0.0.1:5000".parse().unwrap();
        for round in 0..50 {
            let tasks: Vec<_> = ["viewer-a", "viewer-b"]
                .into_iter()
                .cycle()
                .take(8)
                .map(|pw| {
                    let state = state.clone();
                    tokio::spawn(async move {
                        super::set_viewer_password(
                            State(state),
                            ConnectInfo(peer),
                            bearer_headers("admin-pw"),
                            Json(super::PwReq { password: pw.into() }),
                        )
                        .await
                        .unwrap();
                    })
                })
                .collect();
            for t in tasks {
                t.await.unwrap();
            }
            let hash = super::configured_viewer_hash(&state).unwrap();
            let pw = ["viewer-a", "viewer-b"]
                .into_iter()
                .find(|p| super::sha256_hex(p) == hash)
                .unwrap();
            assert_eq!(
                super::viewer_canonical(&state),
                Some(super::canonical_hash(pw)),
                "round {round}"
            );
        }
    }

    #[test]
    fn canonical_decodes_every_layer() {
        assert_eq!(super::canonical("pass%252520word"), "pass word");
        // 层数多少都解到底：编码 20 层的与原文同一规范形。
        let mut deep = "pass word".to_owned();
        for _ in 0..20 {
            deep = deep.replace('%', "%25").replace(' ', "%20");
        }
        assert_eq!(super::canonical(&deep), "pass word");
        assert_eq!(super::canonical(" plain "), "plain");
        assert_eq!(super::canonical("100%"), "100%");
    }

    #[test]
    fn url_credentials_are_redacted_anywhere_in_text() {
        let r = |s: &str| super::redact_url_credentials(s).into_owned();
        assert_eq!(
            r(r#"{"ban_reason":"[proxy] invalid proxy URL: http://user:secret@host:0","x":1}"#),
            r#"{"ban_reason":"[proxy] invalid proxy URL: http://user:***@host:0","x":1}"#
        );
        assert_eq!(r(r#"["http://tok@h:2"]"#), r#"["http://***@h:2"]"#);
        // 密码里有未编码的 `/`、`'`、空格与转义过的引号，都不漏半截。
        assert_eq!(r(r#""http://u:pa/ss\"x@h:1""#), r#""http://u:***@h:1""#);
        assert_eq!(
            r(r#"{"proxy":"http://user:sec'ret@host:8080"}"#),
            r#"{"proxy":"http://user:***@host:8080"}"#
        );
        assert_eq!(
            r("invalid proxy URL: http://u:se cret<x>@h:0"),
            "invalid proxy URL: http://u:***@h:0"
        );
        // userinfo 只有 token、路径里又有 `@`：token 不能留。
        assert_eq!(r(r#""http://token@host:8080/path@tail""#), r#""http://***@tail""#);
        assert_eq!(r(r#""http://u:p@host:8080/a@b""#), r#""http://u:***@b""#);
        // 同一串里后面还有 `@` 时宁可多遮：host 丢了，密码不漏。
        assert_eq!(r("socks5h://u:p@h:1 and http://tok@h:2"), "socks5h://u:***@h:2");
        // 没有 userinfo 的地址、邮箱原样不动，且不分配新串。
        let plain = r#"{"url":"https://claude.ai/oauth?x=1","email":"a@b.com"}"#;
        assert!(matches!(super::redact_url_credentials(plain), std::borrow::Cow::Borrowed(_)));
    }

    #[tokio::test]
    async fn setup_requires_the_token_even_from_loopback() {
        use axum::{
            Json,
            extract::{ConnectInfo, State},
            http::StatusCode,
        };
        let state = test_state();
        let setup = |peer: &str, token: Option<&str>, password: &str| {
            super::setup(
                State(state.clone()),
                ConnectInfo(peer.parse().unwrap()),
                with_host("127.0.0.1:4600"),
                Json(super::SetupReq { password: password.into(), token: token.map(Into::into) }),
            )
        };
        let lo = "127.0.0.1:5000";
        assert_eq!(setup(lo, None, "pw1234").await.unwrap_err().0, StatusCode::FORBIDDEN);
        assert_eq!(setup(lo, Some("wrong"), "pw1234").await.unwrap_err().0, StatusCode::FORBIDDEN);
        let token = state.setup_token.to_string();
        assert_eq!(
            setup(lo, Some(&token), "pw").await.unwrap_err().0,
            StatusCode::BAD_REQUEST,
            "口令对、密码太短"
        );
        assert!(!super::admin_configured(&state), "失败的请求不能落库");

        let _ = setup("172.17.0.1:5000", Some(&format!(" {token} ")), "pw1234")
            .await
            .expect("带对口令应能设密码（首尾空白忽略）");
        assert!(super::admin_configured(&state));

        // 设过之后：带不带口令都回「已设置」，而不是一句口令不对。
        for t in [None, Some(token.as_str())] {
            let again = setup(lo, t, "pw5678").await.unwrap_err();
            assert_eq!(again.0, StatusCode::BAD_REQUEST, "设过之后不能再用 setup 覆盖");
            assert_eq!(again.1, "an admin password is already set");
        }
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn percent_decode_matches_encode_uri_component() {
        // encodeURIComponent('密码é%') === '%E5%AF%86%E7%A0%81%C3%A9%25'
        assert_eq!(percent_decode("%E5%AF%86%E7%A0%81%C3%A9%25").as_deref(), Some("密码é%"));
        assert_eq!(percent_decode("plain"), None);
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%+1"), None);
        assert_eq!(percent_decode("abc%2"), None);
    }
}
