use super::*;
use crate::oauth::PkceChallenge;

fn tokens(account: Option<&str>, org: Option<&str>) -> oauth::TokenSet {
    oauth::TokenSet {
        access_token: "at".into(),
        refresh_token: "rt".into(),
        expires_at: 0,
        account: Some("from-token@example.com".into()),
        account_uuid: account.map(Into::into),
        organization_uuid: org.map(Into::into),
    }
}

/// 重新授权的同号校验：同一个人、同一个账号 UUID，选错组织（个人订阅 vs 团队席位）要拒绝。
#[test]
fn reauth_rejects_a_different_org_of_the_same_account() {
    let team = oauth::Profile {
        account_uuid: Some("acct".into()),
        org_uuid: Some("org-team".into()),
        org_name: Some("Pkspa".into()),
        ..Default::default()
    };
    let personal = oauth::Profile {
        org_uuid: Some("org-personal".into()),
        org_name: Some("x's Organization".into()),
        ..team.clone()
    };
    let t = tokens(None, None);

    assert_eq!(reauth_mismatch(Some("acct"), Some("org-team"), Some(&team), &t), None);
    assert_eq!(
        reauth_mismatch(Some("acct"), Some("org-team"), Some(&personal), &t),
        Some(ReauthMismatch::Organization("x's Organization".into()))
    );
    // profile 没拉到：用交换响应里的组织 UUID 比，名字退回 UUID。
    assert_eq!(
        reauth_mismatch(
            Some("acct"),
            Some("org-team"),
            None,
            &tokens(Some("acct"), Some("org-personal"))
        ),
        Some(ReauthMismatch::Organization("org-personal".into()))
    );
    assert_eq!(
        reauth_mismatch(
            Some("acct"),
            Some("org-team"),
            None,
            &tokens(Some("acct"), Some("org-team"))
        ),
        None
    );
}

/// 缺一边无从比对就放行；账号不对先报账号。
#[test]
fn reauth_mismatch_skips_missing_sides_and_checks_account_first() {
    let p = oauth::Profile {
        account_uuid: Some("other".into()),
        email: Some("other@example.com".into()),
        org_uuid: Some("org-b".into()),
        ..Default::default()
    };
    let t = tokens(None, None);
    assert_eq!(
        reauth_mismatch(Some("acct"), Some("org-a"), Some(&p), &t),
        Some(ReauthMismatch::Account("other@example.com".into()))
    );
    let same_acct = oauth::Profile { account_uuid: Some("acct".into()), ..p.clone() };
    assert_eq!(
        reauth_mismatch(Some("acct"), None, Some(&same_acct), &t),
        None,
        "库里没有组织 UUID（旧号）"
    );
    assert_eq!(reauth_mismatch(Some("acct"), Some(" "), Some(&same_acct), &t), None, "空串当没有");
    let no_org = oauth::Profile { org_uuid: None, ..same_acct.clone() };
    assert_eq!(
        reauth_mismatch(Some("acct"), Some("org-a"), Some(&no_org), &t),
        None,
        "这次没拿到组织 UUID"
    );
    assert_eq!(reauth_mismatch(None, None, Some(&p), &t), None);
}

/// 前端按原文匹配这两句做中文化（`localizeBackendMessage`），措辞不能悄悄变。
#[test]
fn reauth_mismatch_messages_match_the_frontend_patterns() {
    assert_eq!(
        ReauthMismatch::Account("a@b.c".into()).message(),
        "the authorized account (a@b.c) is not this account; sign in with the original account and try again"
    );
    assert_eq!(
        ReauthMismatch::Organization("Pkspa".into()).message(),
        "the authorized organization (Pkspa) is not this account's organization; sign in and choose the original organization, then try again"
    );
}

/// 两条只差密码的代理，给访客打码后 URL 一模一样；账号视图带上代理池 id，前端按 id 查名称
/// 才不会串位。
#[sqlx::test]
async fn credential_view_carries_the_exact_proxy_id(pool: sqlx::PgPool) {
    let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
    let pa = store.add_proxy(1, "A", "http://u:one@h:1").await.unwrap();
    let pb = store.add_proxy(1, "B", "http://u:two@h:1").await.unwrap();
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap();
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).await.unwrap();
    let c = store.insert("c", None, "tc", "rc", 0, None, None, 1).await.unwrap();
    store.set_proxy(a.id, Some("http://u:one@h:1")).await.unwrap();
    store.set_proxy(b.id, Some("http://u:two@h:1")).await.unwrap();
    store.set_proxy(c.id, Some("http://u:other@h:9")).await.unwrap();
    let state = AppState::for_test(store);
    let admin = state.store.admin_user().await.unwrap();
    let views =
        list_credentials(State(state.clone()), Extension(Actor::from(admin))).await.unwrap().0;
    let id_of = |id: i64| views.iter().find(|v| v["id"] == id).unwrap()["proxy_id"].as_i64();
    assert_eq!(id_of(a.id), Some(pa.id));
    assert_eq!(id_of(b.id), Some(pb.id));
    assert_eq!(id_of(c.id), None, "不在池里的自定义地址");
    assert_eq!(view_of(&state, a.id).await.unwrap().0.proxy_id, Some(pa.id));
}

/// 已删账号的流水留到保留期满：按号接口对它给 404（账号自己的明细弹框靠这个区分「号没了」
/// 与「没有请求」），全局接口带 `cred_id` 照样查得到——趋势拆分表里点已删账号那一行走的
/// 就是这条，别再把它导到按号接口去。
#[sqlx::test]
async fn usage_of_a_deleted_credential_stays_reachable_by_cred_id(pool: sqlx::PgPool) {
    let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap();
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).await.unwrap();
    for cid in [a.id, a.id, b.id] {
        let rec =
            store::UsageRecord { cred_id: Some(cid), cred_label: "x".into(), ..Default::default() };
        store.insert_usage_log(&rec).await.unwrap();
    }
    assert!(store.delete(a.id).await.unwrap());
    let state = AppState::for_test(store.clone());

    let admin = Actor::from(state.store.admin_user().await.unwrap());
    let err = list_credential_usage(
        State(state.clone()),
        Extension(admin.clone()),
        Path(a.id),
        Query(UsageQuery::default()),
    )
    .await
    .err()
    .expect("按号接口对已删账号给 404");
    assert_eq!(err.0, StatusCode::NOT_FOUND);

    let q = UsageQuery { cred_id: Some(a.id), ..Default::default() };
    let page = list_usage(State(state), Extension(admin), Query(q)).await.unwrap().0;
    assert_eq!(page.total, Some(2), "已删账号的两条流水都还在");
    assert!(page.logs.iter().all(|l| l.cred_id == Some(a.id)), "只给这个号的");
}

/// 重新授权后该自动启用的只有 token 那几档；封号、订阅未生效、额度暂停保持原状。
#[test]
fn token_pause_only_covers_token_problems() {
    // 刷新作废、刷新失败暂停（带恢复时刻）、代理建不出来。
    assert!(is_token_pause(r#"[refresh 400] {"error":"invalid_grant"}"#, false));
    assert!(is_token_pause("[refresh-failed] connection refused", true));
    assert!(is_token_pause("[proxy] invalid proxy url", false));
    // 保活 / 转发撞上的 token 吊销。
    assert!(is_token_pause(
        "[keepalive/profile 401] authentication_error: OAuth token has been revoked. Please obtain a new token.",
        false
    ));
    assert!(is_token_pause("upstream 401: OAuth token has expired", false));

    // 封号特征优先，哪怕同时提到 token。
    assert!(!is_token_pause(
        "[keepalive/profile 403] permission_error: account suspended; token invalid",
        false
    ));
    assert!(!is_token_pause("upstream 400: account_on_hold", false));
    // 订阅未生效、额度暂停、手动停用（无原因由调用方挡掉）不算。
    assert!(!is_token_pause(
        "[subscription-inactive 403] organization does not allow OAuth authentication",
        false
    ));
    assert!(!is_token_pause("5h quota exhausted; token usage 100%, invalid until reset", true));
    assert!(!is_token_pause("upstream 403: forbidden", false));
}

/// 自动名只取 host:port，不带 user:pass；撞名时依次加序号。
#[test]
fn auto_proxy_label_uses_host_port_and_dedupes() {
    let url = "socks5h://user:secret@10.0.0.1:1080";
    assert_eq!(auto_proxy_label(url, &[]), "10.0.0.1:1080");
    let existing = vec!["10.0.0.1:1080".to_string(), "10.0.0.1:1080 #2".to_string()];
    assert_eq!(auto_proxy_label(url, &existing), "10.0.0.1:1080 #3");
}

/// 保活 401/403 的封号上下文要能区分「类型」：permission_error 与 authentication_error
/// 在 reason / error_type 里都得看得见，状态码与上游 request-id 一并带上。
#[test]
fn keepalive_ban_context_keeps_status_type_message_and_request_id() {
    let rej = oauth::AuthRejection {
            endpoint: "bootstrap",
            status: 403,
            body: r#"{"error":{"type":"permission_error","message":"Your organization does not have access to Claude Code."}}"#.into(),
            request_id: Some("req_abc".into()),
        };
    let ctx = keepalive_ban_context(&rej);
    assert_eq!(ctx.source, "keepalive");
    assert_eq!(ctx.status, Some(403));
    assert_eq!(ctx.error_type.as_deref(), Some("permission_error"));
    assert_eq!(
        ctx.error_message.as_deref(),
        Some("Your organization does not have access to Claude Code.")
    );
    assert_eq!(ctx.upstream_request_id.as_deref(), Some("req_abc"));
    assert!(ctx.request_id.is_none(), "保活没有来访请求");
    assert_eq!(
        ctx.reason,
        "[keepalive/bootstrap 403] permission_error: Your organization does not have access to Claude Code."
    );

    // 非 JSON 体（网关页面）：类型缺省、整段体进 error_message、reason 截到 200 字符。
    let html = format!("<html>{}</html>", "x".repeat(500));
    let rej = oauth::AuthRejection {
        endpoint: "event_logging",
        status: 401,
        body: html.clone(),
        request_id: None,
    };
    let ctx = keepalive_ban_context(&rej);
    assert!(ctx.error_type.is_none());
    assert_eq!(ctx.error_message.as_deref(), Some(html.as_str()));
    assert!(ctx.reason.starts_with("[keepalive/event_logging 401] <html>"));
    assert_eq!(ctx.reason.chars().count(), 200);
    assert!(ctx.upstream_request_id.is_none());

    // 空体：写明，别留一个光秃秃的前缀。
    let rej = oauth::AuthRejection {
        endpoint: "bootstrap",
        status: 403,
        body: String::new(),
        request_id: None,
    };
    let ctx = keepalive_ban_context(&rej);
    assert_eq!(ctx.reason, "[keepalive/bootstrap 403] (empty body)");
    assert_eq!(ctx.error_message.as_deref(), Some("(empty body)"));
}

/// 返回值要如实反映「确实停用了」：账号级错误 → 停用且落事件、`true`；非账号级 → 号
/// 原样启用、`false`；号已不存在 → `false`。
#[sqlx::test]
async fn handle_keepalive_rejection_reports_whether_the_ban_landed(pool: sqlx::PgPool) {
    let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
    let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap();
    let b = store.insert("b", None, "tb", "rb", 0, None, None, 1).await.unwrap();
    let rej = |status: u16, t: &str, m: &str| oauth::AuthRejection {
        endpoint: "event_logging",
        status,
        body: serde_json::json!({"error": {"type": t, "message": m}}).to_string(),
        request_id: Some("req_k".into()),
    };

    // 非账号级：不停用。
    let org_policy = rej(
        403,
        "permission_error",
        "OAuth authentication is currently not allowed for this organization.",
    );
    assert_eq!(
        handle_keepalive_rejection(&store, &a, &org_policy).await,
        KeepaliveRejection::SubscriptionInactive
    );
    let got = store.get(a.id).await.unwrap().unwrap();
    // 不是封号，但订阅未生效要暂停调度：不带恢复时刻（等人工或连通性测试）。
    assert!(got.disabled && got.resume_at.is_none(), "订阅未生效应暂停调度");
    assert!(!got.is_banned(), "不是封号");
    assert!(got.ban_reason.as_deref().unwrap().contains(store::ORG_OAUTH_SUSPEND_MARKER));
    // 其它非账号级的 403（权限 / 区域）仍只记日志，号照常启用。
    let region = rej(403, "permission_error", "This model is not available in your region");
    assert_eq!(
        handle_keepalive_rejection(&store, &b, &region).await,
        KeepaliveRejection::NotBanned
    );
    assert!(!store.get(b.id).await.unwrap().unwrap().disabled);
    // 人工停用的号撞上同一句：不改成暂停（保活对它照发），但结论仍是订阅未生效——调用方
    // 据此不撤握手标记，免得每轮重发一遍启动握手。
    store.set_disabled(b.id, true).await.unwrap();
    assert_eq!(
        handle_keepalive_rejection(&store, &b, &org_policy).await,
        KeepaliveRejection::SubscriptionInactive
    );
    let got = store.get(b.id).await.unwrap().unwrap();
    assert!(got.disabled && got.ban_reason.is_none(), "人工停用保持原样");
    store.set_disabled(b.id, false).await.unwrap();
    // 恢复 a，下面接着测账号级那档。
    store.set_disabled(a.id, false).await.unwrap();

    // 账号级：停用、记原因。
    let revoked = rej(401, "authentication_error", "OAuth token has been revoked");
    assert_eq!(handle_keepalive_rejection(&store, &a, &revoked).await, KeepaliveRejection::Banned);
    let got = store.get(a.id).await.unwrap().unwrap();
    assert!(got.is_banned());
    assert_eq!(
        got.ban_reason.as_deref(),
        Some("[keepalive/event_logging 401] authentication_error: OAuth token has been revoked")
    );

    // 号已经被删：没有主体，`false`，b 不受影响。
    store.delete(a.id).await.unwrap();
    assert_eq!(
        handle_keepalive_rejection(&store, &a, &revoked).await,
        KeepaliveRejection::NotBanned
    );
    assert!(!store.get(b.id).await.unwrap().unwrap().is_banned());
}

/// 保活的 401/403 不再一律停用：只有账号级错误才算，判据与转发路径同一套。
#[test]
fn keepalive_rejection_bans_only_account_level_errors() {
    let rej = |status: u16, body: &str| oauth::AuthRejection {
        endpoint: "bootstrap",
        status,
        body: body.to_string(),
        request_id: None,
    };
    let err = |t: &str, m: &str| {
        serde_json::json!({"type": "error", "error": {"type": t, "message": m}}).to_string()
    };
    // 该停用：token 作废、账号 / 组织被停用。
    for (status, body) in [
        (401, err("authentication_error", "Invalid bearer token")),
        (401, err("authentication_error", "OAuth token has been revoked")),
        (403, err("permission_error", "Your organization has been disabled.")),
        (403, err("permission_error", "This account has been suspended for policy violations")),
    ] {
        assert!(keepalive_rejection_is_account_level(&rej(status, &body)), "{status} {body}");
    }
    // 不该停用：订阅未生效（续费 / 订阅后就好）、权限 / 能力不足、网关页面、空体。
    for (status, body) in [
        (
            403,
            err(
                "permission_error",
                "OAuth authentication is currently not allowed for this organization.",
            ),
        ),
        (
            401,
            err(
                "authentication_error",
                "OAuth authentication is currently not allowed for this organization.",
            ),
        ),
        (403, err("permission_error", "Your account does not have access to claude-opus-5")),
        (403, err("permission_error", "This model is not available in your region")),
        (403, "<html>403 Forbidden</html>".to_string()),
        (401, String::new()),
    ] {
        assert!(!keepalive_rejection_is_account_level(&rej(status, &body)), "{status} {body}");
    }
}

/// 对 New API 输出的倍率必须按它的基准（1.0 = $2/MTok）换算，且缓存倍率按模型区分。
/// 这里核对几个代表值，防止基准常量或字段映射被改错后静默给下游错价。
#[test]
fn pricing_items_follow_newapi_ratio_base() {
    let items = pricing_items();
    let find = |m: &str| items.iter().find(|i| i.model_name == m).expect(m);

    let opus = find("claude-opus-5");
    assert_eq!(opus.quota_type, 0);
    assert_eq!(opus.model_ratio, 2.5, "$5/MTok ÷ $2 基准");
    assert_eq!(opus.completion_ratio, 5.0, "$25 / $5");
    assert_eq!(opus.cache_ratio, 0.10);
    assert_eq!(opus.create_cache_ratio, 1.25);

    let fable = find("claude-fable-5-1[1m]");
    assert_eq!(fable.model_ratio, 5.0);
    assert_eq!(fable.completion_ratio, 5.0);
    assert_eq!(fable.cache_ratio, 0.025, "Fable 5.1 缓存读特例");

    let s5 = find("claude-sonnet-5");
    assert_eq!(s5.model_ratio, 1.0, "$2/MTok 正好是基准");
    assert_eq!(s5.completion_ratio, 5.0);

    let haiku = find("claude-haiku-4-5");
    assert_eq!(haiku.model_ratio, 0.5);

    // 序列化字段名必须是 New API 认的那几个。
    let v = serde_json::to_value(opus).unwrap();
    for key in [
        "model_name",
        "quota_type",
        "model_ratio",
        "completion_ratio",
        "cache_ratio",
        "create_cache_ratio",
    ] {
        assert!(v.get(key).is_some(), "缺字段 {key}");
    }
}

/// **并发的多次登录不得互相顶掉。**
///
/// 这是一条真实 bug 的护栏：原先 PKCE 只有一个全局槽位，两个标签页（或两个人）同时点
/// 「添加账号」，后一次生成就把前一次的 verifier/state 覆盖了，前一个人粘贴回来撞上的是
/// 「state 不匹配，可能存在 CSRF 或粘贴错误」——一句会把人引去查 CSRF 的误导性报错。
#[test]
fn concurrent_logins_do_not_clobber_each_other() {
    let now = std::time::Instant::now();
    let mut pending = PendingPkce::new();

    let a = PkceChallenge::generate();
    let b = PkceChallenge::generate();
    let (sa, sb) = (a.state.clone(), b.state.clone());
    let (va, vb) = (a.verifier.clone(), b.verifier.clone());
    assert_ne!(sa, sb, "两次生成的 state 必须不同");

    remember_pkce(&mut pending, 1, a, now);
    remember_pkce(&mut pending, 1, b, now);

    // 先发起的那次照样能换回**自己**的 verifier，而不是被后一次顶掉。
    let got_a = take_pkce(&mut pending, 1, &sa, now).expect("先发起的那次登录不该被顶掉");
    assert_eq!(got_a.verifier, va);
    let got_b = take_pkce(&mut pending, 1, &sb, now).expect("后发起的那次也要在");
    assert_eq!(got_b.verifier, vb);

    // 取出即移除：一次挑战只能用一次，重放拿不到东西。
    assert!(take_pkce(&mut pending, 1, &sa, now).is_none(), "挑战不得被重复使用");
    assert!(pending.is_empty());
}

/// 过期的登录尝试会被清掉，不认识的 state 一律取不到。
#[test]
fn pkce_entries_expire_and_unknown_state_misses() {
    let now = std::time::Instant::now();
    let mut pending = PendingPkce::new();
    let p = PkceChallenge::generate();
    let s = p.state.clone();
    remember_pkce(&mut pending, 1, p, now);

    assert!(take_pkce(&mut pending, 1, "someone-elses-state", now).is_none());
    // 刚好到 TTL 就算过期（条件是严格小于）。
    remember_pkce(&mut pending, 1, PkceChallenge::generate(), now);
    let expired_at = now + PKCE_TTL;
    assert!(take_pkce(&mut pending, 1, &s, expired_at).is_none(), "过期的应被清掉");
    assert!(pending.is_empty(), "过期项不该留在表里");
}

/// 反复点「添加账号」不能把内存撑起来，也不能挤掉别人的：每人超量时只丢自己最旧的，
/// 最新的那次必须留下；全站超量时丢最旧的。
#[test]
fn pkce_table_is_bounded_and_drops_the_oldest() {
    let now = std::time::Instant::now();
    let mut pending = PendingPkce::new();
    let admins = PkceChallenge::generate();
    let admin_state = admins.state.clone();
    remember_pkce(&mut pending, 1, admins, now);
    let mut states = Vec::new();
    for _ in 0..(PKCE_MAX_PER_OWNER + 5) {
        let p = PkceChallenge::generate();
        states.push(p.state.clone());
        remember_pkce(&mut pending, 2, p, now);
    }
    assert_eq!(pending.len(), PKCE_MAX_PER_OWNER + 1);
    assert!(take_pkce(&mut pending, 2, &states[0], now).is_none(), "最旧的应被丢弃");
    let newest = states.last().unwrap();
    assert!(take_pkce(&mut pending, 1, newest, now).is_none(), "别人发起的取不到");
    assert!(take_pkce(&mut pending, 2, newest, now).is_some(), "最新的一次必须还在");
    assert!(take_pkce(&mut pending, 1, &admin_state, now).is_some(), "别人刷不掉 admin 的");

    for owner in 0..(PKCE_MAX_PENDING as i64 + 5) {
        remember_pkce(&mut pending, 100 + owner, PkceChallenge::generate(), now);
    }
    assert_eq!(pending.len(), PKCE_MAX_PENDING, "全站总量有上限");
}

/// 整数设置一律按非负存：负数落成 0（不限），正数原样。
#[sqlx::test]
async fn nonneg_int_settings_clamp_negatives_to_zero(pool: sqlx::PgPool) {
    let store = std::sync::Arc::new(CredentialStore::for_test(pool.clone()).await);
    let state = AppState::for_test(store.clone());
    let req = |v: i64| {
        Json(serde_json::from_value(serde_json::json!({ "default_rpm_limit": v })).unwrap())
    };
    let _ = set_default_rpm_limit(State(state.clone()), req(-5)).await.unwrap();
    assert_eq!(store.get_setting(store::DEFAULT_RPM_LIMIT).unwrap().as_deref(), Some("0"));
    let _ = set_default_rpm_limit(State(state.clone()), req(42)).await.unwrap();
    assert_eq!(store.get_setting(store::DEFAULT_RPM_LIMIT).unwrap().as_deref(), Some("42"));

    let ttl = |v: i64| {
        Json(serde_json::from_value(serde_json::json!({ "session_binding_ttl_secs": v })).unwrap())
    };
    let _ = set_session_ttl(State(state), ttl(-1)).await.unwrap();
    assert_eq!(store.get_setting(store::SESSION_BINDING_TTL).unwrap().as_deref(), Some("0"));
}

/// 趋势接口响应里声明的桶宽就是实际用的那个：请求 60 秒，数据是 15 分钟一格，回 900。
#[sqlx::test]
async fn series_endpoints_report_the_bucket_width_actually_used(pool: sqlx::PgPool) {
    let store = std::sync::Arc::new(CredentialStore::for_test(pool.clone()).await);
    let state = AppState::for_test(store);
    for (asked, used) in [(60, 900), (1000, 1800), (3600, 3600)] {
        let q = |hours: i64| {
            Query(
                serde_json::from_value(serde_json::json!({ "hours": hours, "bucket_secs": asked }))
                    .unwrap(),
            )
        };
        let ttft = get_ttft_series(State(state.clone()), q(24)).await.unwrap().0;
        assert_eq!(ttft["bucket_secs"], used, "ttft 请求 {asked}");
        assert_eq!(ttft["since"].as_i64().unwrap() % 900, 0, "窗口起点对齐到 15 分钟");
        let cache = get_cache_series(State(state.clone()), q(25)).await.unwrap().0;
        assert_eq!(cache["bucket_secs"], used, "cache 请求 {asked}");
    }
}

/// 代理和用户只能把号放进开放给自己的分组（不能选的按不存在回）；建分组只有 admin。
#[sqlx::test]
async fn members_can_only_use_groups_opened_to_them(pool: sqlx::PgPool) {
    let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
    let admin = store.admin_user().await.unwrap();
    let user_id =
        store.create_user("u1", "", store::UserRole::User, admin.id).await.unwrap().unwrap().id;
    let user = store.user_by_id(user_id).await.unwrap().unwrap();
    let cred = store.insert("c", None, "t", "r", 0, None, None, user_id).await.unwrap().id;
    let opened = store.create_group("opened", "").await.unwrap().unwrap();
    let closed = store.create_group("closed", "").await.unwrap().unwrap();
    store.set_group_grants(opened, &[user_id]).await.unwrap().unwrap();
    let state = AppState::for_test(store.clone());
    let as_user = || Extension(Actor::from(user.clone()));

    let set = |ids: Vec<i64>| {
        set_credential_groups(
            State(state.clone()),
            as_user(),
            Path(cred),
            Json(serde_json::from_value(serde_json::json!({ "group_ids": ids })).unwrap()),
        )
    };
    assert_eq!(set(vec![closed]).await.err().map(|e| e.0), Some(StatusCode::BAD_REQUEST));
    let view = set(vec![opened]).await.unwrap().0;
    assert_eq!(serde_json::to_value(&view).unwrap()["groups"], serde_json::json!([opened]));

    let visible = list_groups(State(state.clone()), as_user()).await.unwrap().0;
    let names: Vec<_> = visible.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, vec!["默认分组", "opened"], "看不到没开放给自己的分组");
    assert!(visible.iter().all(|g| g.credential_count.is_none() && g.grants.is_none()));

    let create = create_group(
        State(state.clone()),
        as_user(),
        Json(serde_json::from_value(serde_json::json!({ "name": "mine" })).unwrap()),
    )
    .await;
    assert_eq!(create.err().map(|e| e.0), Some(StatusCode::FORBIDDEN));
}

/// 账单可见范围：用户只看自己、不能按人拆；代理默认看自己与下属的人头汇总，看下属只到人这
/// 一级（只能按日），看不到别的代理名下的人；admin 看全部。
#[sqlx::test]
async fn billing_scope_follows_the_hierarchy(pool: sqlx::PgPool) {
    let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
    let admin = store.admin_user().await.unwrap();
    let agent =
        store.create_user("ag", "", store::UserRole::Agent, admin.id).await.unwrap().unwrap();
    let other =
        store.create_user("ag2", "", store::UserRole::Agent, admin.id).await.unwrap().unwrap();
    let sub = store.create_user("sub", "", store::UserRole::User, agent.id).await.unwrap().unwrap();
    for (i, owner) in [admin.id, agent.id, other.id, sub.id].into_iter().enumerate() {
        let c = store
            .insert(&format!("c{i}"), None, "t", &format!("r{i}"), 0, None, None, owner)
            .await
            .unwrap()
            .id;
        store
            .insert_usage_log(&store::UsageRecord {
                cred_id: Some(c),
                model: Some("m".into()),
                cost_usd: Some(1.0 + i as f64),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = AppState::for_test(store.clone());
    let q = |by: &str, owner: Option<i64>| {
        let mut s = format!("by={by}");
        if let Some(o) = owner {
            s.push_str(&format!("&owner_id={o}"));
        }
        Query(serde_urlencoded_query(&s))
    };
    let call = |who: &store::User, by: &str, owner: Option<i64>| {
        get_billing(State(state.clone()), Extension(Actor::from(who.clone())), q(by, owner))
    };
    let owners = |resp: Json<BillingResp>| {
        let v = serde_json::to_value(&resp.0).unwrap();
        let mut keys: Vec<String> = v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["key"].as_str().unwrap().to_string())
            .collect();
        keys.sort();
        keys
    };
    let mut mine = vec![agent.id.to_string(), sub.id.to_string()];
    mine.sort();
    assert_eq!(owners(call(&agent, "owner", None).await.unwrap()), mine);
    // 代理看整队或某个下属都能拆到号与模型（与账号池里只读看得到下属的号同口径），只是不按人拆单人。
    assert_eq!(owners(call(&agent, "cred", Some(sub.id)).await.unwrap()).len(), 1);
    assert_eq!(owners(call(&agent, "cred", None).await.unwrap()).len(), 2);
    assert!(call(&agent, "model", None).await.is_ok());
    assert_eq!(
        call(&agent, "owner", Some(sub.id)).await.err().map(|e| e.0),
        Some(StatusCode::FORBIDDEN)
    );
    assert!(call(&agent, "day", Some(sub.id)).await.is_ok());
    assert!(call(&agent, "cred", Some(agent.id)).await.is_ok());
    assert_eq!(
        call(&agent, "day", Some(other.id)).await.err().map(|e| e.0),
        Some(StatusCode::NOT_FOUND)
    );
    assert_eq!(call(&agent, "key", None).await.err().map(|e| e.0), Some(StatusCode::FORBIDDEN));
    assert_eq!(call(&sub, "owner", None).await.err().map(|e| e.0), Some(StatusCode::FORBIDDEN));
    assert_eq!(owners(call(&sub, "cred", None).await.unwrap()).len(), 1);
    assert_eq!(owners(call(&admin, "owner", None).await.unwrap()).len(), 4);
}

/// 测试用：把查询串解析成 [`BillingQuery`]。
fn serde_urlencoded_query(s: &str) -> BillingQuery {
    let uri: axum::http::Uri = format!("/billing?{s}").parse().unwrap();
    axum::extract::Query::<BillingQuery>::try_from_uri(&uri).unwrap().0
}

/// 上号代理：给地址就用地址，给 id 只认本人池里的，两个都给拒；都不给时网页直连、上号 Key
/// 从本人池里按挂号从少到多（在途的也算）测试、用第一条通的，都不通回 502，池子空了直连。
#[sqlx::test]
async fn exchange_proxy_is_explicit_or_auto_assigned_for_provision_keys(pool: sqlx::PgPool) {
    let store = std::sync::Arc::new(CredentialStore::for_test(pool.clone()).await);
    let admin = store.admin_user().await.unwrap().id;
    let hash = "x".to_string();
    let agent =
        store.create_user("agent1", &hash, UserRole::Agent, admin).await.unwrap().unwrap().id;
    let other =
        store.create_user("agent2", &hash, UserRole::Agent, admin).await.unwrap().unwrap().id;
    let pa = store.add_proxy(agent, "A", "http://u:a@h:1").await.unwrap();
    let pb = store.add_proxy(agent, "B", "http://u:b@h:2").await.unwrap();
    let foreign = store.add_proxy(other, "C", "http://u:c@h:3").await.unwrap();
    let busy =
        store.insert("busy", None, "at", "rt-busy", u64::MAX, None, None, other).await.unwrap();
    store.set_proxy(busy.id, Some(&pa.url)).await.unwrap();
    let state = AppState::for_test(store.clone());
    let actor = Actor { id: agent, username: "agent1".into(), role: UserRole::Agent };
    let all_up = |_url: String| async { Ok::<(), String>(()) };
    let req = |proxy: Option<&str>, proxy_id: Option<i64>| ExchangeReq {
        code: "c#s".into(),
        label: None,
        proxy: proxy.map(Into::into),
        proxy_id,
        group_ids: vec![],
    };

    let (url, _) = resolve_proxy(&state, &actor, &req(Some("http://u:x@h:9"), None), true, all_up)
        .await
        .unwrap();
    assert_eq!(url.as_deref(), Some("http://u:x@h:9"));
    let (url, _) =
        resolve_proxy(&state, &actor, &req(None, Some(pa.id)), false, all_up).await.unwrap();
    assert_eq!(url.as_deref(), Some(pa.url.as_str()));
    assert_eq!(
        resolve_proxy(&state, &actor, &req(None, Some(foreign.id)), true, all_up)
            .await
            .err()
            .map(|e| e.0),
        Some(StatusCode::NOT_FOUND)
    );
    assert_eq!(
        resolve_proxy(&state, &actor, &req(Some(&pa.url), Some(pa.id)), true, all_up)
            .await
            .err()
            .map(|e| e.0),
        Some(StatusCode::BAD_REQUEST)
    );
    assert_eq!(
        resolve_proxy(&state, &actor, &req(None, None), false, all_up).await.unwrap().0,
        None
    );

    // A 已经挂了一个号（别人的也算）：先分到 B；B 在途时两边各 1，并列取 id 小的 A；
    // 在途的放掉之后又回到 B。
    let (first, held) =
        resolve_proxy(&state, &actor, &req(None, None), true, all_up).await.unwrap();
    assert_eq!(first.as_deref(), Some(pb.url.as_str()));
    let (second, _held2) =
        resolve_proxy(&state, &actor, &req(None, None), true, all_up).await.unwrap();
    assert_eq!(second.as_deref(), Some(pa.url.as_str()));
    drop(held);
    drop(_held2);
    let (third, _) = resolve_proxy(&state, &actor, &req(None, None), true, all_up).await.unwrap();
    assert_eq!(third.as_deref(), Some(pb.url.as_str()));

    // 导入进来的条目不经校验：建不出客户端的跳过；归一化前的写法按归一化后的地址数挂号。
    let odd = store.create_user("agent4", &hash, UserRole::Agent, admin).await.unwrap().unwrap().id;
    let odd_actor = Actor { id: odd, username: "agent4".into(), role: UserRole::Agent };
    store.add_proxy(odd, "bad", "ftp://h:21").await.unwrap();
    let legacy = store.add_proxy(odd, "legacy", "socks5://u:l@h:5").await.unwrap();
    let fresh = store.add_proxy(odd, "fresh", "socks5h://u:f@h:6").await.unwrap();
    let on_legacy = store.insert("l", None, "at", "rt-l", u64::MAX, None, None, odd).await.unwrap();
    store.set_proxy(on_legacy.id, Some("socks5h://u:l@h:5")).await.unwrap();
    let (url, _) = resolve_proxy(&state, &odd_actor, &req(None, None), true, all_up).await.unwrap();
    assert_eq!(url.as_deref(), Some(fresh.url.as_str()), "legacy #{} 已挂一个号", legacy.id);

    // 测不通的跳过：B 不通就落到 A；都不通回 502，不退回直连。
    let b_url = pb.url.clone();
    let b_down = move |url: String| {
        let down = url == b_url;
        async move { if down { Err("down".to_string()) } else { Ok(()) } }
    };
    let (url, _) = resolve_proxy(&state, &actor, &req(None, None), true, b_down).await.unwrap();
    assert_eq!(url.as_deref(), Some(pa.url.as_str()));
    let all_down = |_url: String| async { Err::<(), String>("down".into()) };
    assert_eq!(
        resolve_proxy(&state, &actor, &req(None, None), true, all_down).await.err().map(|e| e.0),
        Some(StatusCode::BAD_GATEWAY)
    );
    // 显式指定的不测（脚本自己负责）。
    let (url, _) =
        resolve_proxy(&state, &actor, &req(None, Some(pb.id)), true, all_down).await.unwrap();
    assert_eq!(url.as_deref(), Some(pb.url.as_str()));

    let lonely =
        store.create_user("agent3", &hash, UserRole::Agent, admin).await.unwrap().unwrap().id;
    let lonely = Actor { id: lonely, username: "agent3".into(), role: UserRole::Agent };
    assert_eq!(
        resolve_proxy(&state, &lonely, &req(None, None), true, all_up).await.unwrap().0,
        None
    );
}

/// 代理和用户改自己号的调度参数只能比全局更紧：优先级最高 P2，上限不能放开也不能高过全局，
/// 提前停调度阈值不能高过全局；admin 不受限。
#[sqlx::test]
async fn members_can_only_tighten_scheduling(pool: sqlx::PgPool) {
    let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
    let admin = Actor::from(store.admin_user().await.unwrap());
    let agent_id =
        store.create_user("agent1", "x", UserRole::Agent, admin.id).await.unwrap().unwrap().id;
    let agent = Actor { id: agent_id, username: "agent1".into(), role: UserRole::Agent };
    let c = store.insert("c", None, "at", "rt", u64::MAX, None, None, agent_id).await.unwrap().id;
    store.set_setting(store::DEFAULT_RPM_LIMIT, "60").await.unwrap();
    store.set_setting(store::QUOTA_PAUSE_PCT, "90").await.unwrap();
    let state = AppState::for_test(store.clone());

    let prio = |who: &Actor, p: i64| {
        let (state, who) = (state.clone(), who.clone());
        async move {
            set_priority(
                State(state),
                Extension(who),
                Path(c),
                Json(SetPriorityReq { priority: p }),
            )
            .await
            .err()
            .map(|e| e.0)
        }
    };
    assert_eq!(prio(&agent, 1).await, Some(StatusCode::FORBIDDEN));
    assert_eq!(prio(&agent, 3).await, None);
    assert_eq!(prio(&admin, 0).await, None, "admin 不受限");
    assert_eq!(prio(&agent, 1).await, None, "admin 给的 P0 可以往下调");
    assert_eq!(prio(&agent, 0).await, Some(StatusCode::FORBIDDEN), "但调不回去");

    let rpm = |who: &Actor, n: i64| {
        let (state, who) = (state.clone(), who.clone());
        async move {
            set_rpm_limit(
                State(state),
                Extension(who),
                Path(c),
                Json(SetRpmLimitReq { rpm_limit: n }),
            )
            .await
            .err()
            .map(|e| e.0)
        }
    };
    assert_eq!(rpm(&agent, -1).await, Some(StatusCode::FORBIDDEN), "不能放开");
    assert_eq!(rpm(&agent, 61).await, Some(StatusCode::FORBIDDEN), "不能高过全局");
    assert_eq!(rpm(&agent, 60).await, None);
    assert_eq!(rpm(&agent, 0).await, None, "跟随全局");
    assert_eq!(rpm(&admin, -1).await, None);
    assert_eq!(rpm(&agent, -1).await, None, "admin 给的「不限」原样带回来不算放宽");
    assert_eq!(rpm(&agent, 61).await, Some(StatusCode::FORBIDDEN));

    // 全局不限时号主随便设，含「不限」。
    store.set_setting(store::DEFAULT_RPM_LIMIT, "0").await.unwrap();
    assert_eq!(rpm(&agent, -1).await, None);

    let pct = |short: Option<i64>, long: Option<i64>| {
        let (state, agent) = (state.clone(), agent.clone());
        async move {
            set_credential_quota_pause_pct(
                State(state),
                Extension(agent),
                Path(c),
                Json(SetCredentialQuotaPausePctReq {
                    quota_pause_pct: short,
                    quota_pause_pct_7d: long,
                }),
            )
            .await
            .err()
            .map(|e| e.0)
        }
    };
    assert_eq!(pct(Some(0), None).await, Some(StatusCode::FORBIDDEN), "不能「不停」");
    assert_eq!(pct(Some(95), None).await, Some(StatusCode::FORBIDDEN), "不能高过全局");
    assert_eq!(pct(Some(80), Some(50)).await, None, "7d 全局默认不停，随便设");

    // admin 给这个号的 7d 设了「不停」：号主只改 5h、7d 原样带回 0 照样能存；改成比全局宽的不行。
    store.set_setting(store::QUOTA_PAUSE_PCT_7D, "80").await.unwrap();
    store.set_quota_pause_pcts(c, Some(80), Some(0)).await.unwrap();
    assert_eq!(pct(Some(70), Some(0)).await, None);
    assert_eq!(pct(Some(70), Some(90)).await, Some(StatusCode::FORBIDDEN));
    // 原样带回的那一档不写：号主只改 5h 时，这期间 admin 改了 7d，不会被号主那份旧值盖回去。
    store.set_quota_pause_pcts(c, Some(70), Some(30)).await.unwrap();
    assert!(store.set_quota_pause_pcts_partial(c, Some(Some(60)), None).await.unwrap());
    let got = store.get(c).await.unwrap().unwrap();
    assert_eq!((got.quota_pause_pct, got.quota_pause_pct_7d), (Some(60), Some(30)));
}

/// admin 给号主的号配了内网代理（随之进了号主的池子）：号主把这条池代理用到自己别的号上、
/// 改它的名字都照常；没进池的内网地址仍然拒。
#[sqlx::test]
async fn pooled_internal_proxies_stay_usable_for_their_owner(pool: sqlx::PgPool) {
    let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
    let admin = Actor::from(store.admin_user().await.unwrap());
    let uid = store.create_user("u", "x", UserRole::User, admin.id).await.unwrap().unwrap().id;
    let user = Actor { id: uid, username: "u".into(), role: UserRole::User };
    let a = store.insert("a", None, "at", "ra", u64::MAX, None, None, uid).await.unwrap().id;
    let b = store.insert("b", None, "at", "rb", u64::MAX, None, None, uid).await.unwrap().id;
    let state = AppState::for_test(store.clone());
    let set = |who: &Actor, id: i64, url: &str| {
        let (state, who, url) = (state.clone(), who.clone(), url.to_owned());
        async move {
            set_proxy(
                State(state),
                Extension(who),
                Path(id),
                Json(SetProxyReq { proxy: Some(url) }),
            )
            .await
            .err()
            .map(|e| e.0)
        }
    };
    assert_eq!(set(&user, a, "http://10.0.0.5:3128").await, Some(StatusCode::BAD_REQUEST));
    assert_eq!(set(&admin, a, "http://10.0.0.5:3128").await, None);
    assert_eq!(set(&user, b, "http://10.0.0.5:3128").await, None, "已在本人池里");
    let pid = store.list_proxies(Scope::Owner(uid)).await.unwrap()[0].id;
    let renamed = update_saved_proxy(
        State(state.clone()),
        Extension(user.clone()),
        Path(pid),
        Json(UpdateProxyReq { label: "gw".into(), url: "http://10.0.0.5:3128".into() }),
    )
    .await;
    assert!(renamed.is_ok(), "只改名称");
    assert_eq!(set(&user, b, "http://10.0.0.6:3128").await, Some(StatusCode::BAD_REQUEST));
}

/// 号主看自己号的流水：下游的设备 / 会话 / UA 与 luban 的改写细节抹掉，用量与费用照给；
/// admin 看到全量。按号统计里按设备、按客户端的拆分也不给号主。
#[sqlx::test]
async fn owners_see_usage_without_forensics(pool: sqlx::PgPool) {
    let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
    let admin = Actor::from(store.admin_user().await.unwrap());
    let uid = store.create_user("u", "x", UserRole::User, admin.id).await.unwrap().unwrap().id;
    let user = Actor { id: uid, username: "u".into(), role: UserRole::User };
    let c = store.insert("c", None, "at", "rt", u64::MAX, None, None, uid).await.unwrap().id;
    let mut rec = store::UsageRecord {
        cred_id: Some(c),
        cred_label: "c".into(),
        device_id: Some("dev-in".into()),
        ua: Some("claude-cli/2.1".into()),
        cost_usd: Some(1.5),
        ..Default::default()
    };
    rec.forensics.session_id_in = Some("sid-in".into());
    rec.forensics.shape = Some("{}".into());
    rec.forensics.error_type = Some("overloaded_error".into());
    store.insert_usage_log(&rec).await.unwrap();
    let state = AppState::for_test(store.clone());

    let page = |who: &Actor| {
        let (state, who) = (state.clone(), who.clone());
        async move {
            list_credential_usage(
                State(state),
                Extension(who),
                Path(c),
                Query(UsageQuery::default()),
            )
            .await
            .unwrap()
            .0
            .logs
            .remove(0)
        }
    };
    let mine = page(&user).await;
    assert_eq!((mine.device_id, mine.ua), (None, None));
    assert_eq!((mine.forensics.session_id_in, mine.forensics.shape), (None, None));
    assert_eq!(mine.forensics.error_type.as_deref(), Some("overloaded_error"), "错误类型照给");
    assert_eq!(mine.cost_usd, Some(1.5));
    let all = page(&admin).await;
    assert_eq!(all.device_id.as_deref(), Some("dev-in"));
    assert_eq!(all.forensics.session_id_in.as_deref(), Some("sid-in"));
}

/// 代理和用户的出口代理不能指向本机或内网：IP 直写、域名解析出来的都拦；admin 不受限。
#[tokio::test]
async fn member_proxies_cannot_point_inside() {
    for ip in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.100.100.200",
        "0.0.0.0",
        "::1",
        "fd00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "::7f00:1",
        "64:ff9b::a00:5",
        "64:ff9b:1::a9fe:a9fe",
        "2002:7f00:1::",
    ] {
        assert!(is_internal_ip(ip.parse().unwrap()), "{ip} 该算内网");
    }
    for ip in [
        "203.0.113.7",
        "8.8.8.8",
        "2001:db8::1",
        "100.128.0.1",
        "64:ff9b::808:808",
        "2002:808:808::",
    ] {
        assert!(!is_internal_ip(ip.parse().unwrap()), "{ip} 不该算内网");
    }
    let member = Actor { id: 2, username: "u".into(), role: UserRole::User };
    let admin = Actor { id: 1, username: "admin".into(), role: UserRole::Admin };
    for url in ["socks5h://u:p@127.0.0.1:1080", "http://[::1]:8080", "http://localhost:3128"] {
        assert!(check_proxy_target(&member, url).await.is_err(), "{url}");
        assert!(check_proxy_target(&admin, url).await.is_ok(), "admin 不受限：{url}");
    }
    assert!(check_proxy_target(&member, "http://u:p@203.0.113.7:3128").await.is_ok());
}

/// 代理的账号列表连下属用户的号一起列、带号主用户名，别的代理名下的不列；用户只看自己的。
/// 实时流量与列表同口径。
#[sqlx::test]
async fn agents_list_their_team_read_only(pool: sqlx::PgPool) {
    let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
    let admin = Actor::from(store.admin_user().await.unwrap());
    let mk = |name: &'static str, role: UserRole, parent: i64| {
        let store = store.clone();
        async move {
            let id = store.create_user(name, "x", role, parent).await.unwrap().unwrap().id;
            Actor { id, username: name.into(), role }
        }
    };
    let agent = mk("a1", UserRole::Agent, admin.id).await;
    let user = mk("u1", UserRole::User, agent.id).await;
    let other = mk("a2", UserRole::Agent, admin.id).await;
    let stranger = mk("u2", UserRole::User, other.id).await;
    let mut ids = Vec::new();
    for (rt, owner) in [("ra", agent.id), ("ru", user.id), ("rs", stranger.id), ("rd", admin.id)] {
        ids.push(store.insert(rt, None, "at", rt, u64::MAX, None, None, owner).await.unwrap().id);
    }
    // 下属的号配了带密码的出口代理：代理看到的打码，号主自己看到原样。
    store.set_proxy(ids[1], Some("http://u1:secret@203.0.113.7:3128")).await.unwrap();
    let sub_cred = ids[1];
    let state = AppState::for_test(store.clone());
    let proxy_of = |who: &Actor| {
        let (state, who) = (state.clone(), who.clone());
        async move {
            let rows = list_credentials(State(state), Extension(who)).await.unwrap().0;
            rows.into_iter().find(|v| v["id"] == sub_cred).unwrap()["proxy"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    assert!(!proxy_of(&agent).await.contains("secret"));
    assert!(proxy_of(&user).await.contains("secret"));
    let list = |who: &Actor| {
        let (state, who) = (state.clone(), who.clone());
        async move {
            list_credentials(State(state), Extension(who))
                .await
                .unwrap()
                .0
                .into_iter()
                .map(|v| {
                    let v = serde_json::to_value(v).unwrap();
                    (
                        v["id"].as_i64().unwrap(),
                        v["owner"].as_str().map(str::to_owned),
                        v["editable"].as_bool().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        list(&agent).await,
        vec![(ids[0], None, true), (ids[1], Some("u1".to_owned()), false)]
    );
    assert_eq!(list(&user).await, vec![(ids[1], None, true)]);
    assert_eq!(list(&admin).await.len(), 4);
    assert_eq!(store.credential_ids_of_team(agent.id).await.unwrap(), ids[..2].to_vec());
    assert!(store.credential_in_team(ids[1], agent.id).await.unwrap());
    assert!(!store.credential_in_team(ids[2], agent.id).await.unwrap());
}
