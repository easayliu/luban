use crate::proxy::test_support::{
    ACCOUNT_UUID, all_on, detect_for, rewrite_body, rl_headers, test_cred,
};
use crate::proxy::{Bytes, StatusCode, config, header, store};

/// 握手三段**严格串行**，放行信号发在额度探测之后、收尾段之前。
///
/// 回归的是一个只在慢网下才露头的交错：曾经是「lead 一个任务 + 限时等它 + 再 spawn
/// rest」，等超时之后 rest 就发了，而 lead 可能还卡在 policy/settings 上，线上顺序变成
/// `policy/settings → rest → eval → quota`——抓包里 eval 与额度探测排在 penguin/mcp/
/// bootstrap 那批**之前**。这种错序不会有任何编译期或运行期症状，只能靠钉住顺序。
#[tokio::test]
async fn handshake_runs_in_capture_order() {
    use std::sync::{Arc, Mutex};
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let step = |name: &'static str, delay_ms: u64| {
        let log = log.clone();
        async move {
            // lead 故意比放行上限还慢，模拟慢网。
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            log.lock().unwrap().push(name);
        }
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let seq = tokio::spawn(crate::proxy::handshake_sequence(
        step("lead", 30),
        step("quota", 5),
        step("rest", 5),
        tx,
    ));

    // 调用方只等「额度探测发完」这一个信号。
    rx.await.expect("信号该在 quota 之后发出");
    {
        let seen = log.lock().unwrap();
        assert_eq!(*seen, ["lead", "quota"], "放行时 lead 与 quota 已经完成，rest 还没开始");
    }
    seq.await.unwrap();
    assert_eq!(*log.lock().unwrap(), ["lead", "quota", "rest"], "收尾段排在最后");
}

/// 调用方等超时之后，后台那串**照样按顺序跑完**——`rest` 不会越过还没完成的 `lead`。
///
/// 这是上一条的另一半：超时只是「不再等」，不是「取消」，更不是「让 rest 先跑」。
#[tokio::test]
async fn handshake_keeps_its_order_after_the_caller_gives_up() {
    use std::sync::{Arc, Mutex};
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let step = |name: &'static str, delay_ms: u64| {
        let log = log.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            log.lock().unwrap().push(name);
        }
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let seq = tokio::spawn(crate::proxy::handshake_sequence(
        step("lead", 60),
        step("quota", 5),
        step("rest", 5),
        tx,
    ));

    // 调用方 10ms 就放弃等待（真实里是 `HANDSHAKE_LEAD_TIMEOUT_MS`）。
    let waited = tokio::time::timeout(std::time::Duration::from_millis(10), rx).await;
    assert!(waited.is_err(), "该超时");
    assert!(log.lock().unwrap().is_empty(), "此刻 lead 还没跑完");

    // 放弃等待不影响后台：三段仍按序跑完，一段都没被取消。
    seq.await.unwrap();
    assert_eq!(*log.lock().unwrap(), ["lead", "quota", "rest"]);
}

/// 握手 / 额度探测的身份取**主请求实际发出去的那份**，不是重新派生一份。
///
/// `spoof_device_id=false`（严格抓包对齐模式支持的行为）时主请求保留客户端自己的
/// device；`spoof_identity=false` 时整份身份原样透传。这两种配置下再派生一份，
/// 同一个会话在上游看来就来自两台设备。
#[test]
fn handshake_identity_follows_the_outbound_request() {
    const CLIENT_DEV: &str = "client-device-abc";
    const SID: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","max_tokens":64000,"messages":[{{"role":"user","content":"hi"}}],"metadata":{{"user_id":"{{\"device_id\":\"{CLIENT_DEV}\",\"account_uuid\":\"acct-1\",\"session_id\":\"{SID}\"}}"}}}}"#
    ));
    let cred = test_cred();
    // 模拟路径：主请求发的是派生 device + 凭证 account，握手报的就是这一份。
    let sim = detect_for(&body, all_on()).expect("非 CC 形态该走模拟");
    let sent = rewrite_body(&body, &cred, "fp", all_on(), Some(&sim), None);
    let simulated = crate::proxy::outbound_identity(&sent, &cred);
    assert_eq!(simulated.device_id, cred.spoof_device_id("fp").unwrap());
    assert_eq!(simulated.account_uuid, ACCOUNT_UUID);

    // **真实 CC 那条路**（`sim = None`，客户端自带 metadata）才是这两个开关真正生效
    // 的地方：`spoof_identity` 按原格式定点改写，`spoof_device_id` 决定动不动 device。
    let cc_body = Bytes::from(
            String::from_utf8(body.to_vec()).unwrap().replace(
                r#""messages":[{"role":"user","content":"hi"}]"#,
                &format!(
                    r#""messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"{}"}}]"#,
                    config::CC_SYSTEM_IDENTITY
                ),
            ),
        );
    let ident = |flags| {
        let sent = rewrite_body(&cc_body, &cred, "fp", flags, None, None);
        crate::proxy::outbound_identity(&sent, &cred)
    };

    // `spoof_device_id=false`：主请求保留客户端那个 device，握手必须跟着。
    let keep_dev = ident(store::ForwardFlags { spoof_device_id: false, ..all_on() });
    assert_eq!(keep_dev.device_id, CLIENT_DEV, "device 要跟着主请求，不能另派生一个");
    assert_eq!(keep_dev.account_uuid, ACCOUNT_UUID, "account 仍然换成凭证的");

    // `spoof_identity=false`：整份原样透传，两项都跟客户端。
    let passthrough = ident(store::ForwardFlags { spoof_identity: false, ..all_on() });
    assert_eq!(passthrough.device_id, CLIENT_DEV);
    assert_eq!(passthrough.account_uuid, "acct-1", "连 account 都不该换");

    // 额度探测复用主请求那串 `user_id` 的**原文**，逐字节相同。
    let probe = crate::proxy::with_outbound_identity(
        crate::proxy::probe_body(crate::proxy::QUOTA_PROBE_MODEL),
        &keep_dev,
    );
    let v: serde_json::Value = serde_json::from_slice(&probe).unwrap();
    assert_eq!(
        v["metadata"]["user_id"].as_str(),
        keep_dev.raw_user_id.as_deref(),
        "探测与主请求逐字节同一串身份"
    );
    let inner: serde_json::Value =
        serde_json::from_str(v["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(inner["device_id"], CLIENT_DEV, "探测与主请求同一台设备");
    assert_eq!(inner["account_uuid"], ACCOUNT_UUID);
    assert_eq!(inner["session_id"], SID);
}

/// 测试结果里的额度快照直接来自本次响应的限流头（200 与 429 都带）；而响应压根没有这些
/// 头时给 `None` 而不是一坨全空对象——CDN 拦截页、网关错误就是那样，前端不该被迫自己
/// 再判一遍「是不是全空」。
#[test]
fn probe_quota_reads_ratelimit_headers() {
    let hdr = rl_headers;

    let info = hdr(&[
        ("anthropic-ratelimit-unified-status", "allowed_warning"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.32"),
        ("anthropic-ratelimit-unified-5h-reset", "1800000000"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.76"),
        ("anthropic-ratelimit-unified-representative-claim", "7d"),
        ("retry-after", "228721"),
    ]);
    let q = crate::proxy::ProbeQuota::from_info(&info).expect("有限流头就该有快照");
    assert_eq!(q.unified_status.as_deref(), Some("allowed_warning"));
    assert_eq!(q.rl_5h_utilization, Some(0.32));
    assert_eq!(q.rl_5h_reset, Some(1_800_000_000));
    assert_eq!(q.rl_7d_utilization, Some(0.76));
    assert_eq!(q.rl_representative.as_deref(), Some("7d"));
    assert_eq!(q.retry_after_secs, Some(228_721), "429 的等待时间原样带出，不夹");

    // 非限流类的 anthropic- 头会被 RateLimitInfo 收进 raw，但解析不出任何额度字段。
    assert!(
        crate::proxy::ProbeQuota::from_info(&hdr(&[("anthropic-version", "2023-06-01")])).is_none()
    );
    assert!(crate::proxy::ProbeQuota::from_info(&hdr(&[])).is_none());
}

/// 测试要能让**卡片**跟着更新：卡片上的额度快照来自 `latest_quota`，而那读的是
/// `usage_logs` 里最新一条带限流信息的行。所以探测必须落一条日志——否则测出来的额度
/// 只活在弹窗里，卡片照旧显示上一次真实请求时的旧数，两处对不上。
///
/// 同时钉住另外两件事：这条日志按**实际用量**计价（测试真的花了钱，不记等于让累计花费
/// 虚低），且以 `device_id = "probe"` 标出，翻日志时能与真实流量分开。
#[test]
fn probe_usage_log_feeds_the_card_quota() {
    // Arc 包着：落库现在走 spawn_blocking（见 `spawn_usage_log`），要能把 store 交出去。
    // 这个测试不在 tokio 运行时里，故 `Handle::try_current` 失败、退回就地同步写——
    // 下面的断言因此仍能立刻读到结果。
    let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
    let cred = store.insert("t", None, "a", "r", 0, None, None, 1).unwrap();
    let info = rl_headers(&[
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.32"),
        ("anthropic-ratelimit-unified-5h-reset", "1800000000"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.76"),
    ]);
    // 上游 200 的响应体形状（只留计价要用的字段）。
    let body = Bytes::from(
        r#"{"model":"claude-opus-5-20260115","usage":{"input_tokens":320,"output_tokens":1}}"#,
    );
    crate::proxy::ProbeLog {
            store: &store,
            cred: &cred,
            req_model: "claude-opus-5",
            started: &std::time::Instant::now(),
            out_ua: Some(config::CC_USER_AGENT.into()),
            request_id: "lb-probe-test".into(),
            sent: Bytes::from_static(
                br#"{"model":"claude-opus-5","metadata":{"user_id":"{\"device_id\":\"probe-dev-out\",\"account_uuid\":\"a\",\"session_id\":\"probe-sess\"}"}}"#,
            )}
        .record(StatusCode::OK, &body, &info, Some("req_up_test"));
    let logged = &store.list_usage_logs(1).unwrap()[0];
    assert_eq!(
        logged.forensics.device_id_out.as_deref(),
        Some("probe-dev-out"),
        "测试流水也记出站 device_id"
    );
    assert_eq!(logged.forensics.session_id.as_deref(), Some("probe-sess"));
    assert!(logged.forensics.shape.is_some());

    let q = store.latest_quota(cred.id).unwrap().expect("卡片应能读到这次测试的额度");
    assert_eq!(q.rl_5h_utilization, Some(0.32));
    assert_eq!(q.rl_7d_utilization, Some(0.76));
    assert_eq!(q.unified_status.as_deref(), Some("allowed"));

    let logs = store.list_usage_logs(10).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].device_id.as_deref(), Some("probe"), "日志里要能认出这是测试");
    assert_eq!(logs[0].model.as_deref(), Some("claude-opus-5-20260115"), "模型以上游回报为准");
    assert_eq!(logs[0].input_tokens, Some(320));
    // opus $5/MTok 输入 + $25/MTok 输出：320×5 + 1×25 = 1625 微美元。
    assert_eq!(logs[0].cost_usd, Some(0.001625), "按实际用量计价，不是记 0");
    // 测试没有来访客户端，但确实按官方形态发了出去：入站空、出站照实。
    assert_eq!(logs[0].ua, None, "测试不来自任何客户端，入站 UA 必须为空");
    assert_eq!(logs[0].ua_out.as_deref(), Some(config::CC_USER_AGENT), "出站照实记");
}

/// 连通性测试发出去的那条请求本身必须是**官方形态**：`system` 是官方那几块（含上游对
/// OAuth 凭证唯一强制的那句身份声明）、`metadata` 是该凭证自洽的身份、`anthropic-beta`
/// 带 `oauth-2025-04-20`、`Authorization` 是该凭证的 token。
///
/// 真正盯的是**测试与真实转发共用同一套改写**：`probe` 只给一个裸 body，剩下的全交给
/// [`crate::proxy::rewrite_body`]/[`crate::proxy::build_forward_headers`]。若哪天有人图省事在 probe 里
/// 手抄一份 system，改写规则一变就会得到「测试通过但转发失败」——那比没有这个功能更糟。
#[test]
fn probe_request_is_official_shaped() {
    let cred = test_cred();
    const HAIKU: &str = "claude-haiku-4-5-20251001";
    let sim = crate::proxy::probe_simulation(&cred, HAIKU);
    let out =
        rewrite_body(&crate::proxy::probe_body(HAIKU), &cred, "fp", all_on(), Some(&sim), None);
    let s = String::from_utf8(out.to_vec()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();

    // 官方额度探测的顶层 key 序（`cap/2.1.260-2/00004`）：
    // model → max_tokens → messages → metadata。
    // probe 不开 thinking（一条 1 token 的探测不需要），故也不带 `context_management`
    // ——那个字段依赖 thinking，硬补上游回 400，见 [`crate::proxy::ensure_context_management`]。
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["model", "max_tokens", "messages", "metadata"], "\n{s}");
    assert!(v.get("context_management").is_none(), "没开 thinking 就不该补: {s}");
    assert_eq!(v["max_tokens"], 1, "测试只要 1 个 token，别把额度花在正文上");

    // 官方那条额度探测**没有 system**：不发 billing header、不发基座、不发工具。
    assert!(v.get("system").is_none(), "QuotaProbe 不带 system: {s}");
    assert!(v.get("tools").is_none(), "也不带工具: {s}");
    assert!(v.get("diagnostics").is_none(), "更没有 diagnostics: {s}");

    // 身份：伪装 metadata 用的是这个凭证的 account_uuid，不是空串。
    let user_id = v["metadata"]["user_id"].as_str().unwrap();
    assert!(user_id.contains(ACCOUNT_UUID), "metadata 应带该凭证的 account_uuid: {user_id}");

    let headers = crate::proxy::build_forward_headers(
        &crate::proxy::HeaderMap::new(),
        "tok",
        all_on(),
        Some(&sim),
        None,
    );
    let beta = headers.get("anthropic-beta").unwrap().to_str().unwrap();
    assert!(
        beta.split(',').any(|p| p == config::OAUTH_BETA_HEADER),
        "OAuth 鉴权必需这一项: {beta}"
    );
    assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Bearer tok");
    assert_eq!(
        headers.get(header::USER_AGENT).unwrap(),
        config::CC_USER_AGENT,
        "测试请求同样按官方客户端形态发"
    );

    // **别的模型不套额度探测那身皮**：官方那条恒为 haiku-4.5，一条 opus 请求长着
    // 「无 system、无 billing header、那一小串 beta」的样子，官方从不产生。
    // 连通性测试恰恰要逐个模型都测一遍，所以这条必须分开。
    for model in ["claude-opus-5", "claude-fable-5-1", "claude-sonnet-5"] {
        let sim = crate::proxy::probe_simulation(&cred, model);
        assert_ne!(
            sim.profile.kind,
            config::CcProfileKind::QuotaProbe,
            "{model} 不该套 QuotaProbe"
        );
        let out =
            rewrite_body(&crate::proxy::probe_body(model), &cred, "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap_or_else(|| panic!("{model} 该有 system"));
        assert!(
            sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"),
            "{model}: 主线程形态要带 billing header"
        );
        assert_eq!(sys[1]["text"], config::CC_SYSTEM_IDENTITY, "{model}: 身份声明");
        let beta = crate::proxy::simulated_beta(&sim.beta, None);
        assert!(beta.contains(config::CC_BETA_CLAUDE_CODE), "{model}: 主线程串带 claude-code");

        // **不能只换 profile**：`max_tokens:1` 的体配主线程的 system/beta，会得到一条
        // 没有 `thinking`/`context_management`/`output_config`、还非流式的请求——同样是
        // 抓包里不存在的混合形态，也验证不了真实主线程链路。逐项钉住。
        assert_eq!(v["thinking"]["type"], "adaptive", "{model}: 要有 thinking\n{v}");
        assert_eq!(
            v["context_management"]["edits"][0]["type"], "clear_thinking_20251015",
            "{model}: 要有 context_management\n{v}"
        );
        assert_eq!(v["output_config"]["effort"], "high", "{model}: 官方主线程恒带\n{v}");
        assert_eq!(v["stream"], true, "{model}: 官方主线程恒为流式\n{v}");
        assert_eq!(
            v["diagnostics"],
            serde_json::json!({ "previous_message_id": serde_json::Value::Null }),
            "{model}: 首轮 diagnostics\n{v}"
        );
        let tools = v["tools"].as_array().unwrap_or_else(|| panic!("{model} 该注入工具"));
        assert!(tools.iter().any(|t| t["name"] == "Bash"), "{model}: 要有官方工具\n{v}");
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys.first(), Some(&"model"), "{model}: key 序");
        assert_eq!(keys.last(), Some(&"stream"), "{model}: stream 在队尾");
    }
}

/// 主线程形态的探活收到的是 SSE，要攒回一条整段 Message 再交给后面那套读法
/// （封号判定、[`crate::proxy::probe_report`] 解 model/error_type）。
#[test]
fn probe_aggregates_the_streamed_response() {
    const SSE: &str = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","#,
        r#""role":"assistant","model":"claude-opus-5","content":[],"stop_reason":null,"#,
        r#""usage":{"input_tokens":10,"output_tokens":1}}}"#,
        "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        "\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let (out, end) = crate::proxy::aggregate_probe_sse(SSE.as_bytes());
    assert_eq!(end, super::ProbeStreamEnd::Complete);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["model"], "claude-opus-5", "probe_report 靠这个字段: {v}");
    assert_eq!(v["content"][0]["text"], "ok");

    // 半截流不能悄悄报成功：原样交回，并如实标成 Incomplete。
    let half = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n";
    let (out, end) = crate::proxy::aggregate_probe_sse(half.as_bytes());
    assert_eq!(out, Bytes::from(half));
    assert!(matches!(end, super::ProbeStreamEnd::Incomplete(_)), "{end:?}");

    // 流里来了错误事件：交回那份错误 JSON，标成 UpstreamError。
    let err = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","content":[]}}"#,
        "\n\n",
        "event: error\n",
        r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        "\n\n",
    );
    let (out, end) = crate::proxy::aggregate_probe_sse(err.as_bytes());
    assert_eq!(end, super::ProbeStreamEnd::UpstreamError);
    assert_eq!(crate::proxy::parse_upstream_error(&out).1, "Overloaded");
}

/// 通过只有一种：2xx 且拿到完整有效的 Message。200 里的错误事件、半截流、不是 Message 的体
/// 都算失败，并说清原因——此前它们一律报 `ok: true`，暂停中的号会被一次坏掉的测试放回池子。
#[test]
fn probe_report_passes_only_on_a_complete_message() {
    use super::{ProbeStreamEnd as End, probe_report};
    let ok = StatusCode::OK;
    let msg = br#"{"type":"message","model":"claude-opus-5","content":[]}"#;
    let r = probe_report(ok, msg, End::Complete, 1, None);
    assert!(r.ok && r.model.as_deref() == Some("claude-opus-5") && r.error.is_none());
    // 非流式（haiku 额度探测）的 Message 同样算通过。
    assert!(probe_report(ok, msg, End::NotStreamed, 1, None).ok);

    let err = br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    let r = probe_report(ok, err, End::UpstreamError, 1, None);
    assert!(!r.ok);
    assert_eq!(r.error_type.as_deref(), Some("overloaded_error"));
    assert_eq!(r.error.as_deref(), Some("Overloaded"));

    let r = probe_report(
        ok,
        b"event: message_start\n",
        End::Incomplete("no message_stop event"),
        1,
        None,
    );
    assert!(
        !r.ok && r.error.as_deref().unwrap().contains("no message_stop event"),
        "{:?}",
        r.error
    );

    for body in [&b"not json"[..], br#"{"type":"error"}"#, br#"{"model":"x"}"#] {
        let r = probe_report(ok, body, End::NotStreamed, 1, None);
        assert!(!r.ok, "{}", String::from_utf8_lossy(body));
    }

    // 解不开的压缩编码：2xx 也不算通过，错误里不出现压缩字节。
    let r = probe_report(ok, b"\x1f\x8b\x08\x00garbage", End::Undecodable, 1, None);
    assert!(!r.ok && r.error.as_deref().unwrap().contains("cannot decode"), "{:?}", r.error);
    assert!(!r.error.as_deref().unwrap().contains("garbage"));

    let r = probe_report(
        StatusCode::FORBIDDEN,
        br#"{"error":{"type":"permission_error","message":"no"}}"#,
        End::NotStreamed,
        1,
        None,
    );
    assert!(!r.ok && r.error_type.as_deref() == Some("permission_error"));
}

/// 回归：订阅未生效暂停中的号，一次**失败**的测试（200 里带错误事件、半截流）不能把它放回
/// 池子；只有完整 Message 那次才恢复。恢复与否只看 `report.ok`，这里把报告与恢复连起来测。
#[test]
fn broken_probe_does_not_resume_a_subscription_pause() {
    use super::{ProbeStreamEnd as End, probe_report, settle_passing_probe};
    let store = store::CredentialStore::open_in_memory().unwrap();
    let cred = store.insert("a", None, "ta", "ra", 0, None, None, 1).unwrap();
    assert!(crate::proxy::park_org_oauth_disallowed(&store, &cred, 403, "forward"));
    let info = super::RateLimitInfo::from_headers(&crate::proxy::HeaderMap::new());
    let settle = |status, body: &[u8], end| {
        let report = probe_report(status, body, end, 1, None);
        if report.ok {
            settle_passing_probe(&store, &cred, "claude-opus-5", &info, true);
        }
        report.ok
    };
    let err = br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    assert!(!settle(StatusCode::OK, err, End::UpstreamError));
    assert!(!settle(
        StatusCode::OK,
        b"event: message_start\n",
        End::Incomplete("no message_stop event")
    ));
    assert!(store.get(cred.id).unwrap().unwrap().is_subscription_paused(), "坏掉的测试不该恢复");

    let msg = br#"{"type":"message","model":"claude-opus-5","content":[]}"#;
    assert!(settle(StatusCode::OK, msg, End::Complete));
    let got = store.get(cred.id).unwrap().unwrap();
    assert!(!got.disabled && got.ban_reason.is_none(), "完整 Message 那次才恢复");
}
