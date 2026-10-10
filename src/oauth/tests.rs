use super::{AUTH_REJECTION_BODY_CAP, KeepaliveResult};

/// 401/403 必须把状态码、响应体、上游 request-id 带回来；别的状态码照旧只定档。
#[test]
fn keepalive_result_keeps_the_rejection_context() {
    let body =
        br#"{"error":{"type":"permission_error","message":"Organization has been disabled."}}"#;
    match KeepaliveResult::from_parts("bootstrap", 403, Some("req_1".into()), body) {
        KeepaliveResult::AuthRejected(r) => {
            assert_eq!(r.endpoint, "bootstrap");
            assert_eq!(r.status, 403);
            assert_eq!(r.request_id.as_deref(), Some("req_1"));
            assert!(r.body.contains("Organization has been disabled"));
        }
        other => panic!("expected AuthRejected, got {other:?}"),
    }
    assert!(KeepaliveResult::from_parts("x", 200, None, b"").is_ok());
    assert!(KeepaliveResult::from_parts("x", 304, None, b"").is_ok());
    assert_eq!(KeepaliveResult::from_parts("x", 503, None, b"oops"), KeepaliveResult::Failed);
    // 超长体按字符截断，别把几十 KB 的网关页面整个塞进停号原因。
    let long = "é".repeat(AUTH_REJECTION_BODY_CAP * 2);
    let KeepaliveResult::AuthRejected(r) =
        KeepaliveResult::from_parts("x", 401, None, long.as_bytes())
    else {
        panic!("401 must be AuthRejected");
    };
    assert_eq!(r.body.chars().count(), AUTH_REJECTION_BODY_CAP);
}

/// `releases/latest` 的体是一行裸版本串；别的形态一律不认。
#[test]
fn release_body_must_be_a_bare_three_part_version() {
    let p = super::parse_release_body;
    assert_eq!(p("2.1.260"), Some((2, 1, 260)), "抓包原样");
    assert_eq!(p("2.1.260\n"), Some((2, 1, 260)), "尾随换行");
    assert_eq!(p(""), None, "空体");
    assert_eq!(p("2.1"), None, "两段不够");
    assert_eq!(p("2.1.260.1"), None, "四段太多");
    assert_eq!(p("2.1.260-beta.1"), None, "带后缀");
    assert_eq!(p("<html>Not Found</html>"), None, "错误页");
}

/// 缓存只升不降；库同步（启动/导入/手动改删）则以库为准，能升也能降、能清空。
#[tokio::test]
async fn release_cache_learns_upward_and_syncs_from_store_both_ways() {
    let c = super::ReleaseCache::new();
    assert_eq!(c.get(), None);
    assert!(c.learn((2, 1, 260)).await, "空缓存学到就记");
    assert!(!c.learn((2, 1, 258)).await, "更旧的忽略");
    assert!(!c.learn((2, 1, 260)).await, "相同的不算变");
    assert!(c.learn((2, 1, 265)).await, "更新的记");
    assert_eq!(c.get(), Some((2, 1, 265)));

    c.sync_from_store(async { Ok::<_, ()>(Some((2, 1, 261))) }).await.unwrap();
    assert_eq!(c.get(), Some((2, 1, 261)), "库为准，能降");
    c.sync_from_store(async { Ok::<_, ()>(None) }).await.unwrap();
    assert_eq!(c.get(), None, "库里删了缓存也清");
    assert!(!c.has_pending(), "库同步之后不欠落库");
    assert_eq!(
        c.sync_from_store(async { Err::<Option<_>, _>("io") }).await,
        Err("io"),
        "写库失败原样交回"
    );
    assert_eq!(c.get(), None, "写库失败缓存不动");
}

/// 后台正拿着锁往库里写旧值时，管理员的改动排在它后面落地——库里最终是管理员的值。
///
/// 复现的正是那条竞态：后台拿到锁 → 管理员写库并同步缓存 → 后台那笔迟到的写入盖掉管理员
/// 的值。把管理员的写库也放进同一把锁，顺序就变成后台写完 → 管理员写。
#[tokio::test]
async fn an_admin_write_lands_after_an_in_flight_background_persist() {
    let c = std::sync::Arc::new(super::ReleaseCache::new());
    // 「库」：按落地顺序记下每一笔写入。
    let db = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let db_bg = db.clone();
    c.install_persister(move |v| {
        let (started_tx, db_bg) = (started_tx.clone(), db_bg.clone());
        Box::pin(async move {
            started_tx.send(()).unwrap();
            // 写库很慢：给管理员留出在这期间动手的窗口。
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            db_bg.lock().push(("bg", v));
            Ok(())
        })
    });

    let c_bg = c.clone();
    let bg = tokio::spawn(async move {
        c_bg.learn((2, 1, 260)).await;
    });
    started_rx.recv().await.unwrap();
    // 后台正在锁内写库；管理员此刻改成 2.1.265。
    let db_admin = db.clone();
    c.sync_from_store(async {
        db_admin.lock().push(("admin", (2, 1, 265)));
        Ok::<_, ()>(Some((2, 1, 265)))
    })
    .await
    .unwrap();
    bg.await.unwrap();

    assert_eq!(&*db.lock(), &[("bg", (2, 1, 260)), ("admin", (2, 1, 265))], "管理员的写入最后落地");
    assert_eq!(c.get(), Some((2, 1, 265)));
    assert!(!c.has_pending());
}

/// 写库失败不丢：欠着，下次 `persist_pending` 补上；写成之后不再重复写。
#[tokio::test]
async fn release_cache_retries_a_failed_persist() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let c = super::ReleaseCache::new();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let written = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let (calls2, written2) = (calls.clone(), written.clone());
    c.install_persister(move |v| {
        // 头一次写失败，之后都成功。
        let failed = calls2.fetch_add(1, Ordering::SeqCst) == 0;
        let written2 = written2.clone();
        Box::pin(async move {
            if failed {
                anyhow::bail!("disk full");
            }
            written2.lock().push(v);
            Ok(())
        })
    });

    assert!(c.learn((2, 1, 260)).await);
    assert!(c.has_pending(), "第一次写失败，欠着");
    assert!(written.lock().is_empty());

    // 下一轮拉到同一个版本：缓存没变，但欠的那次要补。
    assert!(!c.learn((2, 1, 260)).await);
    assert!(!c.has_pending(), "补写成功");
    assert_eq!(&*written.lock(), &[(2, 1, 260)]);

    c.persist_pending().await;
    assert_eq!(written.lock().len(), 1, "已落库的不重复写");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// 并发学到两个版本时库里落的是较新的那个：落库写的是缓存值，缓存只升不降。
#[tokio::test]
async fn release_cache_persists_the_newest_value_not_the_callers() {
    let c = super::ReleaseCache::new();
    let written = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let w = written.clone();
    c.install_persister(move |v| {
        w.lock().push(v);
        Box::pin(async { Ok(()) })
    });
    // 模拟「A 学到 2.5.0、B 学到 2.6.0 并写库、A 最后才去写」：A 那次落库写的是当时的
    // 缓存值 2.6.0，而且因为 B 已经写过、根本不欠，A 什么都不写。
    assert!(c.learn((2, 6, 0)).await);
    assert!(!c.learn((2, 5, 0)).await);
    c.persist_pending().await;
    assert_eq!(&*written.lock(), &[(2, 6, 0)]);
    assert_eq!(super::release_string((2, 6, 0)), "2.6.0");
}

use super::{
    KeepaliveCtx, PkceChallenge, TokenEndpointError, exchange_body, parse_token_set, refresh_body,
    tier_from, urlencode,
};
use crate::config;
use wreq::StatusCode;

/// axios 那套辅助端点的头序表逐条对上抓包（`cap/2.1.260-2`）。
///
/// 这是 [`config::AXIOS_SHAPES`] 唯一的正确性依据。钉住它是因为「头序」这种东西改错了
/// 不会有任何运行时症状——请求照样 200，只是每个会话十来条请求整整齐齐地与官方不一样。
#[test]
fn axios_header_orders_match_the_captures() {
    // (端点, 抓包编号, 逐字头序)
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "policy_limits",
            "00001",
            &[
                "Accept",
                "Authorization",
                "anthropic-beta",
                "User-Agent",
                "If-None-Match",
                "Accept-Encoding",
                "Host",
                "Connection",
            ],
        ),
        (
            "settings",
            "00002",
            &[
                "Accept",
                "Authorization",
                "anthropic-beta",
                "User-Agent",
                "Cache-Control",
                "Pragma",
                "If-None-Match",
                "Accept-Encoding",
                "Host",
                "Connection",
            ],
        ),
        (
            "penguin_mode",
            "00005",
            &[
                "Accept",
                "Authorization",
                "anthropic-beta",
                "User-Agent",
                "Accept-Encoding",
                "Host",
                "Connection",
            ],
        ),
        (
            "mcp_registry",
            "00007",
            &["Accept", "User-Agent", "Accept-Encoding", "Host", "Connection"],
        ),
        (
            "bootstrap",
            "00008",
            &[
                "Accept",
                "Content-Type",
                "User-Agent",
                "Authorization",
                "anthropic-beta",
                "Accept-Encoding",
                "Host",
                "Connection",
            ],
        ),
        (
            "event_logging",
            "00016",
            &[
                "Accept",
                "Content-Type",
                "User-Agent",
                "x-service-name",
                "Authorization",
                "anthropic-beta",
                "Content-Length",
                "Accept-Encoding",
                "Host",
                "Connection",
            ],
        ),
        (
            "datadog",
            "00017",
            &[
                "Accept",
                "Content-Type",
                "DD-API-KEY",
                "User-Agent",
                "Content-Length",
                "Accept-Encoding",
                "Host",
                "Connection",
            ],
        ),
    ];
    for (name, cap, order) in cases {
        assert_eq!(config::axios_shape(name), *order, "{name}（cap/2.1.260-2/{cap}）");
    }

    // `Authorization` 与 `User-Agent` 的先后在两个端点上正好相反——这正是「不能合并成
    // 一张总表」的证据，别哪天又想着统一。
    let pos = |ep: &str, h: &str| config::axios_shape(ep).iter().position(|x| *x == h).unwrap();
    assert!(pos("policy_limits", "Authorization") < pos("policy_limits", "User-Agent"));
    assert!(pos("bootstrap", "User-Agent") < pos("bootstrap", "Authorization"));

    // 尾部三件套 11 类一致。
    for shape in config::AXIOS_SHAPES {
        assert_eq!(
            &shape.order[shape.order.len() - 3..],
            &["Accept-Encoding", "Host", "Connection"],
            "{} 的尾部",
            shape.name
        );
    }

    // eval 不是 axios：Bun 的 fetch，`Connection` 在串中间而不是队尾。
    let eval = config::AXIOS_SHAPE_EVAL.order;
    assert_eq!(eval.last(), Some(&"Content-Length"), "eval 队尾是 Content-Length");
    assert!(eval.contains(&"Connection"));
    assert_ne!(eval.last(), Some(&"Connection"));
}

/// `If-None-Match` 里那串 `sha256:…` 是**客户端自己算的响应体摘要**，不是服务端 ETag。
///
/// 依据：`cap/2.1.260-2/00002` 那条 settings 发的是
/// `"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"`，
/// 而那正是 `{}` 的 sha256。
#[test]
fn conditional_get_hashes_the_cached_body() {
    const EMPTY_OBJECT_SHA: &str =
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
    // 用一个本测试专属的凭证 id，免得和别的用例共用那张进程级表。
    let id = 90_001;

    // 没缓存过就不带这个头——一台刚装好的机器第一次跑就是这个形态。
    assert!(super::etag_of(id, "settings").is_none());

    super::remember_etag(id, "settings", 200, b"{}");
    assert_eq!(
        super::etag_of(id, "settings").as_deref(),
        Some(format!("\"sha256:{EMPTY_OBJECT_SHA}\"").as_str()),
        "与 cap/2.1.260-2/00002 逐字相同"
    );

    // 304 不动缓存：上游说的就是「没变」，拿一个空 body 覆盖会把下次的条件请求打歪。
    super::remember_etag(id, "settings", 304, b"");
    assert_eq!(
        super::etag_of(id, "settings").as_deref(),
        Some(format!("\"sha256:{EMPTY_OBJECT_SHA}\"").as_str()),
        "304 保留原值"
    );

    // 换了内容就换摘要；不同端点各记各的。
    super::remember_etag(id, "settings", 200, b"{\"a\":1}");
    assert_ne!(
        super::etag_of(id, "settings").as_deref(),
        Some(format!("\"sha256:{EMPTY_OBJECT_SHA}\"").as_str())
    );
    assert!(super::etag_of(id, "policy_limits").is_none(), "两个端点不共用一个键");
}

/// 保活循环靠这个标记跳过自己那份重复的启动串：新会话的握手已经把
/// bootstrap / penguin / policy_limits / settings 全打过一遍了。
#[test]
fn a_recent_handshake_is_visible_to_the_keepalive_loop() {
    use std::time::Duration;
    let id = 90_002;
    assert!(!super::handshake_recent(id, Duration::from_secs(1800)), "没握过手就没有标记");
    super::note_handshake(id);
    assert!(super::handshake_recent(id, Duration::from_secs(1800)), "刚握过");
    // 窗口足够短时又算「不近」——保活下一跳照常发自己那份。
    assert!(!super::handshake_recent(id, Duration::ZERO));
    assert!(!super::handshake_recent(90_003, Duration::from_secs(1800)), "别的号不受影响");
}

/// 有近期真实会话时，保活事件与 Datadog 日志挂在那个会话的身份上：同一个 session_id /
/// device_id / 版本 / 模型 / beta 串，`auth` 块带组织 id。没有时退回按账号派生的身份。
#[sqlx::test]
async fn keepalive_attaches_to_the_real_session_when_there_is_one(pool: sqlx::PgPool) {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let store = crate::store::CredentialStore::for_test(pool.clone()).await;
    let cred = store.insert("t", None, "a", "r", 0, None, None, 1).await.unwrap();
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

/// statsig eval 的 `attributes`：`rateLimitTier` 发**原值**、`firstTokenTime` 取凭证
/// 入库那一刻，`subscriptionCreatedAt` 取 profile 存下的原串换算的毫秒数（没存就不发）。
///
/// 官方那份的键序（`cap/2.1.260-2/00003`）：
/// `… userType, subscriptionType, rateLimitTier, organizationRole,
///    subscriptionCreatedAt, firstTokenTime, appVersion, entrypoint`。
#[sqlx::test]
async fn eval_attributes_carry_the_raw_rate_limit_tier(pool: sqlx::PgPool) {
    let store = crate::store::CredentialStore::for_test(pool.clone()).await;
    let cred = store.insert("t", None, "a", "r", 0, None, Some("claude_team"), 1).await.unwrap();
    store.set_rate_limit_tier(cred.id, Some("default_claude_max_5x")).await.unwrap();
    let cred = store.get(cred.id).await.unwrap().unwrap();

    let attrs = KeepaliveCtx::new(&cred, 1.0, None, None).eval_body();
    let attrs = &attrs["attributes"];
    assert_eq!(attrs["rateLimitTier"], "default_claude_max_5x", "发原值，不是界面上的 Max 5x");
    assert_eq!(attrs["subscriptionType"], "team");
    // 入库那一刻的毫秒时间戳，不是 0、也不是编出来的常量。
    let first = attrs["firstTokenTime"].as_i64().expect("凭证有 created_at 就该有这一项");
    assert_eq!(first, cred.created_at as i64 * 1000);
    assert!(first > 1_700_000_000_000, "毫秒量级: {first}");
    // 还没存订阅创建时刻的号明确不发——填个常量会让同一批号全都是同一个订阅创建时间。
    assert!(attrs.get("subscriptionCreatedAt").is_none(), "没存就不发，别填伪造常量: {attrs}");
    assert!(attrs.get("organizationUUID").is_none(), "响应头没学到、凭证上也没存");

    // 键序照抓包：rateLimitTier 夹在 subscriptionType 与 organizationRole 之间。
    let keys: Vec<&str> = attrs.as_object().unwrap().keys().map(String::as_str).collect();
    let at = |k: &str| keys.iter().position(|x| *x == k).unwrap();
    assert!(at("subscriptionType") < at("rateLimitTier"));
    assert!(at("rateLimitTier") < at("organizationRole"));
    assert!(at("organizationRole") < at("firstTokenTime"));
    assert!(at("firstTokenTime") < at("appVersion"));

    // profile 存下来之后：subscriptionCreatedAt 是毫秒数、夹在 organizationRole 与
    // firstTokenTime 之间（`cap/2.1.260-2/00003` 的键序）；organizationUUID 用凭证上那份垫底。
    store.set_subscription_created_at(cred.id, Some("2026-04-15T13:03:55.239Z")).await.unwrap();
    store.set_org_uuid(cred.id, Some("09520b85-f6b6-432f-97e2-6ecb804a083f")).await.unwrap();
    let cred = store.get(cred.id).await.unwrap().unwrap();
    let attrs = KeepaliveCtx::new(&cred, 1.0, None, None).eval_body();
    let attrs = &attrs["attributes"];
    assert_eq!(attrs["subscriptionCreatedAt"], 1776258235239i64);
    assert_eq!(attrs["organizationUUID"], "09520b85-f6b6-432f-97e2-6ecb804a083f");
    let keys: Vec<&str> = attrs.as_object().unwrap().keys().map(String::as_str).collect();
    let at = |k: &str| keys.iter().position(|x| *x == k).unwrap();
    assert!(at("platform") < at("organizationUUID") && at("organizationUUID") < at("accountUUID"));
    assert!(at("organizationRole") < at("subscriptionCreatedAt"));
    assert!(at("subscriptionCreatedAt") < at("firstTokenTime"));
    // 响应头学到的优先于凭证上存的。
    let fresh =
        KeepaliveCtx::new(&cred, 1.0, Some("11111111-2222-3333-4444-555555555555".into()), None);
    assert_eq!(
        fresh.eval_body()["attributes"]["organizationUUID"],
        "11111111-2222-3333-4444-555555555555"
    );
    // 解析不了的串按没有处理。
    store.set_subscription_created_at(cred.id, Some("garbage")).await.unwrap();
    let cred = store.get(cred.id).await.unwrap().unwrap();
    assert!(
        KeepaliveCtx::new(&cred, 1.0, None, None).eval_body()["attributes"]
            .get("subscriptionCreatedAt")
            .is_none()
    );

    // 旧库里没回填过的号：这一项整个不发，而不是发一个空串。
    let bare = store.insert("t2", None, "a2", "r2", 0, None, None, 1).await.unwrap();
    let bare = KeepaliveCtx::new(&bare, 1.0, None, None).eval_body();
    assert!(bare["attributes"].get("rateLimitTier").is_none());
}

/// 团队号的档位只能从 `rate_limit_tier` 读——实测 `cred_id=9`（`claude_team`）的
/// `account.has_claude_max`/`has_claude_pro` **都是 false**，而
/// `organization.rate_limit_tier` 是 `default_claude_max_5x`。
/// 此前这条路返回的是 `"team"`：额度档整个丢了，大小写也和 `Max`/`Pro` 不一致。
#[test]
fn team_tier_comes_from_the_rate_limit_field() {
    assert_eq!(
        tier_from(
            Some(false),
            Some(false),
            Some("claude_team"),
            Some("default_claude_max_5x"),
            None
        ),
        Some("Max 5x".into())
    );
    // 企业号同理。
    assert_eq!(
        tier_from(None, None, Some("claude_enterprise"), Some("default_claude_max_20x"), None),
        Some("Max 20x".into())
    );
    // 读不出档位时退回组织类型，但首字母大写，别在界面上混着小写词。
    assert_eq!(tier_from(None, None, Some("claude_team"), None, None), Some("Team".into()));
    assert_eq!(
        tier_from(None, None, Some("claude_team"), Some("weird"), None),
        Some("Team".into())
    );
}

/// 团队号有席位档时按席位显示：实测 `claude_team` 的 `rate_limit_tier` 是 `default_raven`
/// （读不出档位），`seat_tier` 是 `team_standard`。席位档优先于 `rate_limit_tier`。
#[test]
fn team_tier_prefers_the_seat_tier() {
    let team = |rate, seat| tier_from(Some(false), Some(false), Some("claude_team"), rate, seat);
    assert_eq!(team(Some("default_raven"), Some("team_standard")), Some("Team Standard".into()));
    assert_eq!(team(Some("default_raven"), Some("team_premium")), Some("Team Premium".into()));
    assert_eq!(
        team(Some("default_claude_max_5x"), Some("team_premium")),
        Some("Team Premium".into())
    );
    // 夹了变体段的席位（实测 `team_labs_premium` + `default_claude_max_5x`）收成普通档位名。
    assert_eq!(
        team(Some("default_claude_max_5x"), Some("team_labs_premium")),
        Some("Team Premium".into())
    );
    assert_eq!(team(Some("default_raven"), Some("team_tier_1")), Some("Team Tier 1".into()));
    assert_eq!(team(Some("default_raven"), Some(" ")), Some("Team".into()), "空席位档退回组织类型");
    // 个人号就算带了席位档也不看。
    assert_eq!(
        tier_from(Some(true), None, Some("claude_max"), Some("default_claude_max_5x"), Some("x")),
        Some("Max 5x".into())
    );
}

/// 个人组织里 `has_claude_max`/`has_claude_pro` 最权威。
#[test]
fn personal_tier_comes_from_the_account_flags() {
    assert_eq!(
        tier_from(Some(true), None, Some("claude_max"), Some("default_claude_max_20x"), None),
        Some("Max 20x".into())
    );
    assert_eq!(tier_from(None, Some(true), Some("claude_pro"), None, None), Some("Pro".into()));
    assert_eq!(
        tier_from(Some(true), None, None, Some("default_claude_max_5x"), None),
        Some("Max 5x".into())
    );
    assert_eq!(tier_from(Some(false), Some(false), None, None, None), Some("Free".into()));
    assert_eq!(tier_from(None, None, None, None, None), None);
}

/// 账号级的 `has_claude_max` 跟着人走：同一个人有个人 Max、又在团队里占一个席位时，
/// 团队组织那份 profile 里它也是 true（实测）。组织号不能看它，否则团队席位被判成 `Max`。
#[test]
fn org_tier_ignores_the_account_flags() {
    let team =
        |max, seat| tier_from(max, Some(false), Some("claude_team"), Some("default_raven"), seat);
    assert_eq!(team(Some(true), Some("team_standard")), Some("Team Standard".into()));
    assert_eq!(team(Some(true), None), Some("Team".into()));
    assert_eq!(
        tier_from(
            None,
            Some(true),
            Some("claude_enterprise"),
            Some("default_claude_max_20x"),
            None
        ),
        Some("Max 20x".into())
    );
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

/// token / profile 两条的头序**没有抓包**，这里钉的是按 axios 规律推出来的那份
/// （`Accept` 打头，调用点显式头按书写序，缺省 UA 在其后，再接尾部三件套），以及
/// 这两条**不该**带的头：profile 此前多发了 `anthropic-beta` / `anthropic-version`。
/// 拿到真实抓包后若不一致，改 `config::AXIOS_SHAPES` 里那两行并把这里改成抓包序。
#[test]
fn oauth_token_and_profile_shapes_follow_axios_rules() {
    assert_eq!(
        config::axios_shape("oauth_token"),
        &[
            "Accept",
            "Content-Type",
            "User-Agent",
            "Content-Length",
            "Accept-Encoding",
            "Host",
            "Connection"
        ]
    );
    assert_eq!(
        config::axios_shape("oauth_profile"),
        &[
            "Accept",
            "Authorization",
            "Content-Type",
            "User-Agent",
            "Accept-Encoding",
            "Host",
            "Connection"
        ]
    );
    for h in ["anthropic-beta", "anthropic-version"] {
        assert!(!config::axios_shape("oauth_profile").contains(&h), "profile 不带 {h}");
        assert!(!config::axios_shape("oauth_token").contains(&h), "token 不带 {h}");
    }
    assert_eq!(config::AXIOS_DEFAULT_USER_AGENT, config::DATADOG_USER_AGENT);
}

/// 请求体键序照官方 `services/oauth/client.ts`；刷新必须带固定的 `scope`。
#[test]
fn token_bodies_match_official_key_order() {
    let keys =
        |v: &serde_json::Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    let ex = exchange_body("CODE", "STATE", "VERIFIER");
    assert_eq!(
        keys(&ex),
        ["grant_type", "code", "redirect_uri", "client_id", "code_verifier", "state"]
    );
    assert_eq!(ex["redirect_uri"], config::REDIRECT_URI);

    let rf = refresh_body("RT");
    assert_eq!(keys(&rf), ["grant_type", "refresh_token", "client_id", "scope"]);
    assert_eq!(rf["scope"], config::REFRESH_SCOPES);
    assert_eq!(
        config::REFRESH_SCOPES,
        "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
        "官方 CLAUDE_AI_OAUTH_SCOPES 五项，无 org:create_api_key"
    );
}

/// 刷新响应缺 `refresh_token` 时沿用旧值（官方 `newRefreshToken = refreshToken`）；
/// 授权码交换没有旧值可退，缺了要报错。
#[test]
fn missing_refresh_token_falls_back_to_the_old_one() {
    let without = r#"{"access_token":"AT","expires_in":3600}"#;
    let set = parse_token_set(without, Some("OLD")).unwrap();
    assert_eq!(set.access_token, "AT");
    assert_eq!(set.refresh_token, "OLD");
    assert!(parse_token_set(without, None).is_err(), "交换时缺 refresh_token 是错");

    let with = r#"{"access_token":"AT","refresh_token":"NEW","expires_in":3600,"account":{"email_address":"a@b.c"}}"#;
    let set = parse_token_set(with, Some("OLD")).unwrap();
    assert_eq!(set.refresh_token, "NEW", "响应给了新的就用新的");
    assert_eq!(set.account.as_deref(), Some("a@b.c"));

    let empty = r#"{"access_token":"AT","refresh_token":"","expires_in":3600}"#;
    assert_eq!(parse_token_set(empty, Some("OLD")).unwrap().refresh_token, "OLD", "空串按缺失处理");
}

/// 授权 URL 的查询串照 JS `URLSearchParams`：空格是 `+`，`~` 转义、`*` 不转义。
#[test]
fn urlencode_matches_url_search_params() {
    assert_eq!(urlencode("user:profile user:inference"), "user%3Aprofile+user%3Ainference");
    assert_eq!(urlencode("a~b*c"), "a%7Eb*c");
    assert_eq!(
        urlencode("https://platform.claude.com/oauth/code/callback"),
        "https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback"
    );

    let pkce = PkceChallenge::generate();
    let url = pkce.authorize_url(config::SCOPES);
    assert!(url.contains("&scope=org%3Acreate_api_key+user%3Aprofile+"), "{url}");
    assert!(!url.contains("%20"), "{url}");
    // 参数顺序照官方 `authUrl.searchParams.append` 的书写序。
    let order = [
        "code=",
        "client_id=",
        "response_type=",
        "redirect_uri=",
        "scope=",
        "code_challenge=",
        "code_challenge_method=",
        "state=",
    ];
    let mut last = 0;
    for k in order {
        let i = url
            .find(&format!("{}{}", if last == 0 { "?" } else { "&" }, k))
            .unwrap_or_else(|| panic!("{k} 缺失或顺序不对: {url}"));
        assert!(i > last, "{k} 顺序不对: {url}");
        last = i;
    }
}

/// 起一个本地 HTTP/1.1 服务：收完请求头与（按 `Content-Length`）请求体，回给定响应，
/// 把收到的请求**原始字节**（头 + 体）交回来。
fn serve_once(json_body: &'static str) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        json_body.len(),
        json_body
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let h = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut r = BufReader::new(&stream);
        let mut raw = String::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap();
            }
            let end = line == "\r\n";
            raw.push_str(&line);
            if end {
                break;
            }
        }
        let mut body = vec![0u8; content_length];
        r.read_exact(&mut body).unwrap();
        raw.push_str(std::str::from_utf8(&body).unwrap());
        (&stream).write_all(response.as_bytes()).unwrap();
        raw
    });
    (addr, h)
}

/// 把原始请求拆成（请求行，头名按线上顺序与拼写，头值表，体）。
fn split_raw(
    raw: &str,
) -> (String, Vec<String>, std::collections::HashMap<String, String>, String) {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap().to_string();
    let mut names = Vec::new();
    let mut values = std::collections::HashMap::new();
    for l in lines {
        let (n, v) = l.split_once(": ").unwrap();
        names.push(n.to_string());
        values.insert(n.to_ascii_lowercase(), v.to_string());
    }
    (request_line, names, values, body.to_string())
}

/// token 端点**线上**形态：不是比常量表，是真发一条到本地监听口、看原始字节。
///
/// 钉的仍是按 axios 规律推出的那份（无抓包，见 `config::AXIOS_SHAPES` 里那两行的注释），
/// 但这里验的是「常量表 → `OrigHeaderMap` → 线上」这一整条链确实产出那个形态：头名
/// 拼写、顺序、`Connection: close`、`Host` / `Content-Length` 的位置、没有多余头、
/// 体的键序与 `scope`。拿到真实抓包后若头序不同，改常量表，这条测试跟着改。
#[tokio::test]
async fn token_request_wire_shape_is_axios() {
    let (addr, server) = serve_once(r#"{"access_token":"AT","expires_in":3600,"scope":"x"}"#);
    let client = crate::clients::upstream_client(None).unwrap();
    let set = super::post_token_to(
        &client,
        &format!("http://{addr}/v1/oauth/token"),
        refresh_body("OLD"),
        Some("OLD"),
    )
    .await
    .unwrap();
    assert_eq!(set.access_token, "AT");
    assert_eq!(set.refresh_token, "OLD", "响应没给新 refresh_token 就沿用旧的");

    let raw = server.join().unwrap();
    let (line, names, values, body) = split_raw(&raw);
    assert_eq!(line, "POST /v1/oauth/token HTTP/1.1");
    assert_eq!(
        names,
        [
            "Accept",
            "Content-Type",
            "User-Agent",
            "Content-Length",
            "Accept-Encoding",
            "Host",
            "Connection"
        ],
        "\n{raw}"
    );
    assert_eq!(values["accept"], config::AXIOS_ACCEPT);
    assert_eq!(values["content-type"], "application/json");
    assert_eq!(values["user-agent"], "axios/1.15.2");
    assert_eq!(values["accept-encoding"], config::AXIOS_ACCEPT_ENCODING);
    assert_eq!(values["connection"], "close");
    assert_eq!(values["host"], addr.to_string());
    assert_eq!(values["content-length"], body.len().to_string());
    assert_eq!(
        body,
        format!(
            r#"{{"grant_type":"refresh_token","refresh_token":"OLD","client_id":"{}","scope":"{}"}}"#,
            config::CLIENT_ID,
            config::REFRESH_SCOPES
        ),
        "体的键序照官方 refreshOAuthToken"
    );
}

/// profile 端点**线上**形态，同上：`Authorization` 在 `Content-Type` 前（官方 headers 的
/// 书写序）、GET 上带 `Content-Type`、没有 `anthropic-beta` / `anthropic-version`。
#[tokio::test]
async fn profile_request_wire_shape_is_axios() {
    let (addr, server) = serve_once(
        r#"{"account":{"uuid":"9922ef8e-7945-4f5a-ab4f-cf5f521531df","email":"a@b.c","has_claude_max":false,"has_claude_pro":false},"organization":{"uuid":"09520b85-f6b6-432f-97e2-6ecb804a083f","name":"Acme","organization_type":"claude_team","billing_type":"stripe_subscription","rate_limit_tier":"default_raven","seat_tier":"team_standard","has_extra_usage_enabled":false,"subscription_status":"active","subscription_created_at":"2026-04-15T13:03:55.239Z"}}"#,
    );
    let client = crate::clients::upstream_client(None).unwrap();
    let profile =
        super::fetch_profile_from(&client, &format!("http://{addr}/api/oauth/profile"), "TOKEN")
            .await
            .unwrap();
    assert_eq!(profile.account_uuid.as_deref(), Some("9922ef8e-7945-4f5a-ab4f-cf5f521531df"));
    assert_eq!(profile.org_uuid.as_deref(), Some("09520b85-f6b6-432f-97e2-6ecb804a083f"));
    assert_eq!(profile.subscription_created_at.as_deref(), Some("2026-04-15T13:03:55.239Z"));
    assert_eq!(profile.rate_limit_tier.as_deref(), Some("default_raven"));
    assert_eq!(profile.org_type.as_deref(), Some("claude_team"));
    assert_eq!(profile.tier.as_deref(), Some("Team Standard"));
    assert_eq!(profile.org_name.as_deref(), Some("Acme"));
    assert_eq!(profile.seat_tier.as_deref(), Some("team_standard"));
    assert_eq!(profile.subscription_status.as_deref(), Some("active"));
    assert_eq!(profile.extra_usage_enabled, Some(false));

    let raw = server.join().unwrap();
    let (line, names, values, body) = split_raw(&raw);
    assert_eq!(line, "GET /api/oauth/profile HTTP/1.1");
    assert_eq!(
        names,
        [
            "Accept",
            "Authorization",
            "Content-Type",
            "User-Agent",
            "Accept-Encoding",
            "Host",
            "Connection"
        ],
        "\n{raw}"
    );
    assert_eq!(values["authorization"], "Bearer TOKEN");
    assert_eq!(values["content-type"], "application/json");
    assert_eq!(values["user-agent"], "axios/1.15.2");
    assert_eq!(values["accept"], config::AXIOS_ACCEPT);
    assert_eq!(values["accept-encoding"], config::AXIOS_ACCEPT_ENCODING);
    assert_eq!(values["connection"], "close");
    assert!(body.is_empty());
    for h in ["anthropic-beta", "anthropic-version"] {
        assert!(!values.contains_key(h), "官方 profile 不带 {h}:\n{raw}");
    }
}
