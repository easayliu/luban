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
    state.store.set_setting(crate::store::ADMIN_PASSWORD, &super::sha256_hex("pw1234")).unwrap();
    let local = protected_status(&state, "127.0.0.1:5000", "127.0.0.1:4600", None).await;
    assert_eq!(local, StatusCode::UNAUTHORIZED);
    let local = protected_status(&state, "127.0.0.1:5000", "127.0.0.1:4600", Some("pw1234")).await;
    assert_eq!(local, StatusCode::OK);
    let remote = protected_status(&state, "172.17.0.1:5000", "luban.example", Some("pw1234")).await;
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
            .route_layer(axum::middleware::from_fn_with_state(state.clone(), super::require_admin))
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
    for pw in ["pass word", "pass%20word", "pass%2520word", "pass%252520word", "pass%20%77ord"] {
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
    assert_eq!(role_of_bearer(&state, &admin, "pass%2520word"), Some(Role::Admin), "管理员网页端");
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
    state.store.set_setting(crate::store::ADMIN_PASSWORD, &super::sha256_hex("admin-pw")).unwrap();
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
    state.store.set_setting(crate::store::ADMIN_PASSWORD, &super::sha256_hex("old-pw")).unwrap();
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
        let pw =
            ["viewer-a", "viewer-b"].into_iter().find(|p| super::sha256_hex(p) == hash).unwrap();
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
