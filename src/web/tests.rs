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
#[tokio::test]
async fn credential_view_carries_the_exact_proxy_id() {
    let store = Arc::new(CredentialStore::open_in_memory().unwrap());
    let pa = store.add_proxy("A", "http://u:one@h:1").unwrap();
    let pb = store.add_proxy("B", "http://u:two@h:1").unwrap();
    let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap();
    let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap();
    let c = store.insert("c", None, "tc", "rc", 0, None, None).unwrap();
    store.set_proxy(a.id, Some("http://u:one@h:1")).unwrap();
    store.set_proxy(b.id, Some("http://u:two@h:1")).unwrap();
    store.set_proxy(c.id, Some("http://u:other@h:9")).unwrap();
    let state = AppState::for_test(store);
    let views = list_credentials(State(state.clone())).await.unwrap().0;
    let id_of = |id: i64| views.iter().find(|v| v.id == id).unwrap().proxy_id;
    assert_eq!(id_of(a.id), Some(pa.id));
    assert_eq!(id_of(b.id), Some(pb.id));
    assert_eq!(id_of(c.id), None, "不在池里的自定义地址");
    assert_eq!(credential_view(&state, a.id).unwrap().0.proxy_id, Some(pa.id));
}

/// 已删账号的流水留到保留期满：按号接口对它给 404（账号自己的明细弹框靠这个区分「号没了」
/// 与「没有请求」），全局接口带 `cred_id` 照样查得到——趋势拆分表里点已删账号那一行走的
/// 就是这条，别再把它导到按号接口去。
#[tokio::test]
async fn usage_of_a_deleted_credential_stays_reachable_by_cred_id() {
    let store = Arc::new(CredentialStore::open_in_memory().unwrap());
    let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap();
    let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap();
    for cid in [a.id, a.id, b.id] {
        let rec =
            store::UsageRecord { cred_id: Some(cid), cred_label: "x".into(), ..Default::default() };
        store.insert_usage_log(&rec).unwrap();
    }
    assert!(store.delete(a.id).unwrap());
    let state = AppState::for_test(store.clone());

    let err = list_credential_usage(State(state.clone()), Path(a.id), Query(UsageQuery::default()))
        .await
        .err()
        .expect("按号接口对已删账号给 404");
    assert_eq!(err.0, StatusCode::NOT_FOUND);

    let q = UsageQuery { cred_id: Some(a.id), ..Default::default() };
    let page = list_usage(State(state), Query(q)).await.unwrap().0;
    assert_eq!(page.total, 2, "已删账号的两条流水都还在");
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
#[test]
fn handle_keepalive_rejection_reports_whether_the_ban_landed() {
    let store = CredentialStore::open_in_memory().unwrap();
    let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap();
    let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap();
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
        handle_keepalive_rejection(&store, &a, &org_policy),
        KeepaliveRejection::SubscriptionInactive
    );
    let got = store.get(a.id).unwrap().unwrap();
    assert!(store.list_ban_events(None, 10).unwrap().is_empty(), "不是封号，不落封号事件");
    // 但订阅未生效要暂停调度：不带恢复时刻（等人工或连通性测试），不落封号事件。
    assert!(got.disabled && got.resume_at.is_none(), "订阅未生效应暂停调度");
    assert!(got.ban_reason.as_deref().unwrap().contains(store::ORG_OAUTH_SUSPEND_MARKER));
    // 其它非账号级的 403（权限 / 区域）仍只记日志，号照常启用。
    let region = rej(403, "permission_error", "This model is not available in your region");
    assert_eq!(handle_keepalive_rejection(&store, &b, &region), KeepaliveRejection::NotBanned);
    assert!(!store.get(b.id).unwrap().unwrap().disabled);
    // 人工停用的号撞上同一句：不改成暂停（保活对它照发），但结论仍是订阅未生效——调用方
    // 据此不撤握手标记，免得每轮重发一遍启动握手。
    store.set_disabled(b.id, true).unwrap();
    assert_eq!(
        handle_keepalive_rejection(&store, &b, &org_policy),
        KeepaliveRejection::SubscriptionInactive
    );
    let got = store.get(b.id).unwrap().unwrap();
    assert!(got.disabled && got.ban_reason.is_none(), "人工停用保持原样");
    store.set_disabled(b.id, false).unwrap();
    // 恢复 a，下面接着测账号级那档。
    store.set_disabled(a.id, false).unwrap();

    // 账号级：停用、事件带完整上下文。
    let revoked = rej(401, "authentication_error", "OAuth token has been revoked");
    assert_eq!(handle_keepalive_rejection(&store, &a, &revoked), KeepaliveRejection::Banned);
    let got = store.get(a.id).unwrap().unwrap();
    assert!(got.is_banned());
    assert_eq!(
        got.ban_reason.as_deref(),
        Some("[keepalive/event_logging 401] authentication_error: OAuth token has been revoked")
    );
    let ev = &store.list_ban_events(None, 10).unwrap()[0];
    assert_eq!(ev.source, "keepalive");
    assert_eq!(ev.status, Some(401));
    assert_eq!(ev.error_type.as_deref(), Some("authentication_error"));
    assert_eq!(ev.upstream_request_id.as_deref(), Some("req_k"));

    // 号已经被删：没有主体，`false`，b 不受影响。
    store.delete(a.id).unwrap();
    assert_eq!(handle_keepalive_rejection(&store, &a, &revoked), KeepaliveRejection::NotBanned);
    assert!(!store.get(b.id).unwrap().unwrap().is_banned());
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

    remember_pkce(&mut pending, a, now);
    remember_pkce(&mut pending, b, now);

    // 先发起的那次照样能换回**自己**的 verifier，而不是被后一次顶掉。
    let got_a = take_pkce(&mut pending, &sa, now).expect("先发起的那次登录不该被顶掉");
    assert_eq!(got_a.verifier, va);
    let got_b = take_pkce(&mut pending, &sb, now).expect("后发起的那次也要在");
    assert_eq!(got_b.verifier, vb);

    // 取出即移除：一次挑战只能用一次，重放拿不到东西。
    assert!(take_pkce(&mut pending, &sa, now).is_none(), "挑战不得被重复使用");
    assert!(pending.is_empty());
}

/// 过期的登录尝试会被清掉，不认识的 state 一律取不到。
#[test]
fn pkce_entries_expire_and_unknown_state_misses() {
    let now = std::time::Instant::now();
    let mut pending = PendingPkce::new();
    let p = PkceChallenge::generate();
    let s = p.state.clone();
    remember_pkce(&mut pending, p, now);

    assert!(take_pkce(&mut pending, "someone-elses-state", now).is_none());
    // 刚好到 TTL 就算过期（条件是严格小于）。
    remember_pkce(&mut pending, PkceChallenge::generate(), now);
    let expired_at = now + PKCE_TTL;
    assert!(take_pkce(&mut pending, &s, expired_at).is_none(), "过期的应被清掉");
    assert!(pending.is_empty(), "过期项不该留在表里");
}

/// 反复点「添加账号」不能把内存撑起来：超量时丢最旧的，最新的那次必须留下。
#[test]
fn pkce_table_is_bounded_and_drops_the_oldest() {
    let now = std::time::Instant::now();
    let mut pending = PendingPkce::new();
    let mut states = Vec::new();
    for _ in 0..(PKCE_MAX_PENDING + 5) {
        let p = PkceChallenge::generate();
        states.push(p.state.clone());
        remember_pkce(&mut pending, p, now);
    }
    assert_eq!(pending.len(), PKCE_MAX_PENDING);
    assert!(take_pkce(&mut pending, &states[0], now).is_none(), "最旧的应被丢弃");
    let newest = states.last().unwrap();
    assert!(take_pkce(&mut pending, newest, now).is_some(), "最新的一次必须还在");
}

/// 整数设置一律按非负存：负数落成 0（不限），正数原样。
#[tokio::test]
async fn nonneg_int_settings_clamp_negatives_to_zero() {
    let store = std::sync::Arc::new(CredentialStore::open_in_memory().unwrap());
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

/// prefill / sampling 策略：`strip` 与空串删键回默认，`reject` / `off` 规整成小写存下，其余 400。
#[tokio::test]
async fn strip_policies_store_reset_and_reject_the_same_way() {
    let store = std::sync::Arc::new(CredentialStore::open_in_memory().unwrap());
    let state = AppState::for_test(store.clone());
    let prefill =
        |v: &str| Json(serde_json::from_value(serde_json::json!({ "prefill_policy": v })).unwrap());
    let sampling = |v: &str| {
        Json(serde_json::from_value(serde_json::json!({ "sampling_policy": v })).unwrap())
    };

    let _ = set_prefill_policy(State(state.clone()), prefill(" Reject ")).await.unwrap();
    assert_eq!(store.get_setting(store::PREFILL_POLICY).unwrap().as_deref(), Some("reject"));
    let _ = set_prefill_policy(State(state.clone()), prefill("")).await.unwrap();
    assert_eq!(store.get_setting(store::PREFILL_POLICY).unwrap(), None);

    let _ = set_sampling_policy(State(state.clone()), sampling("off")).await.unwrap();
    assert_eq!(store.get_setting(store::SAMPLING_POLICY).unwrap().as_deref(), Some("off"));
    let _ = set_sampling_policy(State(state.clone()), sampling("strip")).await.unwrap();
    assert_eq!(store.get_setting(store::SAMPLING_POLICY).unwrap(), None);

    let Err((status, msg)) = set_sampling_policy(State(state), sampling("drop")).await else {
        panic!("未知取值必须拒");
    };
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(msg, r#"sampling_policy must be "strip", "reject", or "off""#);
}

/// 趋势接口响应里声明的桶宽就是实际用的那个：请求 60 秒，数据是 15 分钟一格，回 900。
#[tokio::test]
async fn series_endpoints_report_the_bucket_width_actually_used() {
    let store = std::sync::Arc::new(CredentialStore::open_in_memory().unwrap());
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
