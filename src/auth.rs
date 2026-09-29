//! 管理界面登录鉴权。
//!
//! 密码以 sha256 存于 SQLite（`admin_password_sha256`），或由 `LUBAN_ADMIN_PASSWORD`
//! 环境接管。设置后 `/api/*` 管理接口需带 `Authorization: Bearer <password>`。
//! 转发代理 `/v1/*` 不走这里。
//!
//! **未设密码时只对本机免密**（判定见 [`is_local_request`]）。默认监听 `0.0.0.0`、Docker 也把
//! 端口发布到所有网卡，此前未设密码等于管理面对网络匿名敞开：谁先连上谁就能设密码、导出全部
//! 明文 token。远程来访要先设密码，而设密码得带启动日志里打印的一次性初始化口令
//! （[`AppState::setup_token`]），证明来人能看到这台服务的日志。

use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{StatusCode, header},
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

/// 是否已启用管理鉴权（环境接管或库里存了哈希）。
///
/// 给那些「开着鉴权才允许」的接口用：管理接口在未设密码时是**完全敞开**的
/// （见 [`require_admin`]），而个别接口给出去的东西比「能改配置」更重
/// （如导出含明文 token 的迁移文件），它们得自己确认这道门锁着。
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

/// 这条请求是不是**本机浏览器直接打开的控制台**：TCP 对端是回环地址，且 `Host` 是
/// `localhost` 或回环 IP。未设密码时只有这类请求免密。
///
/// 两个条件缺一不可：
/// - 对端地址挡的是网络上的其它机器。只认 TCP 对端，不看 `x-forwarded-for`——那是来访自己
///   填的。Docker 端口映射进来的请求对端是网桥网关（如 `172.17.0.1`），不算本机，这是有意的：
///   宿主机外面的人同样经这条路进来，分不出来。
/// - `Host` 挡的是 **DNS rebinding**：管理员浏览器里的恶意网页把自己的域名重新解析到
///   127.0.0.1，请求的对端就是回环，页面与控制台还同源，能读写全部接口。这种请求的 `Host`
///   必然是攻击者的域名，回环 IP 字面量与 `localhost` 它伪造不出来。同机反代（nginx 转到
///   127.0.0.1）带的是对外域名，同样不算本机——反代后面的人本来就可能来自网络。
pub(crate) fn is_local_request(
    peer: Option<std::net::SocketAddr>,
    headers: &header::HeaderMap,
) -> bool {
    let Some(peer) = peer else { return false };
    if !peer.ip().to_canonical().is_loopback() {
        return false;
    }
    headers.get(header::HOST).and_then(|v| v.to_str().ok()).is_some_and(host_is_loopback)
}

/// `Host` 头（可带端口）是不是 `localhost`、`*.localhost` 或回环 IP 字面量。
fn host_is_loopback(host: &str) -> bool {
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 字面量：`[::1]` 或 `[::1]:4600`。
        match rest.split_once(']') {
            Some((ip, _)) => ip,
            None => return false,
        }
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
            _ => host,
        }
    };
    if let Ok(ip) = name.parse::<std::net::IpAddr>() {
        return ip.to_canonical().is_loopback();
    }
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost" || name.ends_with(".localhost")
}

/// 从中间件拿到的请求里取 TCP 对端（`into_make_service_with_connect_info` 放进扩展的）。
fn peer_of(req: &Request) -> Option<std::net::SocketAddr> {
    req.extensions().get::<ConnectInfo<std::net::SocketAddr>>().map(|c| c.0)
}

/// 未设密码的远程来访被拒时回的文案，前端据此切到「初始化管理密码」页。
const SETUP_REQUIRED: &str =
    "set an admin password first: remote access requires the setup token from the server log";

/// 中间件：已设密码时校验 `Authorization: Bearer <password>`；未设时只放行本机来访
/// （[`is_local_request`]），其余一律 401。
///
/// 回 401 而不是 403：前端的拦截器见 401 就重新拉一次鉴权状态，未设密码的远程来访由此
/// 落到初始化页，不必再为这一种情况单写一条分支。
pub async fn require_admin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(hash) = admin_hash(&state) else {
        if is_local_request(peer_of(&req), req.headers()) {
            return next.run(req).await;
        }
        return (StatusCode::UNAUTHORIZED, SETUP_REQUIRED).into_response();
    };
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|pw| {
            let pw = pw.trim();
            // 先按原文比（脚本直接带明文），再按百分号解码后比（网页端为支持非 ASCII 密码会编码）。
            sha256_hex(pw) == hash
                || percent_decode(pw).is_some_and(|d| sha256_hex(d.trim()) == hash)
        })
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "admin password required").into_response()
    }
}

#[derive(Serialize)]
pub struct StateResp {
    /// 是否已设置管理密码（true = 需登录）。
    configured: bool,
    /// 是否由环境变量接管（true = 网页不可改）。
    env_managed: bool,
    /// 未设密码、且这条请求不是本机来访：要先用初始化口令设密码才能进控制台。
    setup_required: bool,
}

/// 鉴权状态（公开）。
pub async fn state(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
) -> Json<StateResp> {
    let configured = admin_hash(&state).is_some();
    Json(StateResp {
        configured,
        env_managed: state.admin_env.is_some(),
        setup_required: !configured && !is_local_request(Some(peer), &headers),
    })
}

#[derive(Deserialize)]
pub struct PwReq {
    password: String,
}

#[derive(Deserialize)]
pub struct SetupReq {
    password: String,
    /// 启动日志里的初始化口令；本机来访可不带。
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

/// 未设密码时把初始化口令打进日志：远程来访设密码只能靠它。启动时与清除密码后各打一次。
pub(crate) fn log_setup_token(token: &str) {
    tracing::warn!(
        setup_token = %token,
        "no admin password is set: the console only accepts requests from this machine; \
         to set a password from another machine (including through Docker port mapping), \
         open the console and enter this setup token"
    );
}

/// 串行化首次设置密码的「查有没有设过 → 写入」：两条并发的 setup 不能都判成「还没设」、
/// 后写的那条悄悄把先设的密码盖掉。
static SETUP_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

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
        Some(h) if sha256_hex(req.password.trim()) == h => {
            tracing::info!(%ip, "admin login succeeded");
            Ok(ok_json())
        }
        _ => {
            tracing::warn!(%ip, "admin login failed: wrong password");
            Err((StatusCode::UNAUTHORIZED, "wrong password".into()))
        }
    }
}

/// 首次设置密码（仅未配置时，公开）。本机来访直接设；其余须带启动日志里的初始化口令，
/// 否则谁先连上端口谁就能把密码定下来、接管整个管理面。
pub async fn setup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<SetupReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ip = client_ip(&headers, peer);
    let local = is_local_request(Some(peer), &headers);
    if !local {
        let given = req.token.as_deref().map(str::trim).unwrap_or("");
        if !constant_time_eq(given.as_bytes(), state.setup_token.as_bytes()) {
            tracing::warn!(%ip, "admin password setup rejected: invalid setup token");
            return Err((StatusCode::FORBIDDEN, "invalid setup token".into()));
        }
    }
    let pw = req.password.trim();
    if pw.len() < 4 {
        return Err((StatusCode::BAD_REQUEST, "password must be at least 4 characters".into()));
    }
    let _guard = SETUP_LOCK.lock();
    if admin_hash(&state).is_some() {
        return Err((StatusCode::BAD_REQUEST, "an admin password is already set".into()));
    }
    state.store.set_setting(store::ADMIN_PASSWORD, &sha256_hex(pw)).map_err(internal)?;
    // 谁在什么时候把密码定下来的，比任何一项设置变更都更该留痕。
    tracing::info!(%ip, local, "admin password set for the first time");
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
    if cleared {
        state.store.delete_setting(store::ADMIN_PASSWORD).map_err(internal)?;
    } else {
        if pw.len() < 4 {
            return Err((StatusCode::BAD_REQUEST, "password must be at least 4 characters".into()));
        }
        state.store.set_setting(store::ADMIN_PASSWORD, &sha256_hex(pw)).map_err(internal)?;
    }
    tracing::info!(
        ip = %client_ip(&headers, peer),
        cleared,
        "admin password changed"
    );
    if cleared {
        // 清掉之后远程来访又得靠初始化口令，把它重新打出来，免得还要翻启动时那一行。
        log_setup_token(&state.setup_token);
    }
    Ok(ok_json())
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, host_is_loopback, is_local_request, percent_decode};
    use axum::http::{HeaderMap, HeaderValue, header};

    fn with_host(host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        h
    }

    #[test]
    fn loopback_hosts() {
        for host in [
            "localhost",
            "localhost:4600",
            "LOCALHOST.",
            "app.localhost:1",
            "127.0.0.1",
            "127.0.0.1:4600",
            "127.8.9.10",
            "[::1]",
            "[::1]:4600",
            "[::ffff:127.0.0.1]:4600",
        ] {
            assert!(host_is_loopback(host), "{host} 应算本机");
        }
        for host in [
            "evil.example",
            "evil.example:4600",
            "localhost.evil.example",
            "192.168.1.5:4600",
            "0.0.0.0",
            "[::]",
            "[::1",
            "",
            "luban.example.com",
        ] {
            assert!(!host_is_loopback(host), "{host} 不应算本机");
        }
    }

    #[test]
    fn local_request_needs_loopback_peer_and_host() {
        let lo: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let mapped: std::net::SocketAddr = "[::ffff:127.0.0.1]:50000".parse().unwrap();
        let docker: std::net::SocketAddr = "172.17.0.1:50000".parse().unwrap();
        assert!(is_local_request(Some(lo), &with_host("127.0.0.1:4600")));
        assert!(is_local_request(Some(mapped), &with_host("localhost:4600")));
        // DNS rebinding：对端是回环，Host 是攻击者的域名。
        assert!(!is_local_request(Some(lo), &with_host("evil.example:4600")));
        // Docker 端口映射 / 局域网：对端不是回环，Host 写什么都不算。
        assert!(!is_local_request(Some(docker), &with_host("localhost:4600")));
        assert!(!is_local_request(Some(lo), &HeaderMap::new()), "没有 Host 不算本机");
        assert!(!is_local_request(None, &with_host("localhost")), "拿不到对端不算本机");
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

    #[tokio::test]
    async fn unset_password_only_admits_local_requests() {
        use axum::http::StatusCode;
        let state = test_state();
        let local = protected_status(&state, "127.0.0.1:5000", "127.0.0.1:4600", None).await;
        assert_eq!(local, StatusCode::OK, "本机直连免密");
        let docker = protected_status(&state, "172.17.0.1:5000", "localhost:4600", None).await;
        assert_eq!(docker, StatusCode::UNAUTHORIZED, "Docker 端口映射 / 局域网要先设密码");
        let rebinding = protected_status(&state, "127.0.0.1:5000", "evil.example:4600", None).await;
        assert_eq!(rebinding, StatusCode::UNAUTHORIZED, "DNS rebinding 不算本机");

        // 设了密码之后本机也要带密码，带对了远程也能进。
        state
            .store
            .set_setting(crate::store::ADMIN_PASSWORD, &super::sha256_hex("pw1234"))
            .unwrap();
        let local = protected_status(&state, "127.0.0.1:5000", "127.0.0.1:4600", None).await;
        assert_eq!(local, StatusCode::UNAUTHORIZED);
        let remote =
            protected_status(&state, "172.17.0.1:5000", "luban.example", Some("pw1234")).await;
        assert_eq!(remote, StatusCode::OK);
    }

    #[tokio::test]
    async fn remote_setup_requires_the_token() {
        use axum::{
            Json,
            extract::{ConnectInfo, State},
        };
        let state = test_state();
        let remote: std::net::SocketAddr = "172.17.0.1:5000".parse().unwrap();
        let setup = |token: Option<&str>| {
            super::setup(
                State(state.clone()),
                ConnectInfo(remote),
                with_host("luban.example:4600"),
                Json(super::SetupReq { password: "pw1234".into(), token: token.map(Into::into) }),
            )
        };
        assert_eq!(setup(None).await.unwrap_err().0, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(setup(Some("wrong")).await.unwrap_err().0, axum::http::StatusCode::FORBIDDEN);
        assert!(!super::admin_configured(&state), "口令不对不能落库");
        setup(Some(state.setup_token.as_str())).await.expect("带对口令应能设密码");
        assert!(super::admin_configured(&state));
        let again = setup(Some(state.setup_token.as_str())).await.unwrap_err();
        assert_eq!(again.0, axum::http::StatusCode::BAD_REQUEST, "设过之后不能再用 setup 覆盖");
    }

    #[tokio::test]
    async fn local_setup_needs_no_token() {
        use axum::{
            Json,
            extract::{ConnectInfo, State},
        };
        let state = test_state();
        super::setup(
            State(state.clone()),
            ConnectInfo("127.0.0.1:5000".parse().unwrap()),
            with_host("localhost:4600"),
            Json(super::SetupReq { password: "pw1234".into(), token: None }),
        )
        .await
        .expect("本机来访设密码不需要口令");
        assert!(super::admin_configured(&state));
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
