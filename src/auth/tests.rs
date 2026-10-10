use super::constant_time_eq;
use crate::store::{LEGACY_SHA256_PREFIX, UserRole};
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    routing::{get, post},
};
use tower::ServiceExt;

fn with_host(host: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
    h
}

async fn test_state(pool: sqlx::PgPool) -> crate::web::AppState {
    let store = std::sync::Arc::new(crate::store::CredentialStore::for_test(pool.clone()).await);
    crate::web::AppState::for_test(store)
}

/// 给 admin 设上密码（直接写哈希，不走 setup）。
async fn set_admin_password(state: &crate::web::AppState, pw: &str) -> i64 {
    let admin = state.store.admin_user().await.unwrap();
    let hash = super::hash_password_blocking(pw).unwrap();
    state.store.set_user_password_hash(admin.id, &hash, None).await.unwrap();
    admin.id
}

/// 给某个账号签一个会话（指纹按它此刻的密码算），回 token 明文。
async fn session_for(state: &crate::web::AppState, user_id: i64) -> String {
    let user = state.store.user_by_id(user_id).await.unwrap().unwrap();
    let hash = state.store.user_password_hash(user_id).await.unwrap().unwrap_or_default();
    let verified =
        super::Verified { tag: super::password_tag(state, user.role, &hash), expected_hash: None };
    super::issue_session(state, user_id, &verified).await.unwrap().unwrap()
}

async fn create(state: &crate::web::AppState, name: &str, role: UserRole, parent: i64) -> i64 {
    let hash = super::hash_password_blocking("pw1234").unwrap();
    state.store.create_user(name, &hash, role, parent).await.unwrap().unwrap().id
}

async fn add_cred(state: &crate::web::AppState, owner: i64, rt: &str) -> i64 {
    state.store.insert(rt, None, "at", rt, u64::MAX, None, None, owner).await.unwrap().id
}

/// 与真实装配同形的路由（挂在 `/api` 下走 `MatchedPath`），handler 回当前身份。
fn app(state: &crate::web::AppState) -> Router {
    let who =
        |axum::Extension(a): axum::Extension<super::Actor>| async move { format!("{:?}", a.role) };
    Router::new().nest(
        "/api",
        Router::new()
            .route("/credentials", get(who))
            .route("/credentials/disabled", post(who))
            .route("/credentials/{id}/label", post(who))
            .route("/credentials/{id}/usage", get(who))
            .route("/proxies/{id}", post(who))
            .route("/proxies/delete", post(who))
            .route("/settings", get(who))
            .route("/settings/forwarding", post(who))
            .route("/export", get(who))
            .route("/users", get(who).post(who))
            .route("/auth/me", get(who))
            .route("/auth/logout", post(who))
            .route("/authorize", get(who))
            .route("/exchange", post(who))
            .route("/groups", get(who).post(who))
            .route("/provision-keys", get(who).post(who))
            .route("/proxies", get(who))
            .route_layer(axum::middleware::from_fn_with_state(state.clone(), super::require_login))
            .with_state(state.clone()),
    )
}

async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: &str,
) -> StatusCode {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req =
        req.header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_owned()));
    app.clone().oneshot(req.unwrap()).await.unwrap().status()
}

/// 未设密码一律拒，带什么都不行；设了之后只认会话 token，老的「密码当 Bearer」不再认。
#[sqlx::test]
async fn unset_password_rejects_and_only_sessions_are_accepted(pool: sqlx::PgPool) {
    let state = test_state(pool.clone()).await;
    let app = app(&state);
    let admin = state.store.admin_user().await.unwrap();
    // 未设密码时连有效会话也不放（清除管理密码后残留的会话）。
    let stale = session_for(&state, admin.id).await;
    assert_eq!(
        call(&app, Method::GET, "/api/credentials", Some(&stale), "").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, Method::GET, "/api/credentials", None, "").await,
        StatusCode::UNAUTHORIZED
    );

    set_admin_password(&state, "pw1234").await;
    assert_eq!(
        call(&app, Method::GET, "/api/credentials", Some("pw1234"), "").await,
        StatusCode::UNAUTHORIZED,
        "密码本身不能当 Bearer 用"
    );
    let token = session_for(&state, admin.id).await;
    assert_eq!(call(&app, Method::GET, "/api/credentials", Some(&token), "").await, StatusCode::OK);
    assert_eq!(
        call(&app, Method::GET, "/api/credentials", Some("nope"), "").await,
        StatusCode::UNAUTHORIZED
    );
}

/// 代理和用户：只给列出的路由，落在别人的号 / 代理上的回 404；默认只给 admin。
#[sqlx::test]
async fn members_are_scoped_to_their_own_things(pool: sqlx::PgPool) {
    let state = test_state(pool.clone()).await;
    let admin_id = set_admin_password(&state, "pw1234").await;
    let app = app(&state);
    let agent = create(&state, "agent1", UserRole::Agent, admin_id).await;
    let user = create(&state, "user1", UserRole::User, agent).await;
    let mine = add_cred(&state, agent, "r-agent").await;
    let theirs = add_cred(&state, user, "r-user").await;
    let admins = add_cred(&state, admin_id, "r-admin").await;
    let my_proxy = state.store.add_proxy(agent, "p", "http://h:1").await.unwrap().id;
    let admin_proxy = state.store.add_proxy(admin_id, "p", "http://h:1").await.unwrap().id;
    let a = session_for(&state, agent).await;
    let u = session_for(&state, user).await;

    let s = |m: Method, uri: String, tok: &str, body: String| {
        let app = app.clone();
        let tok = tok.to_owned();
        async move { call(&app, m, &uri, Some(&tok), &body).await }
    };
    // 默认只给 admin。
    assert_eq!(
        s(Method::GET, "/api/settings".into(), &a, String::new()).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        s(Method::POST, "/api/settings/forwarding".into(), &a, String::new()).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        s(Method::GET, "/api/export".into(), &u, String::new()).await,
        StatusCode::FORBIDDEN
    );
    // 自己的号放行，别人的（下属的、admin 的）一律 404。
    assert_eq!(
        s(Method::POST, format!("/api/credentials/{mine}/label"), &a, "{}".into()).await,
        StatusCode::OK
    );
    assert_eq!(
        s(Method::GET, format!("/api/credentials/{theirs}/usage"), &a, String::new()).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s(Method::POST, format!("/api/credentials/{admins}/label"), &u, "{}".into()).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s(Method::GET, "/api/credentials/99999/usage".into(), &a, String::new()).await,
        StatusCode::NOT_FOUND
    );
    // 批量接口按 body 里的 ids 核对，混进一个别人的就整批拒。
    let body = |ids: &[i64]| format!(r#"{{"ids":{ids:?},"disabled":true}}"#);
    assert_eq!(
        s(Method::POST, "/api/credentials/disabled".into(), &a, body(&[mine])).await,
        StatusCode::OK
    );
    assert_eq!(
        s(Method::POST, "/api/credentials/disabled".into(), &a, body(&[mine, theirs])).await,
        StatusCode::NOT_FOUND
    );
    // 出口代理同理。
    assert_eq!(
        s(Method::POST, format!("/api/proxies/{my_proxy}"), &a, "{}".into()).await,
        StatusCode::OK
    );
    assert_eq!(
        s(Method::POST, format!("/api/proxies/{admin_proxy}"), &a, "{}".into()).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s(Method::POST, "/api/proxies/delete".into(), &a, format!(r#"{{"ids":[{admin_proxy}]}}"#))
            .await,
        StatusCode::NOT_FOUND
    );
    // 用户管理：代理能进，用户不能。
    assert_eq!(s(Method::GET, "/api/users".into(), &a, String::new()).await, StatusCode::OK);
    assert_eq!(s(Method::GET, "/api/users".into(), &u, String::new()).await, StatusCode::FORBIDDEN);
    assert_eq!(s(Method::GET, "/api/auth/me".into(), &u, String::new()).await, StatusCode::OK);
}

/// 访客只能打 GET，`/export`、`/users` 也不给；admin 全部放行。
#[sqlx::test]
async fn viewer_is_read_only_and_admin_sees_everything(pool: sqlx::PgPool) {
    let state = test_state(pool.clone()).await;
    let admin_id = set_admin_password(&state, "pw1234").await;
    let app = app(&state);
    state.store.upsert_viewer(&super::hash_password_blocking("view-pw").unwrap()).await.unwrap();
    let viewer = state.store.viewer_user().await.unwrap().unwrap();
    let v = session_for(&state, viewer.id).await;
    let ad = session_for(&state, admin_id).await;
    let user = create(&state, "user1", UserRole::User, admin_id).await;
    let theirs = add_cred(&state, user, "r-user").await;

    assert_eq!(call(&app, Method::GET, "/api/credentials", Some(&v), "").await, StatusCode::OK);
    assert_eq!(call(&app, Method::GET, "/api/settings", Some(&v), "").await, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Method::GET, "/api/export", Some(&v), "").await, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Method::GET, "/api/users", Some(&v), "").await, StatusCode::FORBIDDEN);
    assert_eq!(
        call(&app, Method::POST, &format!("/api/credentials/{theirs}/label"), Some(&v), "{}").await,
        StatusCode::FORBIDDEN
    );
    for (m, uri) in [
        (Method::GET, "/api/settings".to_string()),
        (Method::GET, "/api/export".to_string()),
        (Method::POST, format!("/api/credentials/{theirs}/label")),
        (Method::GET, "/api/users".to_string()),
    ] {
        assert_eq!(call(&app, m, &uri, Some(&ad), "{}").await, StatusCode::OK, "{uri}");
    }
    // 清掉访客：它的会话随即失效。
    state.store.delete_viewer().await.unwrap();
    assert_eq!(
        call(&app, Method::GET, "/api/credentials", Some(&v), "").await,
        StatusCode::UNAUTHORIZED
    );
}

/// 停代理连带它名下的用户：会话失效、登录被拒、名下的号不接流量。
#[sqlx::test]
async fn disabling_an_agent_disables_its_branch(pool: sqlx::PgPool) {
    use axum::{
        Json,
        extract::{ConnectInfo, State},
    };
    let state = test_state(pool.clone()).await;
    let admin_id = set_admin_password(&state, "pw1234").await;
    let app = app(&state);
    let agent = create(&state, "agent1", UserRole::Agent, admin_id).await;
    let user = create(&state, "user1", UserRole::User, agent).await;
    let cred = add_cred(&state, user, "r-user").await;
    let u = session_for(&state, user).await;
    assert_eq!(call(&app, Method::GET, "/api/credentials", Some(&u), "").await, StatusCode::OK);

    state.store.set_user_disabled(agent, true, None).await.unwrap();
    assert_eq!(
        call(&app, Method::GET, "/api/credentials", Some(&u), "").await,
        StatusCode::UNAUTHORIZED
    );
    let login = super::login(
        State(state.clone()),
        ConnectInfo("127.0.0.1:5000".parse().unwrap()),
        with_host("127.0.0.1:4600"),
        Json(super::LoginReq { username: "user1".into(), password: "pw1234".into() }),
    )
    .await;
    assert_eq!(login.err().map(|e| e.0), Some(StatusCode::FORBIDDEN));

    let sel = crate::store::Select::default();
    assert!(state.store.select_for_device(sel).await.is_err(), "号主停用后这个号不接流量");
    state.store.set_user_disabled(agent, false, None).await.unwrap();
    let sel = crate::store::Select::default();
    assert_eq!(state.store.select_for_device(sel).await.unwrap().id, cred);
}

/// 旧版 settings 里的无盐 sha256 迁移进 users，登录校验通过后换成 argon2。
#[sqlx::test]
async fn legacy_admin_hash_logs_in_and_gets_upgraded(pool: sqlx::PgPool) {
    use axum::{
        Json,
        extract::{ConnectInfo, State},
    };
    let state = test_state(pool.clone()).await;
    let admin = state.store.admin_user().await.unwrap();
    let legacy = format!("{LEGACY_SHA256_PREFIX}{}", super::sha256_hex("old-pw"));
    state.store.set_user_password_hash(admin.id, &legacy, None).await.unwrap();
    let login = |pw: &str| {
        super::login(
            State(state.clone()),
            ConnectInfo("127.0.0.1:5000".parse().unwrap()),
            with_host("127.0.0.1:4600"),
            Json(super::LoginReq { username: "ADMIN".into(), password: pw.into() }),
        )
    };
    assert_eq!(login("wrong").await.err().map(|e| e.0), Some(StatusCode::UNAUTHORIZED));
    let ok = login("old-pw").await.expect("旧哈希应能登录（用户名不区分大小写）");
    assert_eq!(ok.0.role, UserRole::Admin);
    let stored = state.store.user_password_hash(admin.id).await.unwrap().unwrap();
    assert!(stored.starts_with("$argon2id$"), "登录后应换成 argon2：{stored}");
    let _ = login("old-pw").await.expect("换成 argon2 后照样能登录");
}

/// 同一用户名连续失败到上限后，窗口内对的密码也回 429。
#[sqlx::test]
async fn repeated_failures_lock_the_username(pool: sqlx::PgPool) {
    use axum::{
        Json,
        extract::{ConnectInfo, State},
    };
    let state = test_state(pool.clone()).await;
    let admin_id = set_admin_password(&state, "pw1234").await;
    create(&state, "locked-user", UserRole::User, admin_id).await;
    let login = |pw: &str| {
        super::login(
            State(state.clone()),
            ConnectInfo("127.0.0.1:5000".parse().unwrap()),
            with_host("127.0.0.1:4600"),
            Json(super::LoginReq { username: "locked-user".into(), password: pw.into() }),
        )
    };
    for _ in 0..super::LOGIN_FAIL_MAX {
        assert_eq!(login("bad").await.err().map(|e| e.0), Some(StatusCode::UNAUTHORIZED));
    }
    assert_eq!(login("pw1234").await.err().map(|e| e.0), Some(StatusCode::TOO_MANY_REQUESTS));
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

#[sqlx::test]
async fn setup_requires_the_token_even_from_loopback(pool: sqlx::PgPool) {
    use axum::{
        Json,
        extract::{ConnectInfo, State},
    };
    let state = test_state(pool.clone()).await;
    let setup = |peer: &str, token: Option<&str>, password: &str| {
        super::setup(
            State(state.clone()),
            ConnectInfo(peer.parse().unwrap()),
            with_host("127.0.0.1:4600"),
            Json(super::SetupReq { password: password.into(), token: token.map(Into::into) }),
        )
    };
    let lo = "127.0.0.1:5000";
    assert_eq!(setup(lo, None, "pw1234").await.err().unwrap().0, StatusCode::FORBIDDEN);
    assert_eq!(setup(lo, Some("wrong"), "pw1234").await.err().unwrap().0, StatusCode::FORBIDDEN);
    let token = state.setup_token.to_string();
    assert_eq!(
        setup(lo, Some(&token), "pw").await.err().unwrap().0,
        StatusCode::BAD_REQUEST,
        "口令对、密码太短"
    );
    assert!(!super::admin_configured(&state).await, "失败的请求不能落库");

    let resp = setup("172.17.0.1:5000", Some(&format!(" {token} ")), "pw1234")
        .await
        .expect("带对口令应能设密码（首尾空白忽略）");
    assert!(super::admin_configured(&state).await);
    assert!(!resp.0.token.is_empty(), "设完密码直接登录");

    // 设过之后：带不带口令都回「已设置」，而不是一句口令不对。
    for t in [None, Some(token.as_str())] {
        let again = setup(lo, t, "pw5678").await.err().unwrap();
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

/// 会话绑定密码指纹：改了密码、环境变量里的密码换了或撤了，旧会话都失效；访客能退出登录。
#[sqlx::test]
async fn sessions_die_with_the_password_they_were_issued_under(pool: sqlx::PgPool) {
    let state = test_state(pool.clone()).await;
    let admin_id = set_admin_password(&state, "pw1234").await;
    let user = create(&state, "user1", UserRole::User, admin_id).await;
    let tok = session_for(&state, user).await;
    let main = app(&state);
    assert_eq!(call(&main, Method::GET, "/api/auth/me", Some(&tok), "").await, StatusCode::OK);
    // 库里的密码被重置（哪怕会话没被显式删掉），指纹对不上即失效。
    let hash = super::hash_password_blocking("new-pw").unwrap();
    state.store.set_user_password_hash(user, &hash, None).await.unwrap();
    assert_eq!(
        call(&main, Method::GET, "/api/auth/me", Some(&tok), "").await,
        StatusCode::UNAUTHORIZED
    );

    // 环境变量接管的管理密码换掉之后，按旧环境密码签的会话失效。
    let mut env_a = state.clone();
    env_a.admin_env = Some(std::sync::Arc::new("env-a".into()));
    super::sync_env_accounts(&env_a).await;
    let admin_tok = session_for(&env_a, admin_id).await;
    // 库里只落环境密码的 argon2 哈希与版本号，会话指纹只是版本号。
    let stored = state.store.get_setting(crate::store::ADMIN_ENV_PASSWORD_HASH).unwrap().unwrap();
    assert!(stored.starts_with("$argon2id$"), "{stored}");
    assert_eq!(super::password_tag(&env_a, UserRole::Admin, ""), "env:v1");
    assert_eq!(
        call(&app(&env_a), Method::GET, "/api/auth/me", Some(&admin_tok), "").await,
        StatusCode::OK
    );
    let mut env_b = state.clone();
    env_b.admin_env = Some(std::sync::Arc::new("env-b".into()));
    super::sync_env_accounts(&env_b).await;
    assert_eq!(
        call(&app(&env_b), Method::GET, "/api/auth/me", Some(&admin_tok), "").await,
        StatusCode::UNAUTHORIZED
    );

    // 撤掉访客的环境密码后，旧访客会话失效。
    let mut with_viewer = state.clone();
    with_viewer.viewer_env = Some(std::sync::Arc::new("view-env".into()));
    super::sync_env_accounts(&with_viewer).await;
    let viewer = state.store.viewer_user().await.unwrap().unwrap();
    let v = session_for(&with_viewer, viewer.id).await;
    let vapp = app(&with_viewer);
    assert_eq!(call(&vapp, Method::GET, "/api/credentials", Some(&v), "").await, StatusCode::OK);
    assert_eq!(
        call(&app(&state), Method::GET, "/api/credentials", Some(&v), "").await,
        StatusCode::UNAUTHORIZED
    );

    // 访客退出登录（POST）放行，退出后会话即失效。
    let v2 = session_for(&with_viewer, viewer.id).await;
    assert_eq!(call(&vapp, Method::POST, "/api/auth/logout", Some(&v2), "").await, StatusCode::OK);
}

/// 校验到签发之间密码被改了：按旧哈希核对的签发不落会话。
#[sqlx::test]
async fn session_is_not_issued_against_a_stale_password_hash(pool: sqlx::PgPool) {
    let state = test_state(pool.clone()).await;
    let admin_id = set_admin_password(&state, "pw1234").await;
    let stale = state.store.user_password_hash(admin_id).await.unwrap().unwrap();
    let verified = super::Verified {
        tag: super::password_tag(&state, UserRole::Admin, &stale),
        expected_hash: Some(stale),
    };
    set_admin_password(&state, "changed").await;
    assert!(super::issue_session(&state, admin_id, &verified).await.unwrap().is_none());
}

/// 超长用户名在入口就拒，不进失败计数表。
#[sqlx::test]
async fn overlong_usernames_are_rejected_without_being_tracked(pool: sqlx::PgPool) {
    use axum::{
        Json,
        extract::{ConnectInfo, State},
    };
    let state = test_state(pool.clone()).await;
    set_admin_password(&state, "pw1234").await;
    let name = "x".repeat(super::MAX_LOGIN_USERNAME_LEN + 1);
    let resp = super::login(
        State(state.clone()),
        ConnectInfo("127.0.0.1:5000".parse().unwrap()),
        with_host("127.0.0.1:4600"),
        Json(super::LoginReq { username: name.clone(), password: "x".into() }),
    )
    .await;
    assert_eq!(resp.err().map(|e| e.0), Some(StatusCode::UNAUTHORIZED));
    let guard = super::LOGIN_GUARD.lock();
    assert!(!guard.known.contains_key(&name) && !guard.unknown.contains_key(&name));
}

/// 并发登录：名额在校验密码之前预占，同时涌进来 20 个错密码请求，只有 10 个进得了校验。
#[sqlx::test]
async fn concurrent_logins_cannot_exceed_the_attempt_budget(pool: sqlx::PgPool) {
    use axum::{
        Json,
        extract::{ConnectInfo, State},
    };
    let state = test_state(pool.clone()).await;
    let admin_id = set_admin_password(&state, "pw1234").await;
    create(&state, "burst-user", UserRole::User, admin_id).await;
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let state = state.clone();
            tokio::spawn(async move {
                super::login(
                    State(state),
                    ConnectInfo("127.0.0.1:5000".parse().unwrap()),
                    with_host("127.0.0.1:4600"),
                    Json(super::LoginReq { username: "burst-user".into(), password: "bad".into() }),
                )
                .await
                .err()
                .map(|e| e.0)
            })
        })
        .collect();
    let mut unauthorized = 0;
    let mut throttled = 0;
    for t in tasks {
        match t.await.unwrap() {
            Some(StatusCode::UNAUTHORIZED) => unauthorized += 1,
            Some(StatusCode::TOO_MANY_REQUESTS) => throttled += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!((unauthorized, throttled), (super::LOGIN_FAIL_MAX as usize, 10));
}

/// 不存在的用户名再多也挤不掉真实账号的锁定记录；不存在的那张表满了只淘汰没锁住的。
#[test]
fn unknown_username_flood_cannot_unlock_a_real_account() {
    let key = "flood-target-admin";
    for _ in 0..super::LOGIN_FAIL_MAX {
        assert!(super::reserve_login_attempt(key, true));
    }
    assert!(!super::reserve_login_attempt(key, true));
    for i in 0..super::LOGIN_UNKNOWN_CAPACITY + 50 {
        super::reserve_login_attempt(&format!("flood-nobody-{i}"), false);
    }
    assert!(!super::reserve_login_attempt(key, true), "真实账号的锁定不能被挤掉");
    assert!(super::LOGIN_GUARD.lock().unknown.len() <= super::LOGIN_UNKNOWN_CAPACITY);
}

/// 上号 Key：只认上号那几条路由，以所属账号的身份进 handler；停用、删除、账号停用都随即失效。
#[sqlx::test]
async fn provision_keys_only_reach_the_add_account_routes(pool: sqlx::PgPool) {
    let state = test_state(pool.clone()).await;
    let admin_id = set_admin_password(&state, "pw1234").await;
    let app = app(&state);
    let agent = create(&state, "agent1", UserRole::Agent, admin_id).await;
    let user = create(&state, "user1", UserRole::User, agent).await;
    let new_key = async |owner: i64| {
        let key = crate::store::generate_provision_key();
        let hash = state.store.user_password_hash(owner).await.unwrap().unwrap_or_default();
        let tag = super::password_tag(&state, UserRole::User, &hash);
        (state.store.create_provision_key(owner, "script", &key, &tag).await.unwrap(), key)
    };
    let (id, key) = new_key(user).await;

    for (m, uri) in [
        (Method::GET, "/api/authorize"),
        (Method::POST, "/api/exchange"),
        (Method::GET, "/api/groups"),
        (Method::GET, "/api/proxies"),
    ] {
        assert_eq!(call(&app, m, uri, Some(&key), "{}").await, StatusCode::OK, "{uri}");
    }
    for (m, uri) in [
        (Method::GET, "/api/credentials"),
        (Method::POST, "/api/groups"),
        (Method::GET, "/api/provision-keys"),
        (Method::POST, "/api/provision-keys"),
        (Method::GET, "/api/auth/me"),
        (Method::GET, "/api/export"),
    ] {
        assert_eq!(call(&app, m, uri, Some(&key), "{}").await, StatusCode::FORBIDDEN, "{uri}");
    }
    assert_eq!(
        call(&app, Method::GET, "/api/authorize", Some("lbp-nope"), "").await,
        StatusCode::UNAUTHORIZED
    );

    // 上级停用：所属账号生效停用，Key 跟着失效。
    state.store.set_user_disabled(agent, true, None).await.unwrap();
    assert_eq!(
        call(&app, Method::GET, "/api/authorize", Some(&key), "").await,
        StatusCode::UNAUTHORIZED
    );
    state.store.set_user_disabled(agent, false, None).await.unwrap();

    // 别人改不了、删不了；本人停用后失效。
    assert!(!state.store.update_provision_key(id, Some(agent), "x", true).await.unwrap());
    assert!(!state.store.delete_provision_key(id, Some(agent)).await.unwrap());
    assert!(state.store.update_provision_key(id, Some(user), "script", true).await.unwrap());
    assert_eq!(
        call(&app, Method::GET, "/api/authorize", Some(&key), "").await,
        StatusCode::UNAUTHORIZED
    );
    let keys = state.store.list_provision_keys(None).await.unwrap();
    assert_eq!((keys.len(), keys[0].username.as_str()), (1, "user1"));
    assert!(keys[0].last_used_at.is_some());
    assert!(state.store.list_provision_keys(Some(agent)).await.unwrap().is_empty());

    // 改密码（含被重置）：建 Key 时的指纹对不上，Key 作废并被删掉。
    let (id2, key2) = new_key(user).await;
    assert_eq!(call(&app, Method::GET, "/api/authorize", Some(&key2), "").await, StatusCode::OK);
    let reset = super::hash_password_blocking("new-pw").unwrap();
    state.store.set_user_password_hash(user, &reset, None).await.unwrap();
    assert_eq!(
        call(&app, Method::GET, "/api/authorize", Some(&key2), "").await,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        !state.store.list_provision_keys(None).await.unwrap().iter().any(|k| k.id == id2),
        "作废的 Key 认证时就删掉"
    );

    // 删掉账号连带删 Key。
    state.store.delete_user(user, None).await.unwrap().unwrap();
    assert!(state.store.list_provision_keys(None).await.unwrap().is_empty());
}
