use super::*;

// ---- 重构对照 ----
//
// 设了 `LUBAN_TELEMETRY_DUMP=<目录>` 时，每次 `ingest` 之后把全部待发批次规范化后追加到
// `<目录>/<测试名>.txt`：随机 uuid 按首次出现编号，时间戳换成相对测试起点的毫秒。改动
// `process` 这类大段代码前后各跑一遍、diff 两个目录，就能确认产出的事件逐字没变。
// 测试里的「当前时刻」统一走 [`frozen_now`]，否则两次运行之间的微秒抖动会让毫秒级字段跳 1。

thread_local! {
    /// 每个测试线程首次取时定下、取整到秒，之后不再走。
    static FROZEN: SystemTime = {
        let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    };
    static DUMP_SEQ: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static DUMP_UUIDS: std::cell::RefCell<HashMap<String, usize>> =
        std::cell::RefCell::new(HashMap::new());
}

fn frozen_now() -> SystemTime {
    FROZEN.with(|t| *t)
}

pub(super) fn dump_pending(st: &State) {
    use std::fmt::Write as _;
    let Some(dir) = std::env::var_os("LUBAN_TELEMETRY_DUMP") else { return };
    let name = std::thread::current().name().unwrap_or("main").replace("::", "-");
    let seq = DUMP_SEQ.with(|c| c.replace(c.get() + 1));
    let ts = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut out = format!("== ingest {seq}\n");
    let mut keys: Vec<_> = st.pending.keys().collect();
    keys.sort();
    for k in keys {
        let p = &st.pending[k];
        let _ = writeln!(
            out,
            "-- {k:?} version={} sub={} model={} betas={} prompt={} started={:?} export_at={:?}",
            p.version,
            p.subscription_type,
            p.model,
            p.betas,
            p.prompt_id,
            p.started_wall.map(|t| ts(t.into())),
            p.export_at.map(ts),
        );
        let _ = writeln!(out, "identity {:?}", p.identity);
        for (t, e) in &p.events {
            let _ = writeln!(out, "ev {} {e}", ts(*t));
        }
        for d in &p.dd {
            let _ = writeln!(out, "dd {d}");
        }
        for m in &p.metrics {
            let _ = writeln!(out, "metric {m:?}");
        }
    }
    let text = DUMP_UUIDS.with(|u| normalize_dump(&out, &mut u.borrow_mut()));
    let path = std::path::Path::new(&dir).join(format!("{name}.txt"));
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    std::io::Write::write_all(&mut f, text.as_bytes()).unwrap();
}

/// uuid → `<uN>`（按首次出现编号），一天以内的 RFC3339 时间戳 → `T+毫秒`（相对
/// [`frozen_now`]；`build_time` 那种固定时刻原样留着），base64 的 JSON 串先解开再处理，
/// 随真实日期走的 `build_age_mins` 抹掉。
fn normalize_dump(s: &str, uuids: &mut HashMap<String, usize>) -> String {
    let s = &decode_b64_strings(s);
    let base: DateTime<Utc> = frozen_now().into();
    let b = s.as_bytes();
    let is_uuid = |w: &[u8]| {
        w.len() == 36
            && w.iter().enumerate().all(|(i, c)| match i {
                8 | 13 | 18 | 23 => *c == b'-',
                _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
            })
    };
    let is_ts_head = |w: &[u8]| {
        w.len() >= 11
            && w[..4].iter().all(u8::is_ascii_digit)
            && w[4] == b'-'
            && w[7] == b'-'
            && w[10] == b'T'
    };
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if boundary && i + 36 <= b.len() && is_uuid(&b[i..i + 36]) {
            let next = uuids.len() + 1;
            let n = *uuids.entry(s[i..i + 36].to_string()).or_insert(next);
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("<u{n}>"));
            i += 36;
            continue;
        }
        if boundary && is_ts_head(&b[i..b.len().min(i + 11)]) {
            let end = b[i..]
                .iter()
                .position(|c| !(c.is_ascii_digit() || b"-:T.Z+".contains(c)))
                .map_or(b.len(), |p| i + p);
            if let Ok(t) = DateTime::parse_from_rfc3339(&s[i..end])
                && (t.with_timezone(&Utc) - base).num_hours().abs() < 24
            {
                let ms = (t.with_timezone(&Utc) - base).num_milliseconds();
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("T{ms:+}"));
                i = end;
                continue;
            }
        }
        for key in ["\"build_age_mins\":", "\"buildAgeMins\":"] {
            if s[i..].starts_with(key) {
                let digits = b[i + key.len()..].iter().take_while(|c| c.is_ascii_digit()).count();
                out.push_str(key);
                out.push('N');
                i += key.len() + digits;
            }
        }
        let Some(ch) = s[i..].chars().next() else { break };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `"eyJ…"` 这种 base64 编码的 JSON 串换成 `b64:{…}`，好让里面的 uuid 与时间戳也被规范化。
fn decode_b64_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find("\"eyJ") {
        out.push_str(&rest[..p + 1]);
        rest = &rest[p + 1..];
        let len =
            rest.bytes().take_while(|c| c.is_ascii_alphanumeric() || b"+/=".contains(c)).count();
        match STANDARD.decode(&rest[..len]).ok().and_then(|v| String::from_utf8(v).ok()) {
            Some(json) if rest[len..].starts_with('"') => {
                out.push_str("b64:");
                out.push_str(&json);
                rest = &rest[len..];
            }
            _ => {}
        }
    }
    out.push_str(rest);
    out
}

fn identity() -> Identity {
    Identity {
        session_id: "111e3644-948f-43fb-9bc2-cac60e65fd32".into(),
        device_id: "b9".repeat(32),
        account_uuid: "9922ef8e-7945-4f5a-ab4f-cf5f521531df".into(),
        organization_uuid: Some("09520b85-f6b6-432f-97e2-6ecb804a083f".into()),
        subscription_type: "team".into(),
        version: "2.1.258".into(),
        agent_id: None,
        ..Default::default()
    }
}

/// 与抓包一致的 CC 请求体（截取要紧的字段）。
fn cc_body(last_user_text: bool) -> Vec<u8> {
    let last = if last_user_text {
        json!({"role":"user","content":[{"type":"text","text":"<system-reminder>x</system-reminder>"},{"type":"text","text":"hello there"}]})
    } else {
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]})
    };
    json!({
            "model": "claude-opus-5",
            "messages": [
                {"role":"user","content":"first"},
                {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]},
                last
            ],
            "system": [
                {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=0f7f8; cc_prompt_id=6c079143-0c53-4c48-817d-105460b3f622;"},
                {"type":"text","text":"You are Claude Code"},
                {"type":"text","text":"base prompt","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}},
                {"type":"text","text":"While auto mode is active: rules","cache_control":{"type":"ephemeral","ttl":"1h"}}
            ],
            "tools": [
                {"name":"Bash","description":"run","input_schema":{"type":"object"}},
                {"name":"DeferredToolPlaceholder","description":"d","input_schema":{"type":"object"},"defer_loading":true}
            ],
            "thinking": {"type":"adaptive"},
            "output_config": {"effort":"high"},
            "metadata": {"user_id": "{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"},
            "max_tokens": 64000,
            "stream": true
        })
        .to_string()
        .into_bytes()
}

fn call(body: Vec<u8>, request_id: &str, stop: &str) -> ApiCall {
    ApiCall {
            cred_id: 7,
            account_uuid: Some("9922ef8e-7945-4f5a-ab4f-cf5f521531df".into()),
            org_type: Some("claude_team".into()),
            body: Bytes::from(body),
            betas: Some(
                "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,effort-2025-11-24"
                    .into(),
            ),
            session_header: None,
            ua_out: "claude-cli/2.1.258 (external, cli)".into(),
            organization_id: Some("09520b85-f6b6-432f-97e2-6ecb804a083f".into()),
            started_at: frozen_now() - Duration::from_secs(8),
            ttft_ms: Some(1800),
            total_ms: 6118,
            request_id: Some(request_id.into()),
            client_request_id: Some("3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e".into()),
            agent: AgentHeaders::default(),
            message_id: Some("msg_011Cedjuoa4oBPzoB2CSUNEB".into()),
            stop_reason: Some(stop.into()),
            resp_model: Some("claude-opus-5".into()),
            input_tokens: 2,
            output_tokens: 31,
            cache_read_tokens: 26736,
            cache_creation_tokens: 8729,
            cache_creation_5m_tokens: None,
            cache_creation_1h_tokens: None,
            iterations: 0,
            text_chars: 87,
            reply_input_chars: 87,
            thinking_chars: 0,
            saw_thinking: false,
            stream_events: Some(12),
            tool_use_lens: Vec::new(),
            tool_calls: Vec::new(),
            cost_usd: Some(0.18),
            speed: None,
            failure: None,
            aborted: false,
            cache_keepalive_ok: false,
            client_tools: false,
            link_prompt_id: None,
        bare: false,
            dispatch: Some("v2d".into()),
            prior_attempts: Vec::new(),
        }
}

/// 把一条调用改成「客户端那头失败了」。
fn failed(mut c: ApiCall, status: Option<u16>, etype: &str, message: &str) -> ApiCall {
    c.stop_reason = None;
    c.message_id = None;
    c.resp_model = None;
    c.input_tokens = 0;
    c.output_tokens = 0;
    c.cache_read_tokens = 0;
    c.cache_creation_tokens = 0;
    c.text_chars = 0;
    c.cost_usd = None;
    c.failure = Some(CallFailure {
        status,
        error_type: (!etype.is_empty()).then(|| etype.to_string()),
        message: message.to_string(),
        in_band: status.is_none() && !etype.is_empty(),
    });
    c
}

/// 错误消息的哈希照官方 `Dp`：`${status} ${JSON.stringify(body)}` 规整后取 SHA-256 前 12 位，request-id 换成占位
/// （`cap/auto-2.1.306-20261021-retry` 十次注入的 529 全是 `028822c6cf53`）。
#[test]
fn error_message_hash_matches_the_capture() {
    for rid in ["req_injected1792566703261005", "req_injected1792566756200010"] {
        assert_eq!(
            super::shape::error_message_hash(
                Some(529),
                Some("overloaded_error"),
                "Overloaded",
                Some(rid)
            ),
            "028822c6cf53"
        );
    }
}

/// 客户端自己重发的几次（`cap/auto-2.1.306-20261021-retry`）。`-p` 一条请求：`fails` 次 529 之后，`ok` 为真再成功一次，否则
/// 让扣住超时按最终失败补报。每次尝试 8ms 失败，间隔照抓包 0 / 606 / 1152ms。
fn retried_sdk_turn(fails: usize, ok: bool, ua: &str) -> Vec<(String, Value)> {
    retried_sdk_turn_with(fails, ok, ua, true)
}

/// 同上；`real` 为真时照真实客户端换 dispatch（头一次 `v2d`，之后都是 `v2p`），否则照模拟路径一律 `v2d`。
fn retried_sdk_turn_with(fails: usize, ok: bool, ua: &str, real: bool) -> Vec<(String, Value)> {
    let t = Telemetry::default();
    let body = String::from_utf8(cc_body(true))
        .unwrap()
        .replace("cc_entrypoint=cli", "cc_entrypoint=sdk-cli");
    let base = frozen_now() - Duration::from_secs(60);
    let gaps = [0u64, 606, 1_152, 2_000, 4_000];
    let mut at = base;
    for (i, gap) in gaps.iter().enumerate().take(fails) {
        let mut c = failed(
            call(
                body.clone().into_bytes(),
                &format!("req_injected17925667032610{i:02}"),
                "end_turn",
            ),
            Some(529),
            "overloaded_error",
            "Overloaded",
        );
        c.ua_out = ua.into();
        c.started_at = at;
        c.total_ms = 8;
        c.dispatch = Some(if real && i > 0 { "v2p" } else { "v2d" }.into());
        t.ingest(c);
        at += Duration::from_millis(8 + gap);
    }
    if ok {
        let mut c = call(body.into_bytes(), "req_ok", "end_turn");
        c.ua_out = ua.into();
        c.started_at = at;
        c.total_ms = 1_936;
        c.dispatch = Some(if real && fails > 0 { "v2p" } else { "v2d" }.into());
        t.ingest(c);
    } else {
        give_up_retries(&t);
    }
    let st = t.0.state.lock();
    st.pending
        .values()
        .flat_map(|p| p.events.iter())
        .map(|(_, e)| (ev_name(e).to_string(), meta_of(e)))
        .collect()
}

/// R1：头一次 529 换 `v2p` 原样重发（`dispatch_header_fallback`，不算重试）后成功——只报一次成功、`attempt: 1`、
/// `dispatch: v2p`，`durationMsIncludingRetries` 从头一次算起；没有失败收尾，结果是一条 `success`。
#[test]
fn a_dispatch_fallback_then_success_is_one_turn() {
    let ev = retried_sdk_turn(1, true, "claude-cli/2.1.306 (external, sdk-cli)");
    let all =
        |n: &str| ev.iter().filter(|(x, _)| x == n).map(|(_, m)| m.clone()).collect::<Vec<_>>();
    let fb = all("tengu_dispatch_header_fallback");
    assert_eq!(fb.len(), 1);
    assert_eq!(
        (&fb[0]["dispatch"], &fb[0]["resend_dispatch"], &fb[0]["reason"], &fb[0]["status"]),
        (&json!("v2d"), &json!("v2p"), &json!("5xx"), &json!("529"))
    );
    assert!(all("tengu_api_retry").is_empty() && all("tengu_api_error").is_empty());
    let ok = all("tengu_api_success");
    assert_eq!(ok.len(), 1);
    assert_eq!((&ok[0]["attempt"], &ok[0]["dispatch"]), (&json!(1), &json!("v2p")));
    assert!(ok[0]["durationMsIncludingRetries"].as_i64().unwrap() >= 1_936 + 8);
    let r = all("tengu_sdk_result");
    assert_eq!(r.len(), 1, "只有一条结果");
    assert_eq!(
        (&r[0]["subtype"], &r[0]["is_error"], &r[0]["saw_retry"]),
        (&json!("success"), &json!(false), &json!(false))
    );
    assert_eq!(all("tengu_turn_end").len(), 1);
}

/// R2：`CLAUDE_CODE_MAX_RETRIES=2`，四次 529（换 dispatch 重发一次 + 重试两次 + 最后一次）。中间只有一条
/// `dispatch_header_fallback` 与两条 `api_retry`（attempt 1 / 2，带错误哈希与 306 的栈），最后一条才收尾：
/// `api_error{attempt: 3}`、`turn_end{api_error}` 与结果 `success` + `is_error`，带两项状态码、API 耗时 0；之前没有成功过的
/// 主请求，就没有会话结束的逐出提示。
#[test]
fn retries_exhausted_end_the_turn_once() {
    let ev = retried_sdk_turn(4, false, "claude-cli/2.1.306 (external, sdk-cli)");
    let all =
        |n: &str| ev.iter().filter(|(x, _)| x == n).map(|(_, m)| m.clone()).collect::<Vec<_>>();
    assert_eq!(all("tengu_dispatch_header_fallback").len(), 1);
    let retries = all("tengu_api_retry");
    assert_eq!(retries.len(), 2);
    for (i, r) in retries.iter().enumerate() {
        assert_eq!(r["attempt"], i as u64 + 1);
        assert_eq!(r["error"], "API error: type=server_overload status=529");
        assert_eq!(
            (&r["errorType"], &r["status"], &r["attempt_duration_ms"]),
            (&json!("server_overload"), &json!(529), &json!(8))
        );
        assert_eq!(r["error_message_hash"], "028822c6cf53");
        assert_eq!(
            (&r["error_constructor"], &r["error_stack_hash"]),
            (&json!("Yzt"), &json!("5b32b751db51"))
        );
    }
    assert_eq!(retries[0]["delayMs"], 606);
    assert_eq!(retries[1]["delayMs"], 1_152);
    let err = all("tengu_api_error");
    assert_eq!(err.len(), 1, "只在最后一次报失败");
    assert_eq!((&err[0]["attempt"], &err[0]["dispatch"]), (&json!(3), &json!("v2p")));
    assert_eq!(err[0]["error_message_hash"], "028822c6cf53");
    assert_eq!(all("tengu_turn_end").len(), 1);
    let r = all("tengu_sdk_result");
    assert_eq!(r.len(), 1, "只有一条结果");
    assert_eq!(
        (
            &r[0]["subtype"],
            &r[0]["is_error"],
            &r[0]["saw_retry"],
            &r[0]["retry_status"],
            &r[0]["api_error_status"],
            &r[0]["duration_api_ms"]
        ),
        (&json!("success"), &json!(true), &json!(true), &json!(529), &json!(529), &json!(0))
    );
    let keys: Vec<&str> = r[0].as_object().unwrap().keys().map(String::as_str).collect();
    let pos = |k: &str| keys.iter().position(|x| *x == k).unwrap();
    assert_eq!(
        (pos("retry_status"), pos("api_error_status")),
        (pos("saw_compact") + 1, pos("saw_compact") + 2)
    );
    assert!(all("tengu_cache_eviction_hint").is_empty(), "之前没有成功过的主请求");
}

/// 换了别的请求就说明前面那串已经放弃了：它的最后一次按最终失败补报，再处理新的这条。
#[test]
fn a_new_request_closes_a_held_retry_chain() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.306 (external, cli)";
    let mut f = failed(
        call(cc_body(true), "req_f", "end_turn"),
        Some(529),
        "overloaded_error",
        "Overloaded",
    );
    f.ua_out = ua.into();
    f.started_at = frozen_now() - Duration::from_secs(60);
    t.ingest(f);
    assert!(
        t.0.state
            .lock()
            .pending
            .get(&key())
            .is_none_or(|p| p.events.iter().all(|(_, e)| ev_name(e) != "tengu_api_error")),
        "先扣着"
    );
    let mut next = call(with_prompt(cc_body(true), PROMPT_2), "req_next", "end_turn");
    next.ua_out = ua.into();
    next.started_at = frozen_now() - Duration::from_secs(30);
    t.ingest(next);
    let st = t.0.state.lock();
    let names: Vec<&str> = st.pending[&key()].events.iter().map(|(_, e)| ev_name(e)).collect();
    let err = names.iter().position(|n| *n == "tengu_api_error").expect("补报了最终失败");
    let ok = names.iter().position(|n| *n == "tengu_api_success").unwrap();
    assert!(err < ok);
}

/// 模拟路径的出站 dispatch 恒为 `v2d`：重发没有真换成 `v2p`，就不算换 dispatch 重发，头一次失败照普通重试报，成功那条的
/// `dispatch` 也照实际报 `v2d`。
#[test]
fn a_resend_without_v2p_is_an_ordinary_retry() {
    let ev = retried_sdk_turn_with(1, true, "claude-cli/2.1.306 (external, sdk-cli)", false);
    let all =
        |n: &str| ev.iter().filter(|(x, _)| x == n).map(|(_, m)| m.clone()).collect::<Vec<_>>();
    assert!(all("tengu_dispatch_header_fallback").is_empty());
    let retries = all("tengu_api_retry");
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0]["attempt"], 1);
    let ok = all("tengu_api_success");
    assert_eq!((&ok[0]["attempt"], &ok[0]["dispatch"]), (&json!(2), &json!("v2d")));
}

/// `/clear` 之后新会话的头一条遇到 529、重发成功：扣着的那次不能先去判会话来历（那一步会把旧会话标成已清），
/// 重发那条到了照样认出是 `/clear` 过来的——不出启动串、带着旧会话的 `parent_session_id`。
#[test]
fn a_retry_after_clear_keeps_the_lineage() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(300);
    let s2 = "22222222-2222-4222-8222-222222222222";
    t.ingest(quota_probe_call("11111111-1111-4111-8111-111111111111", base));
    t.ingest(main_call(SESSION, "req_1", "msg_1", base + Duration::from_secs(5)));
    let at = base + Duration::from_secs(60);
    t.ingest(failed(
        main_call(s2, "req_f", "msg_f", at),
        Some(529),
        "overloaded_error",
        "Overloaded",
    ));
    t.ingest(main_call(s2, "req_2", "msg_2", at + Duration::from_millis(20)));
    let ev = events_of(&t, s2);
    assert!(!ev.is_empty());
    assert!(!ev.iter().any(|(n, _, _)| n == "tengu_startup_telemetry"), "不出启动串");
    for (n, _, d) in ev.iter().filter(|(_, _, d)| d.get("event_name").is_some()) {
        assert_eq!(d["parent_session_id"], SESSION, "{n}");
    }
}

/// 合并重发之后，请求前缀（提交、`api_query`）挂在头一次尝试上，早于重发事件；首字与成功收尾按最后那次算
/// （`cap/auto-2.1.306-20261021-retry` R1：`api_query` .534、换 dispatch .545、成功 36.482）。
#[test]
fn a_retried_request_keeps_its_prefix_at_the_first_attempt() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.306 (external, cli)";
    let base = frozen_now() - Duration::from_secs(60);
    let mut f = failed(
        call(cc_body(true), "req_f", "end_turn"),
        Some(529),
        "overloaded_error",
        "Overloaded",
    );
    f.ua_out = ua.into();
    f.started_at = base;
    f.total_ms = 8;
    f.dispatch = Some("v2d".into());
    t.ingest(f);
    let mut ok = call(cc_body(true), "req_ok", "end_turn");
    ok.ua_out = ua.into();
    ok.started_at = base + Duration::from_millis(10);
    ok.total_ms = 1_936;
    ok.dispatch = Some("v2p".into());
    t.ingest(ok);
    let st = t.0.state.lock();
    let at = |n: &str| {
        st.pending[&key()].events.iter().find(|(_, e)| ev_name(e) == n).map(|(dt, _)| *dt).unwrap()
    };
    let start: DateTime<Utc> = base.into();
    assert!(at("tengu_api_query") <= at("tengu_dispatch_header_fallback"));
    assert!(at("tengu_input_prompt") <= at("tengu_api_query"));
    assert!(
        at("tengu_api_query") < start + chrono::Duration::milliseconds(8),
        "前缀在头一次尝试上"
    );
    assert_eq!(at("tengu_api_success"), start + chrono::Duration::milliseconds(10 + 1_936));
}

/// `/clear` 后新会话的头一条扣着等重发时，同一台设备上的旧会话就算到了闲置期限也留着：重发成功后照样认出
/// `/clear` 来历。
#[test]
fn a_held_retry_keeps_the_cleared_predecessor() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(300);
    let s2 = "22222222-2222-4222-8222-222222222222";
    t.ingest(quota_probe_call("11111111-1111-4111-8111-111111111111", base));
    t.ingest(main_call(SESSION, "req_1", "msg_1", base + Duration::from_secs(5)));
    let at = base + Duration::from_secs(60);
    t.ingest(failed(
        main_call(s2, "req_f", "msg_f", at),
        Some(529),
        "overloaded_error",
        "Overloaded",
    ));
    let idle = Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS);
    let now = Instant::now();
    t.0.state.lock().sessions.get_mut(&key()).unwrap().last_seen =
        now - idle - Duration::from_secs(1);
    t.gc(now);
    assert!(t.0.state.lock().sessions.contains_key(&key()), "旧会话留着");
    t.ingest(main_call(s2, "req_2", "msg_2", at + Duration::from_millis(20)));
    let ev = events_of(&t, s2);
    assert!(!ev.iter().any(|(n, _, _)| n == "tengu_startup_telemetry"), "不出启动串");
    for (n, _, d) in ev.iter().filter(|(_, _, d)| d.get("event_name").is_some()) {
        assert_eq!(d["parent_session_id"], SESSION, "{n}");
    }
}

/// 等重试的那段算在逻辑请求里：首条请求重发了 60 秒才成功，进程起点仍从头一次尝试往前推；第二次提交与上一次只隔
/// 1 秒、自己又等了 60 秒重试，用户敲字的时长窗口仍按两次提交的间隔算。
#[test]
fn retry_wait_does_not_shift_process_start_or_typing_time() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.306 (external, cli)";
    let base = frozen_now() - Duration::from_secs(600);
    let attempt = |body: Vec<u8>, rid: &str, at: SystemTime, fail: bool| {
        let mut c = call(body, rid, "end_turn");
        c.ua_out = ua.into();
        c.started_at = at;
        c.total_ms = 1_000;
        if fail {
            c = failed(c, Some(529), "overloaded_error", "Overloaded");
            c.total_ms = 8;
        }
        c
    };
    t.ingest(attempt(cc_body(true), "req_f1", base, true));
    t.ingest(attempt(cc_body(true), "req_ok1", base + Duration::from_secs(60), false));
    {
        let st = t.0.state.lock();
        let wall = st.sessions[&key()].started_wall;
        assert!(wall < base, "进程起点在头一次尝试之前");
        assert!(base.duration_since(wall).unwrap() < Duration::from_secs(10), "{wall:?}");
    }
    drop(t);
    // 第一次提交很快完成，第二次只隔 1 秒，自己等了 60 秒重试：敲字估算（0.8 + 0.1 × 11 字 = 1.9 秒）要截到
    // 两次提交的间隔 1 秒，不能拿等重试那 60 秒当窗口。
    let t = Telemetry::default();
    t.ingest(attempt(cc_body(true), "req_1", base, false));
    let p2 = base + Duration::from_secs(1);
    t.ingest(attempt(with_prompt(cc_body(true), PROMPT_2), "req_f2", p2, true));
    t.ingest(attempt(
        with_prompt(cc_body(true), PROMPT_2),
        "req_ok2",
        p2 + Duration::from_secs(60),
        false,
    ));
    let st = t.0.state.lock();
    let users: Vec<f64> = st.pending[&key()].metrics.iter().map(|m| m.user_secs).collect();
    let last = *users.last().unwrap();
    assert!((last - 1.0).abs() < 1e-6, "窗口是两次提交的间隔 1 秒：{users:?}");
}

/// 扣着等重发的会话不算闲置：快到闲置期限时遇到 529，清理时会话照样留着，不报退出。
#[test]
fn a_session_waiting_for_a_retry_is_not_idle() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.306 (external, cli)";
    let mut first = call(cc_body(true), "req_1", "end_turn");
    first.ua_out = ua.into();
    t.ingest(first);
    let idle = Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS);
    let now = Instant::now();
    t.0.state.lock().sessions.get_mut(&key()).unwrap().last_seen =
        now - idle + Duration::from_secs(1);
    let mut f = failed(
        call(with_prompt(cc_body(true), PROMPT_2), "req_f", "end_turn"),
        Some(529),
        "overloaded_error",
        "Overloaded",
    );
    f.ua_out = ua.into();
    t.ingest(f);
    t.gc(now + Duration::from_secs(2));
    let st = t.0.state.lock();
    assert!(st.sessions.contains_key(&key()), "会话还在");
    let main = (7, SESSION.to_string(), RetryLane::Main);
    assert!(st.held_retries.contains_key(&main), "还在等重发");
}

/// 扣着的重试链只留最后一次的整份请求，前面几次只留概况。
#[test]
fn held_retry_keeps_only_the_last_body() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.306 (external, cli)";
    let mut first = call(cc_body(true), "req_1", "end_turn");
    first.ua_out = ua.into();
    t.ingest(first);
    for i in 0..3 {
        let mut f = failed(
            call(with_prompt(cc_body(true), PROMPT_2), &format!("req_f{i}"), "end_turn"),
            Some(529),
            "overloaded_error",
            "Overloaded",
        );
        f.ua_out = ua.into();
        t.ingest(f);
    }
    let st = t.0.state.lock();
    let held = &st.held_retries[&(7, SESSION.to_string(), RetryLane::Main)];
    assert_eq!(held.prior.len(), 2);
    assert_eq!(held.last.request_id.as_deref(), Some("req_f2"));
}

/// 可重试的失败会先扣着等客户端重发（[`config::TELEMETRY_RETRY_HOLD_SECS`]）：让它超时，按最终失败补报。
fn give_up_retries(t: &Telemetry) {
    t.gc(Instant::now() + Duration::from_secs(config::TELEMETRY_RETRY_HOLD_SECS + 1));
}

/// `cc_body` 里 metadata 的 session_id；待发批次按 (凭证, 会话) 取。
const SESSION: &str = "4dc73702-d904-4887-809d-17b93cc5357c";
fn key() -> (i64, String) {
    (7, SESSION.to_string())
}

/// 事件名；GrowthBook 曝光事件没有 `event_name`，用 `experiment_id`。
fn ev_name(e: &Value) -> &str {
    e["event_data"]["event_name"]
        .as_str()
        .or_else(|| e["event_data"]["experiment_id"].as_str())
        .unwrap_or("")
}

/// 事件时间戳；GrowthBook 那类叫 `timestamp`。
fn ev_ts(e: &Value) -> &str {
    e["event_data"]["client_timestamp"]
        .as_str()
        .or_else(|| e["event_data"]["timestamp"].as_str())
        .unwrap_or("")
}

/// 新会话的首批带完整的启动那串 + 每轮输入那串 + 首轮版本检查，身份与占位符按会话替换；
/// 同一会话第二条请求不再有启动与首轮那两串；握手任务每个会话排一次。
#[test]
fn new_session_gets_the_startup_burst() {
    let t = Telemetry::default();
    t.ingest(call(cc_body(true), "req_1", "end_turn"));
    {
        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
        assert!(names.len() > 150, "启动串 121 + 输入串 + api 链 + 收尾串，实际 {}", names.len());
        for expected in [
            "tengu_cli_flags",
            "tengu_started",
            "tengu_init",
            "tengu_startup_telemetry",
            "tengu_policy_limits_fetch",
            "tengu_carved_slate",
            "tengu_input_prompt",
            "tengu_api_query",
            "tengu_policy_limits_cache_state_at_first_prompt",
            "tengu_api_success",
            "tengu_prompt_suggestion",
            "tengu_tip_shown",
            "tengu_native_auto_updater_start",
            "tengu_native_version_cleanup",
        ] {
            assert!(names.contains(&expected), "缺 {expected}");
        }
        assert_eq!(names.iter().filter(|n| **n == "tengu_skill_loaded").count(), 26);
        let init = p
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_init")
            .map(|(_, e)| e["event_data"].clone())
            .unwrap();
        let meta: Value = serde_json::from_slice(
            &STANDARD.decode(init["additional_metadata"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["permissionMode"], "auto", "占位符按请求体替换");
        // 启动早期的事件**只有** `subscription_type`：那会儿界面还没画、用户一个字都
        // 没输，`renderer_mode` 与 `cc_prompt_id` 都还不存在，见 [`super::MetaStage`]。
        assert_eq!(meta["cc_prompt_id"], Value::Null, "启动事件不写 cc_prompt_id");
        assert_eq!(meta["renderer_mode"], Value::Null, "也不写 renderer_mode");
        assert_eq!(meta["subscription_type"], "team", "只有这一项");
        // 界面起来之后、用户提交之前的那几条：有 renderer_mode，仍没有 cc_prompt_id。
        let probe = p
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_terminal_probe")
            .map(|(_, e)| e["event_data"].clone())
            .expect("startup 模板里有这一条");
        let probe_meta: Value = serde_json::from_slice(
            &STANDARD.decode(probe["additional_metadata"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(probe_meta["renderer_mode"], "default");
        assert_eq!(probe_meta["cc_prompt_id"], Value::Null, "还没提交就没有这一项");
        // 用户提交之后那一批三项齐全。
        let query = p
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_api_query")
            .map(|(_, e)| e["event_data"].clone())
            .unwrap();
        let query_meta: Value = serde_json::from_slice(
            &STANDARD.decode(query["additional_metadata"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(query_meta["renderer_mode"], "default");
        assert_eq!(query_meta["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622");
        assert_eq!(init["session_id"], SESSION);
        assert_eq!(init["model"], "claude-opus-5[1m]");
        let timer = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_timer")
            .map(|(_, e)| {
                serde_json::from_slice::<Value>(
                    &STANDARD
                        .decode(e["event_data"]["additional_metadata"].as_str().unwrap())
                        .unwrap(),
                )
                .unwrap()
            })
            .find(|m| m["event"] == "startup")
            .unwrap();
        assert_eq!(timer["resumed"], false);
        assert_eq!(timer["durationMs"], 373);
        let setting = p
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_startup_manual_model_config")
            .map(|(_, e)| {
                serde_json::from_slice::<Value>(
                    &STANDARD
                        .decode(e["event_data"]["additional_metadata"].as_str().unwrap())
                        .unwrap(),
                )
                .unwrap()
            })
            .unwrap();
        assert_eq!(setting["settings_file"], "opus[1m]");
        let growth = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_time_shell").unwrap();
        let g = &growth.1["event_data"];
        assert_eq!(growth.1["event_type"], "GrowthbookExperimentEvent");
        assert_eq!(g["environment"], "production");
        assert_eq!(g["user_attributes"], "{\"appVersion\":\"2.1.258\"}");
        assert_eq!(g["experiment_metadata"], "{\"feature_id\":\"tengu_stone_shell\"}");
        assert_eq!(g["auth"]["organization_uuid"], "09520b85-f6b6-432f-97e2-6ecb804a083f");
        assert_eq!(g["session_id"], SESSION);
        // Datadog 只收那几类。
        assert!(p.dd.iter().any(|d| d["message"] == "tengu_started"));
        assert!(
            p.dd.iter().any(|d| d["message"] == "tengu_init" && d["permission_mode"] == "auto")
        );
        assert!(
            p.dd.iter().any(|d| d["feature_name"] == "ca_certs_load" && d["cert_count"] == 144)
        );
        assert!(!p.dd.iter().any(|d| d["message"] == "tengu_skill_loaded"));
    }

    // 同一会话第二轮：有输入串、没有启动串与首轮那串。
    let mut second = call(cc_body(true), "req_2", "end_turn");
    second.started_at = frozen_now();
    t.ingest(second);
    let st = t.0.state.lock();
    let names: Vec<&str> = st.pending[&key()].events.iter().map(|(_, e)| ev_name(e)).collect();
    assert_eq!(names.iter().filter(|n| **n == "tengu_started").count(), 1);
    assert_eq!(names.iter().filter(|n| **n == "tengu_native_auto_updater_start").count(), 1);
    assert_eq!(names.iter().filter(|n| **n == "tengu_input_prompt").count(), 2);
    assert_eq!(names.iter().filter(|n| **n == "tengu_paste_text").count(), 2, "输入串每轮都有");
    assert_eq!(
        names.iter().filter(|n| **n == "tengu_policy_limits_cache_state_at_first_prompt").count(),
        1,
        "首次输入才有"
    );
}

#[test]
fn model_setting_follows_the_settings_alias() {
    assert_eq!(model_setting("claude-opus-5[1m]"), "opus[1m]");
    assert_eq!(model_setting("claude-fable-5-1"), "fable");
    assert_eq!(model_setting("claude-haiku-4-5-20251001"), "haiku");
    assert_eq!(model_setting("claude-sonnet-5"), "sonnet");
}

#[test]
fn sessions_on_one_credential_are_batched_separately() {
    let t = Telemetry::default();
    t.ingest(call(cc_body(true), "req_a", "end_turn"));
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["metadata"]["user_id"] = json!(
        "{\"device_id\":\"aa\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"other-session\"}"
    );
    t.ingest(call(body.to_string().into_bytes(), "req_b", "end_turn"));
    {
        let st = t.0.state.lock();
        assert_eq!(st.pending.len(), 2, "两个会话两个批次");
        assert!(st.pending.contains_key(&key()));
        assert!(st.pending.contains_key(&(7, "other-session".to_string())));
    }
    let due =
        t.take_due(Instant::now() + Duration::from_secs(config::TELEMETRY_EVENT_FLUSH_SECS + 1));
    assert_eq!(due.len(), 2, "同一张凭证两个会话各发各的");
    for f in &due {
        assert_eq!(f.cred_id, 7);
        let sids: std::collections::HashSet<&str> =
            f.events.iter().map(|e| e["event_data"]["session_id"].as_str().unwrap()).collect();
        assert_eq!(sids.len(), 1, "一个批次里只有一个 session_id");
        let dids: std::collections::HashSet<&str> =
            f.dd.iter().map(|e| e["session_id"].as_str().unwrap()).collect();
        assert_eq!(dids.len(), 1);
    }
}

#[test]
fn latest_session_hands_the_real_identity_to_the_keepalive() {
    let t = Telemetry::default();
    assert!(t.latest_session(7, Duration::from_secs(3600)).is_none(), "还没有会话");
    t.ingest(call(cc_body(true), "req_1", "end_turn"));
    let s = t.latest_session(7, Duration::from_secs(3600)).expect("刚有过请求");
    assert_eq!(s.session_id, SESSION);
    assert_eq!(s.device_id.len(), 64);
    assert_eq!(s.account_uuid, "9922ef8e-7945-4f5a-ab4f-cf5f521531df");
    assert_eq!(s.version, "2.1.258");
    assert_eq!(s.model, "claude-opus-5[1m]");
    assert_eq!(
        s.betas,
        "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
             interleaved-thinking-2025-05-14,redact-thinking-2026-02-12",
        "会话级那份始终带 redact-thinking，哪怕请求头里没有"
    );
    assert_eq!(s.prompt_id, "6c079143-0c53-4c48-817d-105460b3f622");
    assert!(t.latest_session(8, Duration::from_secs(3600)).is_none(), "别的凭证没有");
    assert!(t.latest_session(7, Duration::ZERO).is_none(), "超过闲置上限就不算近期");

    // **侧查询不许把会话级上下文改掉**：标题生成用的是 haiku + 一套完全不同的 beta，
    // 覆盖之后，从它结束到下一条主请求之间，保活挂的身份与指标导出报的就都是标题生成
    // 那套——一个「会话主模型 opus、会话 beta 却是标题那套」的组合，官方不产生。
    let mut title = call(
            json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 32000,
                "stream": true,
                "thinking": {"type": "disabled"},
                "system": [
                    {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.ced; cc_entrypoint=cli; cch=b1b2c;"},
                    {"type":"text","text":"You are naming a coding session so the user can pick it out of a long list of sessions."}
                ],
                "messages": [{"role":"user","content":"<session>\nhi\n</session>\n\nWrite the title"}],
                "metadata": {"user_id": "{\"device_id\":\"b9\",\"account_uuid\":\"a\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"}
            })
            .to_string()
            .into_bytes(),
            "req_title",
            "end_turn",
        );
    title.betas = Some(
        "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             structured-outputs-2025-12-15"
            .into(),
    );
    t.ingest(title);
    // 侧查询先被扣住等新一轮 prompt id；覆盖发生在**补发**那一刻，所以要让它到期。
    assert_eq!(t.0.state.lock().sessions[&key()].deferred.len(), 1, "先扣住");
    t.gc(Instant::now() + Duration::from_secs(config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1));
    assert!(t.0.state.lock().sessions[&key()].deferred.is_empty(), "补发了");

    let after = t.latest_session(7, Duration::from_secs(3600)).expect("会话还在");
    assert_eq!(after.model, s.model, "会话主模型不该被标题生成改成 haiku");
    assert_eq!(after.betas, s.betas, "会话级 beta 不该被标题生成那套覆盖");
    assert_eq!(after.prompt_id, s.prompt_id, "prompt_id 同样不该被侧查询改掉");
}

/// PDF 按 `pages` 分页读：`file_operation` 的内容哈希是固定串 `PDF pages <pages>`（官方 305 `OE` 那处），
/// 不是渲染出来那张图的 base64；不带 `pages` 的图片读照旧哈希 base64。
#[test]
fn a_paged_pdf_read_hashes_the_pages_string() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.305 (external, cli)";
    let step = |tool: Value, result: Value, rid: &str, stop: &str| {
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[tool]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":result}]});
        let mut c = call(body.to_string().into_bytes(), rid, stop);
        c.ua_out = ua.into();
        c
    };
    let image = |data: &str| json!([{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":data}}]);
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = ua.into();
    t.ingest(first);
    t.ingest(step(
        json!({"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/p/doc.PDF","pages":"1-5"}}),
        image("QUJD"),
        "req_2",
        "tool_use",
    ));
    t.ingest(step(
        json!({"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/p/pic.png"}}),
        image("REVG"),
        "req_3",
        "end_turn",
    ));
    let st = t.0.state.lock();
    let ops: Vec<Value> = st.pending[&key()]
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_file_operation")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(ops[0]["contentHash"], sha256_hex(b"PDF pages 1-5"));
    assert_eq!(ops[1]["contentHash"], sha256_hex(b"REVG"));
}

/// 读写文件那一组：Read 带原文哈希的 `file_operation`；头一次 Edit 先备份原文件、记一笔，再报改动行数、
/// `file_operation{edit}`、字符串长度与 `file_activity`；auto 模式自动放行，编辑决定的来源是 `config`。改动行数与
/// 编辑决定进 OTel 指标。
#[test]
fn file_tools_report_the_edit_group() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.305 (external, cli)";
    let step = |tool: Value, result: &str, rid: &str, stop: &str| {
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[tool]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":result}]});
        let mut c = call(body.to_string().into_bytes(), rid, stop);
        c.ua_out = ua.into();
        c
    };
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = ua.into();
    t.ingest(first);
    t.ingest(step(
        json!({"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/p/a.md"}}),
        "1\t# a\n2\t",
        "req_2",
        "tool_use",
    ));
    t.ingest(step(
        json!({"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/p/a.md","old_string":"# a","new_string":"# bb"}}),
        "The file /p/a.md has been updated successfully.",
        "req_3",
        "end_turn",
    ));
    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let got = |n: &str| -> Vec<Value> {
        p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
    };
    let read = &got("tengu_file_operation")[0];
    assert_eq!(read["operation"], "read");
    assert_eq!(read["filePathHash"], crate::telemetry::files::path_hash("/p/a.md"));
    assert_eq!(read["contentHash"], sha256_hex(b"# a\n"));
    let edit = &got("tengu_file_operation")[1];
    assert_eq!((&edit["operation"], &edit["tool"]), (&json!("edit"), &json!("FileEditTool")));
    assert!(edit.get("contentHash").is_none(), "改动不带内容哈希");
    assert_eq!(got("tengu_file_history_backup_file_created")[0]["fileSize"], 4);
    assert_eq!(got("tengu_file_history_track_edit_success")[0]["isNewFile"], false);
    let changed = &got("tengu_file_changed")[0];
    assert_eq!((&changed["lines_added"], &changed["lines_removed"]), (&json!(1), &json!(1)));
    let len = &got("tengu_edit_string_lengths")[0];
    assert_eq!((&len["oldStringBytes"], &len["newStringBytes"]), (&json!(3), &json!(4)));
    let activity = &got("tengu_file_activity")[0];
    assert_eq!((&activity["activity"], &activity["fileExtension"]), (&json!("edit"), &json!("md")));
    let m = metrics_body(&p.metrics, "2.1.305", "team", None);
    let names: Vec<&str> =
        m["metrics"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
    let at = |n: &str| names.iter().position(|x| *x == n).unwrap();
    assert!(at("claude_code.session.count") < at("claude_code.lines_of_code.count"));
    assert!(at("claude_code.lines_of_code.count") < at("claude_code.cost.usage"));
    assert!(at("claude_code.token.usage") < at("claude_code.code_edit_tool.decision"));
    let loc = &m["metrics"][at("claude_code.lines_of_code.count")]["data_points"];
    assert_eq!((&loc[0]["attributes"]["type"], &loc[0]["value"]), (&json!("added"), &json!(1)));
    assert_eq!((&loc[1]["attributes"]["type"], &loc[1]["value"]), (&json!("removed"), &json!(1)));
    let d =
        &m["metrics"][at("claude_code.code_edit_tool.decision")]["data_points"][0]["attributes"];
    assert_eq!(
        (&d["decision"], &d["source"], &d["tool_name"], &d["language"]),
        (&json!("accept"), &json!("config"), &json!("Edit"), &json!("Markdown"))
    );
}

/// 改动行数记在发起那次工具调用的回复模型名下：请求 opus、回落到 sonnet 写的文件，下一条（仍请求 opus）带回
/// 写入结果时，`lines_of_code` 的 `model` 是 sonnet。NotebookEdit 的编辑决定按 `notebook_path` 的扩展名报语言
/// （`.ipynb` 查不到是 `unknown`），不是弹框那条按单元格类型的 python。
#[test]
fn edit_metrics_follow_the_reply_model_and_the_path_language() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.305 (external, cli)";
    let step = |tool: Value, result: &str, rid: &str, stop: &str| {
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[tool]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":result}]});
        let mut c = call(body.to_string().into_bytes(), rid, stop);
        c.ua_out = ua.into();
        c
    };
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = ua.into();
    first.resp_model = Some("claude-sonnet-5-5".into());
    t.ingest(first);
    let mut second = step(
        json!({"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/p/new.py","content":"a\nb\n"}}),
        "File created successfully at: /p/new.py",
        "req_2",
        "tool_use",
    );
    second.resp_model = Some("claude-opus-5".into());
    t.ingest(second);
    t.ingest(step(
        json!({"type":"tool_use","id":"t1","name":"NotebookEdit","input":{"notebook_path":"/p/a.ipynb","new_source":"x=1","cell_type":"code"}}),
        "Updated cell",
        "req_3",
        "end_turn",
    ));
    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let with_loc: Vec<&str> =
        p.metrics.iter().filter(|c| !c.loc.is_empty()).map(|c| c.loc_model.as_str()).collect();
    assert_eq!(with_loc, ["claude-sonnet-5-5"], "Write 是 sonnet 那条回复调的");
    let langs: Vec<(String, Option<String>)> = p
        .metrics
        .iter()
        .flat_map(|c| c.decisions.iter())
        .map(|d| (d.tool.clone(), d.language.clone()))
        .collect();
    assert!(
        langs.contains(&("NotebookEdit".to_string(), Some("unknown".to_string()))),
        "{langs:?}"
    );
    // NotebookEdit 成功后一条 `file_activity{notebook_edit}`，扩展名取 `notebook_path`。
    let activity: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_file_activity")
        .map(|(_, e)| meta_of(e))
        .filter(|m| m["toolName"] == "NotebookEdit")
        .collect();
    assert_eq!(activity.len(), 1, "{activity:?}");
    assert_eq!(
        (&activity[0]["activity"], &activity[0]["fileExtension"], &activity[0]["isNewFile"]),
        (&json!("notebook_edit"), &json!("ipynb"), &json!(false))
    );
}

/// 压缩后重建的附件账本按版本：技能清单 2.1.305 起才在压缩时放回（`cap/auto-2.1.303-20261019-full/00175` 没有
/// 这一段，305 的 `00173` 有），303 的账本里不该有它，其余放回的几类两个版本都有。
#[test]
fn post_compact_ledger_restores_the_skill_listing_from_2_1_305() {
    let mut posts = Vec::new();
    for (ua, skills) in [
        ("claude-cli/2.1.303 (external, cli)", None),
        ("claude-cli/2.1.305 (external, cli)", Some(1)),
    ] {
        let t = Telemetry::default();
        let base = frozen_now() - Duration::from_secs(120);
        let mut first = call(cc_body(true), "req_1", "end_turn");
        first.ua_out = ua.into();
        first.started_at = base;
        t.ingest(first);
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"] = json!([
            {"role":"user","content":[{"type":"text","text":"hello there"}]},
            {"role":"assistant","content":[{"type":"text","text":"ok"}]},
            {"role":"user","content":[{"type":"text","text":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools."}]},
            {"role":"system","content":"<total_tokens>0 tokens left</total_tokens>"}
        ]);
        let mut c = call(body.to_string().into_bytes(), "req_c", "end_turn");
        c.ua_out = ua.into();
        c.started_at = base + Duration::from_secs(30);
        c.agent = AgentHeaders {
            agent_id: None,
            agent_type: None,
            request_class: Some("compaction".into()),
        };
        t.ingest(c);
        let after = t.0.state.lock().sessions[&key()].attachment_counts.clone();
        let has = |k: &str| after.iter().find(|(x, _)| x == k).map(|(_, n)| *n);
        assert_eq!(has("skill_listing"), skills, "{ua}: {after:?}");
        for k in ["agent_listing_delta", "mcp_instructions_delta", "environment", "model"] {
            assert_eq!(has(k), Some(1), "{ua} {k}: {after:?}");
        }
        let (post, model) = t.0.state.lock().pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_reactive_compact_succeeded")
            .map(|(_, e)| {
                let post = meta_of(e)["postCompactTokens"].as_u64().unwrap();
                (post, e["event_data"]["model"].as_str().unwrap().to_string())
            })
            .unwrap();
        posts.push((post, model));
    }
    // `postCompactTokens` 里估的放回附件与账本同一口径：两个版本只差技能清单那一段。
    let mut skill = json!({ "attachment_types": ["skill_listing"] });
    super::kind::fill_attachment_estimates(&mut skill, "unknown", false, &posts[1].1);
    let skill: u64 = skill["attachment_token_estimates"][0].as_str().unwrap().parse().unwrap();
    assert!(skill > 0);
    assert_eq!(posts[1].0 - posts[0].0, skill, "{posts:?}");
}

/// 关了工具搜索（请求里没有 `defer_loading` 的工具）：压缩时不放回延迟工具清单，恢复条数、账本一起少这一条
/// （305 没有恢复文件、计划文件时是 5 条，开着是 6 条）。
#[test]
fn restored_attachment_count_drops_the_deferred_tools_when_tool_search_is_off() {
    for (deferred, count) in [(true, 6), (false, 5)] {
        let t = Telemetry::default();
        let ua = "claude-cli/2.1.305 (external, cli)";
        let base = frozen_now() - Duration::from_secs(120);
        let strip = |mut v: Value| {
            if !deferred {
                v["tools"].as_array_mut().unwrap().retain(|x| x.get("defer_loading").is_none());
            }
            v
        };
        let mut first = call(
            strip(serde_json::from_slice(&cc_body(true)).unwrap()).to_string().into_bytes(),
            "req_1",
            "end_turn",
        );
        first.ua_out = ua.into();
        first.started_at = base;
        t.ingest(first);
        let mut body = strip(serde_json::from_slice(&cc_body(false)).unwrap());
        body["messages"] = json!([
            {"role":"user","content":[{"type":"text","text":"hello there"}]},
            {"role":"assistant","content":[{"type":"text","text":"ok"}]},
            {"role":"user","content":[{"type":"text","text":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools."}]}
        ]);
        let mut c = call(body.to_string().into_bytes(), "req_c", "end_turn");
        c.ua_out = ua.into();
        c.started_at = base + Duration::from_secs(30);
        c.agent = AgentHeaders {
            agent_id: None,
            agent_type: None,
            request_class: Some("compaction".into()),
        };
        t.ingest(c);
        let st = t.0.state.lock();
        let succeeded = st.pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_reactive_compact_succeeded")
            .map(|(_, e)| meta_of(e))
            .unwrap();
        assert_eq!(succeeded["restoredAttachmentCount"], count, "deferred={deferred}");
        let after = &st.sessions[&key()].attachment_counts;
        assert_eq!(after.iter().any(|(k, _)| k == "deferred_tools_delta"), deferred, "{after:?}");
        assert_eq!(after.iter().map(|(_, n)| *n as usize).sum::<usize>(), count, "{after:?}");
    }
}

/// 重读账本里已有的文件：先报 `file_read_reread{priorOp}`；文件没变、又是同一段时客户端直接回去重那句话，只再报
/// `file_read_dedup{ext}`（种进来的多一项 `source: seeded`），不报 `file_operation`，账本里的原文也不动——之后真的
/// 重读仍按上一次改动报 `priorOp: edit_write`、带新内容的哈希。
#[test]
fn rereading_an_unchanged_file_is_deduplicated() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.305 (external, cli)";
    let step = |tool: Value, result: &str, rid: &str| {
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[tool]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":result}]});
        let mut c = call(body.to_string().into_bytes(), rid, "tool_use");
        c.ua_out = ua.into();
        c
    };
    let read = |p: &str| json!({"type":"tool_use","id":"t1","name":"Read","input":{"file_path":p}});
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = ua.into();
    t.ingest(first);
    t.ingest(step(read("/p/a.md"), "1\t# a\n2\t", "req_2"));
    t.ingest(step(
        read("/p/a.md"),
        "Wasted call \u{2014} file unchanged since your last Read. Refer to that earlier tool_result instead.",
        "req_3",
    ));
    t.ingest(step(
        json!({"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/p/a.md","old_string":"# a","new_string":"# bb"}}),
        "The file /p/a.md has been updated successfully.",
        "req_4",
    ));
    t.ingest(step(
        read("/p/a.md"),
        "<system-reminder>This file is already in your context (see \"Contents of /p/a.md\" above) and has not changed on disk. Use that content instead of re-reading.</system-reminder>",
        "req_5",
    ));
    t.ingest(step(read("/p/a.md"), "1\t# bb\n2\t", "req_6"));
    // 没有扩展名的文件 `ext` 不带；超过 10 个字符的是 `other`。
    t.ingest(step(read("/p/Makefile"), "1\tall:\n2\t", "req_7"));
    t.ingest(step(read("/p/Makefile"), "File unchanged since last read. The content from the earlier Read tool_result in this conversation is still current.", "req_8"));
    t.ingest(step(read("/p/x.verylongextension"), "1\tz\n2\t", "req_9"));
    t.ingest(step(read("/p/x.verylongextension"), "File unchanged since last read.", "req_10"));
    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let got = |n: &str| -> Vec<Value> {
        p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
    };
    let ops: Vec<String> = got("tengu_file_operation")
        .iter()
        .map(|m| m["operation"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ops, ["read", "edit", "read", "read", "read"], "去重命中的两次不报 file_operation");
    assert_eq!(
        got("tengu_file_operation")[2]["contentHash"],
        sha256_hex(b"# bb\n"),
        "去重没有清掉账本，Edit 之后的原文照旧"
    );
    let reread: Vec<String> = got("tengu_file_read_reread")
        .iter()
        .map(|m| m["priorOp"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(reread, ["read", "seeded", "edit_write", "read", "read"]);
    let dedup = got("tengu_file_read_dedup");
    assert_eq!(dedup.len(), 4);
    // meta 里前三项是阶段上下文，后面才是事件自己的。
    let own = |m: &Value| -> Vec<(String, Value)> {
        m.as_object().unwrap().iter().skip(3).map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    assert_eq!(own(&dedup[0]), [("ext".to_string(), json!("md"))]);
    assert_eq!(
        own(&dedup[1]),
        [("source".to_string(), json!("seeded")), ("ext".to_string(), json!("md"))]
    );
    assert_eq!(own(&dedup[2]), [], "没有扩展名不带 ext");
    assert_eq!(own(&dedup[3]), [("ext".to_string(), json!("other"))]);
    // 同一次 Read 里 reread 在 dedup 前、都在工具收尾前。
    let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
    let i = names.iter().position(|n| *n == "tengu_file_read_dedup").unwrap();
    assert_eq!(names[i - 1], "tengu_file_read_reread");
}

/// 终端里跑的会话：进程启动 30 秒多那条 `ide_detect{found:false}`，一个进程一条，不到 30 秒就退出的没有；
/// 启动段模板里 IDE 底子带来的两条 `found:true` 不再发（`cap/auto-2.1.305-20261021-full` 16 个会话里活过 30 秒的
/// 9 个都是启动后 30.4 ~ 30.8 秒一条 `found:false`，2.1.291 起每份抓包同此）。
#[test]
fn ide_detect_fires_once_thirty_seconds_after_startup() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(300);
    let mut c1 = call(cc_body(true), "req_1", "end_turn");
    c1.ua_out = "claude-cli/2.1.305 (external, cli)".into();
    c1.started_at = base;
    t.ingest(c1);
    let ide = |st: &State| -> Vec<(String, Value)> {
        st.pending[&key()]
            .events
            .iter()
            .filter(|(_, e)| {
                ev_name(e) == "tengu_feature_ok" && meta_of(e)["feature_name"] == "ide_detect"
            })
            .map(|(_, e)| (ev_ts(e).to_string(), meta_of(e)))
            .collect()
    };
    assert!(ide(&t.0.state.lock()).is_empty(), "首条请求时进程还没起满 30 秒");
    for (i, secs) in [40u64, 80].iter().enumerate() {
        let mut c = call(cc_body(true), &format!("req_{}", i + 2), "end_turn");
        c.ua_out = "claude-cli/2.1.305 (external, cli)".into();
        c.started_at = base + Duration::from_secs(*secs);
        t.ingest(c);
    }
    let st = t.0.state.lock();
    let got = ide(&st);
    assert_eq!(got.len(), 1, "一个进程一条：{got:?}");
    assert_eq!(
        (&got[0].1["found"], &got[0].1["feature_name"]),
        (&json!(false), &json!("ide_detect"))
    );
    // Datadog 那份同发；2.1.293 起 `feature_name` 排在 `found` 后面。
    let dd: Vec<&Value> =
        st.pending[&key()].dd.iter().filter(|d| d["feature_name"] == "ide_detect").collect();
    assert_eq!(dd.len(), 1, "{dd:?}");
    let order = |m: &Value| -> Vec<String> {
        let keys = m.as_object().unwrap().keys();
        keys.filter(|k| matches!(k.as_str(), "found" | "feature_name")).cloned().collect()
    };
    assert_eq!(order(&got[0].1), ["found", "feature_name"]);
    assert_eq!(order(dd[0]), ["found", "feature_name"]);
    let at: DateTime<Utc> = got[0].0.parse().unwrap();
    let base_dt: DateTime<Utc> = base.into();
    let off = (at - base_dt).num_milliseconds();
    // 进程起点在首条请求前 6.3s，探测在起点后 30.45 ~ 30.85s。
    assert!((24_100..=24_600).contains(&off), "offset {off}ms");
    drop(st);

    // 起满 30 秒之后没再发请求就退出的：退出那串前面补一条。
    let t2 = Telemetry::default();
    let mut only = call(cc_body(true), "req_only", "end_turn");
    only.ua_out = "claude-cli/2.1.305 (external, cli)".into();
    only.started_at = frozen_now() - Duration::from_secs(60);
    t2.ingest(only);
    let later = Instant::now() + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1);
    t2.gc(later);
    let due = t2.take_due(later);
    let names: Vec<&str> = due
        .iter()
        .flat_map(|f| f.events.iter())
        .map(|e| {
            if ev_name(e) == "tengu_feature_ok" && meta_of(e)["feature_name"] == "ide_detect" {
                "ide"
            } else {
                ev_name(e)
            }
        })
        .collect();
    let i = names.iter().position(|n| *n == "ide").expect("退出时补上");
    assert_eq!(names.iter().filter(|n| **n == "ide").count(), 1);
    assert!(i < names.iter().rposition(|n| *n == "tengu_paste_text").unwrap(), "在退出那串前面");
    let dd = due.iter().flat_map(|f| f.dd.iter()).filter(|d| d["feature_name"] == "ide_detect");
    assert_eq!(dd.count(), 1, "Datadog 那份也补上");
}

/// 带字段的 `tengu_feature_ok` 在 2.1.293 之前 `feature_name` 排最前，之后排最后：IDE 探测
/// （`cap/auto-2.1.291-20261006-full` 7 条 `feature_name, found`）与 `/compact` 提交后那条更新检查都跟着版本走。
#[test]
fn feature_name_position_follows_the_version() {
    for (v, leads) in [("2.1.291", true), ("2.1.305", false)] {
        let ua = format!("claude-cli/{v} (external, cli)");
        let t = Telemetry::default();
        let base = frozen_now() - Duration::from_secs(300);
        let at = |secs: u64, id: &str, class: Option<&str>| {
            let mut c = call(cc_body(true), id, "end_turn");
            c.ua_out = ua.clone();
            c.started_at = base + Duration::from_secs(secs);
            c.agent = AgentHeaders {
                agent_id: None,
                agent_type: None,
                request_class: class.map(str::to_string),
            };
            c
        };
        t.ingest(at(0, "req_1", None));
        // 起满 30 秒后的下一条补 IDE 探测，再来一次 `/compact`。
        t.ingest(at(40, "req_2", None));
        t.ingest(at(50, "req_c", Some("compaction")));
        let st = t.0.state.lock();
        let compact_at: DateTime<Utc> = (base + Duration::from_secs(50)).into();
        let (mut ide, mut compact_update) = (false, false);
        for (_, e) in &st.pending[&key()].events {
            if ev_name(e) != "tengu_feature_ok" {
                continue;
            }
            let m = meta_of(e);
            let name = m["feature_name"].as_str().unwrap_or("");
            if !matches!(name, "ide_detect" | "update_detection") {
                continue;
            }
            let keys: Vec<&str> = m
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .filter(|k| !matches!(*k, "renderer_mode" | "subscription_type" | "cc_prompt_id"))
                .collect();
            let expect = if leads { keys.first() } else { keys.last() };
            assert_eq!(expect, Some(&"feature_name"), "{v} {name}: {keys:?}");
            let ts: DateTime<Utc> = ev_ts(e).parse().unwrap();
            ide |= name == "ide_detect";
            compact_update |= name == "update_detection" && ts > compact_at;
        }
        assert!(
            ide && compact_update,
            "{v}: IDE 探测 {ide}，压缩提交后的更新检查 {compact_update}"
        );
    }
}

/// 服务端下发了 heron 文本的会话：首次输入报一条；压缩成功后官方清掉系统段缓存（`XEn("compaction")`），下一条主线程
/// 请求重新组装再报一条——哪怕它是工具结果续轮；之后不再报。
#[test]
fn heron_is_reported_again_after_a_successful_compaction() {
    const CRED: i64 = -3_081;
    let boot = json!({ "client_data": { "tengu_heron_brook": "x".repeat(2790) } }).to_string();
    crate::proxy::client_data::note_bootstrap(CRED, "claude-opus-5", boot.as_bytes());
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.308 (external, cli)";
    let base = frozen_now() - Duration::from_secs(120);
    let send = |body: Vec<u8>, id: &str, stop: &str, at: u64, compact: bool| {
        let mut c = call(body, id, stop);
        c.cred_id = CRED;
        c.ua_out = ua.into();
        c.started_at = base + Duration::from_secs(at);
        if compact {
            c.agent = AgentHeaders {
                agent_id: None,
                agent_type: None,
                request_class: Some("compaction".into()),
            };
        }
        t.ingest(c);
    };
    let herons = || {
        let st = t.0.state.lock();
        st.pending[&(CRED, SESSION.to_string())]
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_heron_brook_applied")
            .count()
    };
    send(cc_body(true), "req_1", "end_turn", 0, false);
    assert_eq!(herons(), 1);
    send(cc_body(false), "req_2", "end_turn", 10, false);
    assert_eq!(herons(), 1, "段缓存还在，续轮不报");
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"] = json!([
        {"role":"user","content":[{"type":"text","text":"hello there"}]},
        {"role":"user","content":[{"type":"text","text":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools."}]}
    ]);
    send(body.to_string().into_bytes(), "req_c", "end_turn", 20, true);
    send(cc_body(false), "req_3", "tool_use", 30, false);
    assert_eq!(herons(), 2, "压缩后的下一条主线程请求重新组装");
    send(cc_body(false), "req_4", "end_turn", 40, false);
    assert_eq!(herons(), 2);
}

/// 用户取消的 `/compact`（没有失败、只是取消）不清系统段缓存：之后的主线程请求不再报 heron。
#[test]
fn a_cancelled_compaction_does_not_report_heron_again() {
    const CRED: i64 = -3_082;
    let boot = json!({ "client_data": { "tengu_heron_brook": "x".repeat(2790) } }).to_string();
    crate::proxy::client_data::note_bootstrap(CRED, "claude-opus-5", boot.as_bytes());
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(120);
    let send = |body: Vec<u8>, id: &str, stop: &str, at: u64, compact: bool| {
        let mut c = call(body, id, stop);
        c.cred_id = CRED;
        c.ua_out = "claude-cli/2.1.308 (external, cli)".into();
        c.started_at = base + Duration::from_secs(at);
        if compact {
            c.agent = AgentHeaders {
                agent_id: None,
                agent_type: None,
                request_class: Some("compaction".into()),
            };
            c.aborted = true;
        }
        t.ingest(c);
    };
    send(cc_body(true), "req_1", "end_turn", 0, false);
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"] = json!([
        {"role":"user","content":[{"type":"text","text":"hello there"}]},
        {"role":"user","content":[{"type":"text","text":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools."}]}
    ]);
    send(body.to_string().into_bytes(), "req_c", "end_turn", 20, true);
    send(cc_body(false), "req_3", "tool_use", 30, false);
    send(cc_body(true), "req_4", "end_turn", 40, false);
    let st = t.0.state.lock();
    let herons = st.pending[&(CRED, SESSION.to_string())]
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_heron_brook_applied")
        .count();
    assert_eq!(herons, 1);
}

/// `/compact` 收尾：恢复文件之后重宣告两个池子（`messagesLength` 是保留的消息数），`compact_reactive` 之后、逐出提示
/// 之前一条 `reactive_compact_succeeded`（键序照官方：分组与保留、token、用量、上下文统计、百分比）；压缩完主线程的
/// 读状态清空，再读不算重读。
#[test]
fn compact_reports_success_with_the_context_breakdown() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.305 (external, cli)";
    let base = frozen_now() - Duration::from_secs(120);
    let mut first = call(cc_body(true), "req_1", "end_turn");
    first.ua_out = ua.into();
    first.started_at = base;
    first.input_tokens = 100;
    first.cache_read_tokens = 5_000;
    first.cache_creation_tokens = 0;
    first.output_tokens = 50;
    t.ingest(first);
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"] = json!([
        {"role":"user","content":[{"type":"text","text":"hello there"}]},
        {"role":"system","content":"<total_tokens>1 tokens left</total_tokens>"},
        {"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"s"},{"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"/p/a.md"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"r1","content":"1\t# a\n2\t"},{"type":"text","text":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools."}]},
        {"role":"system","content":"<total_tokens>0 tokens left</total_tokens>"}
    ]);
    let mut c = call(body.to_string().into_bytes(), "req_c", "end_turn");
    c.ua_out = ua.into();
    c.started_at = base + Duration::from_secs(30);
    c.input_tokens = 20;
    c.cache_read_tokens = 5_100;
    c.cache_creation_tokens = 30;
    c.output_tokens = 400;
    c.text_chars = 1_600;
    c.agent =
        AgentHeaders { agent_id: None, agent_type: None, request_class: Some("compaction".into()) };
    // 压缩前账本里已有各轮宣告过的附件：这一条的统计照用，压完之后旧附件都不在上下文里了，账本清空。
    t.0.state
        .lock()
        .sessions
        .get_mut(&key())
        .unwrap()
        .attachment_counts
        .push(("plan_mode".into(), 2));
    t.ingest(c);
    // 压完之后账本换成上下文里还在的：保留那组的 `<total_tokens>`，加上放回去的清单、MCP 说明、环境与模型；
    // 压缩前各轮宣告过、已经被摘要掉的（种进去的 plan_mode）不在了。
    let after = t.0.state.lock().sessions[&key()].attachment_counts.clone();
    let has = |k: &str| after.iter().find(|(x, _)| x == k).map(|(_, n)| *n);
    assert_eq!(has("total_tokens_reminder"), Some(1), "{after:?}");
    for k in
        ["agent_listing_delta", "skill_listing", "mcp_instructions_delta", "environment", "model"]
    {
        assert_eq!(has(k), Some(1), "{k}: {after:?}");
    }
    assert_eq!(has("plan_mode"), None, "{after:?}");
    let succeeded = t.0.state.lock().pending[&key()]
        .events
        .iter()
        .find(|(_, e)| ev_name(e) == "tengu_reactive_compact_succeeded")
        .map(|(_, e)| meta_of(e))
        .unwrap();
    assert_eq!(succeeded["attachment_plan_mode_count"], 2);
    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let names: Vec<String> = p
        .events
        .iter()
        .map(|(_, e)| match ev_name(e) {
            "tengu_feature_ok" => format!("ok:{}", meta_of(e)["feature_name"].as_str().unwrap()),
            n => n.to_string(),
        })
        .collect();
    let i = names.iter().position(|n| n == "ok:compact_reactive").expect("压缩收尾");
    assert_eq!(names[i + 1], "tengu_reactive_compact_succeeded");
    assert_eq!(names[i + 2], "tengu_cache_eviction_hint");
    let j = names.iter().position(|n| n == "tengu_mcp_instructions_pool_change").unwrap();
    assert!(j < i, "池子重宣告在压缩收尾之前");
    // 首轮模板里也有一条（messagesLength 0），压缩那条在后面。
    let pool = p
        .events
        .iter()
        .rev()
        .find(|(_, e)| ev_name(e) == "tengu_mcp_instructions_pool_change")
        .map(|(_, e)| meta_of(e))
        .unwrap();
    assert_eq!(pool["messagesLength"], 3);
    assert_eq!(pool["attachmentCount"], 0);
    let deferred = p
        .events
        .iter()
        .rev()
        .find(|(_, e)| ev_name(e) == "tengu_deferred_tools_pool_change")
        .map(|(_, e)| meta_of(e))
        .unwrap();
    assert_eq!(
        (&deferred["callSite"], &deferred["querySource"]),
        (&json!("reactive_compact"), &json!("unknown"))
    );
    let m = p
        .events
        .iter()
        .find(|(_, e)| ev_name(e) == "tengu_reactive_compact_succeeded")
        .map(|(_, e)| meta_of(e))
        .unwrap();
    let keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
    let head: Vec<&str> = keys.iter().copied().take_while(|k| *k != "total_tokens").collect();
    assert_eq!(
        head,
        [
            "renderer_mode",
            "subscription_type",
            "cc_prompt_id",
            "effort_level",
            "attempts",
            "groupsPreserved",
            "totalGroups",
            "splitKind",
            "headTruncations",
            "preservedUuidCount",
            "preservedMessageCount",
            "keptThinkingBlockCount",
            "keptThinkingStripped",
            "keptThinkingStripDecidedBy",
            "forkAssistantMessageCount",
            "summaryPromptArm",
            "trigger",
            "preCompactTokens",
            "postCompactTokens",
            "restoredAttachmentCount",
            "durationMs",
            "userWaitMs",
            "precomputed",
            "manualPrecomputeReuse",
            "compactionInputTokens",
            "compactionOutputTokens",
            "compactionCacheReadTokens",
            "compactionCacheCreationTokens",
            "compactionTotalTokens",
            "cacheHitRate",
            "cacheCold"
        ]
    );
    assert_eq!(m["trigger"], "manual");
    assert_eq!(m["effort_level"], "high");
    assert_eq!((&m["preservedMessageCount"], &m["keptThinkingBlockCount"]), (&json!(3), &json!(1)));
    // 上一条主线程 100 + 5000 + 50 = 5150，加最后一条 assistant 之后那两条的估算。
    assert_eq!(m["preCompactTokens"], json!(5150 + 2 + 11));
    assert_eq!(m["compactionTotalTokens"], json!(20 + 5_100 + 30 + 400));
    assert!(m["tool_request_Read_tokens"].as_u64().unwrap() > 0);
    assert!(m["tool_result_Read_tokens"].as_u64().unwrap() > 0);
    assert_eq!(m["attachment_total_tokens_reminder_count"], 1, "首轮宣告过的附件按类计数");
    assert_eq!(m["duplicate_read_file_count"], 0);
    assert_eq!(m["restoredAttachmentCount"], 6, "没有读过的文件：只有固定那 6 条附件");
    assert!(keys.last().unwrap().ends_with("_percent"));
    assert_eq!(
        m["human_message_percent"].as_u64().unwrap()
            + m["assistant_message_percent"].as_u64().unwrap()
            + m["tool_request_percent"].as_u64().unwrap()
            + m["tool_result_percent"].as_u64().unwrap()
            + ((m["other_tokens"].as_f64().unwrap() / m["total_tokens"].as_f64().unwrap()) * 100.0)
                .round() as u64
            + m["local_command_output_percent"].as_u64().unwrap(),
        100,
        "{m}"
    );
}

/// 会话活动心跳（`tengu_cache_heartbeat_shadow`）：号的 `tengu_dapper_dawn` 开着、出站 ≥ 2.1.305 才报。
/// 首轮：开工压着，头一条主循环请求带 `interactive` 并补上开工，收尾停手；闲置到期退出时先报敲 `/exit` 的
/// `user_input`，再紧跟逐出提示报 `session_end`。
#[test]
fn cache_heartbeat_follows_the_account_flag() {
    let heartbeats_with = |cred: i64, flag: bool, ua: &str, with_id: bool| {
        let eval = json!({"tengu_dapper_dawn": {"value": flag}});
        crate::proxy::dapper_dawn::note_eval(cred, eval.as_object().unwrap());
        let t = Telemetry::default();
        let mut c = call(cc_body(true), "req_hb", "end_turn");
        c.cred_id = cred;
        c.ua_out = ua.into();
        if !with_id {
            c.request_id = None;
        }
        c.started_at = frozen_now() - Duration::from_secs(60);
        t.ingest(c);
        let now = Instant::now();
        t.gc(now + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1));
        let due = t.take_due(now + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1));
        due.iter()
            .flat_map(|f| f.events.iter())
            .filter(|e| {
                ev_name(e) == "tengu_cache_heartbeat_shadow"
                    || ev_name(e) == "tengu_cache_eviction_hint"
            })
            .map(|e| (ev_name(e).to_string(), meta_of(e)))
            .collect::<Vec<_>>()
    };
    let heartbeats = |cred: i64, flag: bool, ua: &str| heartbeats_with(cred, flag, ua, true);
    let got = heartbeats(9_305, true, "claude-cli/2.1.305 (external, cli)");
    let kinds: Vec<String> = got
        .iter()
        .map(|(n, m)| match n.as_str() {
            "tengu_cache_eviction_hint" => "hint".to_string(),
            _ => format!(
                "{}{}",
                m["kind"].as_str().unwrap(),
                m.get("work").map_or(String::new(), |w| format!(":{}", w.as_str().unwrap()))
            ),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "main_request",
            "work_start:turn_running",
            "work_end:turn_running",
            "user_input",
            "hint",
            "session_end"
        ]
    );
    assert_eq!(got[0].1["interactive"], true);
    assert_eq!(got[0].1["requestId"], "req_hb");
    assert!(got[1].1.get("interactive").is_none());
    assert_eq!(got[5].1["requestId"], "req_hb");

    let off = heartbeats(9_306, false, "claude-cli/2.1.305 (external, cli)");
    assert!(off.iter().all(|(n, _)| n != "tengu_cache_heartbeat_shadow"), "开关关着不报");
    let old = heartbeats(9_307, true, "claude-cli/2.1.300 (external, cli)");
    assert!(old.iter().all(|(n, _)| n != "tengu_cache_heartbeat_shadow"), "301 之前按没有算");
    // 301 / 303 的客户端已经整套接好，号的开关开着就报（303 抓包没有只是那天开关是关的）。
    for (cred, ua) in [
        (9_319, "claude-cli/2.1.301 (external, cli)"),
        (9_320, "claude-cli/2.1.303 (external, cli)"),
    ] {
        let got = heartbeats(cred, true, ua);
        assert!(got.iter().any(|(n, _)| n == "tengu_cache_heartbeat_shadow"), "{ua}: {got:?}");
    }
    // 主循环请求没有 request-id：官方 `tEt` 直接返回，这条不报，时刻也不记，之后一直开不了口。
    let no_id = heartbeats_with(9_308, true, "claude-cli/2.1.305 (external, cli)", false);
    assert!(no_id.iter().all(|(n, _)| n != "tengu_cache_heartbeat_shadow"), "{no_id:?}");
}

/// 心跳测试用：号开关、2.1.305 的 UA、按秒排时刻，并把队列里的心跳取成 `(kind[:work], cc_prompt_id)`。
fn heartbeat_calls(cred: i64) -> impl Fn(&mut ApiCall, u64, u64, u64) {
    let eval = json!({"tengu_dapper_dawn": {"value": true, "source": "force"}});
    crate::proxy::dapper_dawn::note_eval(cred, eval.as_object().unwrap());
    let base = frozen_now() - Duration::from_secs(120);
    move |c: &mut ApiCall, start_secs: u64, ttft_ms: u64, total_ms: u64| {
        c.cred_id = cred;
        c.ua_out = "claude-cli/2.1.305 (external, cli)".into();
        c.started_at = base + Duration::from_secs(start_secs);
        c.ttft_ms = Some(ttft_ms);
        c.total_ms = total_ms;
    }
}

fn heartbeats_of(t: &Telemetry, cred: i64) -> Vec<(String, String)> {
    let st = t.0.state.lock();
    let Some(p) = st.pending.get(&(cred, SESSION.to_string())) else { return Vec::new() };
    let mut got: Vec<(DateTime<Utc>, String, String)> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_cache_heartbeat_shadow")
        .map(|(dt, e)| {
            let m = meta_of(e);
            let kind = format!(
                "{}{}",
                m["kind"].as_str().unwrap(),
                m.get("work").map_or(String::new(), |w| format!(":{}", w.as_str().unwrap()))
            );
            (*dt, kind, m["cc_prompt_id"].as_str().unwrap_or("").to_string())
        })
        .collect();
    got.sort_by_key(|(dt, _, _)| *dt);
    got.into_iter().map(|(_, k, p)| (k, p)).collect()
}

/// 把主线程请求体换成另一轮的 prompt id。
fn with_prompt(body: Vec<u8>, prompt_id: &str) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&body).unwrap();
    v["system"][0]["text"] = json!(format!(
        "x-anthropic-billing-header: cc_version=2.1.305.abc; cc_entrypoint=cli; cch=0f7f8; cc_prompt_id={prompt_id};"
    ));
    v.to_string().into_bytes()
}

const PROMPT_1: &str = "6c079143-0c53-4c48-817d-105460b3f622";
const PROMPT_2: &str = "16d7a19d-7939-4638-9703-b31d2fc92661";
const PROMPT_3: &str = "7f5dfb62-1111-4222-8333-944455556666";

/// 提交前敲字那条 `user_input` 挂上一轮 prompt（客户端那时还没换），提交后的 `work_start` 才是这一轮的
/// （`cap/auto-2.1.305-20261021-full` B1：.139 的 `user_input` 是 82f0dc87，.142 的 `work_start` 才是 227ea7bd）。
#[test]
fn cache_heartbeat_user_input_before_submit_keeps_the_previous_prompt() {
    let cred = 9_311;
    let at = heartbeat_calls(cred);
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "end_turn");
    at(&mut first, 0, 1_000, 3_000);
    t.ingest(first);
    let mut second = call(with_prompt(cc_body(true), PROMPT_2), "req_2", "end_turn");
    at(&mut second, 30, 1_000, 3_000);
    t.ingest(second);
    let got = heartbeats_of(&t, cred);
    let kinds: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "main_request",
            "work_start:turn_running",
            "work_end:turn_running",
            "user_input",
            "work_start:turn_running",
            "main_request",
            "work_end:turn_running"
        ]
    );
    assert_eq!(got[3], ("user_input".to_string(), PROMPT_1.to_string()), "敲字时还是上一轮");
    assert_eq!(got[4].1, PROMPT_2, "提交后的开工才换");
    assert_eq!(got[5].1, PROMPT_2);
}

/// `/clear`：敲命令那下的 `user_input` 还挂旧会话与它最后一轮的 prompt；清场那一刻的逐出提示、
/// `conversation_clear` 心跳与 `shell_set_cwd` 是旧会话 id、parent 指向自己、`/clear` 自己的 prompt；
/// `session_start` 起才换新会话，且不带 `cc_prompt_id`（`cap/auto-2.1.306-20261021-full/00485`）。
#[test]
fn clear_keeps_the_old_session_until_session_start() {
    let cred = 9_313;
    let at = heartbeat_calls(cred);
    let t = Telemetry::default();
    let s2 = "22222222-2222-4222-8222-222222222222";
    let mut probe = quota_probe_call("11111111-1111-4111-8111-111111111111", SystemTime::now());
    at(&mut probe, 0, 300, 400);
    t.ingest(probe);
    let mut first = main_call(SESSION, "req_1", "msg_1", SystemTime::now());
    first.body = Bytes::from(with_prompt(first.body.to_vec(), PROMPT_1));
    at(&mut first, 5, 1_000, 3_000);
    t.ingest(first);
    let mut second = main_call(s2, "req_2", "msg_2", SystemTime::now());
    second.body = Bytes::from(with_prompt(second.body.to_vec(), PROMPT_2));
    at(&mut second, 60, 1_000, 3_000);
    t.ingest(second);
    let st = t.0.state.lock();
    let ev: Vec<Value> =
        st.pending[&(cred, s2.to_string())].events.iter().map(|(_, e)| e.clone()).collect();
    let find = |name: &str, kind: Option<&str>| {
        ev.iter()
            .find(|e| ev_name(e) == name && kind.is_none_or(|k| meta_of(e)["kind"] == k))
            .unwrap_or_else(|| panic!("{name} {kind:?}"))
    };
    let typed = find("tengu_cache_heartbeat_shadow", Some("user_input"));
    assert_eq!(typed["event_data"]["session_id"], SESSION);
    assert!(typed["event_data"].get("parent_session_id").is_none(), "{typed}");
    assert_eq!(meta_of(typed)["cc_prompt_id"], PROMPT_1, "敲 /clear 时还是旧会话最后一轮");
    let clear_prompt = meta_of(find("tengu_cache_eviction_hint", None))["cc_prompt_id"].clone();
    assert!(clear_prompt.as_str().is_some_and(|p| !p.is_empty() && p != PROMPT_1 && p != PROMPT_2));
    for (name, kind) in [
        ("tengu_cache_eviction_hint", None),
        ("tengu_cache_heartbeat_shadow", Some("conversation_clear")),
        ("tengu_shell_set_cwd", None),
    ] {
        let e = find(name, kind);
        assert_eq!(e["event_data"]["session_id"], SESSION, "{name}");
        assert_eq!(e["event_data"]["parent_session_id"], SESSION, "{name}");
        assert_eq!(meta_of(e)["cc_prompt_id"], clear_prompt, "{name}");
    }
    for name in ["tengu_session_start", "tengu_input_command"] {
        let e = find(name, None);
        assert_eq!(e["event_data"]["session_id"], s2, "{name}");
        assert!(meta_of(e).get("cc_prompt_id").is_none(), "{name}: 下一次提交前没有 prompt");
    }
    assert_eq!(meta_of(find("tengu_input_prompt", None))["cc_prompt_id"], PROMPT_2);
}

/// 后台子代理：派出那一刻由拉起它的那条主线程请求登记 `subagent_running`（子代理首条落地时主线程续轮已经
/// 完成，晚登记会被当过时丢掉）；续轮出字切回 `turn_running`，流收尾又切回 `subagent_running`，一轮结束
/// 先切回 `turn_running` 再停手（`cap/auto-2.1.305-20261021-full` A8a / A8b / A19 三处同序）。子代理跑完
/// 之后的一轮不再有这一下。
#[test]
fn cache_heartbeat_follows_a_background_subagent() {
    let cred = 9_312;
    let at = heartbeat_calls(cred);
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "end_turn");
    at(&mut first, 0, 1_000, 3_000);
    t.ingest(first);
    // 第二轮：回复里调 `Agent`。
    let mut spawn = call(with_prompt(cc_body(true), PROMPT_2), "req_spawn", "tool_use");
    at(&mut spawn, 30, 1_800, 6_000);
    spawn.tool_calls =
        vec![ToolCall { id: "t1".into(), name: "Agent".into(), input: json!({}), verdict: None }];
    spawn.tool_use_lens = vec![("Agent".into(), 80)];
    t.ingest(spawn);
    // 主线程续轮（子代理在后台跑，工具结果立刻回来）先于子代理首条落地。
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Agent","input":{}}]});
    let mut cont =
        call(with_prompt(body.to_string().into_bytes(), PROMPT_2), "req_cont", "end_turn");
    at(&mut cont, 36, 1_000, 2_000);
    t.ingest(cont);
    let agent = AgentHeaders {
        agent_id: Some("a1".into()),
        agent_type: Some("Explore".into()),
        request_class: Some("subagent".into()),
    };
    let mut s1 = call(with_prompt(cc_body(true), PROMPT_2), "req_s1", "tool_use");
    at(&mut s1, 30, 800, 9_000);
    s1.agent = agent.clone();
    s1.tool_calls =
        vec![ToolCall { id: "t2".into(), name: "Read".into(), input: json!({}), verdict: None }];
    t.ingest(s1);
    let mut s2 = call(with_prompt(cc_body(false), PROMPT_2), "req_s2", "end_turn");
    at(&mut s2, 40, 800, 2_000);
    s2.agent = agent;
    t.ingest(s2);
    // 第三轮：子代理已经跑完，一轮结束直接停手。
    let mut third = call(with_prompt(cc_body(true), PROMPT_3), "req_3", "end_turn");
    at(&mut third, 60, 1_000, 3_000);
    t.ingest(third);
    let got = heartbeats_of(&t, cred);
    let kinds: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "main_request",
            "work_start:turn_running",
            "work_end:turn_running",
            // 第二轮
            "user_input",
            "work_start:turn_running",
            "work_end:turn_running",
            "work_start:subagent_running",
            "main_request",
            // 续轮：出字切回，收尾切走，一轮结束切回再停手
            "work_end:subagent_running",
            "work_start:turn_running",
            "work_end:turn_running",
            "work_start:subagent_running",
            "main_request",
            "work_end:subagent_running",
            "work_start:turn_running",
            "work_end:turn_running",
            // 第三轮
            "user_input",
            "work_start:turn_running",
            "main_request",
            "work_end:turn_running",
        ]
    );
}

/// 一条回复并行派两个 Agent：工具长度表按名字聚合只有一项，计数要按实际调用。头一个跑完、另一个还在跑，
/// 主线程续轮照样有 `subagent_running` 来回切。
#[test]
fn cache_heartbeat_counts_parallel_agents_by_call() {
    let cred = 9_314;
    let at = heartbeat_calls(cred);
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "end_turn");
    at(&mut first, 0, 1_000, 3_000);
    t.ingest(first);
    let agent_call = |id: &str| ToolCall {
        id: id.into(),
        name: "Agent".into(),
        input: json!({}),
        verdict: None,
    };
    let mut spawn = call(with_prompt(cc_body(true), PROMPT_2), "req_spawn", "tool_use");
    at(&mut spawn, 30, 1_800, 6_000);
    spawn.tool_calls = vec![agent_call("t1"), agent_call("t2")];
    spawn.tool_use_lens = vec![("Agent".into(), 160)];
    t.ingest(spawn);
    // 头一个子代理跑完。
    let mut s1 = call(with_prompt(cc_body(true), PROMPT_2), "req_s1", "end_turn");
    at(&mut s1, 37, 800, 2_000);
    s1.agent = AgentHeaders {
        agent_id: Some("a1".into()),
        agent_type: Some("Explore".into()),
        request_class: Some("subagent".into()),
    };
    t.ingest(s1);
    // 另一个还在跑时主线程续轮回来。
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Agent","input":{}}]});
    let mut cont =
        call(with_prompt(body.to_string().into_bytes(), PROMPT_2), "req_cont", "end_turn");
    at(&mut cont, 40, 1_000, 2_000);
    t.ingest(cont);
    let got = heartbeats_of(&t, cred);
    let kinds: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
    let from = kinds.iter().rposition(|k| *k == "main_request").unwrap();
    assert_eq!(
        kinds[from - 4..].to_vec(),
        [
            "work_end:subagent_running",
            "work_start:turn_running",
            "work_end:turn_running",
            "work_start:subagent_running",
            "main_request",
            "work_end:subagent_running",
            "work_start:turn_running",
            "work_end:turn_running"
        ],
        "{kinds:?}"
    );
}

/// 嵌套代理：主线程派父代理、父代理再派孙代理，孙代理先跑完不扣主线程那份——父代理还在跑，主线程续轮
/// 照样有 `subagent_running` 来回切；父代理跑完之后的续轮就没有了。父子关系请求头里没有，按首条用户消息里
/// 那段 `prompt` 认（对上主线程派的那份才算）。
#[test]
fn cache_heartbeat_ignores_grandchild_agents() {
    let cred = 9_315;
    let at = heartbeat_calls(cred);
    let t = Telemetry::default();
    let agent_call = |id: &str, prompt: &str| ToolCall {
        id: id.into(),
        name: "Agent".into(),
        input: json!({ "prompt": prompt }),
        verdict: None,
    };
    // 子代理首条：首条用户消息是 system-reminder + 派它的 prompt，客户端常在末尾补一个换行
    // （`cap/auto-2.1.305-20261021-full` 六个子代理里五个）。
    let agent_body = |prompt: &str| {
        let mut v: Value = serde_json::from_slice(&with_prompt(cc_body(true), PROMPT_2)).unwrap();
        v["messages"] = json!([{"role":"user","content":[
            {"type":"text","text":"<system-reminder>ctx</system-reminder>"},
            {"type":"text","text":format!("{prompt}\n")}
        ]}]);
        v.to_string().into_bytes()
    };
    let headers = |id: &str| AgentHeaders {
        agent_id: Some(id.into()),
        agent_type: Some("general-purpose".into()),
        request_class: Some("subagent".into()),
    };
    let continuation = |rid: &str| {
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Agent","input":{}}]});
        call(with_prompt(body.to_string().into_bytes(), PROMPT_2), rid, "tool_use")
    };
    let mut first = call(cc_body(true), "req_1", "end_turn");
    at(&mut first, 0, 1_000, 3_000);
    t.ingest(first);
    let mut spawn = call(with_prompt(cc_body(true), PROMPT_2), "req_spawn", "tool_use");
    at(&mut spawn, 30, 1_800, 6_000);
    spawn.tool_calls = vec![agent_call("t1", "parent task")];
    spawn.tool_use_lens = vec![("Agent".into(), 80)];
    t.ingest(spawn);
    // 父代理首条：再派一个孙代理。
    let mut p1 = call(agent_body("parent task"), "req_p1", "tool_use");
    at(&mut p1, 37, 800, 1_000);
    p1.agent = headers("parent");
    p1.tool_calls = vec![agent_call("t9", "grandchild task")];
    p1.tool_use_lens = vec![("Agent".into(), 80)];
    t.ingest(p1);
    // 孙代理一条就跑完。
    let mut g1 = call(agent_body("grandchild task"), "req_g1", "end_turn");
    at(&mut g1, 39, 800, 1_000);
    g1.agent = headers("grandchild");
    t.ingest(g1);
    // 父代理还在跑：主线程续轮照样切。
    let mut c1 = continuation("req_c1");
    at(&mut c1, 41, 1_000, 2_000);
    t.ingest(c1);
    let kinds = |t: &Telemetry| -> Vec<String> {
        heartbeats_of(t, cred).into_iter().map(|(k, _)| k).collect()
    };
    let got = kinds(&t);
    let from = got.iter().rposition(|k| k == "main_request").unwrap();
    assert_eq!(
        got[from - 2..from + 1].to_vec(),
        ["work_end:turn_running", "work_start:subagent_running", "main_request"],
        "孙代理跑完不该把父代理那份扣掉：{got:?}"
    );
    // 父代理跑完，再来一条续轮就不切了。
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t9","name":"Agent","input":{}}]});
    let mut p2 = call(with_prompt(body.to_string().into_bytes(), PROMPT_2), "req_p2", "end_turn");
    at(&mut p2, 44, 800, 1_000);
    p2.agent = headers("parent");
    t.ingest(p2);
    let mut c2 = continuation("req_c2");
    at(&mut c2, 46, 1_000, 2_000);
    t.ingest(c2);
    let got = kinds(&t);
    let from = got.iter().rposition(|k| k == "main_request").unwrap();
    assert_eq!(
        got[from - 2..from + 1].to_vec(),
        ["work_end:subagent_running", "work_start:turn_running", "main_request"],
        "父代理跑完之后不再有委派的活：{got:?}"
    );
}

/// 上一轮的后台代理晚到：它的 `prompt` 与这一轮派的对不上，不能占掉这一轮那份。它跑完之后，这一轮派的
/// 还在跑，主线程续轮照样切 `subagent_running`。
#[test]
fn cache_heartbeat_a_late_agent_from_an_earlier_turn_does_not_claim() {
    let cred = 9_316;
    let at = heartbeat_calls(cred);
    let t = Telemetry::default();
    let agent_call = |prompt: &str| ToolCall {
        id: "t1".into(),
        name: "Agent".into(),
        input: json!({ "prompt": prompt }),
        verdict: None,
    };
    let agent_body = |prompt: &str| {
        let mut v: Value = serde_json::from_slice(&with_prompt(cc_body(true), PROMPT_3)).unwrap();
        v["messages"] = json!([{"role":"user","content":[
            {"type":"text","text":"<system-reminder>ctx</system-reminder>"},
            {"type":"text","text":prompt}
        ]}]);
        v.to_string().into_bytes()
    };
    let headers = |id: &str| AgentHeaders {
        agent_id: Some(id.into()),
        agent_type: Some("general-purpose".into()),
        request_class: Some("subagent".into()),
    };
    let mut first = call(cc_body(true), "req_1", "end_turn");
    at(&mut first, 0, 1_000, 3_000);
    t.ingest(first);
    // 第二轮派出 A，A 的首条迟迟不回。
    let mut spawn_a = call(with_prompt(cc_body(true), PROMPT_2), "req_spawn_a", "tool_use");
    at(&mut spawn_a, 20, 1_000, 2_000);
    spawn_a.tool_calls = vec![agent_call("old A task")];
    t.ingest(spawn_a);
    // 第三轮派出 B。
    let mut spawn_b = call(with_prompt(cc_body(true), PROMPT_3), "req_spawn_b", "tool_use");
    at(&mut spawn_b, 40, 1_000, 2_000);
    spawn_b.tool_calls = vec![agent_call("new B task")];
    t.ingest(spawn_b);
    // A 晚到、一条就跑完。
    let mut a1 = call(agent_body("old A task"), "req_a1", "end_turn");
    at(&mut a1, 21, 800, 22_000);
    a1.agent = headers("a");
    t.ingest(a1);
    // B 还没露面，主线程续轮：照样切。
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Agent","input":{}}]});
    let mut cont =
        call(with_prompt(body.to_string().into_bytes(), PROMPT_3), "req_cont", "end_turn");
    at(&mut cont, 45, 1_000, 2_000);
    t.ingest(cont);
    let got: Vec<String> = heartbeats_of(&t, cred).into_iter().map(|(k, _)| k).collect();
    let from = got.iter().rposition(|k| k == "main_request").unwrap();
    assert_eq!(
        got[from - 2..from + 1].to_vec(),
        ["work_end:turn_running", "work_start:subagent_running", "main_request"],
        "{got:?}"
    );
}

/// 上一轮的后台代理晚到，它带的是派它那一轮的 prompt：不能把会话的 prompt 拨回去——下一次提交前敲字那条
/// `user_input` 挂的仍是刚结束那一轮。
#[test]
fn cache_heartbeat_a_late_agent_does_not_rewind_the_prompt() {
    let cred = 9_317;
    let at = heartbeat_calls(cred);
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "end_turn");
    at(&mut first, 0, 1_000, 3_000);
    t.ingest(first);
    let mut spawn = call(with_prompt(cc_body(true), PROMPT_2), "req_spawn", "end_turn");
    at(&mut spawn, 20, 1_000, 2_000);
    t.ingest(spawn);
    let mut third = call(with_prompt(cc_body(true), PROMPT_3), "req_3", "end_turn");
    at(&mut third, 40, 1_000, 2_000);
    t.ingest(third);
    // 第二轮派出的代理这时才落地，带着第二轮的 prompt。
    let mut late = call(with_prompt(cc_body(true), PROMPT_2), "req_late", "end_turn");
    at(&mut late, 21, 800, 25_000);
    late.agent = AgentHeaders {
        agent_id: Some("old".into()),
        agent_type: Some("general-purpose".into()),
        request_class: Some("subagent".into()),
    };
    t.ingest(late);
    // 那个子代理里的 WebFetch 页面处理（没有工具、`auxiliary`、带它的 agent-id）也晚到，同样带第二轮的 prompt。
    let mut v: Value = serde_json::from_slice(&with_prompt(cc_body(true), PROMPT_2)).unwrap();
    v["tools"] = json!([]);
    let mut fetch = call(v.to_string().into_bytes(), "req_fetch", "end_turn");
    at(&mut fetch, 22, 800, 26_000);
    fetch.agent = AgentHeaders {
        agent_id: Some("old".into()),
        agent_type: None,
        request_class: Some("auxiliary".into()),
    };
    t.ingest(fetch);
    let fourth_prompt = "a1b2c3d4-0000-4000-8000-000000000004";
    let mut fourth = call(with_prompt(cc_body(true), fourth_prompt), "req_4", "end_turn");
    at(&mut fourth, 60, 1_000, 2_000);
    t.ingest(fourth);
    let got = heartbeats_of(&t, cred);
    let last_input = got.iter().rposition(|(k, _)| k == "user_input").unwrap();
    assert_eq!(got[last_input].1, PROMPT_3, "{got:?}");
    assert_eq!(got[last_input + 1], ("work_start:turn_running".to_string(), fourth_prompt.into()));
}

/// 进度摘要比子代理首条先落地（它也会给这条支线建档）：子代理首条照样认领派遣，跑完之后主线程续轮不再切
/// `subagent_running`。
#[test]
fn cache_heartbeat_claims_even_when_a_summary_lands_first() {
    let cred = 9_318;
    let at = heartbeat_calls(cred);
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "end_turn");
    at(&mut first, 0, 1_000, 3_000);
    t.ingest(first);
    let mut spawn = call(with_prompt(cc_body(true), PROMPT_2), "req_spawn", "tool_use");
    at(&mut spawn, 30, 1_000, 2_000);
    spawn.tool_calls = vec![ToolCall {
        id: "t1".into(),
        name: "Agent".into(),
        input: json!({ "prompt": "do the task" }),
        verdict: None,
    }];
    spawn.tool_use_lens = vec![("Agent".into(), 80)];
    t.ingest(spawn);
    let headers = |class: &str| AgentHeaders {
        agent_id: Some("a1".into()),
        agent_type: Some("general-purpose".into()),
        request_class: Some(class.into()),
    };
    let mut summary = call(with_prompt(cc_body(true), PROMPT_2), "req_sum", "end_turn");
    at(&mut summary, 33, 500, 800);
    summary.agent = headers("auxiliary");
    t.ingest(summary);
    let mut v: Value = serde_json::from_slice(&with_prompt(cc_body(true), PROMPT_2)).unwrap();
    v["messages"] = json!([{"role":"user","content":[
        {"type":"text","text":"<system-reminder>ctx</system-reminder>"},
        {"type":"text","text":"do the task\n"}
    ]}]);
    let mut s1 = call(v.to_string().into_bytes(), "req_s1", "end_turn");
    at(&mut s1, 32, 800, 4_000);
    s1.agent = headers("subagent");
    t.ingest(s1);
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Agent","input":{}}]});
    let mut cont =
        call(with_prompt(body.to_string().into_bytes(), PROMPT_2), "req_cont", "end_turn");
    at(&mut cont, 40, 1_000, 2_000);
    t.ingest(cont);
    let got: Vec<String> = heartbeats_of(&t, cred).into_iter().map(|(k, _)| k).collect();
    let from = got.iter().rposition(|k| k == "main_request").unwrap();
    assert_eq!(
        got[from - 2..].to_vec(),
        [
            "work_end:subagent_running",
            "work_start:turn_running",
            "main_request",
            "work_end:turn_running"
        ],
        "子代理跑完、派遣已认领：{got:?}"
    );
}

/// `/compact` 被取消或失败：敲字与转入 `compacting` 照样报，收尾后切出来回到闲着，不报 `compaction`、不清场；
/// 成功才报 `compaction`。
#[test]
fn cache_heartbeat_reports_a_cancelled_compaction() {
    for (cred, outcome) in [(9_321, "aborted"), (9_322, "failed"), (9_323, "ok")] {
        let at = heartbeat_calls(cred);
        let t = Telemetry::default();
        let mut first = call(cc_body(true), "req_1", "end_turn");
        at(&mut first, 0, 1_000, 3_000);
        t.ingest(first);
        let mut body: Value =
            serde_json::from_slice(&with_prompt(cc_body(false), PROMPT_2)).unwrap();
        body["messages"] = json!([
            {"role":"user","content":[{"type":"text","text":"hello there"}]},
            {"role":"assistant","content":[{"type":"text","text":"ok"}]},
            {"role":"user","content":[{"type":"text","text":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools."}]}
        ]);
        let mut c = call(body.to_string().into_bytes(), "req_c", "end_turn");
        at(&mut c, 30, 1_000, 20_000);
        c.agent = AgentHeaders {
            agent_id: None,
            agent_type: None,
            request_class: Some("compaction".into()),
        };
        match outcome {
            "aborted" => c.aborted = true,
            "failed" => c = failed(c, Some(500), "api_error", "boom"),
            _ => {}
        }
        t.ingest(c);
        let got: Vec<String> = heartbeats_of(&t, cred).into_iter().map(|(k, _)| k).collect();
        let tail: Vec<&str> = got[3..].iter().map(String::as_str).collect();
        let want: &[&str] = if outcome == "ok" {
            &["user_input", "work_start:compacting", "compaction"]
        } else {
            &["user_input", "work_start:compacting", "work_end:compacting"]
        };
        assert_eq!(tail, want, "{outcome}: {got:?}");
    }
}

/// 上一轮的工具在权限弹框上被拒，代理在下一次输入里才看出来、补报那一段：弹框、拒绝、文件建议刷新、turn 与
/// `turn_end{aborted_tools}` 都是上一轮的事，挂上一轮的 prompt；这次输入才是新的
/// （`cap/auto-2.1.305-20261021-full` B1：.484 ~ .412 全是 1572cea5）。
#[test]
fn rejected_tool_events_keep_the_previous_prompt() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.305 (external, cli)";
    let base = frozen_now() - Duration::from_secs(120);
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = ua.into();
    first.started_at = base;
    first.tool_calls = vec![ToolCall {
        id: "t1".into(),
        name: "Bash".into(),
        input: json!({"command": "ls"}),
        verdict: None,
    }];
    first.tool_use_lens = vec![("Bash".into(), 16)];
    t.ingest(first);
    let mut body: Value = serde_json::from_slice(&with_prompt(cc_body(true), PROMPT_2)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]});
    body["messages"][2] = json!({"role":"user","content":[
        {"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed."},
        {"type":"text","text":"[Request interrupted by user for tool use]\n"},
        {"type":"text","text":"do something else"}
    ]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.ua_out = ua.into();
    second.started_at = base + Duration::from_secs(30);
    t.ingest(second);
    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let pid = |name: &str, pick: &dyn Fn(&Value) -> bool| -> Vec<String> {
        p.events
            .iter()
            .filter(|(_, e)| ev_name(e) == name)
            .map(|(_, e)| meta_of(e))
            .filter(|m| pick(m))
            .map(|m| m["cc_prompt_id"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let all = |_: &Value| true;
    for name in [
        "tengu_tool_use_rejected_in_prompt",
        "tengu_tool_use_can_use_tool_rejected",
        "tengu_permission_request_escape",
    ] {
        assert_eq!(pid(name, &all), [PROMPT_1], "{name}");
    }
    assert_eq!(pid("tengu_unary_event", &all), [PROMPT_1, PROMPT_1], "弹框与拒绝两条");
    assert_eq!(pid("tengu_turn_end", &|m| m["terminal_reason"] == "aborted_tools"), [PROMPT_1]);
    let deny: Vec<&Value> =
        p.dd.iter().filter(|d| d["feature_name"] == "permission_user_deny").collect();
    assert_eq!(deny.len(), 1);
    assert!(deny[0].to_string().contains(PROMPT_1), "Datadog 那份同样挂上一轮：{}", deny[0]);
    assert_eq!(pid("tengu_input_prompt", &all).last().unwrap(), PROMPT_2, "这次输入是新的");
}

/// 号的开关会话中途换了值就跟着换：关了不报、状态机不动，再开又接着报（官方每次触发都重读
/// `tengu_dapper_dawn`）。
#[test]
fn cache_heartbeat_follows_the_flag_as_it_changes() {
    // 开关是按号记的全局状态：用独占的号，别的测试（同用 9_313 的那两条）并发改它会让这里偶发失败。
    let cred = 9_327;
    let at = heartbeat_calls(cred);
    let set = |on: bool| {
        let eval = json!({"tengu_dapper_dawn": {"value": on}});
        crate::proxy::dapper_dawn::note_eval(cred, eval.as_object().unwrap());
    };
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "end_turn");
    at(&mut first, 0, 1_000, 3_000);
    t.ingest(first);
    let n1 = heartbeats_of(&t, cred).len();
    assert_eq!(n1, 3);
    set(false);
    let mut second = call(with_prompt(cc_body(true), PROMPT_2), "req_2", "end_turn");
    at(&mut second, 30, 1_000, 3_000);
    t.ingest(second);
    assert_eq!(heartbeats_of(&t, cred).len(), n1, "关了就不报");
    set(true);
    let mut third = call(with_prompt(cc_body(true), PROMPT_3), "req_3", "end_turn");
    at(&mut third, 60, 1_000, 3_000);
    t.ingest(third);
    let got = heartbeats_of(&t, cred);
    let kinds: Vec<&str> = got[n1..].iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        ["user_input", "work_start:turn_running", "main_request", "work_end:turn_running"],
        "再开接着报"
    );
}

/// 2.1.293 起退出走敲 `/exit`：提交、`cmd_exit`、分派命令，收尾之后是快照、更新检查与输入历史落盘，最后才是
/// 逐出提示；Datadog 那批是 `prompt_submit` / `cmd_exit` / `cmd_dispatch` / `update_detection` / `history_save`，
/// `lsp_shutdown` / `swarm_session_cleanup` 按服务端抽样表只留百分之一（`cap/auto-2.1.305-20261021-full` C2）。
#[test]
fn exit_goes_through_the_exit_command_from_2_1_293() {
    let t = Telemetry::default();
    let mut last = call(cc_body(true), "req_last", "end_turn");
    last.ua_out = "claude-cli/2.1.305 (external, cli)".into();
    last.started_at = frozen_now() - Duration::from_secs(60);
    t.ingest(last);
    let later = Instant::now() + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1);
    t.gc(later);
    let due = t.take_due(later);
    let f = &due[0];
    let names: Vec<String> = f
        .events
        .iter()
        .map(|e| match ev_name(e) {
            "tengu_feature_ok" => format!("ok:{}", meta_of(e)["feature_name"].as_str().unwrap()),
            n => n.to_string(),
        })
        .collect();
    let from = names.iter().rposition(|n| n == "tengu_paste_text").unwrap();
    assert_eq!(
        names[from..]
            .iter()
            .map(String::as_str)
            .filter(|n| *n != "ok:internal_metrics_export")
            .collect::<Vec<_>>(),
        [
            "tengu_paste_text",
            "ok:prompt_submit",
            "ok:cmd_exit",
            "ok:cmd_dispatch",
            "tengu_input_command",
            "tengu_resume_hint_printed",
            "tengu_config_cache_stats",
            "ok:lsp_shutdown",
            "tengu_file_history_snapshot_success",
            "ok:swarm_session_cleanup",
            "ok:update_detection",
            "ok:history_save",
            "tengu_cache_eviction_hint",
        ]
    );
    // 抽中的那百分之一带着 `sample_rate`（[`sample_dd`]），不带的才是每次都发的。
    let dd: Vec<&str> =
        f.dd.iter()
            .filter(|d| d.get("sample_rate").is_none())
            .filter_map(|d| d["feature_name"].as_str())
            .collect();
    // 官方这一批同样以指标导出那条收尾。
    let tail = &dd[dd.len() - 6..];
    assert_eq!(
        tail,
        [
            "prompt_submit",
            "cmd_exit",
            "cmd_dispatch",
            "update_detection",
            "history_save",
            "internal_metrics_export"
        ]
    );
    assert!(!dd.contains(&"lsp_shutdown") && !dd.contains(&"swarm_session_cleanup"));
    // 敲 `/exit` 是一次提交：`paste_text` / `prompt_submit` 还挂上一轮的 prompt，从 `cmd_exit` 起换新的，
    // 一直到逐出提示与指标导出（`cap/auto-2.1.305-20261021-full` C2）。
    let pid = |i: usize| meta_of(&f.events[i])["cc_prompt_id"].as_str().unwrap().to_string();
    let at = |n: &str| names.iter().rposition(|x| x == n).unwrap();
    let old_id = pid(at("ok:prompt_submit"));
    assert_eq!(pid(at("tengu_paste_text")), old_id);
    assert_eq!(old_id, "6c079143-0c53-4c48-817d-105460b3f622", "提交那两条是上一轮的");
    let new_id = pid(at("ok:cmd_exit"));
    assert_ne!(new_id, old_id);
    for n in ["tengu_input_command", "tengu_cache_eviction_hint", "ok:internal_metrics_export"] {
        assert_eq!(pid(at(n)), new_id, "{n}");
    }
}

/// 会话闲置到期 = 客户端退出：补退出事件链，三路立刻到期一起发（`cap/2.1.260-1`）。
#[test]
fn idle_session_ends_like_a_client_exit() {
    let t = Telemetry::default();
    // 一分钟前发的：首轮那串版本检查（api_success 后 6.9s）得落在「退出」之前，真实
    // 情形下退出离最后一条请求至少 3 小时。
    let mut last = call(cc_body(true), "req_last", "end_turn");
    last.started_at = frozen_now() - Duration::from_secs(60);
    t.ingest(last);
    let now = Instant::now();
    t.gc(now);
    assert!(t.latest_session(7, Duration::from_secs(3600)).is_some(), "还没闲置到期");
    assert!(t.take_due(now).is_empty());

    let later = now + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1);
    t.gc(later);
    assert!(t.latest_session(7, Duration::from_secs(u64::MAX / 4)).is_none(), "会话已忘掉");
    let due = t.take_due(later);
    assert_eq!(due.len(), 1);
    let f = &due[0];
    assert_eq!(f.session_id, SESSION);
    let names: Vec<&str> = f.events.iter().map(ev_name).collect();
    let tail = &names[names.len() - 5..];
    assert_eq!(
        tail,
        [
            "tengu_config_cache_stats",
            "tengu_feature_ok",
            "tengu_feature_ok",
            "tengu_feature_ok",
            "tengu_cache_eviction_hint"
        ],
        "队尾是退出那一串，排在这次请求的事件之后"
    );
    let last = f.events.last().unwrap()["event_data"].clone();
    let meta: Value = serde_json::from_slice(
        &STANDARD.decode(last["additional_metadata"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["scope"], "session_end");
    assert_eq!(meta["last_request_id"], "req_last");
    assert_eq!(meta["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622");
    assert_eq!(last["session_id"], SESSION);
    assert_eq!(last["model"], "claude-opus-5[1m]");
    assert_eq!(last["auth"]["organization_uuid"], "09520b85-f6b6-432f-97e2-6ecb804a083f");
    let dd_features: Vec<&str> =
        f.dd.iter()
            .filter_map(|d| d["feature_name"].as_str())
            .filter(|n| {
                ["lsp_shutdown", "swarm_session_cleanup", "internal_metrics_export"].contains(n)
            })
            .collect();
    assert_eq!(dd_features, ["lsp_shutdown", "swarm_session_cleanup", "internal_metrics_export"]);
    assert!(f.metrics.is_some(), "退出时指标也一起发");
    assert!(t.take_due(later + Duration::from_secs(1)).is_empty());
}

/// 已按退出收尾的 session_id 再来 = `--resume`：新进程从头计数（新 chain、无
/// previousRequestId），指标 `start_type` 报 `resume`。
#[test]
fn a_session_id_returning_after_exit_is_a_resume() {
    let t = Telemetry::default();
    t.ingest(call(cc_body(true), "req_1", "end_turn"));
    let now = Instant::now();
    let after_idle = now + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1);
    t.gc(after_idle);
    assert_eq!(t.take_due(after_idle).len(), 1, "退出那批发掉");

    let mut back = call(cc_body(false), "req_2", "end_turn");
    back.started_at = frozen_now();
    t.ingest(back);
    let flush_at = after_idle + Duration::from_secs(config::TELEMETRY_METRICS_FLUSH_SECS + 1);
    let due = t.take_due(flush_at);
    assert_eq!(due.len(), 1);
    let f = &due[0];
    let m = f.metrics.as_ref().expect("metrics");
    let sc = &m["metrics"][0];
    assert_eq!(sc["name"], "claude_code.session.count");
    assert_eq!(sc["data_points"][0]["attributes"]["start_type"], "resume");
    let success =
        f.events.iter().find(|e| e["event_data"]["event_name"] == "tengu_api_success").unwrap();
    let meta: Value = serde_json::from_slice(
        &STANDARD.decode(success["event_data"]["additional_metadata"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert!(meta.get("previousRequestId").is_none(), "新进程不接上一段的 request-id");
    assert_eq!(meta["messageTokens"], 0);
    assert!(
        f.events.iter().any(|e| e["event_data"]["event_name"] == "tengu_input_prompt"),
        "首条请求即便是 tool_result 续轮也按新进程的第一次输入计"
    );

    // 再次闲置退出后又回来，依旧是 resume（表里重新记了一次）。
    let again = flush_at + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1);
    t.gc(again);
    t.take_due(again);
    let mut third = call(cc_body(true), "req_3", "end_turn");
    third.started_at = frozen_now();
    t.ingest(third);
    let due = t.take_due(again + Duration::from_secs(config::TELEMETRY_METRICS_FLUSH_SECS + 1));
    assert_eq!(
        due[0].metrics.as_ref().unwrap()["metrics"][0]["data_points"][0]["attributes"]["start_type"],
        "resume"
    );

    // 另一个从没见过的会话仍是 fresh。
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["metadata"]["user_id"] = json!(
        "{\"device_id\":\"aa\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"brand-new\"}"
    );
    t.ingest(call(body.to_string().into_bytes(), "req_4", "end_turn"));
    let far = again + Duration::from_secs(2 * config::TELEMETRY_METRICS_FLUSH_SECS + 5);
    let due = t.take_due(far);
    let fresh = due.iter().find(|f| f.session_id == "brand-new").unwrap();
    assert_eq!(
        fresh.metrics.as_ref().unwrap()["metrics"][0]["data_points"][0]["attributes"]["start_type"],
        "fresh"
    );
}

/// 启动时的额度探测请求不产生任何 api 事件（`cap/2.1.260-1`）。
#[test]
fn quota_probe_is_not_reported() {
    let body = json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 1,
        "messages": [{"role":"user","content":"quota"}],
        "metadata": {"user_id": "{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"}
    });
    assert!(parse_shape(body.to_string().as_bytes()).unwrap().quota_probe);
    let t = Telemetry::default();
    t.ingest(call(body.to_string().into_bytes(), "req_q", "max_tokens"));
    assert!(t.0.state.lock().pending.is_empty());
    assert!(t.latest_session(7, Duration::from_secs(60)).is_none(), "也不算开了会话");
}

/// 每次指标导出都伴随一条 `internal_metrics_export`，进事件与 Datadog 两路的下一批。
#[test]
fn metrics_export_queues_its_feature_event() {
    let t = Telemetry::default();
    t.ingest(call(cc_body(true), "req_1", "end_turn"));
    let now = Instant::now();
    // 先把 30s / 15s 那两路清掉，只剩指标在攒。
    t.take_due(now + Duration::from_secs(config::TELEMETRY_EVENT_FLUSH_SECS + 1));
    let at = now + Duration::from_secs(config::TELEMETRY_METRICS_FLUSH_SECS + 1);
    let due = t.take_due(at);
    assert_eq!(due.len(), 1);
    assert!(due[0].metrics.is_some());
    assert!(due[0].events.is_empty() && due[0].dd.is_empty(), "导出事件刚入队，还没到期");
    let dd = t.take_due(at + Duration::from_secs(config::TELEMETRY_DATADOG_FLUSH_SECS + 1));
    assert_eq!(dd.len(), 1);
    assert_eq!(dd[0].dd.len(), 1);
    assert_eq!(dd[0].dd[0]["feature_name"], "internal_metrics_export");
    assert_eq!(dd[0].dd[0]["model"], "claude-opus-5");
    assert!(dd[0].events.is_empty(), "事件那路 30s 才到");
    let ev = t.take_due(at + Duration::from_secs(config::TELEMETRY_EVENT_FLUSH_SECS + 1));
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].events.len(), 1);
    let d = &ev[0].events[0]["event_data"];
    assert_eq!(d["event_name"], "tengu_feature_ok");
    assert_eq!(d["session_id"], SESSION);
    assert_eq!(d["model"], "claude-opus-5[1m]");
    let meta: Value = serde_json::from_slice(
        &STANDARD.decode(d["additional_metadata"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["feature_name"], "internal_metrics_export");
}

fn meta_of(e: &Value) -> Value {
    serde_json::from_slice(
        &STANDARD.decode(e["event_data"]["additional_metadata"].as_str().unwrap()).unwrap(),
    )
    .unwrap()
}

/// 正文改成订阅端延迟形态：`ToolSearch` + `DeferredToolPlaceholder` 那一对都在
/// （`cap/2.1.258/00012`）。
fn tool_search_body() -> Vec<u8> {
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["tools"] = json!([
        {"name":"Bash","description":"run","input_schema":{"type":"object"}},
        {"name":"ToolSearch","description":"search","input_schema":{"type":"object"}},
        {"name":"DeferredToolPlaceholder","description":"d","input_schema":{"type":"object"},"defer_loading":true}
    ]);
    body.to_string().into_bytes()
}

/// 正文改成 API-key 端那种**全量声明、无延迟**的工具形态（`cap/2.1.258-api/00006`：内建 +
/// 两个 `mcp__ide__*`，没有 `defer_loading`）。
fn undeferred_body() -> Vec<u8> {
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["tools"] = json!([
        {"name":"Bash","description":"run","input_schema":{"type":"object"}},
        {"name":"Read","description":"read","input_schema":{"type":"object"}},
        {"name":"mcp__ide__getDiagnostics","description":"d","input_schema":{"type":"object"}},
        {"name":"mcp__ide__executeCode","description":"e","input_schema":{"type":"object"}}
    ]);
    body.to_string().into_bytes()
}

/// `tengu_tool_search_mode_decision` 按正文里**实际启用**的能力取值，三档对齐抓包：延迟形态
/// `tst_enabled`；全量声明无延迟 `not_registered` 且 `mcpToolCount` 等于正文里 `mcp__*` 的
/// 个数（`cap/2.1.258-api/00022` 报 2，正文正好两个 `mcp__ide__*`）；无工具
/// `no_tools_in_request`。此前只要有工具就报 `tst_enabled`——模拟路径与 API-key 端的正文都
/// 没有 ToolSearch，遥测却说延迟加载开着。
///
/// **反例**：只有 `defer_loading` 占位、没有 `ToolSearch`（`cc_body(true)` 正是 Bash +
/// DeferredToolPlaceholder）——延迟声明在、搜索能力不在，报 `not_registered`。官方样本里两者
/// 总是同时出现，判据落在 `ToolSearch` 上才不会给半抄的正文宣称一个没有的能力。
#[test]
fn tool_search_decision_follows_the_body() {
    let official = parse_shape(&tool_search_body()).unwrap();
    assert!(official.has_tool_search && official.deferred_tools == 1);
    let m = tool_search_decision(&official, "claude-opus-5[1m]", false);
    assert_eq!(m["enabled"], true);
    assert_eq!(m["reason"], "tst_enabled");
    assert_eq!(m["checkedModel"], "claude-opus-5[1m]");

    let placeholder_only = parse_shape(&cc_body(true)).unwrap();
    assert!(!placeholder_only.has_tool_search && placeholder_only.deferred_tools == 1);
    let m = tool_search_decision(&placeholder_only, "claude-opus-5[1m]", false);
    assert_eq!(m["enabled"], false, "只有占位、没有 ToolSearch 不算启用");
    assert_eq!(m["reason"], "not_registered");

    let full = parse_shape(&undeferred_body()).unwrap();
    assert_eq!(full.deferred_tools, 0);
    assert_eq!(full.mcp_tools, 2);
    let m = tool_search_decision(&full, "claude-opus-5[1m]", false);
    assert_eq!(m["enabled"], false);
    assert_eq!(m["reason"], "not_registered");
    assert_eq!(m["mcpToolCount"], 2);

    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["tools"] = json!([]);
    let none = parse_shape(&body.to_string().into_bytes()).unwrap();
    let m = tool_search_decision(&none, "claude-haiku-4-5-20251001", false);
    assert_eq!(m["enabled"], false);
    assert_eq!(m["reason"], "no_tools_in_request");
    assert_eq!(m["mcpToolCount"], 0);
}

/// 首轮模板里的三条延迟工具相关事件跟着正文走：工具池两样看 `defer_loading` 声明、搜索
/// 模式看 `ToolSearch`。正文没有 `defer_loading` 工具时，不发
/// `tengu_deferred_tools_pool_change`、`tengu_attachments` 里没有 `deferred_tools_delta`、
/// `tengu_tool_search_mode_decision` 报 `not_registered`——与 `cap/2.1.258-api/00002`
/// 一致；有延迟工具时三样照旧（`cap/2.1.258/00020`）。
#[test]
fn template_deferred_tool_events_follow_the_body() {
    let run = |body: Vec<u8>| -> (usize, Vec<Value>, Vec<Value>) {
        let t = Telemetry::default();
        t.ingest(call(body, "req_1", "end_turn"));
        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let by_name = |n: &str| -> Vec<Value> {
            p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
        };
        (
            by_name("tengu_deferred_tools_pool_change").len(),
            by_name("tengu_attachments"),
            by_name("tengu_tool_search_mode_decision"),
        )
    };
    let has_delta = |atts: &[Value]| {
        atts.iter().any(|a| {
            a["attachment_types"]
                .as_array()
                .is_some_and(|ts| ts.iter().any(|t| t == "deferred_tools_delta"))
        })
    };

    let (pool, atts, tst) = run(undeferred_body());
    assert_eq!(pool, 0, "没有延迟工具就没有工具池变更");
    assert!(!atts.is_empty() && !has_delta(&atts), "附件里不该有 deferred_tools_delta");
    assert!(!tst.is_empty());
    assert!(
        tst.iter().all(|m| m["reason"] == "not_registered" && m["enabled"] == false),
        "{tst:?}"
    );
    assert!(tst.iter().all(|m| m["mcpToolCount"] == 2), "{tst:?}");

    let (pool, atts, tst) = run(tool_search_body());
    assert!(pool >= 1, "延迟形态照发工具池变更");
    assert!(has_delta(&atts));
    assert!(tst.iter().all(|m| m["reason"] == "tst_enabled" && m["enabled"] == true), "{tst:?}");

    // 只有占位、没有 ToolSearch：工具池两样跟着延迟声明走仍发，搜索模式却不算启用。
    let (pool, atts, tst) = run(cc_body(true));
    assert!(pool >= 1 && has_delta(&atts));
    assert!(
        tst.iter().all(|m| m["reason"] == "not_registered" && m["enabled"] == false),
        "{tst:?}"
    );
}

/// 工具长度表的 hash 是长度表 JSON 的 sha256 前 12 位：无工具时是 sha256("{}") 的前缀
/// `44136fa355b3`，`cap/2.1.260-1` 那 16 个工具的表算出来是 `65b78f5c8f58`。
#[test]
fn tool_schema_hash_is_over_the_length_table() {
    assert_eq!(&sha256_hex(b"{}")[..12], "44136fa355b3");
    let table = r#"{"Agent":3078,"Artifact":37405,"AskUserQuestion":4926,"Bash":2352,"Edit":993,"ListAgents":1180,"Read":1617,"ReportFindings":2206,"ScheduleWakeup":4660,"SendFeedback":5537,"ShareOnboardingGuide":1326,"Skill":1832,"ToolSearch":1469,"Workflow":5384,"Write":668}"#;
    assert_eq!(&sha256_hex(table.as_bytes())[..12], "65b78f5c8f58");
    let s = parse_shape(&cc_body(true)).unwrap();
    assert_eq!(s.tools_hash, &sha256_hex(s.tool_lens.as_bytes())[..12]);
}

/// 续轮（tool_result）：工具事件补在这条之前、depth +1、首字在工具之后。
#[test]
fn tool_use_continuation_emits_tool_events_and_deepens_the_chain() {
    let t = Telemetry::default();
    // 第一条 tool_use 收尾、没有正文。
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.text_chars = 0;
    first.started_at = frozen_now() - Duration::from_secs(20);
    t.ingest(first);
    // 续轮：上一条 assistant 是 Bash 调用，末条是 tool_result。
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la","description":"list"}}]});
    body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"total 8\nfile"}]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.started_at = frozen_now() - Duration::from_secs(10);
    second.ua_out = "claude-cli/2.1.260 (external, cli)".into();
    t.ingest(second);

    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let by_name = |n: &str| -> Vec<Value> {
        p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
    };
    let granted = by_name("tengu_tool_use_granted_in_config");
    assert_eq!(granted.len(), 1, "auto 模式下每个工具一条");
    assert_eq!(granted[0]["toolName"], "Bash");
    assert_eq!(granted[0]["messageID"], "msg_011Cedjuoa4oBPzoB2CSUNEB");
    let allowed = by_name("tengu_tool_use_can_use_tool_allowed");
    assert_eq!(allowed[0]["requestId"], "req_1", "指上一条回复");
    assert_eq!(allowed[0]["queryDepth"], 0);
    let bash = by_name("tengu_bash_tool_command_executed");
    assert_eq!(bash[0]["tool_use_id"], "t1");
    assert_eq!(bash[0]["stdout_length"], "total 8\nfile\n".len(), "原始输出末尾的换行照算");
    assert_eq!(bash[0]["bash_argv0"], "ls");
    assert_eq!(bash[0]["bash_command_class"], "file_search");
    let ok = by_name("tengu_tool_use_success");
    assert_eq!(ok[0]["bashCommandLen"], "ls -la".len());
    assert_eq!(ok[0]["toolResultSizeBytes"], "total 8\nfile".len());
    assert!(
        by_name("tengu_feature_ok").iter().any(|m| m["feature_name"] == "shell_snapshot_create")
    );
    assert_eq!(by_name("tengu_query_before_attachments")[0]["toolResultsCount"], 1);
    // 归一化计数：续轮 = post + 5 + 第几次输入(1) + 续轮次数(1)；apiSystemMessageCount = 1 + 1。
    let pre = by_name("tengu_api_before_normalize");
    let post = by_name("tengu_api_after_normalize");
    assert_eq!(pre[1]["preNormalizedMessageCount"], 3 + 5 + 1 + 1);
    assert_eq!(post[1]["apiSystemMessageCount"], 1 + 1);
    let queries = by_name("tengu_api_query");
    assert_eq!(queries[0]["queryDepth"], 0);
    assert_eq!(queries[1]["queryDepth"], 1, "续轮 depth +1");
    assert_eq!(queries[1]["queryChainId"], queries[0]["queryChainId"], "同一轮同一条链");
    assert_eq!(queries[1]["previousRequestId"], "req_1");
    // 完成时刻相减：第一条 20s 前发、6.1s 跑完；第二条 10s 前发、6.1s 跑完 → 10.0s，
    // 而不是工具执行那 3.9s 的空档。
    let gap = by_name("tengu_api_success")[1]["timeSinceLastApiCallMs"].as_u64().unwrap();
    assert!((9_900..=10_100).contains(&gap), "{gap}");
    let first_text = by_name("tengu_turn_first_text");
    assert_eq!(first_text.len(), 1, "第一条没有正文，首字落在续轮");
    assert_eq!(first_text[0]["first_text_path"], "after_tool_use");
    assert_eq!(first_text[0]["requests_before_first_text"], 2);
    assert_eq!(first_text[0]["tool_calls_before_first_text"], 1);
    let successes = by_name("tengu_api_success");
    assert!(
        successes[0].get("thinkingContentLength").is_none(),
        "没有思考块就不带，哪怕整条没有正文"
    );
    assert!(successes[1].get("thinkingContentLength").is_none());
    assert_eq!(successes[1]["systemPromptSource"], "live_unrecorded", "2.1.258 以上才有");
    let ends = by_name("tengu_turn_end");
    assert_eq!(ends.len(), 1, "tool_use 那条不结束这一轮");
    assert!(ends[0]["duration_ms"].as_i64().unwrap() >= 10_000, "整轮时长，从提交算起");
    // active_time：cli 只算 end_turn 收尾的那条（6.118s），tool_use 那条不算；user 是
    // 敲那 11 个字（`hello there`）的时长估算 0.8 + 0.1×11 = 1.9s。
    let m = metrics_body(&p.metrics, "2.1.260", "team", None);
    let active = m["metrics"][3]["data_points"].as_array().unwrap();
    let by_type = |ty: &str| {
        active.iter().find(|d| d["attributes"]["type"] == ty).unwrap()["value"].as_f64().unwrap()
    };
    assert!((by_type("cli") - 6.118).abs() < 0.01, "{}", by_type("cli"));
    assert!((by_type("user") - 1.9).abs() < 0.01, "{}", by_type("user"));
    // durationMsIncludingRetries 比 durationMs 多 1–5ms，按 request-id 稳定。
    let s0 = &successes[0];
    let d = s0["durationMsIncludingRetries"].as_i64().unwrap() - s0["durationMs"].as_i64().unwrap();
    assert!((1..=5).contains(&d), "{d}");
    assert!(p.dd.iter().any(|d| d["message"] == "tengu_tool_use_success"));
    assert!(p.dd.iter().any(
            |d| d["message"] == "tengu_bash_tool_command_executed" && d["tool_use_id"] == "t1"
        ));
    assert_eq!(by_name("tengu_input_prompt").len(), 1, "续轮不是新输入");
}

/// 猜下一句：带工具、末条用户消息以 `[SUGGESTION MODE:` 开头。自己算一轮，接在主线程后。
#[test]
fn prompt_suggestion_is_its_own_auxiliary_turn() {
    let t = Telemetry::default();
    let mut main = call(cc_body(true), "req_main", "end_turn");
    main.started_at = frozen_now() - Duration::from_secs(20);
    t.ingest(main);
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["messages"][2] = json!({"role":"user","content":"[SUGGESTION MODE: Suggest what the user might naturally type next into Claude Code.]\n\nFIRST: ..."});
    body["system"][0]["text"] = json!(
        "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=b6499; cc_prev_req=req_main;"
    );
    let mut sugg = call(body.to_string().into_bytes(), "req_sugg", "end_turn");
    sugg.started_at = frozen_now() - Duration::from_secs(5);
    sugg.ua_out = "claude-cli/2.1.260 (external, cli)".into();
    t.ingest(sugg);
    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let metas: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_success")
        .map(|(_, e)| meta_of(e))
        .collect();
    let s = &metas[1];
    assert_eq!(s["querySource"], "prompt_suggestion");
    assert_eq!(s["queryDepth"], 2, "主线程 depth 0 + 2");
    assert_ne!(s["queryChainId"], metas[0]["queryChainId"], "自己一条链");
    assert_eq!(s["previousRequestId"], "req_main");
    assert!(s.get("is_default_model").is_none());
    assert_eq!(s["effort_level"], "high");
    assert_eq!(s["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622", "沿用主线程的 prompt");
    // 归一化计数用主线程最后一条的 depth（0），不是自己 +2 过的那个：
    // pre = 3 + 5 + 1 + 0，apiSystemMessageCount = 1 + 0。
    let pre: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_before_normalize")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(pre[1]["preNormalizedMessageCount"], 3 + 5 + 1);
    let post: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_after_normalize")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(post[1]["apiSystemMessageCount"], 1);
    let bp: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_cache_breakpoints")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(bp.last().unwrap()["skipCacheWrite"], true);
    let fork = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_fork_agent_query").unwrap();
    let fm = meta_of(&fork.1);
    assert_eq!(fm["forkLabel"], "prompt_suggestion");
    assert_eq!(fm["queryChainId"], metas[0]["queryChainId"], "fork 统计引用父链");
    assert_eq!(fm["inputTokens"], 2);
    let ends: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_turn_end")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(ends.len(), 2);
    assert_eq!(ends[1]["query_source_category"], "auxiliary");
    assert!(p.dd.iter().any(|d| d["feature_name"] == "prompt_suggestion_generate"));
    assert_eq!(
        p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_prompt_suggestion").count(),
        1,
        "只有首轮那条 suppressed"
    );
    assert_eq!(
        p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_tip_shown").count(),
        1,
        "猜下一句没有 tips"
    );
    assert_eq!(p.metrics.len(), 2);
    assert_eq!(p.metrics[1].category, "auxiliary");
}

/// 带缓存保活的猜下一句（真实 2.1.301 透传，`cap/auto-2.1.301-20261016-full/00056`）：客户端在上一条
/// 主线程提示末尾多打一个标记，messages 里两个断点，`tengu_api_cache_breakpoints` 报
/// `markerCount: 2` 并在其后带 `querySource`、`parentPromptEndMarked`（同目录 `00065` 的事件）。
/// 不带保活 beta 的同一份体照旧是 1、不带这两项。
#[test]
fn keepalive_suggestion_reports_the_parent_prompt_marker() {
    for keepalive in [true, false] {
        let t = Telemetry::default();
        let mut main = call(cc_body(true), "req_main", "end_turn");
        main.started_at = frozen_now() - Duration::from_secs(20);
        t.ingest(main);
        let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        let mark = json!({"type": "ephemeral", "ttl": "1h"});
        body["messages"][0] =
            json!({"role":"user","content":[{"type":"text","text":"first","cache_control":mark}]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"text","text":"[SUGGESTION MODE: Suggest what the user might naturally type next into Claude Code.]\n\nFIRST: ...","cache_control":mark}]});
        body["system"][0]["text"] = json!(
            "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=b6499; cc_prev_req=req_main;"
        );
        let mut sugg = call(body.to_string().into_bytes(), "req_sugg", "end_turn");
        sugg.started_at = frozen_now() - Duration::from_secs(5);
        // 2.1.291 起这条事件按 1% 抽（`sample_event`），测试用更早的版本号让它每次都发；这一支只看
        // 请求类别、保活 beta 与体里的标记数，不看版本。
        sugg.ua_out = "claude-cli/2.1.260 (external, cli)".into();
        sugg.agent.request_class = Some("auxiliary".into());
        let betas = sugg.betas.clone().unwrap();
        sugg.betas = Some(if keepalive {
            format!("{betas},{}", config::CC_BETA_CACHE_KEEPALIVE)
        } else {
            betas
        });
        t.ingest(sugg);
        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let bp = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_cache_breakpoints")
            .map(|(_, e)| meta_of(e))
            .next_back()
            .unwrap();
        assert_eq!(bp["skipCacheWrite"], true);
        assert_eq!(bp["forkPointPinned"], false);
        let keys: Vec<&str> = bp.as_object().unwrap().keys().map(String::as_str).collect();
        if keepalive {
            assert_eq!(bp["markerCount"], 2, "{bp}");
            assert_eq!(bp["querySource"], "prompt_suggestion");
            assert_eq!(bp["parentPromptEndMarked"], true);
            let at = keys.iter().position(|k| *k == "markerCount").unwrap();
            assert_eq!(&keys[at + 1..at + 3], ["querySource", "parentPromptEndMarked"], "{bp}");
        } else {
            assert_eq!(bp["markerCount"], 1, "{bp}");
            assert!(bp.get("querySource").is_none() && bp.get("parentPromptEndMarked").is_none());
        }
    }
}

/// 同上，回放真实抓包：`cap/auto-2.1.301-20261016-full` 的主线程 `00055` 与紧随其后的猜下一句
/// `00056`（auxiliary、带保活 beta、messages 两个断点），后者报 `markerCount: 2`。版本号同样压到
/// 2.1.291 以前，免得被抽样抽掉。
#[test]
fn keepalive_suggestion_capture_reports_two_markers() {
    let t = Telemetry::default();
    let mut session = String::new();
    for (i, f) in ["00055_", "00056_"].into_iter().enumerate() {
        let rel = format!("auto-2.1.301-20261016-full/{f}");
        let Some((body, betas)) = cap_request(&rel) else {
            eprintln!("skipped: cap/auto-2.1.301-20261016-full not present");
            return;
        };
        session = parse_shape(&body).unwrap().session_id.unwrap();
        let mut c = call(body, &format!("req_{f}"), "end_turn");
        c.betas = betas;
        c.ua_out = "claude-cli/2.1.260 (external, cli)".into();
        c.agent.request_class = cap_header(&rel, "x-claude-code-request-class");
        c.started_at = frozen_now() - Duration::from_secs(60 - 30 * i as u64);
        t.ingest(c);
    }
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).unwrap();
    let bp: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_cache_breakpoints")
        .map(|(_, e)| meta_of(e))
        .collect();
    let first = bp.first().unwrap();
    assert_eq!(first["markerCount"], 1, "主线程: {first}");
    assert!(first.get("querySource").is_none());
    let sugg = bp.last().unwrap();
    assert_eq!(sugg["skipCacheWrite"], true, "{sugg}");
    assert_eq!(sugg["markerCount"], 2, "{sugg}");
    assert_eq!(sugg["querySource"], "prompt_suggestion");
    assert_eq!(sugg["parentPromptEndMarked"], true);
}

/// 会话标题生成：无链、default 权限、无缓存、边界缺失、成功后一条 title_generated。
#[test]
fn session_title_generation_is_a_chainless_side_query() {
    let t = Telemetry::default();
    let mut main = call(cc_body(true), "req_main", "end_turn");
    main.started_at = frozen_now() - Duration::from_secs(20);
    t.ingest(main);
    let body = json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 32000,
        "stream": true,
        "thinking": {"type": "disabled"},
        "system": [
            {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.ced; cc_entrypoint=cli; cch=b1b2c;"},
            {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
            {"type":"text","text":"You are naming a coding session so the user can pick it out of a long list of sessions."}
        ],
        "messages": [{"role":"user","content":[{"type":"text","text":"<session>\nhi\n</session>\n\nWrite the title"}]}],
        "metadata": {"user_id": "{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"}
    });
    let mut title = call(body.to_string().into_bytes(), "req_title", "end_turn");
    title.started_at = frozen_now() - Duration::from_secs(5);
    title.resp_model = Some("claude-haiku-4-5-20251001".into());
    // haiku 那条不带 context-1m，也就没有 `[1m]` 展示名。
    title.betas =
        Some("claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14".into());
    title.cache_read_tokens = 0;
    title.cache_creation_tokens = 0;
    title.input_tokens = 896;
    t.ingest(title);
    // 侧查询会先扣住等下一条主线程；这里没有，走超时补发。
    t.gc(Instant::now() + Duration::from_secs(config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1));
    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let query = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_query")
        .map(|(_, e)| meta_of(e))
        .nth(1)
        .unwrap();
    assert_eq!(query["querySource"], "generate_session_title");
    assert!(query.get("queryChainId").is_none() && query.get("queryDepth").is_none());
    assert_eq!(query["permissionMode"], "default");
    assert_eq!(query["thinkingType"], "disabled");
    assert!(query.get("effortValue").is_none());
    assert!(query.get("previousRequestId").is_none());
    let success = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_success")
        .map(|(_, e)| meta_of(e))
        .nth(1)
        .unwrap();
    assert_eq!(success["model"], "claude-haiku-4-5-20251001");
    assert!(success.get("preNormalizedModel").is_none());
    assert_eq!(success["messageTokens"], 0);
    assert!(success.get("effort_level").is_none() && success.get("is_default_model").is_none());
    assert!(success.get("prompt_cache_ttl").is_none());
    assert_eq!(success["toolSchemasHash"], "44136fa355b3");
    assert!(success.get("timeSinceLastApiCallMs").is_some());
    assert!(success.get("systemPromptSource").is_none());
    let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
    assert_eq!(
        names.iter().filter(|n| **n == "tengu_sysprompt_missing_boundary_marker").count(),
        2
    );
    assert!(names.contains(&"tengu_session_title_generated"));
    assert_eq!(names.iter().filter(|n| **n == "tengu_turn_end").count(), 1, "标题那条不算一轮");
    assert_eq!(
        names.iter().filter(|n| **n == "tengu_tool_schema_sizes").count(),
        2,
        "工具集从 16 个变成 0 个"
    );
    let bp: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_cache_breakpoints")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(bp.last().unwrap()["cachingEnabled"], false);
    assert!(p.metrics[1].effort.is_none(), "没有 effort 属性");
    let m = metrics_body(&p.metrics, "2.1.260", "team", None);
    let cost = m["metrics"][1]["data_points"].as_array().unwrap();
    assert!(cost.iter().any(|d| d["attributes"]["query_source"] == "auxiliary"
        && d["attributes"].get("effort").is_none()));
    // 标题那串事件的顶层 model 与 Datadog 的 model 都是会话主模型，不是 haiku。
    let title_ev =
        p.events.iter().find(|(_, e)| ev_name(e) == "tengu_session_title_generated").unwrap();
    assert_eq!(title_ev.1["event_data"]["model"], "claude-opus-5[1m]");
    // Datadog：api_success 那条的 `model` 被 meta 里这条请求的模型盖掉（官方同样如此），
    // 其余条目（如 api_request）用会话主模型。
    let title_dd =
        p.dd.iter()
            .find(|d| d["message"] == "tengu_api_success" && d["request_id"] == "req_title")
            .unwrap();
    assert_eq!(title_dd["model"], "claude-haiku-4-5", "DD 去掉日期后缀");
    assert!(title_dd["ddtags"].as_str().unwrap().contains("model:claude-haiku-4-5,"));
    // event_logging 里标题的 api 事件顶层 model 是它自己的 haiku（跟 meta），其余事件是主模型。
    let title_query =
        p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_api_query").nth(1).unwrap();
    assert_eq!(title_query.1["event_data"]["model"], "claude-haiku-4-5-20251001");
    let title_success =
        p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_api_success").nth(1).unwrap();
    assert_eq!(title_success.1["event_data"]["model"], "claude-haiku-4-5-20251001");
    let schema_events: Vec<&(DateTime<Utc>, Value)> =
        p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_tool_schema_sizes").collect();
    assert_eq!(schema_events[1].1["event_data"]["model"], "claude-opus-5[1m]");
    let api_requests: Vec<&Value> =
        p.dd.iter().filter(|d| d["feature_name"] == "api_request").collect();
    assert_eq!(api_requests.len(), 2);
    assert_eq!(api_requests[1]["model"], "claude-opus-5", "标题那次的 feature_ok 用会话主模型");
    assert!(api_requests[1]["ddtags"].as_str().unwrap().contains("model:claude-opus-5,"));
    drop(st);

    // 第二轮主线程回来：工具集和第一轮一样，不再重发 schema（标题那条空表不算污染）。
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["system"][0]["text"] = json!(
        "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=f850a; cc_prev_req=req_main; cc_prompt_id=16d7a19d-7939-4638-9703-b31d2fc92661;"
    );
    let mut third = call(body.to_string().into_bytes(), "req_main2", "end_turn");
    third.started_at = frozen_now() - Duration::from_secs(2);
    t.ingest(third);
    let st = t.0.state.lock();
    let n = st.pending[&key()]
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_tool_schema_sizes")
        .count();
    assert_eq!(n, 2, "官方整个会话就两条");
}

/// 主线程请求末尾挂着一条 `role:"system"` 附件消息（`cap/2.1.260-2` 三条都是）：判新输入
/// 与续轮都要跳过它。
#[test]
fn trailing_system_message_does_not_hide_the_prompt_or_the_tool_result() {
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["messages"].as_array_mut().unwrap().push(json!({"role":"system","content":[{"type":"text","text":"<system-reminder>tokens</system-reminder>"}]}));
    let s = parse_shape(body.to_string().as_bytes()).unwrap();
    assert!(s.new_prompt, "尾部 system 不算末条");
    assert_eq!(s.prompt_len, "hello there".len());

    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"pwd"}}]});
    body["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"system","content":[{"type":"text","text":"reminder"}]}));
    let s = parse_shape(body.to_string().as_bytes()).unwrap();
    assert!(!s.new_prompt);
    assert_eq!(s.tool_uses.len(), 1, "隔着尾部 system 也认得出 assistant→tool_result");
    assert_eq!(s.tool_uses[0].name, "Bash");

    // 第二次输入带尾部 system：是新输入，chain 换、depth 归零、input_prompt 计到 2。
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "end_turn");
    first.started_at = frozen_now() - Duration::from_secs(30);
    t.ingest(first);
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["system"][0]["text"] = json!(
        "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=f850a; cc_prev_req=req_1; cc_prompt_id=16d7a19d-7939-4638-9703-b31d2fc92661;"
    );
    body["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"system","content":[{"type":"text","text":"reminder"}]}));
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.started_at = frozen_now() - Duration::from_secs(10);
    t.ingest(second);
    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let prompts: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_input_prompt")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[1]["prompt_index"], 2);
    assert_eq!(prompts[1]["is_wakeup"], false);
    assert_eq!(prompts[1]["cc_prompt_id"], "16d7a19d-7939-4638-9703-b31d2fc92661");
    let queries: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_query")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_ne!(queries[0]["queryChainId"], queries[1]["queryChainId"]);
    assert_eq!(queries[1]["queryDepth"], 0);
    assert_eq!(queries[1]["previousRequestId"], "req_1");
    assert!(
        p.events.iter().any(|(_, e)| ev_name(e) == "tengu_paste_text"),
        "第二次输入走 prompt_next 模板"
    );
}

/// 标题生成先扣住，等同会话下一条主线程请求带来新一轮 prompt id 再补发；等不到就超时按
/// 现有 id 发。
#[test]
fn side_queries_wait_for_the_new_prompt_id() {
    fn title_body() -> Vec<u8> {
        json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 32000,
                "stream": true,
                "thinking": {"type": "disabled"},
                "system": [
                    {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.ced; cc_entrypoint=cli; cch=b1b2c;"},
                    {"type":"text","text":"You are naming a coding session so the user can pick it out of a long list of sessions."}
                ],
                "messages": [{"role":"user","content":"<session>\nhi\n</session>\n\nWrite the title"}],
                "metadata": {"user_id": "{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"}
            })
            .to_string()
            .into_bytes()
    }
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "end_turn");
    first.started_at = frozen_now() - Duration::from_secs(30);
    t.ingest(first);
    let mut title = call(title_body(), "req_title", "end_turn");
    title.started_at = frozen_now() - Duration::from_secs(12);
    title.betas = Some("claude-code-20250219,oauth-2025-04-20".into());
    t.ingest(title);
    {
        let st = t.0.state.lock();
        assert_eq!(st.sessions[&key()].deferred.len(), 1, "扣住了");
        assert!(
            !st.pending[&key()]
                .events
                .iter()
                .any(|(_, e)| ev_name(e) == "tengu_session_title_generated")
        );
    }
    // 主线程新一轮到了：标题那条补发，prompt id 是新一轮的。
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["system"][0]["text"] = json!(
        "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=f850a; cc_prev_req=req_1; cc_prompt_id=16d7a19d-7939-4638-9703-b31d2fc92661;"
    );
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.started_at = frozen_now() - Duration::from_secs(11);
    t.ingest(second);
    {
        let st = t.0.state.lock();
        assert!(st.sessions[&key()].deferred.is_empty());
        let p = &st.pending[&key()];
        let title_success = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_success")
            .map(|(_, e)| meta_of(e))
            .find(|m| m["querySource"] == "generate_session_title")
            .expect("标题那条补发了");
        assert_eq!(title_success["cc_prompt_id"], "16d7a19d-7939-4638-9703-b31d2fc92661");
        // 完成时刻相减：标题（12s 前发、6.1s 跑完 → 5.9s 前完成）− 首条（30s 前发 →
        // 23.9s 前完成）= 18.0s；不是后来那条主线程。
        let title_gap = title_success["timeSinceLastApiCallMs"].as_u64().unwrap();
        assert!((17_900..=18_100).contains(&title_gap), "{title_gap}");
        // 主线程第二条（11s 前发 → 4.9s 前完成）距离**标题**的完成（5.9s 前）= 1.0s，
        // 标题虽然被扣住了，完成时刻照样算进去。
        let main_success = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_success")
            .map(|(_, e)| meta_of(e))
            .find(|m| m["requestId"] == "req_2")
            .unwrap();
        let main_gap = main_success["timeSinceLastApiCallMs"].as_u64().unwrap();
        assert!((900..=1_100).contains(&main_gap), "{main_gap}");
        assert!(p.events.iter().any(|(_, e)| ev_name(e) == "tengu_session_title_generated"));
    }
    // Datadog 那份按发生顺序发：标题（先完成）排在后到的主线程之前，尽管它是补发入队的。
    let dd_due =
        t.take_due(Instant::now() + Duration::from_secs(config::TELEMETRY_DATADOG_FLUSH_SECS + 1));
    assert_eq!(dd_due.len(), 1);
    let dd = &dd_due[0].dd;
    assert!(dd_due[0].events.is_empty(), "事件那路 30s 才到，这里只取 Datadog");
    let pos = |rid: &str| {
        dd.iter()
            .position(|d| d["message"] == "tengu_api_success" && d["request_id"] == rid)
            .unwrap()
    };
    assert!(pos("req_title") < pos("req_2"), "标题完成在前");
    // 超时路径：再来一条标题、没有主线程跟上，gc 到 10s 后按现有 id 发。
    let mut title2 = call(title_body(), "req_title2", "end_turn");
    title2.betas = Some("claude-code-20250219,oauth-2025-04-20".into());
    t.ingest(title2);
    let now = Instant::now();
    t.gc(now);
    assert_eq!(t.0.state.lock().sessions[&key()].deferred.len(), 1, "还没到 10s");
    t.gc(now + Duration::from_secs(config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1));
    let st = t.0.state.lock();
    assert!(st.sessions[&key()].deferred.is_empty());
    let n = st.pending[&key()]
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_session_title_generated")
        .count();
    assert_eq!(n, 2);
}

/// 拿真实抓包的请求体重新算长度表：Artifact 37405、toolsCharLength 74633、
/// toolSchemasHash 65b78f5c8f58、requestBodyChars 101459（`cap/2.1.260-2/00057` 的
/// `tengu_api_success`）。抓包目录不入库，本地没有就跳过。
#[test]
fn lengths_match_the_capture_when_it_is_present() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/cap/2.1.260-2/00057_174302.569.req.raw");
    let Ok(raw) = std::fs::read(path) else {
        eprintln!("skipped: {path} not present");
        return;
    };
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("http headers") + 4;
    let body = &raw[sep..];
    let s = parse_shape(body).expect("parses");
    let lens: Value = serde_json::from_str(&s.tool_lens).unwrap();
    assert_eq!(lens["Artifact"], 37405);
    assert_eq!(lens["Agent"], 3078);
    assert_eq!(lens["Bash"], 2352);
    assert_eq!(s.tools_chars, 74633);
    assert_eq!(s.tools_hash, "65b78f5c8f58");
    assert_eq!(js_len(std::str::from_utf8(body).unwrap()), 101459);
    assert_eq!(s.input_text_chars, 14203);
    assert_eq!(s.estimated_tokens, 4735);

    // 同一会话的其它三条：续轮（含 tool_use 块）、猜下一句、标题生成。
    let expect = [
        ("00061_174309.489.req.raw", 15163usize, 5055usize),
        ("00063_174319.456.req.raw", 17539, 5847),
        ("00058_174302.401.req.raw", 221, 55),
    ];
    for (file, chars, est) in expect {
        let path = format!("{}/cap/2.1.260-2/{file}", env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read(&path).unwrap();
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let s = parse_shape(&raw[sep..]).unwrap();
        assert_eq!(s.input_text_chars, chars, "{file}");
        assert_eq!(s.estimated_tokens, est, "{file}");
    }
}

/// 链的权威源是**请求自己带的那份**，不是回程时另算的会话状态。
///
/// 出站体的 billing header 里那个 `cc_prev_req` 与遥测的 `previousRequestId` 说的是
/// 同一件事——`cap/2.1.260-2` 三条续轮逐字相同，官方两处出自同一个 `requestJournal`。
/// luban 这边两者的更新路径不同（前者在 `ReqLog::drop` 里同步写，后者走 ingest 队列），
/// 让遥测复述请求自己说过的话，两份就不可能对不上。
/// `diagnostics.previous_message_id` 同理。
#[test]
fn the_chain_fields_come_from_the_request_itself() {
    const PREV_REQ: &str = "req_011CeiBW8Yx9A2uzWiCBsJsU";
    const PREV_MSG: &str = "msg_011CeiBWZwBH2rmLqr63MhHD";
    let t = Telemetry::default();
    // 会话首条：体里没有 `cc_prev_req`，链上没有上一条。
    t.ingest(call(cc_body(true), "req_1", "end_turn"));
    // 第二条：体里自报了一个**与会话状态不同**的 `cc_prev_req` 和 `previous_message_id`。
    // 会话状态此刻记着 `req_1` / `msg_011Ced…`，两者都该被体里那份顶掉。
    let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    v["system"][0]["text"] = json!(format!(
        "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=b6499; \
             cc_prompt_id=6c079143-0c53-4c48-817d-105460b3f622; cc_prev_req={PREV_REQ};"
    ));
    v["diagnostics"] = json!({ "previous_message_id": PREV_MSG });
    // 换个模型让缓存诊断那条事件发出来（它带 `previousMessageId`）。
    v["model"] = json!("claude-sonnet-5");
    let mut c = call(serde_json::to_vec(&v).unwrap(), "req_2", "end_turn");
    c.resp_model = Some("claude-sonnet-5".into());
    t.ingest(c);

    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    let meta = |name: &str, nth: usize| -> Value {
        let (_, e) = p.events.iter().filter(|(_, e)| ev_name(e) == name).nth(nth).unwrap();
        let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        serde_json::from_slice::<Value>(&raw).unwrap()
    };
    assert!(meta("tengu_api_query", 0).get("previousRequestId").is_none(), "首条没有上一条");
    assert_eq!(
        meta("tengu_api_query", 1)["previousRequestId"],
        PREV_REQ,
        "以体里自报的 cc_prev_req 为准，而不是会话状态里的 req_1"
    );
    assert_eq!(meta("tengu_api_success", 1)["previousRequestId"], PREV_REQ);
    assert_eq!(
        meta("tengu_prompt_cache_diagnosis_received", 0)["previousMessageId"],
        PREV_MSG,
        "以体里 diagnostics.previous_message_id 为准"
    );
}

/// 体里没有 `cc_prev_req` 时仍回落到会话状态：会话首轮、没有 billing header 的来访、
/// 关掉链注入的那些都走这条路。
#[test]
fn the_chain_falls_back_to_session_state_without_a_declared_prev() {
    let t = Telemetry::default();
    t.ingest(call(cc_body(true), "req_1", "end_turn"));
    t.ingest(call(cc_body(false), "req_2", "end_turn"));
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    let (_, e) = p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_api_query").nth(1).unwrap();
    let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
    let m: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(m["previousRequestId"], "req_1", "体里没自报就用会话状态那条链");
}

/// 处理顺序必须等于**入队顺序**（= `ReqLog::drop` 顺序 = 响应完成顺序）。
///
/// 此前每条调用各起一个 `spawn_blocking`，那是往一个几百线程的池子里扔任务，前后脚
/// 提交的两条谁先跑没有保证；而 `process` 里一多半状态是按顺序累积的。这里从**多个
/// 线程**并发 `record`，再断言那条 `previousRequestId` 链严丝合缝——乱序处理会让链
/// 在某一处指回更早的一条，或者干脆指向自己后面那条。
#[test]
fn the_ingest_queue_processes_calls_in_the_order_they_were_recorded() {
    const N: usize = 64;
    // 体要够大，`process` 里那趟 JSON 解析才占得住时间——生产快过消费，队列上才真的
    // 会同时压着好几条。体小的话每条 record 都在下一条入队前就处理完了，什么都测不出来。
    let big = {
        let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        v["system"][3]["text"] = json!("While auto mode is active: ".repeat(8_000));
        serde_json::to_vec(&v).unwrap()
    };
    // 先把 N 条都造好：构造的开销留在计时之外，`record` 那一串才是背靠背的。
    let calls: Vec<ApiCall> =
        (0..N).map(|i| call(big.clone(), &format!("req_{i:02}"), "end_turn")).collect();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .max_blocking_threads(8)
        .enable_all()
        .build()
        .unwrap();
    let t = Telemetry::default();
    rt.block_on(async {
        // 入队是同步的，故入队顺序就是这里的循环顺序。
        for c in calls {
            t.record(c);
        }
        // 等队列排空：最多等 30 秒，正常是几百毫秒。
        for _ in 0..3_000 {
            let done = {
                let st = t.0.state.lock();
                st.pending.get(&key()).is_some_and(|p| {
                    p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_api_query").count() == N
                })
            };
            if done {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("ingest queue did not drain");
    });

    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    let chain: Vec<Option<String>> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_query")
        .map(|(_, e)| {
            let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
            let raw =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
            let m: Value = serde_json::from_slice(&raw).unwrap();
            m.get("previousRequestId").and_then(|v| v.as_str()).map(str::to_string)
        })
        .collect();
    assert_eq!(chain.len(), N);
    assert_eq!(chain[0], None, "首条没有上一条");
    for (i, prev) in chain.iter().enumerate().skip(1) {
        assert_eq!(
            prev.as_deref(),
            Some(format!("req_{:02}", i - 1)).as_deref(),
            "第 {i} 条的 previousRequestId 应当是紧挨着的上一条"
        );
    }
}

/// `toolUseContentLengths`：回复里带工具调用时，`tengu_api_success` 多一个字段，值是
/// 「工具名 → 入参 JSON 字符数」那张表**序列化成的字符串**（与 `toolSchemaCharLengths`
/// 同一种写法），一个工具都没有时整个字段不出现。
#[test]
fn tool_use_content_lengths_ride_along_with_the_success() {
    let t = Telemetry::default();
    let mut c = call(cc_body(true), "req_tu", "tool_use");
    c.tool_use_lens = vec![("Bash".into(), 169), ("Read".into(), 42)];
    t.ingest(c);
    t.ingest(call(cc_body(true), "req_plain", "end_turn"));
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    let successes: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_success")
        .map(|(_, e)| {
            let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
            let raw =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
            serde_json::from_slice::<Value>(&raw).unwrap()
        })
        .collect();
    assert_eq!(successes[0]["toolUseContentLengths"], r#"{"Bash":169,"Read":42}"#);
    assert!(
        successes[1].get("toolUseContentLengths").is_none(),
        "回复里没有 tool_use 块的话整个字段不出现"
    );
}

/// 失败请求走 `tengu_api_error` 而不是 `tengu_api_success`：分类按状态码、文案取上游
/// 原文，收尾多一条 `tengu_feature_bad{api_request}` 与一条
/// `terminal_reason: "api_error"` 的 `tengu_turn_end`；成功那条独有的东西一样都没有。
#[test]
fn a_failed_call_reports_an_api_error_instead_of_a_success() {
    let t = Telemetry::default();
    t.ingest(failed(
        call(cc_body(true), "req_429", "end_turn"),
        Some(429),
        "rate_limit_error",
        "This request would exceed your organization's rate limit",
    ));
    give_up_retries(&t);
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).expect("入队");
    let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
    assert!(names.contains(&"tengu_api_error"));
    assert!(!names.contains(&"tengu_api_success"), "两条互斥");
    assert!(!names.contains(&"tengu_tool_schema_sizes"), "长度表只在成功那条里发");
    let meta_of = |name: &str| {
        let (_, e) = p.events.iter().find(|(_, e)| ev_name(e) == name).unwrap();
        let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        serde_json::from_slice::<Value>(&raw).unwrap()
    };
    let err = meta_of("tengu_api_error");
    assert_eq!(err["errorType"], "rate_limit");
    assert_eq!(err["status"], "429", "状态码是十进制串，不是数字");
    assert_eq!(err["error"], "This request would exceed your organization's rate limit");
    assert_eq!(err["model"], "claude-opus-5[1m]");
    assert_eq!(err["attempt"], 1);
    assert_eq!(err["provider"], "firstParty");
    assert_eq!(err["requestId"], "req_429");
    assert_eq!(err["clientRequestId"], "3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e");
    assert_eq!(err["querySource"], "repl_main_thread");
    assert_eq!(err["queryDepth"], 0);
    assert_eq!(err["requestBodyEncoding"], "identity");
    assert!(err.get("inputTokens").is_none(), "失败那条没有任何用量字段");
    assert!(err.get("costUSD").is_none());
    // 收尾：feature_bad + api_error 两条都进 Datadog，但没有 feature_ok{api_request}。
    let bad = meta_of("tengu_feature_bad");
    assert_eq!(bad["feature_name"], "api_request");
    assert_eq!(bad["error_code"], "api_request_retry_exhausted");
    assert!(p.dd.iter().any(|d| d["message"] == "tengu_api_error"));
    assert!(p.dd.iter().any(|d| d["message"] == "tengu_feature_bad"));
    // 这批里所有 feature 事件的名字：`api_request` 只能以 bad 的形式出现一次。
    let features = |name: &str| -> Vec<String> {
        p.events
            .iter()
            .filter(|(_, e)| ev_name(e) == name)
            .filter_map(|(_, e)| {
                let b64 = e["event_data"]["additional_metadata"].as_str()?;
                let raw =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).ok()?;
                let v: Value = serde_json::from_slice(&raw).ok()?;
                Some(v["feature_name"].as_str()?.to_string())
            })
            .collect()
    };
    assert!(
        !features("tengu_feature_ok").contains(&"api_request".to_string()),
        "官方的 feature_ok{{api_request}} 是成功回包才打的"
    );
    assert!(
        !features("tengu_feature_ok").contains(&"turn".to_string()),
        "失败的那轮没有 feature_ok{{turn}}，也没有 stop hook"
    );
    assert!(!features("tengu_feature_ok").contains(&"hook_stop_handler".to_string()));
    // 一轮到此为止，但走的是 api_error 那条。
    assert_eq!(p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_turn_end").count(), 1);
    let end = meta_of("tengu_turn_end");
    assert_eq!(end["terminal_reason"], "api_error");
    assert_eq!(end["error_kind"], "rate_limit");
    assert_eq!(end["is_error"], true);
    // 指标：会话照数、active_time 照记，但 cost/token 一个数据点都没有。
    let m = metrics_body(&p.metrics, "2.1.260", "team", None);
    let names: Vec<&str> =
        m["metrics"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"claude_code.session.count"));
    assert!(names.contains(&"claude_code.active_time.total"));
    assert!(!names.contains(&"claude_code.cost.usage"), "失败请求不进 cost");
    assert!(!names.contains(&"claude_code.token.usage"), "失败请求不进 token");
}

/// 连 `ReqLog` 都没建起来的那两条路（连接层失败、401 换号）由
/// [`Capture::record_failure`] 就地补一条失败遥测：响应侧的量一概没有，
/// 事件链与正常路径上那条 `tengu_api_error` 同一套。
#[test]
fn a_capture_can_report_a_failure_without_a_response() {
    let t = Telemetry::default();
    let cap = |sink: Telemetry| Capture {
        sink,
        account_uuid: Some("9922ef8e-7945-4f5a-ab4f-cf5f521531df".into()),
        org_type: Some("claude_team".into()),
        body: Bytes::from(cc_body(true)),
        betas: Some("claude-code-20250219,oauth-2025-04-20".into()),
        session_header: None,
        client_request_id: Some("3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e".into()),
        dispatch: None,
        agent: AgentHeaders::default(),
        organization_id: None,
        started_at: frozen_now() - Duration::from_secs(3),
        cache_keepalive_ok: false,
        client_tools: false,
        link_prompt_id: None,
        bare: false,
    };
    // 连接层就失败：没有状态码、没有上游 request-id。
    cap(t.clone()).record_failure(
        7,
        "claude-cli/2.1.260 (external, cli)".into(),
        1_200,
        None,
        CallFailure {
            status: None,
            error_type: None,
            message: "error sending request: connection refused".into(),
            in_band: false,
        },
    );
    give_up_retries(&t);
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).expect("入队");
    let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
    assert!(names.contains(&"tengu_api_error"));
    assert!(!names.contains(&"tengu_api_success"));
    let (_, e) = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_error").unwrap();
    let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
    let err: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(err["errorType"], "connection_error");
    assert!(err.get("status").is_none(), "连接层失败没有 HTTP 状态码");
    assert!(err.get("requestId").is_none(), "上游 request-id 都没拿到");
    assert_eq!(err["clientRequestId"], "3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e");
    assert_eq!(err["durationMs"], 1_200);
    // 请求侧的量照旧从出站体算（那份 body 确实拼好了、只是没发出去）。
    assert_eq!(err["messageCount"], 3);
    assert!(err.get("inputTokens").is_none(), "响应侧一个字段都没有");
    // 一轮到此为止，走 api_error 那条。
    let (_, end) = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_turn_end").unwrap();
    let b64 = end["event_data"]["additional_metadata"].as_str().unwrap();
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
    let end: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(end["terminal_reason"], "api_error");
    assert_eq!(end["error_kind"], "connection_error");
    // 请求没发出去，没有响应头可取组织 id；这个号之前也没有过带那个头的响应，
    // 于是 `auth` 里只有 account——官方对连不上的那类同样拿不到更多。
    assert!(e["event_data"]["auth"].get("organization_uuid").is_none());
}

/// 401 那条路（换号/回 403/原样透传三条出路都绕开 `ReqLog::drop`）报的是带状态码的
/// `auth_error`，且**响应头里的组织 id 要带进 `auth` 块**。
///
/// 组织 id 遥测这边按凭证缓存过一份，同一个号之前有过一条带这个头的响应就还补得上；
/// 但一个进程里头一条就是早退 401 的号没有那份缓存，`auth` 里就会整个少掉
/// `organization_uuid`——而订阅/团队账号官方每条事件都带。所以 401 那条路要把响应头
/// 上那份传下来（连接层失败那条没有响应，只能靠缓存）。
#[test]
fn a_401_early_return_still_reports_an_auth_error() {
    let t = Telemetry::default();
    Capture {
        sink: t.clone(),
        account_uuid: Some("9922ef8e-7945-4f5a-ab4f-cf5f521531df".into()),
        org_type: Some("claude_team".into()),
        body: Bytes::from(cc_body(true)),
        betas: None,
        session_header: None,
        client_request_id: None,
        dispatch: None,
        agent: AgentHeaders::default(),
        organization_id: Some("09520b85-f6b6-432f-97e2-6ecb804a083f".into()),
        started_at: frozen_now() - Duration::from_secs(2),
        cache_keepalive_ok: false,
        client_tools: false,
        link_prompt_id: None,
        bare: false,
    }
    .record_failure(
        7,
        "claude-cli/2.1.260 (external, cli)".into(),
        800,
        Some("req_401".into()),
        CallFailure {
            status: Some(401),
            error_type: Some("authentication_error".into()),
            message: "OAuth token has been revoked".into(),
            in_band: false,
        },
    );
    give_up_retries(&t);
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    let (_, e) = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_error").unwrap();
    let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
    let err: Value = serde_json::from_slice(&raw).unwrap();
    // `token_revoked` 在官方 `ate()` 里排在 401 → auth_error 之前。
    assert_eq!(err["errorType"], "token_revoked");
    assert_eq!(err["status"], "401");
    assert_eq!(err["requestId"], "req_401");
    assert_eq!(err["error"], "OAuth token has been revoked");
    // 这个号在这个进程里还没有过任何一条成功响应，`auth` 里那个组织 id 只能来自
    // 这一发 401 自己的响应头。
    assert_eq!(
        e["event_data"]["auth"]["organization_uuid"], "09520b85-f6b6-432f-97e2-6ecb804a083f",
        "早退 401 的事件也要带上组织 id"
    );
    assert_eq!(e["event_data"]["auth"]["account_uuid"], "9922ef8e-7945-4f5a-ab4f-cf5f521531df");
}

/// 流内错误（`event: error` 裹在 200 里）：SDK 那头没有状态码，官方报
/// `in_band_<上游 type>`，`status` 字段整个不出现。
#[test]
fn an_in_band_stream_error_is_reported_as_in_band() {
    let t = Telemetry::default();
    t.ingest(failed(
        call(cc_body(true), "req_ib", "end_turn"),
        None,
        "overloaded_error",
        "Overloaded",
    ));
    give_up_retries(&t);
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    let (_, e) = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_error").unwrap();
    let b64 = e["event_data"]["additional_metadata"].as_str().unwrap();
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
    let err: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(err["errorType"], "in_band_overloaded_error");
    assert!(err.get("status").is_none(), "流内错误没有 HTTP 状态码");
}

/// `errorType` 的分类顺序与官方 `ate()` 一致：429 在「4xx」之前、529/overloaded 在
/// 「5xx」之前，非重试类的 400 报 `client_error` 且 `error_code` 换成 non_retryable。
#[test]
fn error_kinds_follow_the_official_classifier() {
    let f = |status: Option<u16>, etype: &str, msg: &str| CallFailure {
        status,
        error_type: (!etype.is_empty()).then(|| etype.to_string()),
        message: msg.to_string(),
        in_band: false,
    };
    assert_eq!(error_kind(&f(Some(429), "rate_limit_error", "")), "rate_limit");
    assert_eq!(error_kind(&f(Some(529), "overloaded_error", "")), "server_overload");
    assert_eq!(error_kind(&f(Some(500), "overloaded_error", "")), "server_overload");
    assert_eq!(error_kind(&f(Some(500), "api_error", "")), "server_error");
    assert_eq!(error_kind(&f(Some(401), "authentication_error", "")), "auth_error");
    assert_eq!(error_kind(&f(Some(403), "permission_error", "")), "auth_error");
    assert_eq!(error_kind(&f(Some(413), "", "")), "request_too_large");
    assert_eq!(
        error_kind(&f(Some(400), "invalid_request_error", "prompt is too long: 1 tokens")),
        "prompt_too_long"
    );
    assert_eq!(
        error_kind(&f(Some(400), "invalid_request_error", "text content blocks must be non-empty")),
        "empty_text_block"
    );
    assert_eq!(
        error_kind(&f(Some(404), "not_found_error", "model: claude-nope")),
        "model_not_found"
    );
    assert_eq!(error_kind(&f(Some(400), "invalid_request_error", "whatever")), "client_error");
    assert_eq!(error_kind(&f(None, "", "connection reset")), "connection_error");
    assert_eq!(api_request_error_code(&f(Some(429), "", "")), "api_request_retry_exhausted");
    assert_eq!(api_request_error_code(&f(Some(500), "", "")), "api_request_retry_exhausted");
    assert_eq!(api_request_error_code(&f(None, "", "")), "api_request_retry_exhausted");
    assert_eq!(api_request_error_code(&f(Some(400), "", "")), "api_request_non_retryable");
}

/// `active_time.total{type:user}` 是**打字时长**，与 API 花了多久无关：估算
/// `0.8 + 0.1 × 字数`，再按「上一次提交到这次提交」的窗口截断。
///
/// 三份抓包（`cap/2.1.260-2`）的实测值：2 字 → 0.878、2 字 → 1.118、
/// 3 字 + 20 字 → 3.988。这里复现第三份那个两轮会话。
#[test]
fn user_active_time_tracks_the_typed_length() {
    let t = Telemetry::default();
    let prompt = |text: &str| {
        let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        v["messages"] = json!([{"role":"user","content":[{"type":"text","text":text}]}]);
        serde_json::to_vec(&v).unwrap()
    };
    // 第一轮：3 个字（`hii`），窗口是进程起点到提交那 3.085s → 0.8 + 0.3 = 1.1。
    let mut first = call(prompt("hii"), "req_1", "end_turn");
    first.started_at = frozen_now() - Duration::from_millis(9_800);
    first.total_ms = 3_779;
    t.ingest(first);
    // 第二轮：20 个字，两次提交相隔 6.06s，估算 0.8 + 2.0 = 2.8 < 6.06，取估算值。
    let mut second = call(prompt("hilele what can u do"), "req_2", "end_turn");
    second.started_at = frozen_now() - Duration::from_millis(9_800 - 6_060);
    second.total_ms = 6_338;
    t.ingest(second);

    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    let m = metrics_body(&p.metrics, "2.1.260", "team", None);
    let active = m["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "claude_code.active_time.total")
        .unwrap()["data_points"]
        .as_array()
        .unwrap()
        .clone();
    let by_type = |ty: &str| {
        active.iter().find(|d| d["attributes"]["type"] == ty).unwrap()["value"].as_f64().unwrap()
    };
    assert!((by_type("user") - 3.9).abs() < 0.05, "{}", by_type("user"));
    // cli 仍是两条 end_turn 的时长之和（抓包 10.053）。
    assert!((by_type("cli") - 10.117).abs() < 0.05, "{}", by_type("cli"));
}

/// 用户输入窗口比估算值短时按窗口截断：粘一大段再立刻回车，打字时长不可能超过
/// 「上次提交到这次提交」那段真实时间。
#[test]
fn user_active_time_is_capped_by_the_window() {
    let t = Telemetry::default();
    let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    v["messages"] = json!([{"role":"user","content":[{"type":"text","text":"x".repeat(500)}]}]);
    let mut c = call(serde_json::to_vec(&v).unwrap(), "req_paste", "end_turn");
    c.started_at = frozen_now() - Duration::from_secs(5);
    t.ingest(c);
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    // 估算 0.8 + 50 = 50.8s，但会话起点只比提交早 3.1s。
    assert!((p.metrics[0].user_secs - 3.085).abs() < 0.05, "{}", p.metrics[0].user_secs);
}

/// 工具入参那几样的解析规则（`cap/auto-2.1.285-20260930` 的官方取值），不靠抓包目录。
#[test]
fn tool_input_helpers_follow_the_official_values() {
    let p = bash_profile("git log --oneline | head -3");
    assert_eq!((p.command_type.as_str(), p.class, p.argv0.as_str()), ("git", "vcs", "git"));
    assert_eq!((p.last_argv0.as_str(), p.subcommand.as_deref()), ("head", Some("log")));
    assert!(p.has_pipe && !p.has_chain && p.simple_commands == 2);
    let p = bash_profile("cd /x && find . -name '*.py' | xargs wc -l");
    assert_eq!((p.argv0.as_str(), p.last_argv0.as_str(), p.simple_commands), ("find", "wc", 2));
    let p = bash_profile("python3 -m unittest -v test_calc 2>&1; echo \"---\"");
    assert_eq!(
        (p.command_type.as_str(), p.class, p.argv0.as_str()),
        ("python3", "lang_runtime", "python")
    );
    assert!(p.has_chain && !p.has_redirect, "2>&1 不算重定向");
    assert!(!bash_profile("ls; head -5 README* 2>/dev/null").has_redirect);
    assert!(bash_profile("echo >> README.md").has_redirect);
    assert!(bash_profile("[ -n \"$(tail -c1 x)\" ] && echo").has_subshell);
    // 只读：工作目录外的路径不算。
    let cwd = Some("/w");
    assert!(bash_read_only(&bash_profile("ls -la"), cwd));
    assert!(bash_read_only(&bash_profile("grep -rn x /w/src"), cwd));
    assert!(!bash_read_only(&bash_profile("cat ~/.codex/config.toml"), cwd));
    assert!(!bash_read_only(&bash_profile("python3 calc.py"), cwd));
    assert!(!bash_read_only(&bash_profile("rm test_calc.py"), cwd));
    // 结果末尾客户端追加的提醒不计，输出自己的末行换行照算。
    assert_eq!(
        strip_trailing_reminder("a\nb\n\n<system-reminder>\nx\n</system-reminder>"),
        "a\nb\n"
    );
    assert_eq!(exit_code_of("Exit code 2\nboom"), 2);
    assert_eq!(exit_code_of("boom"), 1);
    // 2.1.293 的 `zsh_nomatch_error`：结果里有 zsh「no matches found」才是 true（`cap/auto-2.1.293-
    // 20261008-full/00071` 那条），紧跟 `executor_shell_overridden`。
    let zsh = |head: &str| {
        let mut m = bash_executed_meta(
            &bash_profile("grep -rn x --include=*.py ."),
            44,
            None,
            false,
            false,
            "auto",
        );
        bash_meta_v293(&mut m, head);
        let keys: Vec<String> = m.as_object().unwrap().keys().cloned().collect();
        let at = keys.iter().position(|k| k == "zsh_nomatch_error").unwrap();
        assert_eq!(keys[at - 1], "executor_shell_overridden");
        m["zsh_nomatch_error"].clone()
    };
    assert_eq!(zsh("Exit code 1\n(eval):1: no matches found: --include=*.py"), json!(true));
    assert_eq!(zsh("Exit code 1\nboom"), json!(false));
    assert_eq!(zsh(""), json!(false));
    // 官方判据 `/^(?:\(eval\)|[A-Za-z_][\w-]*)(?::\d+)?: no matches found: /m` 认的几种：
    for head in [
        "Exit code 1\nzsh: no matches found: *.foo",
        "Exit code 1\nzsh:3: no matches found: *.foo",
        "Exit code 1\nscan_py: no matches found: *.py",
        "Exit code 1\nscan-py:12: no matches found: *.py",
        "Exit code 1\n(eval): no matches found: x",
        "Exit code 1\r(eval):1: no matches found: x",
    ] {
        assert_eq!(zsh(head), json!(true), "{head}");
    }
    // 命令自己的输出里出现这几个字不算：得是整行的 shell 诊断格式。
    for head in [
        "Exit code 1\nsearch: no matches found in index",
        "Exit code 1\n  (eval):1: no matches found: x",
        "Exit code 1\n(eval):1: no matches found",
        "Exit code 1\n1scan: no matches found: x",
        "Exit code 1\nscan py: no matches found: x",
        "Exit code 1\nzsh:x: no matches found: x",
    ] {
        assert_eq!(zsh(head), json!(false), "{head}");
    }
    assert_eq!(read_content_bytes("     1\tab\n     2\tcd"), 5);
    assert_eq!(file_ext("/a/b/calc.py"), "py");
    assert_eq!(file_ext("/a/.env"), "");
    // Edit 的入参补上默认的 replace_all。
    let edit = json!({"file_path": "/a", "old_string": "x", "new_string": "y"});
    assert_eq!(tool_input_len("Edit", &edit), edit.to_string().len() + 20);
    // 猜下一句被忽略：相似度是敲的字数 ÷ 建议的字数。
    let shown = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let submit: DateTime<Utc> = (shown + Duration::from_millis(4_066)).into();
    let m = ignored_suggestion_meta("req_1", shown, 28, submit, 79);
    assert_eq!(m["timeToIgnoreMs"], 4_066);
    assert!((m["similarity"].as_f64().unwrap() - 79.0 / 28.0).abs() < 1e-9);
}

#[test]
fn dd_model_drops_the_date_suffix() {
    assert_eq!(dd_model_short("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
    assert_eq!(dd_model_short("claude-opus-5"), "claude-opus-5");
    assert_eq!(dd_model_short("claude-fable-5-1"), "claude-fable-5-1");
}

#[test]
fn camel_to_snake_matches_datadog_spelling() {
    assert_eq!(camel_to_snake("costUSD"), "cost_u_s_d");
    assert_eq!(camel_to_snake("isTTY"), "is_t_t_y");
    assert_eq!(camel_to_snake("preNormalizedModel"), "pre_normalized_model");
    assert_eq!(camel_to_snake("stop_reason"), "stop_reason");
}

#[test]
fn session_betas_is_a_filtered_subset_in_header_order() {
    let header = "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,server-side-fallback-2026-07-01,fallback-credit-2026-06-01,afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
    assert_eq!(
        session_betas(header, "2.1.258"),
        "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07",
        "cap/2.1.258/00020 那条 opus 事件的 betas"
    );
    // haiku 主线程（`cap/2.1.285/00051` 的出站头）→ 同批事件里的会话级 betas（`00060`）。
    let haiku = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    assert_eq!(
        session_betas(haiku, "2.1.285"),
        "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05"
    );
    // opus-4-6（`cap/2.1.285/00083`）少 `mid-conversation-system`，会话级那份跟着少（`00090`）。
    let opus46 = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    assert_eq!(
        session_betas(opus46, "2.1.285"),
        "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05"
    );
    // 2.1.311 起会话级那份不带 `thinking-token-count`（出站头里照带）：`cap/auto-2.1.311-20261027-full/00070`
    // 的 opus 主线程头 → 同会话事件顶层的 `betas`。
    let opus311 = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,prompt-caching-scope-2026-01-05,per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    assert_eq!(
        session_betas(opus311, "2.1.311"),
        "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05"
    );
    assert!(session_betas(opus311, "2.1.310").contains("thinking-token-count-"));
}

#[test]
fn version_comes_from_the_outbound_ua() {
    assert_eq!(version_from_ua("claude-cli/2.1.260 (external, cli)"), "2.1.260");
    assert_eq!(version_from_ua("claude-code/2.1.258"), "2.1.258");
    assert_eq!(version_from_ua("curl/8.0"), config::CC_VERSION_BASE);
}

/// Datadog 那份日志与 event_logging 是**同一套阶段规则**：启动早期两项都没有，界面起来
/// 后只有 `renderer_mode`，用户提交后才多 `prompt_id`。
///
/// 依据 `cap/2.1.260-2/00017` 一批 80 条：59 条两项皆无（`tengu_started`/`tengu_init`/
/// `tengu_timer`/大部分 `tengu_feature_ok`）、7 条只有 `renderer_mode`、16 条两者都有。
/// 原先这两项无条件写，于是每个会话有近六十条启动日志带着「界面模式」和一个当时还不
/// 存在的 prompt id。
#[test]
fn datadog_entries_follow_the_same_metadata_stages() {
    let id = identity();
    let ctx = EventCtx { model: "claude-opus-5", betas: "a", prompt_id: "p", uptime_secs: 1.0 };
    let at = |stage| id.dd_entry_at(stage, "tengu_started", &ctx, "claude-opus-5", json!({}));

    let early = at(super::MetaStage::Startup);
    assert_eq!(early["renderer_mode"], Value::Null, "启动早期没有界面");
    assert_eq!(early["prompt_id"], Value::Null, "也还没有用户输入");
    assert_eq!(early["session_id"], id.session_id, "别的公共字段照旧");

    let mid = at(super::MetaStage::Renderer);
    assert_eq!(mid["renderer_mode"], "default");
    assert_eq!(mid["prompt_id"], Value::Null, "界面起来了但还没提交");

    let late = at(super::MetaStage::Prompt);
    assert_eq!(late["renderer_mode"], "default");
    assert_eq!(late["prompt_id"], "p");
}

/// `-p` 不起终端界面：两路都不写 `renderer_mode`，`cc_prompt_id` / `prompt_id` 照常分阶段
/// （`cap/auto-2.1.285-20260930` 九个 `-p` 会话：事件 1751 条、Datadog 920 条一条都没有它）。
#[test]
fn sdk_sessions_never_report_a_renderer_mode() {
    let id = Identity { sdk: true, ..identity() };
    let ctx = EventCtx { model: "claude-opus-5", betas: "a", prompt_id: "p", uptime_secs: 1.0 };
    for stage in [super::MetaStage::Renderer, super::MetaStage::Prompt] {
        let dd = id.dd_entry_at(stage, "tengu_started", &ctx, "claude-opus-5", json!({}));
        let b64 = id.metadata_b64_at(stage, "p", json!({}));
        let meta: Value = serde_json::from_slice(&STANDARD.decode(b64).unwrap()).unwrap();
        assert_eq!(dd["renderer_mode"], Value::Null, "{stage:?}");
        assert_eq!(meta["renderer_mode"], Value::Null, "{stage:?}");
    }
    let prompt = id.metadata_b64_at(super::MetaStage::Prompt, "p", json!({}));
    let meta: Value = serde_json::from_slice(&STANDARD.decode(prompt).unwrap()).unwrap();
    assert_eq!(meta["cc_prompt_id"], "p", "提交之后照样带 prompt id");
}

#[test]
fn metadata_is_standard_base64_with_padding() {
    let id = identity();
    let at = |stage| {
        let b64 = id.metadata_b64_at(stage, "p", json!({"feature_name":"notification_show"}));
        let decoded = STANDARD.decode(&b64).expect("standard base64");
        (b64, serde_json::from_slice::<Value>(&decoded).unwrap())
    };
    // 三个阶段各写几项，见 [`super::MetaStage`]。
    let (_, early) = at(super::MetaStage::Startup);
    assert_eq!(early["renderer_mode"], Value::Null, "启动早期没有界面");
    assert_eq!(early["cc_prompt_id"], Value::Null, "也还没有用户输入");
    assert_eq!(early["subscription_type"], "team");
    let (_, mid) = at(super::MetaStage::Renderer);
    assert_eq!(mid["renderer_mode"], "default");
    assert_eq!(mid["cc_prompt_id"], Value::Null, "界面起来了但还没提交");
    let (b64, v) = at(super::MetaStage::Prompt);
    assert_eq!(v["renderer_mode"], "default");
    assert_eq!(v["subscription_type"], "team");
    assert_eq!(v["cc_prompt_id"], "p");
    assert_eq!(v["feature_name"], "notification_show");
    // 抓包里的那串正好以 `=` 收尾；url-safe 无填充版本会解不出来。
    assert!(b64.ends_with('=') || b64.len().is_multiple_of(4));
}

#[test]
fn event_carries_org_and_account_in_auth() {
    let id = identity();
    let ctx =
        EventCtx { model: "claude-opus-5[1m]", betas: "a,b", prompt_id: "p", uptime_secs: 9.5 };
    let ev = id.event("tengu_api_query", Utc::now(), &ctx, json!({}));
    let d = &ev["event_data"];
    assert_eq!(d["event_name"], "tengu_api_query");
    assert_eq!(d["auth"]["organization_uuid"], "09520b85-f6b6-432f-97e2-6ecb804a083f");
    assert_eq!(d["auth"]["account_uuid"], "9922ef8e-7945-4f5a-ab4f-cf5f521531df");
    assert_eq!(d["env"]["build_time"], "2026-09-01T21:54:40Z", "2.1.258 的构建时间");
    assert_eq!(d["model"], "claude-opus-5[1m]");
    let proc: Value =
        serde_json::from_slice(&STANDARD.decode(d["process"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(proc["uptime"], 9.5);
}

#[test]
fn dd_entry_flattens_meta_and_tags_provider() {
    let id = identity();
    let ctx = EventCtx { model: "claude-opus-5", betas: "a", prompt_id: "p", uptime_secs: 1.0 };
    let e = id.dd_entry(
        "tengu_api_success",
        &ctx,
        "claude-opus-5",
        snake_flat(
            &json!({"requestId":"req_1","costUSD":0.5,"provider":"firstParty","cc_prompt_id":"x"}),
        ),
    );
    assert_eq!(e["request_id"], "req_1");
    assert_eq!(e["cost_u_s_d"], 0.5);
    assert!(e.get("cc_prompt_id").is_none(), "base 已有 prompt_id");
    assert_eq!(e["prompt_id"], "p");
    assert!(e["ddtags"].as_str().unwrap().contains("provider:firstParty,subscription_type:team"));
    assert_eq!(e["user_bucket"], 15);
}

#[test]
fn parse_shape_reads_the_cc_body() {
    let s = parse_shape(&cc_body(true)).unwrap();
    assert_eq!(s.model, "claude-opus-5");
    assert_eq!(s.messages_len, 3);
    assert!(s.new_prompt);
    assert_eq!(s.prompt_len, "hello there".len());
    assert_eq!(s.cc_prompt_id.as_deref(), Some("6c079143-0c53-4c48-817d-105460b3f622"));
    assert!(!s.is_subagent);
    assert_eq!(s.system_blocks, 4);
    assert_eq!(s.sys0_len, 132, "billing header 那块的长度与抓包一致");
    assert_eq!(s.tools_count, 2);
    assert_eq!(s.deferred_tools, 1);
    assert_eq!(s.mcp_tools, 0);
    assert_eq!(s.tools_hash.len(), 12);
    assert_eq!(s.thinking_type, "adaptive");
    assert_eq!(s.effort.as_deref(), Some("high"));
    assert_eq!(s.permission_mode, "auto");
    assert!(s.cache_ttl_1h);
    assert_eq!(s.device_id.as_deref().map(str::len), Some(64));
    assert_eq!(s.session_id.as_deref(), Some("4dc73702-d904-4887-809d-17b93cc5357c"));
    let cont = parse_shape(&cc_body(false)).unwrap();
    assert!(!cont.new_prompt, "tool_result 续轮不是新输入");
}

#[test]
fn full_chain_for_a_main_thread_call_and_continuation() {
    let t = Telemetry::default();
    t.ingest(call(cc_body(true), "req_1", "tool_use"));
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).expect("queued under the credential + session");
    let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
    assert!(names.contains(&"tengu_api_query"));
    assert!(names.contains(&"tengu_api_success"));
    assert!(names.contains(&"tengu_input_prompt"), "新输入才有");
    assert!(names.contains(&"tengu_turn_first_text"));
    assert!(!names.contains(&"tengu_turn_end"), "stop_reason=tool_use 这一轮还没结束");
    assert!(names.contains(&"tengu_tool_schema_sizes"), "首次见到这套工具");
    let success = p
        .events
        .iter()
        .find(|(_, e)| e["event_data"]["event_name"] == "tengu_api_success")
        .unwrap();
    let meta: Value = serde_json::from_slice(
        &STANDARD.decode(success.1["event_data"]["additional_metadata"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["requestId"], "req_1");
    assert_eq!(meta["model"], "claude-opus-5");
    assert_eq!(meta["preNormalizedModel"], "claude-opus-5[1m]", "context-1m beta 还原 [1m]");
    assert_eq!(meta["cachedInputTokens"], 26736);
    assert_eq!(meta["uncachedInputTokens"], 8729);
    assert_eq!(meta["messageTokens"], 0, "首条没有上一轮");
    assert!(meta.get("previousRequestId").is_none());
    assert_eq!(meta["gzipSkipReason"], "below_min_size", "请求体不到 4 KiB");
    assert_eq!(meta["cc_prompt_id"], "6c079143-0c53-4c48-817d-105460b3f622");
    // api_success 顶层 model 跟 meta 走，是规范名；api_query 则是展示名。
    assert_eq!(success.1["event_data"]["model"], "claude-opus-5");
    let query = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_query").unwrap();
    assert_eq!(query.1["event_data"]["model"], "claude-opus-5[1m]");
    // API 事件（query/success）报的是**这条请求的完整 beta 串**，含 `effort` 这种
    // 只在请求头上出现的项；界面事件（turn_end 等）报会话级那份，见
    // [`super::Identity::event`]。`cap/2.1.260-2/00016` 同一批里两者取值不同。
    const FULL: &str = "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
             interleaved-thinking-2025-05-14,effort-2025-11-24";
    // 会话级那份**始终带 `redact-thinking`**，哪怕请求头里没有——见 [`super::session_betas`]。
    const SESSION: &str = "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
             interleaved-thinking-2025-05-14,redact-thinking-2026-02-12";
    assert_eq!(success.1["event_data"]["betas"], FULL, "api_success 报完整串");
    assert_eq!(query.1["event_data"]["betas"], FULL, "api_query 也报完整串");
    let other = p
        .events
        .iter()
        .find(|(_, e)| !matches!(ev_name(e), "tengu_api_query" | "tengu_api_success"))
        .expect("这一批里总有别的事件");
    assert_eq!(
        other.1["event_data"]["betas"],
        SESSION,
        "非 API 事件（{}）报会话级那份",
        ev_name(&other.1)
    );
    let dd_success = p.dd.iter().find(|d| d["message"] == "tengu_api_success").unwrap();
    assert_eq!(dd_success["betas"], FULL, "Datadog 的 api_success 同样是完整串");
    assert_eq!(
        success.1["event_data"]["auth"]["organization_uuid"],
        "09520b85-f6b6-432f-97e2-6ecb804a083f"
    );
    assert_eq!(p.dd.iter().filter(|d| d["message"] == "tengu_api_success").count(), 1);
    drop(st);

    // 续轮：tool_result 收尾，end_turn → turn_end；previousRequestId 串上一条。
    // 第二条在第一条结束之后才发出（第一条 8s 前发、跑了 6.1s）。
    let mut second = call(cc_body(false), "req_2", "end_turn");
    second.started_at = frozen_now();
    t.ingest(second);
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    let success2 = p
        .events
        .iter()
        .filter(|(_, e)| e["event_data"]["event_name"] == "tengu_api_success")
        .nth(1)
        .unwrap();
    let meta2: Value = serde_json::from_slice(
        &STANDARD
            .decode(success2.1["event_data"]["additional_metadata"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(meta2["previousRequestId"], "req_1");
    // 上一条 input + cache_read + cache_creation + output：`cap/2.1.258` 那条正是 35498。
    assert_eq!(meta2["messageTokens"], 2 + 26736 + 8729 + 31);
    assert!(meta2.get("timeSinceLastApiCallMs").is_some());
    let names2: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
    assert_eq!(names2.iter().filter(|n| **n == "tengu_turn_end").count(), 1);
    assert_eq!(names2.iter().filter(|n| **n == "tengu_input_prompt").count(), 1, "续轮不算新输入");
    assert_eq!(
        names2.iter().filter(|n| **n == "tengu_tool_schema_sizes").count(),
        1,
        "工具没变不再报"
    );
    assert_eq!(p.metrics.len(), 2);
    assert!(p.metrics[0].new_session && !p.metrics[1].new_session);
}

#[test]
fn helper_calls_skip_turn_events_and_subagents_are_flagged() {
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["tools"] = json!([]);
    let t = Telemetry::default();
    t.ingest(call(body.to_string().into_bytes(), "req_h", "end_turn"));
    let st = t.0.state.lock();
    let names: Vec<String> =
        st.pending[&key()].events.iter().map(|(_, e)| ev_name(e).to_string()).collect();
    assert!(!names.iter().any(|n| n == "tengu_turn_end"));
    assert!(!names.iter().any(|n| n == "tengu_input_prompt"));
    assert!(names.iter().any(|n| n == "tengu_api_success"));
    drop(st);

    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body["system"][0]["text"] = json!(
        "x-anthropic-billing-header: cc_version=2.1.260.660; cc_entrypoint=cli; cch=590f3; cc_is_subagent=true;"
    );
    let s = parse_shape(body.to_string().as_bytes()).unwrap();
    assert!(s.is_subagent);
    assert!(s.cc_prompt_id.is_none());
}

#[test]
fn calls_without_identity_are_ignored() {
    let mut body: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    body.as_object_mut().unwrap().remove("metadata");
    let t = Telemetry::default();
    let mut c = call(body.to_string().into_bytes(), "req_x", "end_turn");
    c.session_header = Some("s".into());
    t.ingest(c);
    assert!(t.0.state.lock().pending.is_empty(), "没有 device_id 就不报");
}

#[test]
fn take_due_respects_the_three_cadences() {
    let t = Telemetry::default();
    t.ingest(call(cc_body(true), "req_1", "end_turn"));
    let now = Instant::now();
    assert!(t.take_due(now).is_empty(), "刚攒下，什么都还没到期");
    let later = now + Duration::from_secs(config::TELEMETRY_DATADOG_FLUSH_SECS + 1);
    let due = t.take_due(later);
    assert_eq!(due.len(), 1);
    assert!(due[0].events.is_empty() && !due[0].dd.is_empty() && due[0].metrics.is_none());
    let later = now + Duration::from_secs(config::TELEMETRY_EVENT_FLUSH_SECS + 1);
    let due = t.take_due(later);
    assert_eq!(due.len(), 1);
    assert!(!due[0].events.is_empty() && due[0].dd.is_empty());
    // 事件按时间排好序。
    let ts: Vec<&str> = due[0].events.iter().map(ev_ts).collect();
    let mut sorted = ts.clone();
    sorted.sort();
    assert_eq!(ts, sorted);
    assert_eq!(due[0].version, "2.1.258");
    let later = now + Duration::from_secs(config::TELEMETRY_METRICS_FLUSH_SECS + 1);
    let due = t.take_due(later);
    let m = due[0].metrics.as_ref().expect("metrics due");
    let names: Vec<&str> =
        m["metrics"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "claude_code.session.count",
            "claude_code.cost.usage",
            "claude_code.token.usage",
            "claude_code.active_time.total"
        ]
    );
    let cost = &m["metrics"][1]["data_points"][0];
    assert_eq!(cost["attributes"]["organization.id"], "09520b85-f6b6-432f-97e2-6ecb804a083f");
    assert_eq!(cost["attributes"]["model"], "claude-opus-5[1m]");
    assert_eq!(cost["attributes"]["query_source"], "main");
    assert_eq!(cost["value"], 0.18);
    assert_eq!(m["metrics"][2]["data_points"].as_array().unwrap().len(), 4);
    assert!(t.take_due(later + Duration::from_secs(1)).is_empty(), "取空后不再有东西");
    assert_eq!(t.org_uuid(7).as_deref(), Some("09520b85-f6b6-432f-97e2-6ecb804a083f"));
}

/// 读一份抓包请求：请求体与出站 `anthropic-beta`。文件不在（打包的源码里没有 `cap/`）返回 `None`。
fn cap_request(rel: &str) -> Option<(Vec<u8>, Option<String>)> {
    let dir = format!("{}/cap/{rel}", env!("CARGO_MANIFEST_DIR"));
    let (dir, prefix) = dir.rsplit_once('/').unwrap();
    let path = std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".req.raw"))
    })?;
    let raw = std::fs::read(path).ok()?;
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = std::str::from_utf8(&raw[..sep]).ok()?;
    let betas = head
        .lines()
        .find_map(|l| {
            l.strip_prefix("anthropic-beta: ").or_else(|| l.strip_prefix("Anthropic-Beta: "))
        })
        .map(str::to_string);
    Some((raw[sep..].to_vec(), betas))
}

/// `omittedBytes` 是 `system[1..]` 与 `tools` 两段 JSON 的长度之和（`cap/2.1.277/00031`
/// 那条续用线程报 85845）；末条 assistant 之后的条数即 `deltaMessageCount`。
#[test]
fn omitted_bytes_match_the_capture_when_it_is_present() {
    let Some((body, _)) = cap_request("2.1.277/00031_") else {
        eprintln!("skipped: cap/2.1.277 not present");
        return;
    };
    let s = parse_shape(&body).unwrap();
    assert_eq!(s.omitted_bytes, 85845);
    assert_eq!(s.after_last_assistant, 2);
}

/// 回放 `cap/2.1.280` 的七条主线程请求（每轮换一次模型、前三轮 auto 后四轮 default），
/// tether 判定、无状态标记、快照来源、每轮输入那几条新事件逐条对官方批次。
#[test]
fn main_thread_replay_matches_the_2_1_280_capture() {
    const FILES: [&str; 7] = ["00021_", "00029_", "00033_", "00038_", "00065_", "00068_", "00073_"];
    let mut reqs = Vec::new();
    for f in FILES {
        let Some(r) = cap_request(&format!("2.1.280/{f}")) else {
            eprintln!("skipped: cap/2.1.280 not present");
            return;
        };
        reqs.push(r);
    }
    let session = parse_shape(&reqs[0].0).unwrap().session_id.unwrap();
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(600);
    for (i, (body, betas)) in reqs.into_iter().enumerate() {
        let model = parse_shape(&body).unwrap().model;
        let mut c = call(body, &format!("req_{i}"), "end_turn");
        c.betas = betas;
        c.ua_out = "claude-cli/2.1.280 (external, cli)".into();
        c.resp_model = Some(model);
        c.started_at = base + Duration::from_secs(20 * i as u64);
        t.ingest(c);
    }
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).expect("queued");
    let metas = |n: &str| -> Vec<Value> {
        p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
    };
    let col = |v: &[Value], k: &str| -> Vec<Value> { v.iter().map(|m| m[k].clone()).collect() };
    let b = |xs: &[bool]| -> Vec<Value> { xs.iter().map(|x| json!(x)).collect() };

    let queries = metas("tengu_api_query");
    assert_eq!(
        col(&queries, "permissionMode"),
        ["auto", "auto", "auto", "default", "default", "default", "default"].map(|s| json!(s)),
        "auto 模式的提示在消息里"
    );

    let dec = metas("tengu_tether_decision");
    assert_eq!(dec.len(), 7);
    assert_eq!(col(&dec, "reason")[0], "first_request");
    assert!(col(&dec, "reason")[1..].iter().all(|r| r == "config_changed"));
    assert!(col(&dec, "decision").iter().all(|d| d == "create"));
    assert_eq!(col(&dec, "messageCount"), [2, 5, 8, 9, 16, 19, 22].map(|n| json!(n)));
    assert_eq!(col(&dec, "prevMessageCount"), [0, 2, 5, 8, 9, 16, 19].map(|n| json!(n)));
    assert_eq!(col(&dec, "changedModel"), b(&[false, true, true, true, true, true, true]));
    assert_eq!(col(&dec, "changedEffort"), b(&[false, true, false, true, true, true, false]));
    assert_eq!(
        col(&dec, "changedLatchedHeaders"),
        b(&[false, false, false, true, false, false, false])
    );
    assert!(col(&dec, "changedTools").iter().all(|v| v == false), "延迟工具不进长度表");
    assert_eq!(col(&dec, "modelHeldStateless"), b(&[true, true, false, false, true, true, false]));
    assert_eq!(
        col(&dec, "classifierHeldStateless"),
        b(&[true, true, true, false, false, false, false])
    );
    assert_eq!(metas("tengu_tether_echo_audit").len(), 6, "首条没有回声审计");
    let live = metas("tengu_tether_live_outcome");
    assert_eq!(
        col(&live, "sentThreadType"),
        ["none", "none", "none", "create", "none", "none", "create"].map(|s| json!(s))
    );
    assert!(col(&live, "omittedBytes").iter().all(|v| v == 0), "没有续用线程的");

    let ok = metas("tengu_api_success");
    assert_eq!(col(&ok, "systemPromptSource")[0], "live_recorded");
    assert!(col(&ok, "systemPromptSource")[1..].iter().all(|s| s == "from_snapshot"));
    let hashes = col(&ok, "snapshotHash");
    assert!(hashes.iter().all(|h| h == &hashes[0] && h.as_str().unwrap().len() == 12));
    assert!(col(&ok, "turn_origin").iter().all(|o| o == "human"));
    assert!(ok.iter().all(|m| m["firstContentMs"].as_i64().unwrap() >= 1800));
    assert!(ok.iter().all(|m| m["clientRequestId"] == "3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e"));

    let inputs = metas("tengu_input_prompt");
    assert_eq!(inputs.len(), 7);
    assert!(inputs[3].get("effort_level").is_none(), "haiku 没有 effort");
    assert_eq!(metas("tengu_sleepy_snowflake_applied").len(), 4, "每个模型一次");
    assert_eq!(metas("tengu_auto_mode_git_state_probe").len(), 3, "只在 auto 下");
    assert_eq!(metas("tengu_declared_tool_set_held").len(), 7);

    // Datadog 那份带 tether 三条，判定那条的 ddtags 按字母序多出 decision / reason。
    let dd_dec: Vec<&Value> =
        p.dd.iter().filter(|d| d["message"] == "tengu_tether_decision").collect();
    assert_eq!(dd_dec.len(), 7);
    assert!(
        dd_dec[1]["ddtags"]
            .as_str()
            .unwrap()
            .contains("client_type:cli,decision:create,entrypoint:cli")
    );
    assert!(
        dd_dec[1]["ddtags"]
            .as_str()
            .unwrap()
            .contains("platform:darwin,reason:config_changed,subscription_type")
    );
    assert_eq!(p.dd.iter().filter(|d| d["message"] == "tengu_tether_live_outcome").count(), 7);
}

/// 回放 `cap/2.1.285` 的 11 条主线程请求（一个会话里 `/model` 轮流切 11 个模型、前三轮
/// auto），逐条对官方批次：只有 fable-5-1 钉成无状态、auto 不再钉；`toolAdditionHistory`
/// 跟着 `mid-conversation-tool-changes` 走；tether 两条与 `api_success` 的新字段落在官方
/// 位置（键序取自 `00040` 批次）；默认 effort 按模型报。
#[test]
fn main_thread_replay_matches_the_2_1_285_capture() {
    const FILES: [&str; 11] = [
        "00030_", "00039_", "00045_", "00051_", "00055_", "00061_", "00067_", "00072_", "00077_",
        "00083_", "00088_",
    ];
    let mut reqs = Vec::new();
    for f in FILES {
        let Some(r) = cap_request(&format!("2.1.285/{f}")) else {
            eprintln!("skipped: cap/2.1.285 not present");
            return;
        };
        reqs.push(r);
    }
    let session = parse_shape(&reqs[0].0).unwrap().session_id.unwrap();
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(900);
    for (i, (body, betas)) in reqs.into_iter().enumerate() {
        let model = parse_shape(&body).unwrap().model;
        let mut c = call(body, &format!("req_{i}"), "end_turn");
        c.betas = betas;
        c.ua_out = "claude-cli/2.1.285 (external, cli)".into();
        c.resp_model = Some(model);
        c.started_at = base + Duration::from_secs(20 * i as u64);
        t.ingest(c);
    }
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).expect("queued");
    let metas = |n: &str| -> Vec<Value> {
        p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
    };
    let col = |v: &[Value], k: &str| -> Vec<Value> { v.iter().map(|m| m[k].clone()).collect() };
    let b = |xs: &[bool]| -> Vec<Value> { xs.iter().map(|x| json!(x)).collect() };
    let keys = |m: &Value| -> Vec<String> {
        m.as_object().unwrap().keys().filter(|k| !k.starts_with("cc_")).cloned().collect()
    };
    let subseq = |got: &[String], want: &[&str]| {
        let mut it = got.iter();
        want.iter().all(|w| it.any(|g| g == w))
    };

    let dec = metas("tengu_tether_decision");
    assert_eq!(dec.len(), 11);
    let only_fable =
        b(&[false, true, false, false, false, false, false, false, false, false, false]);
    assert_eq!(col(&dec, "modelHeldStateless"), only_fable);
    assert!(col(&dec, "classifierHeldStateless").iter().all(|v| v == false), "auto 不再钉");
    assert!(col(&dec, "creditRetryStateless").iter().all(|v| v == false));
    assert!(subseq(
        &keys(&dec[0]),
        &["classifierHeldStateless", "creditRetryStateless", "dropHeldStateless"]
    ));

    let live = metas("tengu_tether_live_outcome");
    assert_eq!(
        col(&live, "sentThreadType"),
        [
            "create", "none", "create", "create", "create", "create", "create", "create", "create",
            "create", "create"
        ]
        .map(|s| json!(s)),
        "按请求体：只有 fable-5-1 那条没写 thread"
    );
    assert_eq!(
        col(&live, "toolAdditionHistory"),
        b(&[true, true, false, false, false, true, true, true, false, false, false])
    );
    assert!(subseq(
        &keys(&live[0]),
        &[
            "classifierHeldStateless",
            "creditRetryStateless",
            "toolResultClearingHeldStateless",
            "serverToolHistory"
        ]
    ));

    let held = metas("tengu_declared_tool_set_held");
    assert_eq!(held.len(), 11);
    assert!(
        held.iter().all(|m| m["noDeferredChannel"] == false && m["unclassifiedDepartures"] == 0)
    );

    let ok = metas("tengu_api_success");
    assert_eq!(ok.len(), 11);
    // `cap/2.1.285/00040` 那条 opus-5-5 的键序（去掉公共头三项与本会话没有的字段）。
    let want = [
        "model",
        "dispatch",
        "betas",
        "echoWireToolInputs",
        "messageCount",
        "messageTokens",
        "inputTokens",
        "outputTokens",
        "cachedInputTokens",
        "uncachedInputTokens",
        "uncoveredTailReason",
        "uncoveredTailMessages",
        "resentTailTokensEst",
        "sentOnceTailTokensEst",
        "unexcusedTailTokensEst",
        "plainInputExcessTokens",
        "durationMs",
        "durationMsIncludingRetries",
        "attempt",
        "ttftMs",
        "firstContentMs",
        "queryOverheadMs",
        "buildAgeMins",
        "provider",
        "requestId",
        "stop_reason",
        "effort_level",
        "turn_origin",
        "is_default_model",
        "default_model",
        "is_default_effort",
        "default_effort_level",
        "costUSD",
        "querySource",
        "requestBodyChars",
        "requestPrepareMs",
        "gzipSkipReason",
        "fastMode",
    ];
    for m in &ok {
        assert!(subseq(&keys(m), &want) || m.get("effort_level").is_none(), "{m}");
        assert_eq!(m["dispatch"], "v2d");
        assert_eq!(m["uncoveredTailReason"], "none");
        assert_eq!(m["plainInputExcessTokens"], m["inputTokens"]);
        let q = m["queryOverheadMs"].as_i64().unwrap();
        assert!((6..=46).contains(&q), "{q}");
    }
    assert_eq!(
        col(&ok, "default_effort_level"),
        [
            json!("medium"),
            json!("high"),
            json!("medium"),
            Value::Null,
            json!("high"),
            json!("high"),
            json!("high"),
            json!("high"),
            json!("xhigh"),
            json!("high"),
            json!("high")
        ]
    );
    // Datadog 那份的 ddtags 多 `uncovered_tail_reason`，落在 subscription_type 与 user_bucket 之间。
    let dd_ok: Vec<&Value> = p.dd.iter().filter(|d| d["message"] == "tengu_api_success").collect();
    assert_eq!(dd_ok.len(), 11);
    let tags = dd_ok[0]["ddtags"].as_str().unwrap();
    assert!(tags.contains(",uncovered_tail_reason:none,user_bucket:15,"), "{tags}");
    let sub = tags.find("subscription_type:").unwrap();
    assert!(sub < tags.find("uncovered_tail_reason:").unwrap(), "{tags}");
}

/// 回放 `cap/2.1.285` 第二批（`00113` 起：主线程工具调用与 thread 续轮、claude-code-guide
/// 子代理首轮与续轮、WebFetch 之后的无工具页面处理、子代理摘要、task 通知那一轮）：
/// 分类、未覆盖尾段的四种取值、续轮的 `deltaMessageCount` 逐条对官方批次。
#[test]
fn tool_and_subagent_replay_matches_the_2_1_285_capture() {
    // 每条的停止原因与回复里的工具调用（工具名 → 入参字符数），取自各自的响应。
    type Step = (&'static str, &'static str, &'static [(&'static str, usize)]);
    const FILES: [Step; 18] = [
        ("00113", "tool_use", &[("Skill", 156)]),
        ("00115", "end_turn", &[]),
        ("00118", "tool_use", &[("Agent", 913)]),
        ("00120", "tool_use", &[("WebFetch", 207)]),
        ("00121", "end_turn", &[]),
        ("00125", "end_turn", &[]),
        ("00127", "tool_use", &[("WebFetch", 493)]),
        ("00131", "tool_use", &[("Read", 192), ("WebFetch", 144)]),
        ("00134", "end_turn", &[]),
        ("00135", "end_turn", &[]),
        ("00136", "tool_use", &[("WebFetch", 149)]),
        ("00140", "end_turn", &[]),
        ("00142", "tool_use", &[("WebFetch", 353)]),
        ("00144", "end_turn", &[]),
        ("00147", "end_turn", &[]),
        ("00148", "end_turn", &[]),
        ("00149", "end_turn", &[]),
        ("00158", "end_turn", &[]),
    ];
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(900);
    let mut session = String::new();
    for (i, (f, stop, tools)) in FILES.iter().enumerate() {
        let rel = format!("2.1.285/{f}_");
        let Some((body, betas)) = cap_request(&rel) else {
            eprintln!("skipped: cap/2.1.285 not present");
            return;
        };
        let shape = parse_shape(&body).unwrap();
        session = shape.session_id.clone().unwrap();
        let mut c = call(body, &format!("req_{f}"), stop);
        c.tool_use_lens = tools.iter().map(|(n, l)| (n.to_string(), *l)).collect();
        c.betas = betas;
        c.ua_out = "claude-cli/2.1.285 (external, cli)".into();
        c.resp_model = Some(shape.model.clone());
        c.message_id = Some(format!("msg_{f}"));
        c.started_at = base + Duration::from_secs(10 * i as u64);
        c.client_request_id = cap_header(&rel, "x-client-request-id");
        c.agent = AgentHeaders {
            agent_id: cap_header(&rel, "x-claude-code-agent-id"),
            agent_type: cap_header(&rel, "x-claude-code-agent-type"),
            request_class: cap_header(&rel, "x-claude-code-request-class"),
        };
        t.ingest(c);
    }
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).expect("queued");
    let ok: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_success")
        .map(|(_, e)| meta_of(e))
        .collect();
    let by_source =
        |src: &str| -> Vec<&Value> { ok.iter().filter(|m| m["querySource"] == src).collect() };
    let count = |n: &str| p.events.iter().filter(|(_, e)| ev_name(e) == n).count();
    let features = |f: &str| {
        p.events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_feature_ok" && meta_of(e)["feature_name"] == f)
            .count()
    };

    // thread 续轮的工具事件链配回来了：主线程 Skill、Agent 各一次，子代理九次（八次
    // WebFetch、一次 Read），条数与官方批次相同。
    let tool_uses: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_tool_use_success")
        .map(|(_, e)| meta_of(e))
        .collect();
    let names: Vec<&str> = tool_uses.iter().map(|m| m["toolName"].as_str().unwrap()).collect();
    assert_eq!(names.iter().filter(|n| **n == "WebFetch").count(), 8, "{names:?}");
    assert_eq!(names.iter().filter(|n| **n == "Read").count(), 1, "{names:?}");
    assert_eq!(names.iter().filter(|n| **n == "Skill" || **n == "Agent").count(), 2);
    assert!(tool_uses.iter().all(|m| m.get("toolResultTokensEst").is_some()));
    // 权限：非 auto 的 Skill 走弹框，其余十个走配置放行，键序官方那样。
    assert_eq!(count("tengu_tool_use_show_permission_request"), 1);
    assert_eq!(count("tengu_tool_use_granted_in_prompt_temporary"), 1);
    assert_eq!(count("tengu_tool_use_granted_in_config"), 10);
    assert_eq!(count("tengu_tool_use_can_use_tool_allowed"), 11);
    let granted = p
        .events
        .iter()
        .find(|(_, e)| ev_name(e) == "tengu_tool_use_granted_in_config")
        .map(|(_, e)| meta_of(e))
        .unwrap();
    let keys: Vec<&str> = granted.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys[keys.len() - 4..], ["messageID", "isMcp", "toolName", "sandboxEnabled"]);
    // 工具跑完那条 `tool_<名>`，Agent / Skill 各有前置的一条。
    assert_eq!(features("tool_web_fetch"), 8);
    assert_eq!(features("tool_read"), 1);
    assert_eq!(features("tool_agent"), 1);
    assert_eq!(features("tool_skill"), 1);
    assert_eq!(features("subagent_launch"), 1);
    assert_eq!(features("skill_invoke"), 1);
    // 那条 58.9KB 的 WebFetch 结果落了盘：原始大小报回去，另有一条落盘事件。
    assert_eq!(count("tengu_tool_result_persisted"), 1);
    assert!(tool_uses.iter().any(|m| m["toolResultWillPersist"] == true));
    // 子代理首步前主线程选定子代理、支线上解析模型；首两步的上下文记录与回放。
    assert_eq!(count("tengu_agent_tool_selected"), 1);
    assert_eq!(features("subagent_model_resolve"), 1);
    // 这段回放里 00113 就是会话首次输入：主线程模板那组一记一放、第二条（00115）再放一次，
    // 子代理首步一记一放、第二步再放一次。
    assert_eq!(count("tengu_reminder_fold_recorded"), 2);
    assert_eq!(count("tengu_reminder_fold_replayed"), 4);
    assert_eq!(count("tengu_wire_tool_input_echo_replayed"), 4);
    // 离开回顾（00158）是一轮辅助调用，收尾照官方。
    assert_eq!(by_source("away_summary").len(), 1);
    assert_eq!(features("away_summary_generate"), 1);
    // 子代理与它的摘要报 claude-code-guide 自带的 dontAsk。
    for m in by_source("agent_summary") {
        assert_eq!(m["permissionMode"], "dontAsk");
    }

    // WebFetch 页面处理：四条，挂在子代理那条支线上但没有链、没有 queryOverheadMs，
    // 不带断点 → `caching_off`，尾段就是整段消息。
    let wfa = by_source("web_fetch_apply");
    assert_eq!(wfa.len(), 4, "{:?}", ok.iter().map(|m| &m["querySource"]).collect::<Vec<_>>());
    for m in &wfa {
        assert!(m.get("queryChainId").is_none() && m.get("queryOverheadMs").is_none(), "{m}");
        assert_eq!(m["uncoveredTailReason"], "caching_off");
        assert_eq!(m["uncoveredTailMessages"], 1);
        assert!(m.get("plainInputExcessTokens").is_none());
        assert_eq!(m["permissionMode"], "default");
    }
    // 子代理摘要：fork 那种尾段。
    let summary = by_source("agent_summary");
    assert_eq!(summary.len(), 2);
    for m in &summary {
        assert_eq!(m["uncoveredTailReason"], "fork_tail_skip_cache_write");
        assert_eq!(m["uncoveredTailMessages"], 1);
        assert!(m.get("plainInputExcessTokens").is_none());
    }
    // thread 续轮：主线程四条（00115、00118、00121、00149）、子代理五条都是 `threaded_continue`；
    // 两条首轮（00113、00120）是 `none` 并报 `plainInputExcessTokens`。
    let main = by_source("repl_main_thread");
    let sub = by_source("agent:builtin:claude-code-guide");
    assert_eq!((main.len(), sub.len()), (5, 6));
    let reasons = |v: &[&Value]| -> Vec<Value> {
        v.iter().map(|m| m["uncoveredTailReason"].clone()).collect()
    };
    assert_eq!(
        reasons(&main),
        [
            "none",
            "threaded_continue",
            "threaded_continue",
            "threaded_continue",
            "threaded_continue"
        ]
        .map(|s| json!(s))
    );
    assert_eq!(reasons(&sub)[0], "none");
    assert!(reasons(&sub)[1..].iter().all(|r| r == "threaded_continue"));
    assert!(main[0].get("plainInputExcessTokens").is_some());
    assert!(main[1].get("plainInputExcessTokens").is_none());

    // tether：续轮的 deltaMessageCount 是增量体的条数——主线程 `[2, 2, 2, 1]`（最后那条 task
    // 通知轮只有一条消息），子代理恒 1（`00122`、`00137`、`00146`、`00151` 批次）。
    let live: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_tether_live_outcome")
        .map(|(_, e)| meta_of(e))
        .collect();
    let deltas = |cat: &str| -> Vec<Value> {
        live.iter()
            .filter(|m| m["sentThreadType"] == "continue" && m["sourceCategory"] == cat)
            .map(|m| m["deltaMessageCount"].clone())
            .collect()
    };
    assert_eq!(deltas("main"), [2, 2, 2, 1].map(|n| json!(n)));
    assert_eq!(deltas("subagent"), [1, 1, 1, 1, 1].map(|n| json!(n)));
    assert!(live.iter().any(|m| m["sentThreadType"] == "continue"));

    // 续轮前那条附件事件带 2.1.285 的两项。
    let att: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_attachments")
        .map(|(_, e)| meta_of(e))
        .collect();
    for m in &att {
        assert_eq!(m["attachment_token_estimates"].as_array().unwrap().last().unwrap(), "23");
        assert!(m["query_source"].is_string(), "{m}");
    }
}

/// **对照工具，默认不跑**：把 `LUBAN_REPLAY_CALLS`（抓包提出来的一串调用，见 scratchpad 里
/// 的 `calls.py`）按原时刻喂进遥测，把生成的 event_logging 事件与 Datadog 条目写到
/// `LUBAN_REPLAY_OUT`，给脚本逐段与官方批次比。
/// `cargo test dump_telemetry_replay -- --ignored --nocapture`。
#[test]
#[ignore]
fn dump_telemetry_replay() {
    let (Ok(inp), Ok(out)) =
        (std::env::var("LUBAN_REPLAY_CALLS"), std::env::var("LUBAN_REPLAY_OUT"))
    else {
        eprintln!("set LUBAN_REPLAY_CALLS / LUBAN_REPLAY_OUT");
        return;
    };
    let calls: Vec<Value> = serde_json::from_slice(&std::fs::read(inp).unwrap()).unwrap();
    // 抓包那个号 2.1.305 起开着会话活动心跳（`tengu_dapper_dawn` 是 force 的 true）。
    let eval = json!({"tengu_dapper_dawn": {"value": true, "source": "force"}});
    crate::proxy::dapper_dawn::note_eval(7, eval.as_object().unwrap());
    // 回放真实 CC 经网关的抓包（`*-api`）时置 1，按透传路径报（[`ApiCall::client_tools`]）。
    let client_tools = std::env::var("LUBAN_REPLAY_CLIENT_TOOLS").is_ok_and(|v| v == "1");
    let t = Telemetry::default();
    let s = |c: &Value, k: &str| c[k].as_str().map(str::to_string);
    for c in &calls {
        let mut a = call(c["body"].as_str().unwrap().as_bytes().to_vec(), "x", "end_turn");
        a.client_tools = client_tools;
        // 模拟 billing-only：体里的 `cc_prompt_id` 被挪到这里（会话链那份），见 [`ApiCall::link_prompt_id`]。
        a.link_prompt_id = s(c, "link_prompt_id");
        a.betas = s(c, "betas");
        a.ua_out = s(c, "ua").unwrap_or_default();
        a.session_header = s(c, "session_header");
        a.organization_id = s(c, "organization_id");
        a.started_at =
            SystemTime::UNIX_EPOCH + Duration::from_millis(c["started_ms"].as_u64().unwrap());
        a.ttft_ms = c["ttft_ms"].as_u64();
        a.total_ms = c["total_ms"].as_u64().unwrap_or(1000);
        a.request_id = s(c, "request_id");
        a.client_request_id = s(c, "client_request_id");
        a.agent = AgentHeaders {
            agent_id: s(c, "agent_id"),
            agent_type: s(c, "agent_type"),
            request_class: s(c, "request_class"),
        };
        a.message_id = s(c, "message_id");
        a.stop_reason = s(c, "stop_reason");
        a.resp_model = s(c, "resp_model");
        a.input_tokens = c["input_tokens"].as_i64().unwrap_or(0);
        a.output_tokens = c["output_tokens"].as_i64().unwrap_or(0);
        a.cache_read_tokens = c["cache_read"].as_i64().unwrap_or(0);
        a.cache_creation_tokens = c["cache_creation"].as_i64().unwrap_or(0);
        a.cache_creation_5m_tokens = c["cache_5m"].as_i64();
        a.cache_creation_1h_tokens = c["cache_1h"].as_i64();
        a.text_chars = c["text_chars"].as_u64().unwrap_or(0) as usize;
        a.reply_input_chars = a.text_chars;
        a.thinking_chars = c["thinking_chars"].as_u64().unwrap_or(0) as usize;
        a.saw_thinking = c["saw_thinking"].as_bool().unwrap_or(false);
        a.stream_events = c["stream_events"].as_u64().map(|n| n as u32);
        a.tool_use_lens = c["tool_use_lens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| (x[0].as_str().unwrap().to_string(), x[1].as_u64().unwrap() as usize))
            .collect();
        a.cost_usd = c["cost_usd"].as_f64();
        a.aborted = c["aborted"].as_bool().unwrap_or(false);
        a.tool_calls = c["tool_calls"]
            .as_array()
            .map(|v| {
                v.iter()
                    .map(|x| ToolCall {
                        id: x[0].as_str().unwrap_or("").to_string(),
                        name: x[1].as_str().unwrap_or("").to_string(),
                        input: x[2].clone(),
                        verdict: x[3].as_str().map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default();
        t.ingest(a);
    }
    let st = t.0.state.lock();
    let mut events = Vec::new();
    let mut dd = Vec::new();
    for p in st.pending.values() {
        for (ts, e) in &p.events {
            let d = &e["event_data"];
            let meta = d
                .get("additional_metadata")
                .and_then(|m| m.as_str())
                .and_then(|m| STANDARD.decode(m).ok())
                .and_then(|m| serde_json::from_slice::<Value>(&m).ok())
                .unwrap_or(Value::Null);
            events.push(json!({
                    "ts": ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    "type": e["event_type"],
                    "name": d.get("event_name").or_else(|| d.get("experiment_id")),
                    "meta": meta,
                    "top": {"model": d.get("model"), "agent_id": d.get("agent_id"), "betas": d.get("betas")},
                    "sid": d.get("session_id"),
                    "parent": d.get("parent_session_id"),
                }));
        }
        dd.extend(p.dd.iter().cloned());
    }
    std::fs::write(out, serde_json::to_vec(&json!({"events": events, "dd": dd})).unwrap()).unwrap();
}

/// 出站版本决定用哪份模板：2.1.285 起是那份（启动段有 `managed_config_ready`、首次输入有
/// 上下文宣告、收尾没有 `tip_shown`），之前的仍是 2.1.260 那份。Datadog 里
/// `is_claude_ai_auth` 紧跟 `betas`。
#[test]
fn the_template_follows_the_outbound_version() {
    let names_for = |ua: &str| {
        let t = Telemetry::default();
        let mut c = call(cc_body(true), "req_tpl", "end_turn");
        c.ua_out = ua.into();
        t.ingest(c);
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).expect("queued");
        let names: Vec<String> = p.events.iter().map(|(_, e)| ev_name(e).to_string()).collect();
        let dd_keys: Vec<String> = p.dd[0].as_object().unwrap().keys().cloned().collect();
        (names, dd_keys)
    };
    let (new, dd) = names_for("claude-cli/2.1.285 (external, cli)");
    for n in ["tengu_managed_config_ready", "tengu_context_announcement", "tengu_prompt_suggestion"]
    {
        assert!(new.iter().any(|x| x == n), "{n}");
    }
    assert!(!new.iter().any(|x| x == "tengu_tip_shown"));
    let at = |k: &str| dd.iter().position(|x| x == k).unwrap();
    assert_eq!(at("is_claude_ai_auth"), at("betas") + 1);
    let (old, _) = names_for("claude-cli/2.1.280 (external, cli)");
    assert!(old.iter().any(|x| x == "tengu_tip_shown"), "2.1.280 仍是 2.1.260 那份模板");
    assert!(!old.iter().any(|x| x == "tengu_managed_config_ready"));
}

/// 同一配置下消息只增不减：`continue/append`；实际走没走线程看请求体。
#[test]
fn same_config_continuation_continues_the_tether_thread() {
    // 去掉 auto 模式的提示，免得分类器把它钉成无状态。
    let plain = |last_user_text| {
        String::from_utf8(cc_body(last_user_text))
            .unwrap()
            .replace("While auto mode is active: rules", "rules")
            .into_bytes()
    };
    let t = Telemetry::default();
    let mut first = call(plain(true), "req_1", "tool_use");
    first.ua_out = "claude-cli/2.1.280 (external, cli)".into();
    t.ingest(first);
    let mut body: Value = serde_json::from_slice(&plain(false)).unwrap();
    body["messages"].as_array_mut().unwrap().splice(
        0..0,
        [json!({"role":"user","content":"earlier"}), json!({"role":"assistant","content":"ok"})],
    );
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.ua_out = "claude-cli/2.1.280 (external, cli)".into();
    let expect_omitted = parse_shape(&second.body).unwrap().omitted_bytes;
    t.ingest(second);
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    let metas = |n: &str| -> Vec<Value> {
        p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
    };
    let dec = metas("tengu_tether_decision");
    assert_eq!(
        (dec[1]["decision"].as_str(), dec[1]["reason"].as_str()),
        (Some("continue"), Some("append"))
    );
    assert_eq!(dec[1]["turnsInThread"], 2);
    assert_eq!(dec[1]["deltaMessageCount"], 2);
    // 判定是接着用，但请求体没写 `thread`（模拟路径就是这样）：实际发出的是无线程的全量，
    // 照请求体报 none、什么都没省。
    let live = metas("tengu_tether_live_outcome");
    assert_eq!(live[1]["engineDecision"], "continue");
    assert_eq!(live[1]["sentThreadType"], "none");
    assert_eq!(live[1]["planReason"], "none");
    assert_eq!(live[1]["omittedBytes"], 0);
    assert!(expect_omitted > 0);
    assert_eq!(live[1]["deltaMessageCount"], 1);
    assert_eq!(metas("tengu_tether_echo_audit")[0]["turnsReceived"], 2);
}

/// 旧版本（出站 UA 2.1.258）不带 2.1.270 起才有的字段与事件。
#[test]
fn older_versions_keep_the_old_layout() {
    let t = Telemetry::default();
    t.ingest(call(cc_body(true), "req_1", "end_turn"));
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).unwrap();
    let names: Vec<&str> = p.events.iter().map(|(_, e)| ev_name(e)).collect();
    for n in
        ["tengu_tether_decision", "tengu_declared_tool_set_held", "tengu_sleepy_snowflake_applied"]
    {
        assert!(!names.contains(&n), "{n}");
    }
    let ok = p.events.iter().find(|(_, e)| ev_name(e) == "tengu_api_success").unwrap();
    let m = meta_of(&ok.1);
    assert!(m.get("firstContentMs").is_none() && m.get("snapshotHash").is_none());
    assert!(m.get("turn_origin").is_none());
}

/// 请求头里的一项（大小写不敏感）。
fn cap_header(rel: &str, name: &str) -> Option<String> {
    let dir = format!("{}/cap/{rel}", env!("CARGO_MANIFEST_DIR"));
    let (dir, prefix) = dir.rsplit_once('/').unwrap();
    let path = std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".req.raw"))
    })?;
    let raw = std::fs::read(path).ok()?;
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    std::str::from_utf8(&raw[..sep]).ok()?.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
    })
}

/// 回放 `cap/2.1.280` 第二个会话：主线程拉起一个内置 Explore 子代理（6 条请求 + 1 条摘要
/// 请求），按官方的完成先后喂进来，子代理那条支线的事件逐项对官方批次。
#[test]
fn subagent_replay_matches_the_2_1_280_capture() {
    // (文件, 回复的 stop_reason, 回复里调了哪些工具)
    const CALLS: [(&str, &str, &[&str]); 11] = [
        ("00161_", "end_turn", &[]),
        ("00164_", "tool_use", &["Agent"]),
        ("00166_", "end_turn", &[]),
        ("00165_", "tool_use", &["Bash"]),
        ("00168_", "end_turn", &[]),
        ("00170_", "tool_use", &["Bash"]),
        ("00171_", "tool_use", &["Bash"]),
        ("00173_", "tool_use", &["Bash"]),
        ("00174_", "end_turn", &[]),
        ("00175_", "tool_use", &["SubagentHandback"]),
        ("00178_", "end_turn", &[]),
    ];
    const AGENT: &str = "a51764a248f499f13";
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(600);
    let mut session = String::new();
    for (i, (f, stop, tools)) in CALLS.iter().enumerate() {
        let rel = format!("2.1.280/{f}");
        let Some((body, betas)) = cap_request(&rel) else {
            eprintln!("skipped: cap/2.1.280 not present");
            return;
        };
        let shape = parse_shape(&body).unwrap();
        session = shape.session_id.clone().unwrap();
        let mut c = call(body, &format!("req_{f}"), stop);
        c.betas = betas;
        c.ua_out = "claude-cli/2.1.280 (external, cli)".into();
        c.resp_model = Some(shape.model.clone());
        c.message_id = Some(format!("msg_{f}"));
        c.started_at = base + Duration::from_secs(10 * i as u64);
        c.tool_use_lens = tools.iter().map(|n| (n.to_string(), 100)).collect();
        c.client_request_id = cap_header(&rel, "x-client-request-id");
        c.agent = AgentHeaders {
            agent_id: cap_header(&rel, "x-claude-code-agent-id"),
            agent_type: cap_header(&rel, "x-claude-code-agent-type"),
            request_class: cap_header(&rel, "x-claude-code-request-class"),
        };
        t.ingest(c);
    }
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).expect("queued");
    let of_agent = |e: &Value| e["event_data"]["agent_id"] == AGENT;
    let sub = |n: &str| -> Vec<Value> {
        p.events
            .iter()
            .filter(|(_, e)| ev_name(e) == n && of_agent(e))
            .map(|(_, e)| meta_of(e))
            .collect()
    };
    let col = |v: &[Value], k: &str| -> Vec<Value> { v.iter().map(|m| m[k].clone()).collect() };

    // 只有支线上的事件带 agent_id；主线程那几条一律不带。
    for (_, e) in &p.events {
        let d = &e["event_data"];
        if d.get("agent_id").is_some() {
            assert_eq!(d["agent_type"], "subagent");
        }
    }
    let main_queries: Vec<&Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_query" && !of_agent(e))
        .map(|(_, e)| e)
        .collect();
    assert_eq!(main_queries.len(), 4, "主线程三条 + 猜下一句");
    assert!(main_queries.iter().all(|e| e["event_data"].get("agent_id").is_none()));

    let q = sub("tengu_api_query");
    assert_eq!(
        col(&q, "querySource"),
        [
            "agent:builtin:Explore",
            "agent:builtin:Explore",
            "agent:builtin:Explore",
            "agent:builtin:Explore",
            "agent_summary",
            "agent:builtin:Explore",
            "agent:builtin:Explore"
        ]
        .map(|s| json!(s))
    );
    assert_eq!(col(&q, "queryDepth"), [2, 3, 4, 5, 3, 6, 7].map(|n| json!(n)));
    let chains = col(&q, "queryChainId");
    assert!([0, 1, 2, 3, 5, 6].iter().all(|&i| chains[i] == chains[0]), "整条支线一个链");
    assert_ne!(chains[4], chains[0], "摘要请求另起链");

    let ok = sub("tengu_api_success");
    assert_eq!(ok[0]["invokingRequestId"], "req_00164_", "拉起它的是调了 Agent 的那条");
    assert_eq!(ok[0]["invocationKind"], "spawn");
    assert!(ok[1].get("invokingRequestId").is_none());
    assert!(ok.iter().all(|m| m.get("is_default_model").is_none()));
    assert_eq!(ok[0]["systemPromptSource"], "live_recorded");
    assert!(ok[1..].iter().all(|m| m["systemPromptSource"] == "from_snapshot"));
    let main_ok: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_success" && !of_agent(e))
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_ne!(ok[0]["snapshotHash"], main_ok[0]["snapshotHash"], "支线自己一份快照");
    let explore: Vec<&Value> =
        ok.iter().filter(|m| m["querySource"] == "agent:builtin:Explore").collect();
    assert!(explore.iter().all(|m| m["attributionAgent"] == "Explore"));
    let summary = ok.iter().find(|m| m["querySource"] == "agent_summary").unwrap();
    assert!(summary.get("attributionAgent").is_none());
    assert_eq!(ok[0]["prompt_cache_ttl_reason"], "default");
    assert_eq!(ok[1]["messageTokens"], ok[0]["inputTokens"].as_i64().unwrap() + 26736 + 8729 + 31);

    // 规范化前后与系统提示词：没有分界标记，块数恒报 6。
    let pre = col(&sub("tengu_api_before_normalize"), "preNormalizedMessageCount");
    let post = sub("tengu_api_after_normalize");
    assert_eq!(pre[..6], [9, 15, 19, 23, 25, 27].map(|n| json!(n)));
    assert_eq!(col(&post, "postNormalizedMessageCount")[..5], [2, 5, 8, 11, 11].map(|n| json!(n)));
    assert_eq!(col(&post, "apiSystemMessageCount")[..5], [1, 2, 3, 4, 4].map(|n| json!(n)));
    assert!(sub("tengu_sysprompt_boundary_found").is_empty());
    let missing = sub("tengu_sysprompt_missing_boundary_marker");
    assert_eq!(missing.len(), 14);
    assert!(missing.iter().all(|m| m["promptBlockCount"] == 6));

    // tether：支线首条另起，之后接着用；摘要请求没有。
    let dec = sub("tengu_tether_decision");
    assert_eq!(dec.len(), 6);
    assert_eq!(dec[0]["reason"], "first_request");
    assert!(dec[1..].iter().all(|d| d["decision"] == "continue" && d["reason"] == "append"));
    assert_eq!(col(&dec, "turnsInThread"), [1, 2, 3, 4, 5, 6].map(|n| json!(n)));
    assert!(dec.iter().all(|d| d["sourceCategory"] == "subagent"));
    assert_eq!(sub("tengu_tether_echo_audit").len(), 5);

    // 支线里的工具续轮与攒附件：深度是上一条的。
    let before = sub("tengu_query_before_attachments");
    assert_eq!(col(&before, "queryDepth")[..3], [2, 3, 4].map(|n| json!(n)));
    let tool_ok = sub("tengu_tool_use_success");
    assert!(
        tool_ok.iter().all(|m| m["subagent_type"] == "Explore" && m["is_built_in_agent"] == true)
    );
    assert_eq!(sub("tengu_auto_mode_git_state_probe").len(), 7, "auto 模式下每条请求前一次");

    // 摘要请求与支线收尾。
    let ends = sub("tengu_turn_end");
    assert_eq!(
        col(&ends, "query_source"),
        ["agent_summary", "agent:builtin:Explore"].map(|s| json!(s))
    );
    assert!(ends.iter().all(|e| e["is_subagent"] == true));
    assert_eq!(ends[1]["query_source_category"], "subagent");
    let fork = sub("tengu_fork_agent_query");
    assert_eq!(fork[0]["forkLabel"], "agent_summary");
    assert_eq!(fork[0]["queryChainId"], chains[0]);
    assert_eq!(fork[0]["relayEligible"], false);
    let done = sub("tengu_agent_tool_completed");
    assert_eq!(done[0]["assistant_message_count"], 6);
    assert_eq!(done[0]["total_tool_uses"], 5);
    assert_eq!(done[0]["agent_type"], "Explore");
    assert_eq!(sub("tengu_cache_eviction_hint")[0]["scope"], "subagent_end");

    // Datadog：支线条目带 agent_id；api_success 不带工具长度表的 hash。
    assert!(p.dd.iter().any(|d| d["agent_id"] == AGENT && d["message"] == "tengu_api_success"));
    assert!(
        p.dd.iter()
            .filter(|d| d["message"] == "tengu_api_success")
            .all(|d| d.get("tool_schemas_hash").is_none())
    );
}

/// 线程增量请求（`thread.type: continue`，体里没有 tools、只有新增的两条消息）按上一条
/// 全量补回客户端视角：`cap/2.1.277` sonnet 那一对，官方报 messageCount 8、toolsCount 19、
/// 实际走线程续用、省掉 85845 字节。
#[test]
fn thread_continuations_are_filled_from_the_thread_base() {
    let mut calls = Vec::new();
    for (f, stop) in [("00031_", "tool_use"), ("00035_", "tool_use")] {
        let rel = format!("2.1.277/{f}");
        let Some((body, betas)) = cap_request(&rel) else {
            eprintln!("skipped: cap/2.1.277 not present");
            return;
        };
        let mut c = call(body, &format!("req_{f}"), stop);
        c.betas = betas;
        c.ua_out = "claude-cli/2.1.280 (external, cli)".into();
        c.agent.request_class = cap_header(&rel, "x-claude-code-request-class");
        // 00031 那条回复：正文 38 字 + `Bash` 与入参 242（响应解压后逐块数的）。
        c.reply_input_chars = 280;
        calls.push(c);
    }
    let session = parse_shape(&calls[0].body).unwrap().session_id.unwrap();
    assert_eq!(parse_shape(&calls[1].body).unwrap().tools_count, 0, "增量请求体里没有工具");
    let t = Telemetry::default();
    for c in calls {
        t.ingest(c);
    }
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).unwrap();
    let metas = |n: &str| -> Vec<Value> {
        p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
    };
    let q = metas("tengu_api_query");
    assert_eq!(q[1]["querySource"], "repl_main_thread", "不是辅助调用");
    assert_eq!(q[1]["messagesLength"], 8);
    let ok = metas("tengu_api_success");
    assert_eq!(ok[1]["toolsCount"], 19);
    // 输入长度含服务端持有、体里没有的那条回复。
    assert_eq!(ok[1]["inputTextCharLength"], 56026);
    assert_eq!(ok[1]["estimatedInputTokens"], 18676);
    let live = metas("tengu_tether_live_outcome");
    assert_eq!(live[0]["sentThreadType"], "create");
    assert_eq!(live[1]["sentThreadType"], "continue");
    assert_eq!(live[1]["omittedBytes"], 85845);
    let dec = metas("tengu_tether_decision");
    assert_eq!(
        (dec[1]["decision"].as_str(), dec[1]["deltaMessageCount"].as_u64()),
        (Some("continue"), Some(3))
    );
}

/// 这一轮是谁发起的以请求自己的 `cc_turn_origin` 为准：`cap/2.1.280` 00179 是同伴会话
/// 发来的（peer）、00180 是后台任务的完成通知（task_notification），官方分别报 `peer` /
/// `task-notification`、`prompt_source: system`，peer 那条不带 `prompt_index` 也不占号。
#[test]
fn turn_origin_follows_the_billing_header() {
    let t = Telemetry::default();
    let mut session = String::new();
    for (i, f) in ["00161_", "00179_", "00180_"].into_iter().enumerate() {
        let rel = format!("2.1.280/{f}");
        let Some((body, betas)) = cap_request(&rel) else {
            eprintln!("skipped: cap/2.1.280 not present");
            return;
        };
        session = parse_shape(&body).unwrap().session_id.unwrap();
        let mut c = call(body, &format!("req_{f}"), "end_turn");
        c.betas = betas;
        c.ua_out = "claude-cli/2.1.280 (external, cli)".into();
        c.agent.request_class = cap_header(&rel, "x-claude-code-request-class");
        c.started_at = frozen_now() - Duration::from_secs(300 - 60 * i as u64);
        t.ingest(c);
    }
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).unwrap();
    let metas = |n: &str| -> Vec<Value> {
        p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
    };
    let inputs = metas("tengu_input_prompt");
    let col = |k: &str| -> Vec<Value> { inputs.iter().map(|m| m[k].clone()).collect() };
    assert_eq!(col("turn_origin"), ["human", "peer", "task-notification"].map(|s| json!(s)));
    assert_eq!(col("prompt_source"), ["typed", "system", "system"].map(|s| json!(s)));
    assert_eq!(col("prompt_index"), [json!(1), Value::Null, json!(2)], "peer 不占号");
    assert!(inputs.iter().all(|m| m["is_wakeup"] == false));
    let ok = metas("tengu_api_success");
    assert_eq!(
        ok.iter().map(|m| m["turn_origin"].clone()).collect::<Vec<_>>(),
        ["human", "peer", "task-notification"].map(|s| json!(s))
    );
}

/// auto 模式只认客户端注入的两种位置；用户正文里引用、工具输出里恰好有、assistant 复述
/// 这段话都不算。
#[test]
fn auto_mode_markers_are_only_read_from_injected_reminders() {
    let body = |msgs: Value| {
        json!({ "model": "claude-sonnet-5", "messages": msgs, "system": [{"type":"text","text":"x"}] })
                .to_string()
                .into_bytes()
    };
    let mode = |msgs: Value| parse_shape(&body(msgs)).unwrap().permission_mode;
    let quoted = "文档里写着 While auto mode is active: 你可以……";
    // 用户引用、工具输出、assistant 复述：都不改模式。
    assert_eq!(
        mode(json!([
            {"role":"user","content":[{"type":"text","text":quoted}]},
            {"role":"assistant","content":[{"type":"text","text":"While auto mode is active: noted"}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"<system-reminder>\nWhile auto mode is active: x"}]}
        ])),
        "default"
    );
    // 注入的提示块与 system 消息：算。
    let enter = json!({"role":"user","content":[
        {"type":"text","text":"<system-reminder>\nWhile auto mode is active:\n\nrules</system-reminder>"},
        {"type":"text","text":"hi"}
    ]});
    assert_eq!(mode(json!([enter.clone()])), "auto");
    assert_eq!(
        mode(
            json!([{"role":"system","content":"# Environment\n... While auto mode is active: ..."}])
        ),
        "auto"
    );
    // 进入之后，工具输出里出现退出那段文字：仍是 auto；注入的退出提示才算退出。
    assert_eq!(
        mode(json!([
            enter.clone(),
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"## Exited Auto Mode\nYou have exited"}]}
        ])),
        "auto"
    );
    assert_eq!(
        mode(json!([
            enter,
            {"role":"user","content":[{"type":"text","text":"<system-reminder>\n## Exited Auto Mode\n\nYou have exited auto mode.</system-reminder>"}]}
        ])),
        "default"
    );
}

/// 会话首轮就是 peer：它不占号（下一次用户输入仍是 1），它的续轮也不再被当成新输入，
/// 首轮那串只发一次、跟着真正的首轮走。
#[test]
fn a_peer_opening_turn_does_not_take_a_prompt_number() {
    let t = Telemetry::default();
    let mut session = String::new();
    let steps =
        [("00179_", "tool_use", true), ("00179_", "end_turn", false), ("00161_", "end_turn", true)];
    for (i, (f, stop, fresh)) in steps.into_iter().enumerate() {
        let rel = format!("2.1.280/{f}");
        let Some((body, betas)) = cap_request(&rel) else {
            eprintln!("skipped: cap/2.1.280 not present");
            return;
        };
        // 第二步是 peer 那一轮的工具续轮：把末条消息换成 tool_result。
        let body = if fresh {
            body
        } else {
            let mut v: Value = serde_json::from_slice(&body).unwrap();
            let msgs = v["messages"].as_array_mut().unwrap();
            msgs.push(json!({"role":"assistant","content":[{"type":"tool_use","id":"tp","name":"Bash","input":{}}]}));
            msgs.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"tp","content":"ok"}]}));
            v.to_string().into_bytes()
        };
        session = parse_shape(&body).unwrap().session_id.unwrap();
        let mut c = call(body, &format!("req_{i}"), stop);
        c.betas = betas;
        c.ua_out = "claude-cli/2.1.280 (external, cli)".into();
        c.started_at = frozen_now() - Duration::from_secs(300 - 60 * i as u64);
        t.ingest(c);
    }
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).unwrap();
    let inputs: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_input_prompt")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(inputs.len(), 2, "peer 的续轮不是新输入");
    assert_eq!(inputs[0]["turn_origin"], "peer");
    assert!(inputs[0].get("prompt_index").is_none());
    assert_eq!(inputs[1]["turn_origin"], "human");
    assert_eq!(inputs[1]["prompt_index"], 1, "peer 不占号");
    let first_prompt_only = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_policy_limits_cache_state_at_first_prompt")
        .count();
    assert_eq!(first_prompt_only, 1, "首轮那串只发一次");
}

/// 正文换成给定的几个工具（各带 `input_schema`），其余同 [`cc_body`]。
fn body_with_tools(names: &[&str]) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    v["tools"] = Value::Array(
        names
            .iter()
            .map(|n| json!({"name": n, "description": "d", "input_schema": {"type": "object"}}))
            .collect(),
    );
    v.to_string().into_bytes()
}

/// 一个 2.1.291 会话的首轮：返回事件链（名字 + meta）。
fn first_turn_2_1_291(body: Vec<u8>) -> Vec<(String, Value)> {
    let t = Telemetry::default();
    let mut c = call(body, "req_291", "end_turn");
    c.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    c.cache_creation_5m_tokens = Some(0);
    c.cache_creation_1h_tokens = Some(8729);
    t.ingest(c);
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).expect("queued");
    p.events
        .iter()
        .map(|(_, e)| {
            let name = e["event_data"]["event_name"]
                .as_str()
                .or_else(|| e["event_data"]["experiment_id"].as_str())
                .unwrap_or("?")
                .to_string();
            let meta = if e["event_data"]["additional_metadata"].is_string() {
                meta_of(e)
            } else {
                Value::Null
            };
            (name, meta)
        })
        .collect()
}

/// 2.1.291 的遥测（`cap/auto-2.1.291-20261006-full`）：服务端抽样配置下 `api_before/after_normalize`、
/// `query_before/after_attachments` 不发；收尾多一条 `message_display_hooks`；会话首条多一条
/// `session_transcript_write`；`api_success` 多按 ttl 拆开的缓存写入；实验曝光换成
/// `tengu_ochre_wren-agnostic-intro`；模板里 `signed_cache_shadow` 多 `verify_micros`。
#[test]
fn version_2_1_291_telemetry_shape() {
    let ev = first_turn_2_1_291(cc_body(true));
    let has = |n: &str| ev.iter().any(|(x, _)| x == n);
    for gone in [
        "tengu_api_before_normalize",
        "tengu_api_after_normalize",
        "tengu_query_before_attachments",
        "tengu_query_after_attachments",
        "tengu_time_shell",
        "tengu_sleepy_shore_v1",
    ] {
        assert!(!has(gone), "{gone}");
    }
    for n in ["tengu_message_display_hooks", "tengu_ochre_wren-agnostic-intro"] {
        assert!(has(n), "{n}");
    }
    assert!(
        ev.iter().any(
            |(n, m)| n == "tengu_feature_ok" && m["feature_name"] == "session_transcript_write"
        ),
        "会话首条写会话记录"
    );
    // 抽中的 1% 带 `sample_rate`，没抽中的整条不发；带着就得是 0.01。
    for (n, m) in &ev {
        if n == "tengu_api_cache_breakpoints" || n.starts_with("tengu_sysprompt_") {
            assert_eq!(m["sample_rate"], json!(0.01), "{n}");
        }
    }
    let success = &ev.iter().find(|(n, _)| n == "tengu_api_success").unwrap().1;
    let keys: Vec<&str> = success.as_object().unwrap().keys().map(String::as_str).collect();
    let at = |k: &str| keys.iter().position(|x| *x == k).unwrap();
    assert_eq!(at("cache_creation_5m_input_tokens"), at("uncachedInputTokens") + 1);
    assert_eq!(at("cache_creation_1h_input_tokens"), at("uncachedInputTokens") + 2);
    assert_eq!(success["cache_creation_1h_input_tokens"], 8729);
    let shadow = &ev.iter().find(|(n, _)| n == "tengu_signed_cache_shadow").unwrap().1;
    assert!(shadow.get("verify_micros").is_some(), "{shadow}");
}

/// 一个 2.1.293 会话的首轮（模型、出站 beta 由调用方给）：返回事件链（名字 + meta + 顶层 betas）。
/// 号是 `cred_id`：`tengu_heron_brook` 按号记（[`crate::proxy::heron_brook`]），各测试别共用。
fn first_turn_2_1_293(
    cred_id: i64,
    model: &str,
    effort: Option<&str>,
    betas: &str,
) -> Vec<(String, Value, Value)> {
    let t = Telemetry::default();
    let mut c = call(body_for(model, effort, true), "req_293", "end_turn");
    c.cred_id = cred_id;
    c.ua_out = "claude-cli/2.1.293 (external, cli)".into();
    c.betas = Some(betas.into());
    c.resp_model = Some(model.into());
    t.ingest(c);
    let st = t.0.state.lock();
    let p = st.pending.get(&(cred_id, SESSION.to_string())).expect("queued");
    p.events
        .iter()
        .map(|(_, e)| {
            let d = &e["event_data"];
            let name = d["event_name"].as_str().or_else(|| d["experiment_id"].as_str());
            let meta = if d["additional_metadata"].is_string() { meta_of(e) } else { Value::Null };
            (name.unwrap_or("?").to_string(), meta, d["betas"].clone())
        })
        .collect()
}

/// 2.1.293 的遥测（`cap/auto-2.1.293-20261008-full`，交互式默认模式的单轮会话）：
/// - 实验曝光 `tengu_ochre_wren-agnostic-intro` 没了（改成强制开）；启动段多 `org_config_gate_shadow`
///   与 `frontmatter_shadow_unknown_key`；
/// - `wire_shape_recorded.inlineTools` 随 `inline-tools` beta 为 true，haiku-4.5 会话不报这条；
/// - 会话级 `betas` 止于 `mid-conversation-system-2026-04-07`，不带 `-clear-at` 那项；
/// - `context_size` 的工具计数：opus 34 / 13942、haiku-5-5 33 / 13912、haiku-4.5 37 / 14413；
/// - haiku-5-5 会话首次输入前多一条 `heron_brook_applied`；haiku-4.5 当主模型时模型目录那条
///   不带 `default_output_match` / `default_effort_match`。
#[test]
fn version_2_1_293_telemetry_shape() {
    let opus_betas = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
        thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
        mid-conversation-system-2026-04-07,per-turn-control-2026-07-01,\
        mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,advisor-tool-2026-03-01,\
        advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
        thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
        extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    let h55_betas = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
        context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
        mid-conversation-system-2026-04-07,claude-code-20250219,per-turn-control-2026-07-01,\
        mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,advisor-tool-2026-03-01,\
        advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
        thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
        extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    let h45_betas = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
        context-management-2025-06-27,prompt-caching-scope-2026-01-05,claude-code-20250219,\
        advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,thinking-binding-controls-2026-08-01,\
        thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
        cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    // 那时 haiku-5-5 的 bootstrap 带着 2790 字的 `client_data.tengu_heron_brook`（`00291`），opus / haiku-4.5 的没有。
    const CRED: i64 = -2_931;
    let heron_text = "x".repeat(2790);
    for (model, client_data) in [
        ("claude-haiku-5-5", json!({ "tengu_heron_brook": heron_text })),
        ("claude-opus-5-5", json!({})),
        ("claude-haiku-4-5", json!({})),
    ] {
        let boot = json!({ "client_data": client_data }).to_string();
        crate::proxy::client_data::note_bootstrap(CRED, model, boot.as_bytes());
    }
    let opus = first_turn_2_1_293(CRED, "claude-opus-5-5", Some("high"), opus_betas);
    let h55 = first_turn_2_1_293(CRED, "claude-haiku-5-5", Some("high"), h55_betas);
    let h45 = first_turn_2_1_293(CRED, "claude-haiku-4-5-20251001", None, h45_betas);
    let has = |ev: &[(String, Value, Value)], n: &str| ev.iter().any(|(x, ..)| x == n);
    let meta = |ev: &[(String, Value, Value)], n: &str| {
        ev.iter().find(|(x, ..)| x == n).map(|(_, m, _)| m.clone()).unwrap_or(Value::Null)
    };

    for ev in [&opus, &h55, &h45] {
        assert!(!has(ev, "tengu_ochre_wren-agnostic-intro"));
        assert!(has(ev, "tengu_org_config_gate_shadow"));
        // 抓包那台机器上某个技能的 frontmatter 带了未知键才有，是环境不是版本，不报。
        assert!(!has(ev, "tengu_frontmatter_shadow_unknown_key"));
        // 会话级 betas 里没有 `-clear-at`。
        for (n, _, b) in ev.iter() {
            if let Some(b) = b.as_str() {
                let session_level = !b.contains("effort-2025") && !b.contains("structured-outputs");
                if session_level {
                    assert!(!b.contains("clear-at"), "{n}: {b}");
                }
            }
        }
    }
    assert_eq!(meta(&opus, "tengu_wire_shape_recorded")["inlineTools"], true);
    assert_eq!(meta(&h55, "tengu_wire_shape_recorded")["inlineTools"], true);
    assert!(!has(&h45, "tengu_wire_shape_recorded"), "haiku-4.5 没有 mid-conversation-system");

    let ctx = |ev: &[(String, Value, Value)]| {
        let m = meta(ev, "tengu_context_size");
        (m["non_mcp_tools_count"].clone(), m["non_mcp_tools_tokens"].clone())
    };
    assert_eq!(ctx(&opus), (json!(34), json!(13942)));
    assert_eq!(ctx(&h55), (json!(33), json!(13912)));
    assert_eq!(ctx(&h45), (json!(37), json!(14413)));

    // haiku-5-5 的默认档是 medium：模拟发的 high 报成「不是默认」（官方会话里 medium 那 18 条是 true）。
    let success = meta(&h55, "tengu_api_success");
    assert_eq!(success["default_effort_level"], "medium");
    assert_eq!(success["is_default_effort"], false);
    assert!(has(&h55, "tengu_heron_brook_applied"));
    assert!(!has(&opus, "tengu_heron_brook_applied") && !has(&h45, "tengu_heron_brook_applied"));
    let heron = h55.iter().position(|(n, ..)| n == "tengu_heron_brook_applied").unwrap();
    assert_eq!(
        (&h55[heron].1["len"], &h55[heron].1["fromClientData"]),
        (&json!(2790), &json!(true))
    );
    assert_eq!(h55[heron + 1].0, "tengu_sleepy_snowflake_applied", "紧挨在 sleepy 前");
    // 服务端没下发（2.1.299 起的 bootstrap、或这个号还没拉过）就没有这一条。
    let undelivered = first_turn_2_1_293(-2_932, "claude-haiku-5-5", Some("high"), h55_betas);
    assert!(!has(&undelivered, "tengu_heron_brook_applied"));

    let catalog = |ev: &[(String, Value, Value)]| meta(ev, "tengu_model_catalog_compare");
    assert!(catalog(&opus).get("default_output_match").is_some());
    assert!(catalog(&h45).get("default_output_match").is_none());
    assert!(catalog(&h45).get("default_effort_match").is_none());
    assert!(catalog(&opus)["picker_old"].as_str().unwrap().ends_with("claude-haiku-5-5"));
}

/// `context_size` 的内建工具 token 随 Artifact / `-p` Agent 的 schema 逐版变（工具数不变），对齐各版抓包：
/// 2.1.300 Artifact +274、2.1.301 −2、2.1.303 +8，2.1.303 的 `-p` Agent +17。2.1.303 抓包里多出来的
/// PublishPlugin（GrowthBook `tengu_copper_gazette`，不随版本）不算，所以 2.1.303 的值是抓包减 1152。
#[test]
fn context_size_tool_tokens_follow_the_schema_per_version() {
    let ctx = |ver: &str, model: &str, sdk: bool| {
        let t = Telemetry::default();
        let mut body = String::from_utf8(body_for(model, Some("high"), true)).unwrap();
        if sdk {
            body = body.replace("cc_entrypoint=cli", "cc_entrypoint=sdk-cli");
        }
        let mut c = call(body.into_bytes(), "req_ctx", "end_turn");
        c.ua_out = format!("claude-cli/{ver} (external, cli)");
        t.ingest(c);
        let st = t.0.state.lock();
        let m = st.pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_context_size")
            .map(|(_, e)| meta_of(e))
            .unwrap();
        (m["non_mcp_tools_count"].as_u64().unwrap(), m["non_mcp_tools_tokens"].as_u64().unwrap())
    };
    let (opus, h55, h45) = ("claude-opus-5-5", "claude-haiku-5-5", "claude-haiku-4-5-20251001");
    for (ver, o, h, l) in [
        ("2.1.299", 14025, 13995, 14496),
        ("2.1.300", 14299, 14269, 14770),
        ("2.1.301", 14297, 14267, 14768),
        ("2.1.303", 15457 - 1152, 15427 - 1152, 15928 - 1152),
        // 2.1.309 Artifact `root` 说明改写 +45（交互式各族一律 15457→15502；Bash 的 `@name` 句不进估算）。
        ("2.1.309", 15502 - 1152, 15472 - 1152, 15973 - 1152),
        // 2.1.311 Artifact `intent` 说明改写 +37（`cap/auto-2.1.311-20261027-full` 交互式各族一律 +37；Bash 那句
        // `cd` 说明不进估算）。
        ("2.1.311", 15539 - 1152, 15509 - 1152, 16010 - 1152),
    ] {
        assert_eq!(ctx(ver, opus, false), (34, o), "{ver} opus");
        assert_eq!(ctx(ver, h55, false), (33, h), "{ver} haiku-5-5");
        assert_eq!(ctx(ver, h45, false), (37, l), "{ver} haiku-4.5");
    }
    assert_eq!(ctx("2.1.301", opus, true), (26, 6573));
    assert_eq!(ctx("2.1.303", opus, true), (26, 7742 - 1152));
    // `-p` 不带 Artifact，2.1.309 的 +45 不落到它头上（Bash 的 `@name` 也不进）：仍是 2.1.303 的值。
    assert_eq!(ctx("2.1.309", opus, true), (26, 7742 - 1152));
    // 2.1.311 的 +37 同样不落到 `-p` 头上（抓包仍是 7742）。
    assert_eq!(ctx("2.1.311", opus, true), (26, 7742 - 1152));

    // 关掉 Artifact（带官方工具集 Agent+Read，留着 ListAgents / SendFeedback，只少 Artifact 一组）时，
    // `artifact_schema_delta` 不加（`!off.artifact` 为假），连 Artifact 的 schema 一起减：opus 主线程
    // 34/13942 + 83(READ) − 6111(Artifact 一组) = 31 / 7914，**2.1.309 的 +45 不落下来**，与 2.1.308 同值。
    let ctx_off_artifact = |ver: &str| {
        let t = Telemetry::default();
        let body = body_with_tools(&["Agent", "Read", "Bash", "ListAgents", "SendFeedback"]);
        let mut c = call(body, "req_ctx_off", "end_turn");
        c.ua_out = format!("claude-cli/{ver} (external, cli)");
        t.ingest(c);
        let st = t.0.state.lock();
        let m = st.pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_context_size")
            .map(|(_, e)| meta_of(e))
            .unwrap();
        (m["non_mcp_tools_count"].as_u64().unwrap(), m["non_mcp_tools_tokens"].as_u64().unwrap())
    };
    assert_eq!(ctx_off_artifact("2.1.309"), (31, 7914), "309 关 Artifact：+45 不落下来");
    assert_eq!(
        ctx_off_artifact("2.1.309"),
        ctx_off_artifact("2.1.308"),
        "关 Artifact 时 309 == 308"
    );
}

/// 真实 CC 经网关透传（[`ApiCall::client_tools`]）时 `context_size` 的工具计数按网关基线报：
/// `cap/auto-2.1.309-20261023-api` 同一会话先后跑 309 / 308 的客户端自报值（308 与
/// `cap/auto-2.1.308-20261022-api` 逐值相同，307 的 opus 也是 14415）。309 比 308 多 46（网关那份 Artifact
/// schema 12432→12614 字符），不是订阅端的 45。
#[test]
fn context_size_tool_tokens_follow_the_gateway_samples() {
    let ctx = |ver: &str, body: Vec<u8>, sdk: bool, client_tools: bool| {
        let t = Telemetry::default();
        let mut body = String::from_utf8(body).unwrap();
        if sdk {
            body = body.replace("cc_entrypoint=cli", "cc_entrypoint=sdk-cli");
        }
        let mut c = call(body.into_bytes(), "req_ctx_gw", "end_turn");
        c.ua_out = format!("claude-cli/{ver} (external, cli)");
        c.client_tools = client_tools;
        t.ingest(c);
        let st = t.0.state.lock();
        let m = st.pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_context_size")
            .map(|(_, e)| meta_of(e))
            .unwrap();
        (m["non_mcp_tools_count"].as_u64().unwrap(), m["non_mcp_tools_tokens"].as_u64().unwrap())
    };
    let gw = |ver: &str, model: &str, sdk: bool| {
        ctx(ver, body_for(model, Some("high"), true), sdk, true)
    };
    let (opus, sonnet, h55, h45) =
        ("claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-haiku-4-5-20251001");
    assert_eq!(gw("2.1.307", opus, false), (34, 14415));
    assert_eq!(gw("2.1.308", opus, false), (34, 14415));
    assert_eq!(gw("2.1.308", sonnet, false), (34, 14415));
    assert_eq!(gw("2.1.308", h55, false), (33, 14385));
    assert_eq!(gw("2.1.309", opus, false), (34, 14461));
    assert_eq!(gw("2.1.309", h55, false), (33, 14431));
    for ver in ["2.1.308", "2.1.309"] {
        assert_eq!(gw(ver, opus, true), (26, 7452), "{ver} -p");
        assert_eq!(gw(ver, h55, true), (26, 7452), "{ver} -p haiku-5-5");
    }
    assert_eq!(
        gw("2.1.309", opus, false).1 - gw("2.1.308", opus, false).1,
        46,
        "网关 308→309 是 +46"
    );
    // 2.1.311 没有网关样本：`intent` 说明那 146 字符在网关那份 schema 里同样多出来，3154→3190 是 +36（推算）。
    assert_eq!(gw("2.1.311", opus, false), (34, 14461 + 36));
    assert_eq!(gw("2.1.311", h55, false), (33, 14431 + 36));
    assert_eq!(gw("2.1.311", opus, true), (26, 7452), "-p 没有 Artifact，不变");

    // 同一份请求走模拟路径（注入的固定资产）仍是订阅端标准值，309 只 +45。
    let std = |ver: &str| ctx(ver, body_for(opus, Some("high"), true), false, false);
    assert_eq!(std("2.1.308"), (34, 14305));
    assert_eq!(std("2.1.309"), (34, 14350));
    // 没有网关样本的：2.1.307 以前、haiku-4.5 主模型，照订阅端报。
    assert_eq!(gw("2.1.303", opus, false), std("2.1.303"));
    assert_eq!(gw("2.1.309", h45, false), ctx("2.1.309", body_for(h45, None, true), false, false));

    // 网关关掉 Artifact：整组不在，只剩其余工具的 +862（订阅端 31 / 7914）。
    let off = body_with_tools(&["Agent", "Read", "Bash", "ListAgents", "SendFeedback"]);
    assert_eq!(ctx("2.1.309", off.clone(), false, true), (31, 7914 + 862));
    assert_eq!(ctx("2.1.309", off.clone(), false, true), ctx("2.1.308", off, false, true));
}

/// 开关 `sim_trim_tools` 的遥测侧：正文里有官方工具却没有 Artifact / ListAgents / SendFeedback
/// 时，按官方用户用环境变量关掉这三项的样子报（`cap/auto-2.1.291-20261006` 关前 / 关后对照）。
#[test]
fn trimmed_tools_are_reported_like_the_env_switches() {
    let full = first_turn_2_1_291(body_with_tools(&[
        "Agent",
        "Artifact",
        "Bash",
        "ListAgents",
        "Read",
        "SendFeedback",
    ]));
    let trimmed = first_turn_2_1_291(body_with_tools(&["Agent", "Bash", "Read"]));
    let meta =
        |ev: &[(String, Value)], n: &str| ev.iter().find(|(x, _)| x == n).map(|(_, m)| m.clone());
    let has = |ev: &[(String, Value)], n: &str| ev.iter().any(|(x, _)| x == n);

    let env_vars = |ev: &[(String, Value)]| meta(ev, "tengu_startup_telemetry").unwrap();
    assert_eq!(env_vars(&full)["set_env_vars"], "CLAUDE_CODE_SSE_PORT");
    let t = env_vars(&trimmed);
    assert_eq!(
        t["set_env_vars"],
        "CLAUDE_CODE_DISABLE_ARTIFACT,CLAUDE_CODE_HARBOR_KITE,CLAUDE_CODE_SEND_FEEDBACK,CLAUDE_CODE_SSE_PORT",
        "按名字排序"
    );
    assert_eq!(t["set_env_var_count"], 4);

    assert!(!has(&full, "tengu_artifact_disabled_session"));
    let disabled = meta(&trimmed, "tengu_artifact_disabled_session").expect("关掉的会话有这一条");
    assert_eq!(disabled["mechanism"], "env");
    assert_eq!(disabled["session_interactivity"], "interactive");
    let timer =
        trimmed.iter().position(|(n, m)| n == "tengu_timer" && m["event"] == "startup").unwrap();
    assert_eq!(trimmed[timer + 1].0, "tengu_artifact_disabled_session", "紧跟启动计时");

    for n in [
        "tengu_artifact_five_class_asks",
        "tengu_artifact_inherited_type_grant",
        "tengu_artifact_text_variant",
        "tengu_uds_startup_bind",
    ] {
        assert!(has(&full, n), "{n}");
        assert!(!has(&trimmed, n), "{n}");
    }
    assert!(has(&trimmed, "tengu_artifact_toolset"), "官方关掉后这条照发（on 仍为 true）");

    let ctx = |ev: &[(String, Value)]| {
        let m = meta(ev, "tengu_context_size").unwrap();
        (m["non_mcp_tools_count"].clone(), m["non_mcp_tools_tokens"].clone())
    };
    assert_eq!(ctx(&full), (json!(34), json!(13834)));
    assert_eq!(ctx(&trimmed), (json!(29), json!(6983)));
}

/// [`cc_body`] 换模型与 effort（`None` 即去掉 `output_config.effort`，haiku 那样）。
fn body_for(model: &str, effort: Option<&str>, last_user_text: bool) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&cc_body(last_user_text)).unwrap();
    v["model"] = json!(model);
    match effort {
        Some(e) => v["output_config"] = json!({ "effort": e }),
        None => {
            v.as_object_mut().unwrap().remove("output_config");
        }
    }
    v.to_string().into_bytes()
}

/// 2.1.291 子代理那条 `agent_tool_selected` 的 `session_effort` / `subagent_effort`：有才报，各取各的
/// ——opus 主线程起的 opus Explore 两项都是 high，haiku 主线程起的 haiku Explore 两项都不在
/// （`cap/auto-2.1.291-20261006-full` A0 与 E4 会话）。
#[test]
fn agent_tool_selected_reports_only_the_efforts_that_exist() {
    for (model, effort) in [("claude-opus-5-5", Some("high")), ("claude-haiku-4-5-20251001", None)]
    {
        let t = Telemetry::default();
        let base = frozen_now() - Duration::from_secs(60);
        let mut main = call(body_for(model, effort, true), "req_main", "tool_use");
        main.ua_out = "claude-cli/2.1.291 (external, cli)".into();
        main.started_at = base;
        main.tool_calls = vec![ToolCall {
            id: "t1".into(),
            name: "Agent".into(),
            input: json!({}),
            verdict: None,
        }];
        t.ingest(main);
        let mut sub = call(body_for(model, effort, true), "req_sub", "end_turn");
        sub.ua_out = "claude-cli/2.1.291 (external, cli)".into();
        sub.started_at = base + Duration::from_secs(5);
        sub.agent = AgentHeaders {
            agent_id: Some("a1".into()),
            agent_type: Some("Explore".into()),
            request_class: Some("subagent".into()),
        };
        t.ingest(sub);
        let st = t.0.state.lock();
        let p = st.pending.get(&key()).expect("queued");
        let selected = p
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_agent_tool_selected")
            .map(|(_, e)| meta_of(e))
            .unwrap_or_else(|| panic!("{model}: 没有 agent_tool_selected"));
        match effort {
            Some(e) => {
                assert_eq!(selected["session_effort"], e, "{model}");
                assert_eq!(selected["subagent_effort"], e, "{model}");
                let keys: Vec<&str> =
                    selected.as_object().unwrap().keys().map(String::as_str).collect();
                let at = |k: &str| keys.iter().position(|x| *x == k).unwrap();
                assert_eq!(at("session_effort"), at("is_fork") + 1);
            }
            None => {
                assert!(selected.get("session_effort").is_none(), "{model}: {selected}");
                assert!(selected.get("subagent_effort").is_none(), "{model}: {selected}");
            }
        }
    }
}

/// 2.1.291 `tether_live_outcome` 的线程计时看锚点（同一条线上一条回复），不看这条是续用还是另起：
/// 会话第一条 -1 / false；换了 effort 另起线程（`create/config_changed`）的那条照样报离上一条回复
/// 收尾的毫秒数，上一条回复带了工具调用就是 `anchorHasToolCall: true`
/// （`cap/auto-2.1.291-20261006-full` A0 那条 config_changed 报 44369）。
#[test]
fn tether_thread_timing_follows_the_anchor_not_the_thread_type() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(120);
    let mut first = call(body_for("claude-opus-5-5", Some("high"), true), "req_1", "tool_use");
    first.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    first.started_at = base;
    first.total_ms = 2_000;
    first.tool_calls =
        vec![ToolCall { id: "t9".into(), name: "Bash".into(), input: json!({}), verdict: None }];
    t.ingest(first);
    // effort 换了：tether 另起线程。
    let mut second = call(body_for("claude-opus-5-5", Some("low"), true), "req_2", "end_turn");
    second.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    second.started_at = base + Duration::from_secs(30);
    t.ingest(second);
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).expect("queued");
    let live: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_tether_live_outcome")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(live.len(), 2);
    assert_eq!(live[0]["threadIdleMs"], -1);
    assert_eq!(live[0]["anchorHasToolCall"], false);
    assert_eq!(live[1]["engineDecision"], "create", "{}", live[1]);
    let idle = live[1]["threadIdleMs"].as_i64().unwrap();
    assert!((27_000..=29_000).contains(&idle), "离上一条收尾约 28s: {idle}");
    assert_eq!(live[1]["threadIdleMonotonicMs"], idle);
    assert_eq!(live[1]["anchorHasToolCall"], true, "上一条回复带了工具调用");
    assert_eq!(live[1]["threadLifetimeMs"], -1);
}

/// 某个会话里 tether 收尾那几条的 meta，按 `sourceCategory` 筛。
fn live_outcomes(t: &Telemetry, category: &str) -> Vec<Value> {
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).expect("queued");
    p.events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_tether_live_outcome")
        .map(|(_, e)| meta_of(e))
        .filter(|m| m["sourceCategory"] == category)
        .collect()
}

/// 子代理的时间锚点是它自己那条支线：首条没有锚点报 -1（`cap/auto-2.1.291-20261006-full` 的
/// `00073`、`00087`、`00172` 等子代理首条都是），第二条从它自己第一条收尾算起，与主线程无关。
#[test]
fn subagent_tether_idle_is_anchored_on_its_own_branch() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(120);
    let mut main = call(body_for("claude-opus-5-5", Some("high"), true), "req_main", "tool_use");
    main.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    main.started_at = base;
    main.total_ms = 2_000;
    main.tool_calls =
        vec![ToolCall { id: "t1".into(), name: "Agent".into(), input: json!({}), verdict: None }];
    t.ingest(main);
    let sub = |rid: &str, at: u64, last_user_text: bool, stop: &str| {
        let mut c = call(body_for("claude-opus-5-5", Some("high"), last_user_text), rid, stop);
        c.ua_out = "claude-cli/2.1.291 (external, cli)".into();
        c.started_at = base + Duration::from_secs(at);
        c.total_ms = 3_000;
        c.agent = AgentHeaders {
            agent_id: Some("a1".into()),
            agent_type: Some("Explore".into()),
            request_class: Some("subagent".into()),
        };
        c
    };
    let mut first = sub("req_sub_1", 10, true, "tool_use");
    first.tool_calls =
        vec![ToolCall { id: "t2".into(), name: "Read".into(), input: json!({}), verdict: None }];
    t.ingest(first);
    t.ingest(sub("req_sub_2", 20, false, "end_turn"));
    let live = live_outcomes(&t, "subagent");
    assert_eq!(live.len(), 2, "{live:?}");
    assert_eq!(live[0]["threadIdleMs"], -1, "子代理首条没有锚点");
    assert_eq!(live[0]["anchorHasToolCall"], false);
    let idle = live[1]["threadIdleMs"].as_i64().unwrap();
    assert!((6_500..=7_500).contains(&idle), "从它自己第一条收尾（13s）算到 20s: {idle}");
    assert_eq!(live[1]["anchorHasToolCall"], true);
}

/// 主线程被 Esc 取消的那条也是时间锚点：之后重建线程的那条 idle 从取消那条收尾算起
/// （`cap/auto-2.1.291-20261006-full/00354` 报 9539），工具锚点仍看上一条有效回复。
#[test]
fn cancelled_main_request_still_anchors_the_tether_idle() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(120);
    let mut ok = call(body_for("claude-opus-5-5", Some("high"), true), "req_1", "tool_use");
    ok.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    ok.started_at = base;
    ok.total_ms = 2_000;
    ok.tool_calls =
        vec![ToolCall { id: "t3".into(), name: "Bash".into(), input: json!({}), verdict: None }];
    t.ingest(ok);
    let mut cancelled = call(body_for("claude-opus-5-5", Some("high"), true), "req_2", "end_turn");
    cancelled.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    cancelled.started_at = base + Duration::from_secs(10);
    cancelled.total_ms = 2_000;
    cancelled.aborted = true;
    t.ingest(cancelled);
    let mut next = call(body_for("claude-opus-5-5", Some("low"), true), "req_3", "end_turn");
    next.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    next.started_at = base + Duration::from_secs(20);
    t.ingest(next);
    let live = live_outcomes(&t, "main");
    let last = live.last().unwrap();
    let idle = last["threadIdleMs"].as_i64().unwrap();
    assert!((7_500..=8_500).contains(&idle), "从取消那条收尾（12s）算到 20s，不是 18s: {idle}");
    assert_eq!(last["anchorHasToolCall"], true, "工具锚点是上一条有效回复");
}

/// 新会话的标题生成先于主线程那条完成（`cap/auto-2.1.291-20261006` 四族：`00064`→`00066` 等）：会话
/// 不能由标题请求建——那样启动模板按「没有工具」出，精简状态（禁用的环境变量、
/// `artifact_disabled_session`）就补不上了。标题先扣着，主线程那条建好会话后再补发。
#[test]
fn a_title_finishing_first_does_not_start_the_session_untrimmed() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(30);
    let title_body = json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 32000,
        "stream": true,
        "thinking": {"type": "disabled"},
        "system": [
            {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.291.ced; cc_entrypoint=cli; cch=b1b2c;"},
            {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
            {"type":"text","text":"You are naming a coding session so the user can pick it out of a long list of sessions."}
        ],
        "messages": [{"role":"user","content":[{"type":"text","text":"<session>\nhi\n</session>\n\nWrite the title"}]}],
        "metadata": {"user_id": "{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}"}
    });
    let mut title = call(title_body.to_string().into_bytes(), "req_title", "end_turn");
    title.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    title.started_at = base;
    title.total_ms = 900;
    title.resp_model = Some("claude-haiku-4-5-20251001".into());
    t.ingest(title);
    assert!(!t.0.state.lock().pending.contains_key(&key()), "标题先扣着，会话还没建");
    let mut main = call(body_with_tools(&["Agent", "Bash", "Read"]), "req_main", "end_turn");
    main.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    main.started_at = base - Duration::from_millis(200);
    t.ingest(main);
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).expect("queued");
    let named = |n: &str| {
        p.events
            .iter()
            .filter(|(_, e)| ev_name(e) == n)
            .map(|(_, e)| meta_of(e))
            .collect::<Vec<_>>()
    };
    let startup = named("tengu_startup_telemetry");
    assert_eq!(startup.len(), 1);
    assert_eq!(
        startup[0]["set_env_vars"],
        "CLAUDE_CODE_DISABLE_ARTIFACT,CLAUDE_CODE_HARBOR_KITE,CLAUDE_CODE_SEND_FEEDBACK,CLAUDE_CODE_SSE_PORT"
    );
    assert_eq!(named("tengu_artifact_disabled_session").len(), 1);
    // 标题那条也补发了。
    assert!(
        named("tengu_api_success").iter().any(|m| m["querySource"] == "generate_session_title"),
        "标题的 api_success 补上了"
    );
}

/// 三个工具逐项判：客户端自己留着 ListAgents（开关开着时它照样出站）就只算另两个关了；Artifact
/// 以延迟声明出现也算开着——判据是完整的工具数组，不是去掉延迟声明后的长度表。
#[test]
fn tools_off_are_judged_per_tool_from_the_full_tool_array() {
    let only_list_agents =
        first_turn_2_1_291(body_with_tools(&["Agent", "Bash", "ListAgents", "Read"]));
    let meta =
        |ev: &[(String, Value)], n: &str| ev.iter().find(|(x, _)| x == n).map(|(_, m)| m.clone());
    let has = |ev: &[(String, Value)], n: &str| ev.iter().any(|(x, _)| x == n);
    let env = meta(&only_list_agents, "tengu_startup_telemetry").unwrap();
    assert_eq!(
        env["set_env_vars"],
        "CLAUDE_CODE_DISABLE_ARTIFACT,CLAUDE_CODE_SEND_FEEDBACK,CLAUDE_CODE_SSE_PORT",
        "ListAgents 还在，不列 HARBOR_KITE"
    );
    assert!(has(&only_list_agents, "tengu_uds_startup_bind"), "ListAgents 开着，套接字照绑");
    assert!(has(&only_list_agents, "tengu_artifact_disabled_session"));
    assert!(!has(&only_list_agents, "tengu_artifact_text_variant"));
    let ctx = meta(&only_list_agents, "tengu_context_size").unwrap();
    assert_eq!(ctx["non_mcp_tools_count"], 30, "34 - Artifact 一家 3 - SendFeedback 1");

    // Artifact 延迟声明着：算开着。
    let mut v: Value =
        serde_json::from_slice(&body_with_tools(&["Agent", "Bash", "Read"])).unwrap();
    v["tools"].as_array_mut().unwrap().push(
        json!({"name": "Artifact", "description": "d", "input_schema": {"type": "object"}, "defer_loading": true}),
    );
    let deferred = first_turn_2_1_291(v.to_string().into_bytes());
    assert!(!has(&deferred, "tengu_artifact_disabled_session"), "延迟声明的 Artifact 不算关");
    assert!(has(&deferred, "tengu_artifact_text_variant"));
    let env = meta(&deferred, "tengu_startup_telemetry").unwrap();
    assert_eq!(
        env["set_env_vars"],
        "CLAUDE_CODE_HARBOR_KITE,CLAUDE_CODE_SEND_FEEDBACK,CLAUDE_CODE_SSE_PORT"
    );
    let ctx = meta(&deferred, "tengu_context_size").unwrap();
    assert_eq!(ctx["non_mcp_tools_count"], 32, "34 - ListAgents 1 - SendFeedback 1");
}

/// [`cc_body`] 系列的体换一个会话 id（同一台设备、同一个账号）。
fn in_session(body: Vec<u8>, sid: &str) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&body).unwrap();
    v["metadata"]["user_id"] = json!(format!(
        "{{\"device_id\":\"b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"{sid}\"}}"
    ));
    v.to_string().into_bytes()
}

/// 某会话的标题生成请求（2.1.291，会话起名那份提示词）。
fn title_call(sid: &str, message_id: &str, started_at: SystemTime) -> ApiCall {
    let body = json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 32000,
        "stream": true,
        "thinking": {"type": "disabled"},
        "system": [
            {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.291.ced; cc_entrypoint=cli; cch=b1b2c;"},
            {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
            {"type":"text","text":"You are naming a coding session so the user can pick it out of a long list of sessions."}
        ],
        "messages": [{"role":"user","content":[{"type":"text","text":"<session>\nhi\n</session>\n\nWrite the title"}]}],
        "metadata": {"user_id": "x"}
    });
    let mut c = call(in_session(body.to_string().into_bytes(), sid), "req_title", "end_turn");
    c.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    c.started_at = started_at;
    c.total_ms = 900;
    c.message_id = Some(message_id.into());
    c.resp_model = Some("claude-haiku-4-5-20251001".into());
    c
}

/// 新进程启动时那条额度探测（只记一个启动标记，见 [`State::process_starts`]）。
fn quota_probe_call(sid: &str, at: SystemTime) -> ApiCall {
    let body = json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 1,
        "messages": [{"role":"user","content":"quota"}],
        "metadata": {"user_id": "x"}
    });
    let mut c = call(in_session(body.to_string().into_bytes(), sid), "req_q", "max_tokens");
    c.started_at = at;
    c
}

/// 一条 2.1.291 主线程请求（默认精简的工具形态）。
fn main_call(sid: &str, rid: &str, message_id: &str, started_at: SystemTime) -> ApiCall {
    let mut c = call(in_session(body_with_tools(&["Agent", "Bash", "Read"]), sid), rid, "end_turn");
    c.ua_out = "claude-cli/2.1.291 (external, cli)".into();
    c.started_at = started_at;
    c.total_ms = 4_000;
    c.message_id = Some(message_id.into());
    c
}

fn events_of(t: &Telemetry, sid: &str) -> Vec<(String, Value, Value)> {
    let st = t.0.state.lock();
    st.pending
        .get(&(7, sid.to_string()))
        .map(|p| {
            p.events
                .iter()
                .map(|(_, e)| {
                    let meta = if e["event_data"]["additional_metadata"].is_string() {
                        meta_of(e)
                    } else {
                        Value::Null
                    };
                    (ev_name(e).to_string(), meta, e["event_data"].clone())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `/clear` 之后新会话的标题先完成：标题扣住时不能先去判会话来历（那一步会把旧会话标成已清），
/// 主线程那条到了照样认出是从旧会话 `/clear` 过来的；它的 `timeSinceLastApiCallMs` 量到标题完成。
#[test]
fn a_title_first_after_clear_keeps_the_lineage_and_the_gap() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(300);
    let s2 = "22222222-2222-4222-8222-222222222222";
    t.ingest(quota_probe_call("11111111-1111-4111-8111-111111111111", base));
    t.ingest(main_call(SESSION, "req_1", "msg_1", base + Duration::from_secs(5)));
    let title_at = base + Duration::from_secs(60);
    t.ingest(title_call(s2, "msg_title", title_at));
    t.ingest(main_call(s2, "req_2", "msg_2", title_at - Duration::from_millis(200)));
    let ev = events_of(&t, s2);
    // `/clear` 是同一个进程里换会话：不再出启动串，事件带着旧会话的 id 当 `parent_session_id`。
    assert!(!ev.iter().any(|(n, _, _)| n == "tengu_startup_telemetry"));
    assert!(!ev.is_empty());
    // （GrowthBook 曝光事件没有这个字段，只看一方事件。）
    for (n, _, d) in ev.iter().filter(|(_, _, d)| d.get("event_name").is_some()) {
        assert_eq!(d["parent_session_id"], SESSION, "{n}: 认出是从旧会话 /clear 过来的");
    }
    let main_success = ev
        .iter()
        .find(|(n, m, _)| n == "tengu_api_success" && m["querySource"] != "generate_session_title")
        .unwrap();
    // 主线程 200ms 后发、跑 4s；标题 900ms 完成：量到标题完成是 4000 - 200 - 900 = 2900。
    assert_eq!(main_success.1["timeSinceLastApiCallMs"], 2900, "{}", main_success.1);
    assert!(
        ev.iter()
            .any(|(n, m, _)| n == "tengu_api_success"
                && m["querySource"] == "generate_session_title"),
        "标题补发了"
    );
    // 标题比主线程早完成：补发它不能把消息锚点拨回标题那条。
    let st = t.0.state.lock();
    assert_eq!(st.sessions[&(7, s2.to_string())].last_message_id.as_deref(), Some("msg_2"));
}

/// `--continue` 的新进程里标题先完成：旧会话还在会话表里，但它属于「会话还没有」——标题扣住时
/// 不能先触发接续（那一步会删掉旧会话与启动标记），主线程那条到了照样按接续起新进程。
#[test]
fn a_title_first_after_continue_keeps_the_lineage() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(600);
    t.ingest(main_call(SESSION, "req_1", "msg_1", base));
    let restart = base + Duration::from_secs(120);
    t.ingest(quota_probe_call("33333333-3333-4333-8333-333333333333", restart));
    let title_at = restart + Duration::from_secs(10);
    t.ingest(title_call(SESSION, "msg_title", title_at));
    t.ingest(main_call(SESSION, "req_2", "msg_2", title_at - Duration::from_millis(200)));
    let st = t.0.state.lock();
    let p = st.pending.get(&key()).expect("queued");
    // 指标依次是：第一个进程的主线程、第二个进程的主线程（按接续起）、补发的标题。
    let marks: Vec<(bool, bool)> = p.metrics.iter().map(|m| (m.new_session, m.continued)).collect();
    assert_eq!(marks, [(true, false), (true, true), (false, false)], "第二个进程按 --continue 起");
    let startups = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_startup_telemetry")
        .map(|(_, e)| meta_of(e))
        .collect::<Vec<_>>();
    // 待发批次里留着的是第二个进程那串（第一个进程那串随它的会话一起收掉了）；它由主线程那条出，
    // 带着精简状态，而不是由先完成的标题按「没有工具」出。
    let last = startups.last().expect("第二个进程有启动串");
    assert!(
        last["set_env_vars"].as_str().unwrap().contains("CLAUDE_CODE_DISABLE_ARTIFACT"),
        "{last}"
    );
    assert_eq!(st.sessions[&key()].last_message_id.as_deref(), Some("msg_2"));
}

/// Artifact 一整组判：客户端留着 `ArtifactComments` 或延迟声明的 `ArtifactData`，都不算关掉
/// Artifact——不报禁用、不列那个环境变量、计数不扣那一组。
#[test]
fn keeping_an_artifact_sub_tool_means_artifact_is_not_off() {
    let has = |ev: &[(String, Value)], n: &str| ev.iter().any(|(x, _)| x == n);
    let meta =
        |ev: &[(String, Value)], n: &str| ev.iter().find(|(x, _)| x == n).map(|(_, m)| m.clone());
    let comments =
        first_turn_2_1_291(body_with_tools(&["Agent", "ArtifactComments", "Bash", "Read"]));
    assert!(!has(&comments, "tengu_artifact_disabled_session"));
    assert!(has(&comments, "tengu_artifact_text_variant"));
    let env = meta(&comments, "tengu_startup_telemetry").unwrap();
    assert!(!env["set_env_vars"].as_str().unwrap().contains("DISABLE_ARTIFACT"), "{env}");
    assert_eq!(meta(&comments, "tengu_context_size").unwrap()["non_mcp_tools_count"], 32);

    let mut v: Value =
        serde_json::from_slice(&body_with_tools(&["Agent", "Bash", "Read"])).unwrap();
    v["tools"].as_array_mut().unwrap().push(
        json!({"name": "ArtifactData", "description": "d", "input_schema": {"type": "object"}, "defer_loading": true}),
    );
    let data = first_turn_2_1_291(v.to_string().into_bytes());
    assert!(!has(&data, "tengu_artifact_disabled_session"), "延迟声明的 ArtifactData 也算留着");
}

/// `/clear` 之后新会话的第一条主线程请求**不**接旧会话的 tether 锚点：报 `threadIdleMs: -1`、
/// `anchorHasToolCall: false`（`cap/auto-2.1.291-20261006-full/00356`，`create/no_continue_pointer`），
/// 哪怕旧会话最后一条回复带了工具调用、或者刚取消过一条。
#[test]
fn tether_anchors_do_not_carry_over_a_clear() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(300);
    let s2 = "44444444-4444-4444-8444-444444444444";
    t.ingest(quota_probe_call("11111111-1111-4111-8111-111111111111", base));
    let mut with_tools = main_call(SESSION, "req_1", "msg_1", base + Duration::from_secs(5));
    with_tools.tool_calls =
        vec![ToolCall { id: "t5".into(), name: "Bash".into(), input: json!({}), verdict: None }];
    t.ingest(with_tools);
    // 旧会话最后一条被 Esc 取消：旧会话里两个锚点都有值，新会话也不接。
    let mut cancelled = main_call(SESSION, "req_2", "msg_2", base + Duration::from_secs(20));
    cancelled.aborted = true;
    cancelled.total_ms = 1_000;
    t.ingest(cancelled);
    t.ingest(main_call(s2, "req_3", "msg_3", base + Duration::from_secs(30)));
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, s2.to_string())).expect("queued");
    let live: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_tether_live_outcome")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(live[0]["threadIdleMs"], -1, "新会话首条没有锚点");
    assert_eq!(live[0]["threadIdleMonotonicMs"], -1);
    assert_eq!(live[0]["anchorHasToolCall"], false, "不看旧会话的回复");
}

/// 2.1.299 抓包那个号的 eval 里那两项：清单是默认值，正文在实验 v4 第 1 组。
const CERF_V4_EVAL: &str = r#"{"features":{"tengu_lucky_cerf":{"value":["claude-opus-5",
    "claude-fable-5","claude-fable-5-1","claude-sonnet-5-5","claude-haiku-5-5"],"source":"defaultValue"},
    "tengu_lucky_cerf_text":{"value":"Text.","source":"experiment",
    "experiment":{"key":"tengu_lucky_cerf_experiment_v4"},
    "experimentResult":{"inExperiment":true,"variationId":1}}}}"#;

/// `tengu_lucky_cerf` 一系的曝光（号的 eval 同 2.1.299 抓包那个号）：会话从完整提示词的 haiku-4.5 开头时
/// 不发；切到 opus-5-5（精简提示词，只读清单）时清单不在实验里，不发；再切到清单里的 sonnet-5-5、头一次
/// 输入时补正文那条（v4，分组照这个会话钉住的那份），之后不再发。
#[test]
fn lucky_cerf_exposure_follows_the_first_lean_prompt_model() {
    const CRED: i64 = 9_301;
    crate::proxy::note_eval_features(CRED, CERF_V4_EVAL.as_bytes());
    let body = |model: &str| {
        let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
        v["model"] = json!(model);
        v["system"][0]["text"] = json!(
            "x-anthropic-billing-header: cc_version=2.1.299.1e2; cc_entrypoint=cli; cch=0f7f8;"
        );
        serde_json::to_vec(&v).unwrap()
    };
    let turn = |model: &str, req: &str, at: SystemTime| {
        let mut c = call(body(model), req, "end_turn");
        c.cred_id = CRED;
        c.ua_out = "claude-cli/2.1.299 (external, cli)".into();
        c.resp_model = Some(model.into());
        c.started_at = at;
        c
    };
    let exposures = |t: &Telemetry| -> Vec<Value> {
        let st = t.0.state.lock();
        st.pending[&(CRED, SESSION.to_string())]
            .events
            .iter()
            .filter(|(_, e)| ev_name(e).starts_with("tengu_lucky_cerf_experiment"))
            .map(|(_, e)| e.clone())
            .collect()
    };
    let t = Telemetry::default();
    t.ingest(turn("claude-haiku-4-5-20251001", "req_1", frozen_now() - Duration::from_secs(45)));
    assert!(exposures(&t).is_empty(), "haiku-4.5 不读这两个开关");
    t.ingest(turn("claude-opus-5-5", "req_2", frozen_now() - Duration::from_secs(30)));
    assert!(exposures(&t).is_empty(), "opus-5-5 只读清单，清单不在实验里");
    t.ingest(turn("claude-sonnet-5-5", "req_3", frozen_now() - Duration::from_secs(15)));
    let ex = exposures(&t);
    assert_eq!(ex.len(), 1, "切到 sonnet-5-5 那次输入补一条");
    assert_eq!(ev_name(&ex[0]), "tengu_lucky_cerf_experiment_v4");
    assert_eq!(ex[0]["event_data"]["variation_id"], 1, "按号的 eval");
    assert_eq!(
        ex[0]["event_data"]["experiment_metadata"],
        "{\"feature_id\":\"tengu_lucky_cerf_text\"}"
    );
    t.ingest(turn("claude-sonnet-5-5", "req_4", frozen_now()));
    assert_eq!(exposures(&t).len(), 1, "一个进程只发一次");
}

/// `/clear` 换会话不换进程：`tengu_lucky_cerf` 曝光在旧会话发过，新会话不再发；新进程才重新算。
#[test]
fn lucky_cerf_exposure_is_not_repeated_after_a_clear() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(300);
    let s2 = "55555555-5555-4555-8555-555555555555";
    // `main_call` 是 opus-5：在清单里，按号的 eval 发正文那条（v4）。用独立的号，不动别的用例共用的 7 号。
    const CRED: i64 = 9_302;
    crate::proxy::note_eval_features(CRED, CERF_V4_EVAL.as_bytes());
    let at_299 = |sid: &str, rid: &str, msg: &str, at: SystemTime| {
        let mut c = main_call(sid, rid, msg, at);
        c.ua_out = "claude-cli/2.1.299 (external, cli)".into();
        c.cred_id = CRED;
        c
    };
    let mut probe = quota_probe_call("11111111-1111-4111-8111-111111111111", base);
    probe.cred_id = CRED;
    t.ingest(probe);
    t.ingest(at_299(SESSION, "req_1", "msg_1", base + Duration::from_secs(5)));
    t.ingest(at_299(s2, "req_2", "msg_2", base + Duration::from_secs(30)));
    let count = |sid: &str| {
        let st = t.0.state.lock();
        st.pending.get(&(CRED, sid.to_string())).map_or(0, |p| {
            p.events.iter().filter(|(_, e)| ev_name(e) == "tengu_lucky_cerf_experiment_v4").count()
        })
    };
    assert_eq!(count(SESSION), 1, "旧会话首次输入发一条");
    assert_eq!(count(s2), 0, "/clear 之后的新会话不再发");
}

/// 2.1.299 起交互式会话退出时，`config_cache_stats` 前多一条 `tengu_resume_hint_printed`（同一毫秒，
/// `cap/auto-2.1.299-20261014-full` 每个交互式会话一条）；更早的版本没有。
#[test]
fn idle_exit_prints_the_resume_hint_from_2_1_299() {
    for (ua, want) in [
        ("claude-cli/2.1.299 (external, cli)", true),
        ("claude-cli/2.1.296 (external, cli)", false),
    ] {
        let t = Telemetry::default();
        let mut last = call(cc_body(true), "req_last", "end_turn");
        last.started_at = frozen_now() - Duration::from_secs(60);
        last.ua_out = ua.into();
        t.ingest(last);
        let later = Instant::now() + Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS + 1);
        t.gc(later);
        let due = t.take_due(later);
        let f = &due[0];
        let names: Vec<&str> = f.events.iter().map(ev_name).collect();
        let at = names.iter().position(|n| *n == "tengu_config_cache_stats").unwrap();
        assert_eq!(names[at - 1] == "tengu_resume_hint_printed", want, "{ua}");
        if want {
            let m = meta_of(&f.events[at - 1]);
            assert_eq!(
                (&m["arg"], &m["title"], &m["worktree"], &m["advice"]),
                (&json!("id"), &json!("absent"), &json!("absent"), &json!(false))
            );
        }
    }
}

/// 2.1.299 的遥测（`cap/auto-2.1.299-20261014-full`，交互式默认模式的单轮会话）：
/// - 出站头不带 `mid-conversation-system` 了，会话级 `betas` 随之止于 `prompt-caching-scope`；
/// - `wire_shape_recorded` 照报、`systemTurns` 仍是 true（靠 `-clear-at` 认），haiku-4.5 照旧不报；
/// - tether 收尾多 `searchResultHistory: false`；启动段的配置拉取多 `stored_copy` 一系。
#[test]
fn version_2_1_299_telemetry_shape() {
    let run = |model: &str, effort: Option<&str>, betas: &str| {
        let t = Telemetry::default();
        let mut c = call(body_for(model, effort, true), "req_299", "end_turn");
        c.ua_out = "claude-cli/2.1.299 (external, cli)".into();
        c.betas = Some(betas.into());
        c.resp_model = Some(model.into());
        t.ingest(c);
        let st = t.0.state.lock();
        st.pending[&key()]
            .events
            .iter()
            .map(|(_, e)| {
                let d = &e["event_data"];
                let name = d["event_name"].as_str().or_else(|| d["experiment_id"].as_str());
                let meta =
                    if d["additional_metadata"].is_string() { meta_of(e) } else { Value::Null };
                (name.unwrap_or("?").to_string(), meta, d["betas"].clone())
            })
            .collect::<Vec<_>>()
    };
    let opus_betas = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
        thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
        per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,\
        advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
        effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
        extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    let h45_betas = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
        context-management-2025-06-27,prompt-caching-scope-2026-01-05,claude-code-20250219,\
        advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,thinking-binding-controls-2026-08-01,\
        thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
        cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    let opus = run("claude-opus-5-5", Some("medium"), opus_betas);
    let h45 = run("claude-haiku-4-5-20251001", None, h45_betas);
    let meta = |ev: &[(String, Value, Value)], n: &str| {
        ev.iter().find(|(x, ..)| x == n).map(|(_, m, _)| m.clone()).unwrap_or(Value::Null)
    };
    let wire = meta(&opus, "tengu_wire_shape_recorded");
    assert_eq!(wire["systemTurns"], true, "{wire}");
    assert_eq!(wire["keptReminders"], true);
    assert!(!h45.iter().any(|(n, ..)| n == "tengu_wire_shape_recorded"));
    let session = opus.iter().find(|(n, ..)| n == "tengu_started").unwrap().2.clone();
    assert_eq!(
        session,
        "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
         redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,\
         prompt-caching-scope-2026-01-05",
        "抓包里主线程会话 4841 条事件的那串"
    );
    let live = meta(&opus, "tengu_tether_live_outcome");
    let keys: Vec<&String> = live.as_object().unwrap().keys().collect();
    let at = keys.iter().position(|k| *k == "serverToolHistory").unwrap();
    assert_eq!(keys[at + 1], "searchResultHistory");
    let fetch = meta(&opus, "tengu_remote_settings_fetch");
    assert_eq!(fetch["stored_copy_gate"], "shadow", "{fetch}");
}

/// 2.1.299 的 Bash 命令画像：开头报模型（不带 `[1m]`），`interrupted` 后报 `zsh_note_in_prompt`（`!` 跑的
/// 不报），`zsh_nomatch_error` 后报三项判定；判据照可执行文件。
#[test]
fn bash_meta_v299_adds_the_new_judgements() {
    use super::tools::{bash_executed_meta, bash_meta_v293, bash_meta_v299, bash_profile};
    let cmd = "make test; echo ${PIPESTATUS[0]}";
    let mut m =
        bash_executed_meta(&bash_profile(cmd), 10, Some("toolu_1"), false, false, "default");
    bash_meta_v293(&mut m, "");
    bash_meta_v299(&mut m, cmd, "ok\n(eval):1: foo not found", "claude-opus-5-5[1m]", false);
    let keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys[0], "model");
    assert_eq!(m["model"], "claude-opus-5-5");
    let after = |k: &str| keys[keys.iter().position(|x| *x == k).unwrap() + 1];
    assert_eq!(after("interrupted"), "zsh_note_in_prompt");
    assert_eq!(after("zsh_nomatch_error"), "zsh_equals_error");
    assert_eq!(after("zsh_equals_error"), "bash_pipestatus_use");
    assert_eq!(after("bash_pipestatus_use"), "bash_word_split_use");
    assert_eq!((&m["zsh_equals_error"], &m["bash_pipestatus_use"]), (&json!(true), &json!(true)));

    let mut typed = bash_executed_meta(&bash_profile("ls"), 3, None, false, true, "default");
    bash_meta_v293(&mut typed, "");
    bash_meta_v299(&mut typed, "ls", "", "claude-sonnet-5-5", true);
    assert!(typed.get("zsh_note_in_prompt").is_none(), "`!` 跑的不报");
    assert_eq!(typed["zsh_equals_error"], false);

    use super::tools::zsh_equals_line;
    assert!(zsh_equals_line("(eval):12: =python not found"));
    assert!(!zsh_equals_line("(eval):1: two words not found"));
    assert!(!zsh_equals_line("zsh: command not found: foo"));
    assert!(!zsh_equals_line("(eval): foo not found"));
}

/// `bash_pipestatus_use` 与 `bash_word_split_use` 照官方 2.1.299 的判定，取值逐条核对可执行文件里那两个
/// 判定函数（本机把它们的原文连同官方自带的解析器拎出来当判题器；`bash_word_split_use` 的解析器是照它逐函数
/// 移植的，在约 12 万条命令——历次审查的反例、按语法上下文构造的、随机拼接的——上与官方逐条一致）。
#[test]
fn bash_judgements_match_the_official_logic() {
    use super::tools::{bash_pipestatus_use, bash_word_split_use};

    // --- bash_pipestatus_use：单引号 / 转义 / 更长变量名都是 false，真用了才 true。 ---
    for (cmd, want) in [
        ("echo '$PIPESTATUS'", false),
        ("echo \\$PIPESTATUS", false),
        ("echo $PIPESTATUS_backup", false),
        ("echo ${PIPESTATUS_backup}", false),
        ("echo $PIPESTATUS", true),
        ("echo ${PIPESTATUS}", true),
        ("make; echo ${PIPESTATUS[0]}", true),
        ("x=$PIPESTATUS; echo $x", true),
        ("echo \"$PIPESTATUS\"", true),
        ("echo $PIPESTATUS.bak", true),
        ("set -o pipefail", false),
    ] {
        assert_eq!(bash_pipestatus_use(cmd), want, "pipestatus: {cmd:?}");
    }

    // --- bash_word_split_use ---
    for (cmd, want) in [
        // 空格字面量 + 命令位置的无引号展开 → true；加引号 / echo / 没空格 → false。
        ("files='a b'; cat $files", true),
        ("files=\"a b\"; cat $files", true),
        ("files='a b'; cat \"$files\"", false),
        ("files='a b'; echo $files", false),
        ("name='John'; echo $name", false),
        ("single='word'; cat $single", false),
        ("v='two words'; grep -r $v .", true),
        // 命令替换 + for/select 迭代 → true。
        ("out=$(ls); for f in $out; do echo $f; done", true),
        ("out=$(ls); select f in $out; do break; done", true),
        ("files=$(ls); cat $files", false),
        // 声明命令按 declaration_command 计入赋值。
        ("declare x='a b'; cat $x", true),
        ("export f=$(ls); for a in $f; do :; done", true),
        ("declare -a x=$v; v='a b'", false),
        // 数组、+= 接缝、双引号里命令替换缩成 x。
        ("files=($v); v='two words'", true),
        ("v+=' b'; cat $v", true),
        ("v=' b'; cat $v", false),
        ("y=\"$(a b)c\"; cat $y", false),
        ("y=\"$(ls) more\"; cat $y", true),
        // 前置过滤：开了 zsh 拆词 / sh 模拟 → false；没有 $ / for / select / =quote → false。
        ("setopt shwordsplit; files='a b'; cat $files", false),
        ("emulate sh; files='a b'; cat $files", false),
        ("emulate -L bash; files='a b'; cat $files", false),
        ("ls -la", false),
        ("x='a b'", false),
        // 命令前缀赋值不计入；a=$a 这类非字面量不计入。
        ("files='a b' cat $files", false),
        ("a='x y'; b=$a; cat $b", false),
        // 递归子命令：子壳 / 命令组 / 命令替换 / 函数体里的赋值与展开都要收集。
        ("(files='a b'; cat $files)", true),
        ("{ files='a b'; cat $files; }", true),
        ("x=$(files='a b'; cat $files); echo $x", true),
        ("out=`files='a b'; cat $files`", true),
        ("f() { files='a b'; cat $files; }", true),
        ("g() ( files='a b'; cat $files )", true),
        // 注释到行尾：注释里的用法不计入。
        ("files='a b'; # cat $files", false),
        ("files='a b'; cat $files # done", true),
        ("# files='a b'; cat $files", false),
        // 双引号里的 `\"` 是转义，不结束引号——里面的 $files 仍在引号内、不算 split。
        ("files=\"a b\"; cat \"x\\\"$files\"", false),
        // ${...} 里的命令替换官方不按命令解析。
        ("cat ${x:-$(files='a b'; cat $files)}", false),
        // 算术不是命令，不收集。
        ("(( x=1 )); cat $x", false),
        // $(...) 平衡要认引号：括号在引号里不算闭合。
        ("x=$(echo \"(\"; files='a b'; cat $files)", true),
        ("x=$(echo ')'; files='a b'; cat $files)", true),
        ("files='a b'; cat $(echo \")\")$files", true),
        // 命令不完整也不 panic（多字节、未闭合）——只要不崩就行。
        ("files='a b'; cat $files; x=$(", true),
        ("x=$(files='a b'; cat $files", true),
        ("files='a b'; cat $(\u{4f60}\u{597d}", false),
        ("x=$(\u{6587}\u{4ef6}='a b'", false),
        // 子命令边界认引号 / 转义 / 注释（问题 2）：引号里、转义的、注释里的括号都不算闭合。
        ("echo $(printf ')'; files='a b'; cat $files)", true),
        ("x=$(echo \\); files='a b'; cat $files)", true),
        ("x=\"$(echo \")\"; files='a b'; cat $files)\"", true),
        ("x=$(# )\nfiles='a b'; cat $files)", true),
        ("f() { echo '}'; files='a b'; cat $files; }", true),
        ("f() { echo \"}\"; files='a b'; cat $files; }", true),
        // 外层双引号包着的 `$(...)`：注释里的 `)` 不能提前闭合，否则内层整段被双引号分支跳过。
        ("x=\"$(# )\nfiles='a b'; cat $files)\"", true),
        ("x=\"$(echo ')' # )\nfiles='a b'; cat $files)\"", true),
        ("x=\"$(echo a#b)\"; files='a b'; cat $files", true),
        ("echo ${#files}; files='a b'; cat $files", true),
        // 重定向目标不是命令参数；命令名之后第一处重定向起都算目标，命令名之前的只吃一个词。
        ("files='a b'; cat < $files", false),
        ("files='a b'; cat <$files", false),
        ("files='a b'; cat 2>$files", false),
        ("files='a b'; cat &> $files", false),
        ("files='a b'; cat >& $files", false),
        ("files='a b'; cat <<< $files", false),
        ("files='a b'; cat a>$files", false),
        ("files='a b'; cat < $files $files", false),
        ("files='a b'; cat 2>&1 $files", false),
        ("files='a b'; cat {fd}> $files", false),
        ("files='a b'; cat $files > out", true),
        ("files='a b'; cat $files>out", true),
        ("files='a b'; > $files cat $files", true),
        ("v='x y'; <in cat $v", true),
        ("files='a b'; cat <(ls) $files", true),
        // 条件表达式（`[[ ]]` 与 `[ ]` 都是 test_command）、`case WORD`、算术都不是命令参数；`test` 是普通命令。
        ("files='a b'; [[ $files == foo ]]", false),
        ("files='a b'; [ $files = foo ]", false),
        ("files='a b'; test $files", true),
        ("files='a b'; [[ -n $files ]] && cat $files", true),
        ("files='a b'; case $files in a) :;; esac", false),
        ("files='a b'; case x in a) cat $files;; esac", true),
        ("files='a b'; (( $files > 1 ))", false),
        // `if` / `elif` / `while` / `until` / `!` 后面是一条普通命令：echo 照样「拆不拆都一样」。
        ("files='a b'; if echo $files; then :; fi", false),
        ("files='a b'; if cat $files; then :; fi", true),
        ("files='a b'; until echo $files; do :; done", false),
        ("files='a b'; while cat $files; do :; done", true),
        ("files='a b'; if true; then :; elif echo $files; then :; fi", false),
        ("files='a b'; ! echo $files", false),
        ("files='a b'; ! cat $files", true),
        // here-doc 正文不是命令；不加引号的定界符正文里的命令替换照样递归。
        ("files='a b'; cat <<EOF\n$files\nEOF", false),
        ("files='a b'; cat <<'EOF'\n$files\nEOF", false),
        ("files='a b'; cat <<-EOF\n\t$files\n\tEOF", false),
        ("files='a b'; cat <<EOF\n$files\nEOF\ncat $files", true),
        ("files='a b'; cat <<EOF\n$(cat $files)\nEOF", true),
        ("files='a b'; cat <<EOF | grep $files\nx\nEOF", true),
        ("files='a b'; (( n = 1 << 2 )); cat $files", true),
        // `[[ … ]]` 整段是一个单元：里面的 `&&` / `||` 不切段；命令替换照样递归（单引号里的不算）。
        ("files='a b'; [[ -n $files && -f $files ]]", false),
        ("files='a b'; [[ -n $files || -f $files ]]", false),
        ("files='a b'; if [[ -n $files && -f $files ]]; then :; fi", false),
        ("files='a b'; ! [[ $files && $files ]]", false),
        ("files='a b'; [[ $files == ']]' && $files ]]", false),
        ("files='a b'; [[ -n $files && -f $files ]] && cat $files", true),
        ("files='a b'; [[ -n x && $(cat $files) ]]", true),
        ("files='a b'; [[ -n `cat $files` && x ]]", true),
        ("files='a b'; [[ '$(cat $files)' ]]", false),
        // here-doc 正文里的命令替换可以跨行；`${…}` 整段 opaque；正文里的反引号官方不认。
        ("files='a b'; cat <<EOF\n$(\ncat $files\n)\nEOF", true),
        ("files='a b'; cat <<EOF\nx $(cat\n$files) y\nEOF", true),
        ("files='a b'; cat <<'EOF'\n$(\ncat $files\n)\nEOF", false),
        ("files='a b'; cat <<EOF\n${x:-$(cat $files)}\nEOF", false),
        ("files='a b'; cat <<EOF\n${#x} $(cat $files)\nEOF", true),
        ("files='a b'; cat <<EOF\n`cat $files`\nEOF", false),
        // 括号边界扫描跳过 here-doc 字面正文：正文里的 `}` / `)` 不闭合函数体 / 命令替换。
        ("f() { cat <<'EOF'\n} v='a b'; cat $v\nEOF\n}", false),
        ("f() { cat <<EOF\n} v='a b'; cat $v\nEOF\n}", false),
        ("x=$(cat <<'EOF'\n) v='a b'; cat $v\nEOF\n)", false),
        ("f() { cat <<'EOF'\n}\nEOF\nv='a b'; cat $v; }", true),
        ("x=$(cat <<EOF\n)\nEOF\nv='a b'; cat $v)", true),
        ("f() { (( x = 1 << 2 )); v='a b'; cat $v; }", true),
        ("x=$(echo $((1<<2)); v='a b'; cat $v)", true),
        // 关键字之后的 `(( … ))` / `( … )` 照命令起始处理：算术里的命令替换递归、展开不计入。
        ("files='a b'; if (( $(cat $files) )); then :; fi", true),
        ("files='a b'; while (( $(cat $files) )); do :; done", true),
        ("files='a b'; until (( `cat $files` )); do :; done", true),
        ("files='a b'; ! (( $(cat $files) ))", true),
        ("files='a b'; if (( $files )); then :; fi", false),
        ("files='a b'; if (cat $files); then :; fi", true),
        // 双引号里的单引号是字面；`[[` 里的注释不算；续行后照样认出 `[[` / `((`。
        ("files='a b'; [[ \"it's $(cat $files)\" ]]", true),
        ("files='a b'; [[ -n x # $(cat $files)\n]]", false),
        ("files='a b'; [[ -n x # '\n]]; cat $files", true),
        ("files='a b'; if \\\n [[ -n $files && -f $files ]]; then :; fi", false),
        ("files='a b'; if \\\n (( $(cat $files) )); then :; fi", true),
        // 子壳里的 here-doc 正文、空定界符、正文里算术展开中的命令替换、case 后续分支的模式。
        ("f() { (cat <<'EOF'\n} v='a b'; cat $v\nEOF\n)\n}", false),
        ("files='a b'; cat <<''\ncat $files\n\n", false),
        ("files='a b'; cat <<EOF\n$(( $(cat $files) ))\nEOF", true),
        ("files='a b'; case x in a) :;; $files) :;; esac", false),
        // 官方解析器自己的怪癖也照样复现：一行两个 here-doc、here-doc 同一行后面的命令、`[ … -a … ]`。
        ("files='a b'; cat <<A <<B\n$files\nA\n$files\nB", true),
        ("files='a b'; cat <<EOF; cat $files\nbody\nEOF", false),
        ("files='a b'; [ -n $files -a -f $files ]", true),
    ] {
        assert_eq!(bash_word_split_use(cmd), want, "word_split: {cmd:?}");
    }
    // 不完整 / 多字节命令绝不 panic（问题 1：panic 会中断整条遥测消费）。
    for cmd in [
        "cat $(\u{4f60}\u{597d}",
        "x=$(\u{6587}",
        "files='a b'; cat ${\u{672a}",
        "v='a b'; (cat $v \u{6587}",
        "`\u{672a}\u{95ed} files='a b'",
        "cat \"\u{672a} $(files='a b'",
        "${x:-$(cat",
        "$((",
        "$(((",
        "`",
        "${",
        "$(",
        // 末尾单反斜杠：`\` 让下标顶过末尾，取段切片会越界（问题 1 的另一面）。
        "files='a b'; cat $files\\",
        "x='a b'\\",
        "cat \\",
        "\\",
        // 反引号内容以 `\` 结尾：截出的内层递归进去同样会顶过末尾。
        "`\\`",
        "x=`ls \\`; cat $x",
        "files='a b'; echo `cat $files\\`",
        "\"`\\`\"",
        "files='a b'; (cat $x)\\",
        "x=$(ls)\\",
        "files='a b'; cat ${x}\\",
    ] {
        let _ = bash_word_split_use(cmd);
        let _ = bash_pipestatus_use(cmd);
    }
    // 兜底：各种被截断 / 多字节的尾巴接在常见命令后都不该 panic。
    for tail in ["\\", "'", "\"", "`", "$(", "${", "$((", "\\\\", " \\", "\u{4f60}", "\"\u{672a}"] {
        for base in ["files='a b'; cat $files", "x='a b'", "cat $(echo", "for a in"] {
            let _ = bash_word_split_use(&format!("{base}{tail}"));
            let _ = bash_pipestatus_use(&format!("{base}{tail}"));
        }
    }

    // 极深的嵌套：超过栈预算就放弃（判 false，官方爆栈时同样被 catch 成 false），不能把线程栈压穿。tokio 的
    // 阻塞线程默认 2MiB 栈，这里照那个大小起线程跑。
    let deep = [
        format!("files='a b'; {}cat $files", "! ".repeat(5000)),
        format!("files='a b'; {}cat $files;{}", "{ ".repeat(5000), " }".repeat(5000)),
        format!("files='a b'; echo $(({}1{})); cat $files", "(".repeat(5000), ")".repeat(5000)),
        format!("files='a b'; [[ {}x{} ]]; cat $files", "( ".repeat(3000), " )".repeat(3000)),
        format!("files='a b'; x={}cat $files{}", "$(".repeat(3000), ")".repeat(3000)),
    ];
    for cmd in deep {
        let r = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || bash_word_split_use(&cmd))
            .unwrap()
            .join()
            .unwrap();
        assert!(!r, "极深嵌套超过栈预算应判 false");
    }
}

/// 2.1.299 会话里第一条 Bash 同批报 `shell_snapshot_create`：快照那段算进这次调用的准备时长
/// （`cap/auto-2.1.299-20261014-full`：快照 799 → 1017 = 外部 60 + 准备 938；842 → 879 = 26 + 850），不会
/// 报出准备只有十几毫秒、快照却要一秒的组合。快照与这次调用都在上一条回复结束到这条请求发出的那段
/// 间隔里：间隔短时快照跟着缩，时长不撑过间隔。
#[test]
fn first_bash_of_a_session_carries_the_shell_snapshot_time() {
    // 两条请求之间隔 `gap` 毫秒（上一条回复结束到这条请求发出），返回（快照, 时长, 外部, 准备）。
    let run = |gap: u64| {
        let t = Telemetry::default();
        let ua = "claude-cli/2.1.299 (external, cli)";
        let start = frozen_now() - Duration::from_secs(20);
        let mut first = call(cc_body(true), "req_1", "tool_use");
        first.text_chars = 0;
        first.started_at = start;
        first.ua_out = ua.into();
        let end = start + Duration::from_millis(first.total_ms);
        t.ingest(first);
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la","description":"list"}}]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"total 8\nfile"}]});
        let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
        second.started_at = end + Duration::from_millis(gap);
        second.ua_out = ua.into();
        t.ingest(second);
        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let metas = |n: &str| -> Vec<Value> {
            p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
        };
        let snap = metas("tengu_feature_ok")
            .into_iter()
            .find(|m| m["feature_name"] == "shell_snapshot_create")
            .unwrap()["duration_ms"]
            .as_i64()
            .unwrap();
        let ok = &metas("tengu_tool_use_success")[0];
        let n = |k: &str| ok[k].as_i64().unwrap();
        (snap, n("durationMs"), n("externalWallMs"), n("preExternalMs"))
    };

    // 间隔约 900ms（抓包里第一条 Bash 的间隔 734～1903ms，都是快命令）：总时长 ≈ 间隔减几十毫秒，快照
    // 填满准备、外部只剩二三十到六十几毫秒，准备占九成以上（`cap/auto-2.1.299-20261014-full`：间隔 891 →
    // 879 = 外部 26 + 准备 850、快照 842）。
    let (snap, dur, ext, pre) = run(900);
    assert_eq!(dur, 850, "间隔减 50ms 的收尾");
    assert!(pre > snap && pre < snap + 140, "准备 ≈ 快照再加几十毫秒: {pre} / {snap}");
    assert!((20..65).contains(&ext), "{ext}");
    assert!(pre * 100 / dur >= 90, "准备占九成以上");

    // 间隔宽（跑了好几秒的命令）：快照照常 700～1300，余量全算外部，总时长仍是间隔减收尾。
    let (snap, dur, ext, pre) = run(10_000);
    assert_eq!(dur, 9_950);
    assert!((700..1300).contains(&snap), "{snap}");
    assert!(pre > snap && pre < snap + 140, "{pre} / {snap}");
    assert!(dur - pre - ext < 15 && ext > 8_000, "余量在外部: {ext}");

    // 间隔只有几百毫秒：时长不撑过间隔，快照跟着缩，仍是准备的一部分。
    for gap in [300, 40] {
        let (snap, dur, ext, pre) = run(gap);
        assert!(dur <= gap as i64, "gap {gap}: {dur}");
        assert!(snap <= pre && pre + ext <= dur, "gap {gap}: {snap} {pre} {ext} {dur}");
        assert!(pre * 100 / dur.max(1) >= 80, "gap {gap}: 按比例分、取整: {pre}/{dur}");
    }

    use super::events::bash_timing;
    // 性质：任意扰动与时长，各段都在时长之内，快照不超过准备；第一条放得下像样快照时准备恒占九成以上。
    for h in 0..100 {
        for duration in [1, 12, 300, 720, 829, 900, 2_000, 6_000] {
            let t = bash_timing(duration, h, Some(1_300));
            assert_eq!(t.duration, duration.max(1), "总时长就是推出来的那份: {h} {t:?}");
            assert!(t.pre + t.external <= t.duration, "{h} {duration} {t:?}");
            let snap = t.snapshot.unwrap();
            assert!(snap <= t.pre, "{h} {duration} {t:?}");
            if snap >= 700 && duration <= 1_500 {
                assert!(
                    t.pre * 100 / t.duration >= 90,
                    "快命令准备占九成以上: {h} {duration} {t:?}"
                );
            }
        }
        let t = bash_timing(110, h, None);
        assert_eq!((t.duration, t.snapshot), (110, None), "之后的 Bash 总时长照旧");
        assert!((14..=50).contains(&(t.pre * 100 / 110)), "准备占两到五成（取整）: {t:?}");
        assert!(t.external + t.pre <= 110);
    }
}

/// 问题 3：default 模式下要用户点头的首条 Bash，间隔里大部分是盯着弹框的等待。官方把等待记进
/// `permissionDurationMs`、不进 `durationMs` / `externalWallMs`。重建把超出「批准后合理执行」的部分挪进
/// `permissionDurationMs`，不让 `externalWallMs` 吞下整段等待。
#[test]
fn a_prompted_bash_keeps_the_dialog_wait_out_of_external_time() {
    // `make test` 不是只读命令，default 模式下要弹框点头；间隔 `gap` 毫秒里大半是等待。
    let run = |gap: u64| {
        let t = Telemetry::default();
        let ua = "claude-cli/2.1.299 (external, cli)";
        let start = frozen_now() - Duration::from_secs(30);
        let mut first = call(cc_body(true), "req_1", "tool_use");
        first.text_chars = 0;
        first.started_at = start;
        first.ua_out = ua.into();
        let end = start + Duration::from_millis(first.total_ms);
        t.ingest(first);
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["system"][3]["text"] = json!("base rules"); // 去掉 auto 标记 → default 模式
        body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"make test","description":"build"}}]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]});
        let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
        second.started_at = end + Duration::from_millis(gap);
        second.ua_out = ua.into();
        t.ingest(second);
        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let ok = p
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_tool_use_success")
            .map(|(_, e)| meta_of(e))
            .unwrap();
        let shown =
            p.events.iter().any(|(_, e)| ev_name(e) == "tengu_tool_use_show_permission_request");
        assert!(shown, "非只读 Bash 在 default 模式下要弹框");
        let n = |k: &str| ok[k].as_i64().unwrap();
        (n("durationMs"), n("externalWallMs"), n("preExternalMs"), n("permissionDurationMs"))
    };

    // 用户盯着弹框等了约 10 秒：externalWallMs 封顶（不会报出八九秒），等待进 permissionDurationMs。
    let (dur, ext, pre, perm) = run(10_000);
    assert!(ext <= 80, "外部执行封顶，不吞等待: {ext}");
    assert!((700..1400).contains(&pre), "快照照旧: {pre}");
    assert!(dur < 1_500, "时长是批准后执行（快照 + 小外部），不含等待: {dur}");
    assert!(perm > 8_000, "约 10 秒的等待进了 permissionDurationMs: {perm}");

    // 点头很快（间隔短）：行为与没弹框时相近，不硬塞一段假等待。
    let (dur, ext, _pre, perm) = run(900);
    assert!(ext <= 80 && dur <= 900, "{ext} {dur}");
    assert!(perm < 200, "没怎么等，permissionDurationMs 不虚高: {perm}");
}

/// 问题 3（混合批次）：一批里既有自动放行的 Read、又有要点头的 Bash 时，盯弹框的等待只进 Bash 的
/// `permissionDurationMs`，不摊到 Read 的时长上——Read 仍报它自己那点执行时长。
#[test]
fn a_mixed_batch_keeps_the_dialog_wait_off_the_auto_approved_tool() {
    let t = Telemetry::default();
    let ua = "claude-cli/2.1.299 (external, cli)";
    let start = frozen_now() - Duration::from_secs(30);
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.text_chars = 0;
    first.started_at = start;
    first.ua_out = ua.into();
    let end = start + Duration::from_millis(first.total_ms);
    t.ingest(first);
    // 一批两个工具：Read（只读、自动放行）+ 非只读 Bash（default 模式要点头）；间隔约 10 秒。
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["system"][3]["text"] = json!("base rules"); // default 模式
    body["messages"][1] = json!({"role":"assistant","content":[
        {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/x"}},
        {"type":"tool_use","id":"t2","name":"Bash","input":{"command":"make test","description":"build"}}
    ]});
    body["messages"][2] = json!({"role":"user","content":[
        {"type":"tool_result","tool_use_id":"t1","content":"file"},
        {"type":"tool_result","tool_use_id":"t2","content":"ok"}
    ]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.started_at = end + Duration::from_millis(10_000);
    second.ua_out = ua.into();
    t.ingest(second);

    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let by_tool = |name: &str| -> Value {
        p.events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_tool_use_success")
            .map(|(_, e)| meta_of(e))
            .find(|m| m["toolName"] == name)
            .unwrap()
    };
    let read = by_tool("Read");
    let bash = by_tool("Bash");
    // Read 自动放行：时长很小、permissionDurationMs 不含等待。
    assert!(read["durationMs"].as_i64().unwrap() < 300, "Read 不该分到弹框等待: {read}");
    assert_eq!(read["permissionDurationMs"], 11);
    // 等待进了 Bash 的 permissionDurationMs。
    assert!(
        bash["permissionDurationMs"].as_i64().unwrap() > 8_000,
        "等待进 Bash 的 permissionDurationMs: {bash}"
    );
    assert!(bash["durationMs"].as_i64().unwrap() < 1_500, "Bash 时长不含等待: {bash}");
}

/// 2.1.300 的遥测（`cap/auto-2.1.300-20261015-full`）：启动段两条配置拉取多 `credential_road: "none"`，
/// 紧跟 `token_source`；2.1.299 没有。auto 模式判决多 `coworkWebAllowOverride`，紧跟
/// `mcpRemoteSessionAllowOverride`。
#[test]
fn version_2_1_300_telemetry_shape() {
    let fetch = |ua: &str| {
        let t = Telemetry::default();
        let mut c = call(body_for("claude-opus-5-5", Some("medium"), true), "req_300", "end_turn");
        c.ua_out = ua.into();
        t.ingest(c);
        let st = t.0.state.lock();
        st.pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_remote_settings_fetch")
            .map(|(_, e)| meta_of(e))
            .unwrap()
    };
    let new = fetch("claude-cli/2.1.300 (external, cli)");
    let keys: Vec<&String> = new.as_object().unwrap().keys().collect();
    let at = keys.iter().position(|k| *k == "token_source").unwrap();
    assert_eq!(keys[at + 1], "credential_road", "{new}");
    assert_eq!(new["credential_road"], "none");
    assert!(fetch("claude-cli/2.1.299 (external, cli)").get("credential_road").is_none());

    let mut d = json!({"mcpAlwaysAllowOverride": false, "mcpServerAskOverride": false});
    super::tools::auto_mode_decision_v291(&mut d);
    super::tools::auto_mode_decision_v300(&mut d);
    let keys: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "mcpAlwaysAllowOverride",
            "mcpRemoteSessionAllowOverride",
            "coworkWebAllowOverride",
            "mcpServerAskOverride",
            "fromTurnHandoff"
        ]
    );
}

/// 2.1.307（`cap/auto-2.1.307-20261022-full` / `-retry`）：工具在弹框上被拒，`can_use_tool_rejected` 之后同一刻多一条
/// `tengu_safety_event`；auto 判决走分类器那种在 `classifierSource` 之后多 `skippedOnAllowRule: false`（快路径那种
/// 不带）；`/compact` 的压缩尝试末尾多 `keepNoRound: false`；5xx 重试带 2.1.307 自己的栈三样。2.1.306 都没有。
/// 主线程回复里的 Bash 在权限弹框上被用户拒了、接着敲了下一句：两条请求过完，按顺序的事件名与 meta。
fn user_rejected_events(ua: &str, cred: i64) -> Vec<(String, Value)> {
    let base = frozen_now() - Duration::from_secs(120);
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = ua.into();
    first.cred_id = cred;
    first.started_at = base;
    first.tool_calls = vec![ToolCall {
        id: "t1".into(),
        name: "Bash".into(),
        input: json!({"command": "rm x"}),
        verdict: None,
    }];
    first.tool_use_lens = vec![("Bash".into(), 16)];
    t.ingest(first);
    let mut body: Value = serde_json::from_slice(&with_prompt(cc_body(true), PROMPT_2)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"rm x"}}]});
    body["messages"][2] = json!({"role":"user","content":[
        {"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed."},
        {"type":"text","text":"[Request interrupted by user for tool use]\n"},
        {"type":"text","text":"ok, keep it"}
    ]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.ua_out = ua.into();
    second.cred_id = cred;
    second.started_at = base + Duration::from_secs(30);
    t.ingest(second);
    let st = t.0.state.lock();
    st.pending[&(cred, SESSION.to_string())]
        .events
        .iter()
        .map(|(_, e)| (ev_name(e).to_string(), meta_of(e)))
        .collect()
}

#[test]
fn version_2_1_307_telemetry_shape() {
    let base = frozen_now() - Duration::from_secs(120);
    let rejected = |ua: &str| user_rejected_events(ua, 7);
    let ev = rejected("claude-cli/2.1.307 (external, cli)");
    let i = ev.iter().position(|(n, _)| n == "tengu_tool_use_can_use_tool_rejected").unwrap();
    assert_eq!(ev[i + 1].0, "tengu_safety_event", "紧跟拒绝那条");
    let s = &ev[i + 1].1;
    let keys: Vec<&str> = s
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .skip_while(|k| *k != "category")
        .collect();
    assert_eq!(
        keys,
        [
            "category",
            "decision",
            "decision_source",
            "reason_code",
            "tool_class",
            "tool_name",
            "tool_source",
            "permission_mode",
            "tool_use_id",
            "sandbox_violation_count",
            "sandbox_enabled"
        ],
        "{s}"
    );
    for (k, v) in [
        ("category", json!("user_rejected")),
        ("decision", json!("reject")),
        ("decision_source", json!("user_reject")),
        ("reason_code", json!("unknown")),
        ("tool_class", json!("bash")),
        ("tool_name", json!("Bash")),
        ("tool_source", json!("builtin")),
        ("tool_use_id", json!("t1")),
        ("sandbox_violation_count", json!(0)),
        ("sandbox_enabled", json!(false)),
    ] {
        assert_eq!(s[k], v, "{k}");
    }
    let old = rejected("claude-cli/2.1.306 (external, cli)");
    assert!(old.iter().all(|(n, _)| n != "tengu_safety_event"));

    let mut full = json!({"classifierQueueWaitMs": 0, "classifierSource": "server",
        "classifierTotalWaitMs": 900});
    super::tools::auto_mode_decision_v307(&mut full);
    let keys: Vec<&str> = full.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "classifierQueueWaitMs",
            "classifierSource",
            "skippedOnAllowRule",
            "classifierTotalWaitMs"
        ]
    );
    assert_eq!(full["skippedOnAllowRule"], false);
    let mut fast = json!({"classifierSource": "server",
        "serverClassifierNoVerdict": "server_call_skipped", "classifierTotalWaitMs": 0});
    let before = fast.clone();
    super::tools::auto_mode_decision_v307(&mut fast);
    assert_eq!(fast, before, "快路径那种不带");

    let compact_attempt = |ua: &str| {
        let t = Telemetry::default();
        let mut first = call(cc_body(true), "req_1", "end_turn");
        first.ua_out = ua.into();
        first.started_at = base;
        t.ingest(first);
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"] = json!([
            {"role":"user","content":[{"type":"text","text":"hello there"}]},
            {"role":"assistant","content":[{"type":"text","text":"hi"}]},
            {"role":"user","content":[{"type":"text","text":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools."}]}
        ]);
        let mut c = call(body.to_string().into_bytes(), "req_c", "end_turn");
        c.ua_out = ua.into();
        c.started_at = base + Duration::from_secs(30);
        c.agent = AgentHeaders {
            agent_id: None,
            agent_type: None,
            request_class: Some("compaction".into()),
        };
        t.ingest(c);
        let st = t.0.state.lock();
        st.pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_reactive_compact_attempt")
            .map(|(_, e)| meta_of(e))
            .unwrap()
    };
    let attempt = compact_attempt("claude-cli/2.1.307 (external, cli)");
    let keys: Vec<&str> = attempt.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys[keys.len() - 2..], ["headTruncations", "keepNoRound"], "{attempt}");
    assert_eq!(attempt["keepNoRound"], false);
    assert!(compact_attempt("claude-cli/2.1.306 (external, cli)").get("keepNoRound").is_none());

    for (ua, frame) in [
        ("claude-cli/2.1.307 (external, sdk-cli)", "chunk-2f0n1d7c.js:11:1510"),
        // 2.1.308 只换了块名（`cap/auto-2.1.308-20261022-retry` 三条同值）。
        ("claude-cli/2.1.308 (external, sdk-cli)", "chunk-zht3j5ev.js:11:1510"),
    ] {
        let ev = retried_sdk_turn(4, false, ua);
        for (n, m) in ev.iter().filter(|(n, _)| n == "tengu_api_retry" || n == "tengu_api_error") {
            assert_eq!(
                (&m["error_constructor"], &m["error_stack_hash"], &m["error_top_frame"]),
                (&json!("WVt"), &json!("5b32b751db51"), &json!(frame)),
                "{ua} {n}"
            );
        }
    }

    // 2.1.309 的 5xx 不带 `error_constructor`（压缩后的 5xx 类名 `xqt` 首字母小写，过不了官方对
    // 构造名的校验），栈哈希不变、顶帧换了块名（`cap/auto-2.1.309-20261023-retry` 两条同值）。
    let ev = retried_sdk_turn(4, false, "claude-cli/2.1.309 (external, sdk-cli)");
    for (n, m) in ev.iter().filter(|(n, _)| n == "tengu_api_retry" || n == "tengu_api_error") {
        assert!(m.get("error_constructor").is_none(), "2.1.309 {n} 不带 error_constructor");
        assert_eq!(
            (&m["error_stack_hash"], &m["error_top_frame"]),
            (&json!("5b32b751db51"), &json!("chunk-t24ds32q.js:11:1510")),
            "2.1.309 {n}"
        );
    }
}

/// 2.1.310 的遥测（`cap/auto-2.1.310-20261024-{full,retry}`）：
/// - 5xx 仍不带 `error_constructor`，栈哈希不变，顶帧只换块名（retry 三条同值）；
/// - 组织配置闸的影子判定在 `settings_cache_age_ms` 后多五项：签名副本两项恒 `trusted`、现拉签名那侧
///   还没答复（`unavailable_pending`）报 `no_answer`、否则 `valid`，`decision_if_strict` 与 `decision` 同值；
///   2.1.309 不带。
#[test]
fn version_2_1_310_telemetry_shape() {
    let ev = retried_sdk_turn(4, false, "claude-cli/2.1.310 (external, sdk-cli)");
    for (n, m) in ev.iter().filter(|(n, _)| n == "tengu_api_retry" || n == "tengu_api_error") {
        assert!(m.get("error_constructor").is_none(), "2.1.310 {n} 不带 error_constructor");
        assert_eq!(
            (&m["error_stack_hash"], &m["error_top_frame"]),
            (&json!("5b32b751db51"), &json!("chunk-36ysyspx.js:11:1510")),
            "2.1.310 {n}"
        );
    }

    let shadow = |ver: &str| {
        let t = Telemetry::default();
        let mut c = call(body_for("claude-opus-5-5", Some("medium"), true), "req_310", "end_turn");
        c.ua_out = format!("claude-cli/{ver} (external, cli)");
        t.ingest(c);
        let st = t.0.state.lock();
        st.pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_org_config_gate_shadow")
            .map(|(_, e)| meta_of(e))
            .unwrap()
    };
    let m = shadow("2.1.310");
    let keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
    let at = keys.iter().position(|k| *k == "settings_cache_age_ms").unwrap();
    assert_eq!(
        keys[at + 1..at + 7],
        [
            "settings_stored_copy",
            "policy_stored_copy",
            "settings_live_signed",
            "policy_live_signed",
            "decision_if_strict",
            "shadow_trigger"
        ]
    );
    assert_eq!(
        (&m["settings_stored_copy"], &m["policy_stored_copy"]),
        (&json!("trusted"), &json!("trusted"))
    );
    let live = |reason: &Value| if reason == "unavailable_pending" { "no_answer" } else { "valid" };
    assert_eq!(m["settings_live_signed"], json!(live(&m["settings_reason"])));
    assert_eq!(m["policy_live_signed"], json!(live(&m["policy_reason"])));
    assert_eq!(m["decision_if_strict"], m["decision"]);
    // 服务端配置那几项按 310 抓包的主流值（2.1.305 起那两份模板）。
    assert_eq!(
        (&m["decision"], &m["exclusion_now"], &m["flag_enabled"], &m["flag_percent"]),
        (&json!("pass"), &json!("rollout_percent"), &json!(true), &json!(1))
    );
    assert_eq!((&m["grace_ttl_hours"], &m["settings_live_signed"]), (&json!(24), &json!("valid")));
    // 2.1.293 ~ 2.1.303 不等配置落定就报，模板保持抓包里的 `blocked` + fallback 那套（`decision_reason` 等只跟
    // `blocked` 一起出现），不跟着改。
    let old_tpl = super::template::template_for("2.1.303", false)
        .startup
        .iter()
        .find(|e| e.name == "tengu_org_config_gate_shadow")
        .map(|e| e.meta.clone())
        .unwrap();
    assert_eq!(
        (&old_tpl["decision"], &old_tpl["flag_source"], &old_tpl["decision_reason"]),
        (&json!("blocked"), &json!("fallback"), &json!("unavailable_pending"))
    );

    let old = shadow("2.1.309");
    for k in ["settings_stored_copy", "policy_live_signed", "decision_if_strict"] {
        assert!(old.get(k).is_none(), "2.1.309 不带 {k}");
    }
}

/// `tengu_lapis_anchor_from_client_data` 只在请求体真按那一档发的时候报（号的 bootstrap 都下发了 off）：完整模拟
/// 按钉住的那一档报；billing-only 与真实 CC 透传发的是客户端自己的 system 与 messages，看出站体里（system 那段，
/// 或换了 system 的客户端只在 messages 里跟的那条）有没有 `<total_tokens>`，与 off 对得上（没有）才报。
#[test]
fn lapis_event_follows_what_the_body_actually_did() {
    let boot = br#"{"client_data":{"tengu_lapis_anchor":"off"}}"#;
    // `reminder`：提醒放在哪（`""` 没有、`"system"` system 末块、`"messages"` 末尾一条 system 消息）。
    let first_turn = |cred_id: i64, bare: bool, client_tools: bool, reminder: &str| {
        crate::proxy::client_data::note_bootstrap(cred_id, "claude-opus-5-5", boot);
        let mut body: Value =
            serde_json::from_slice(&body_for("claude-opus-5-5", Some("medium"), true)).unwrap();
        let line = "<total_tokens>15000000 tokens left</total_tokens>";
        if reminder == "system" {
            let last = body["system"].as_array_mut().unwrap().last_mut().unwrap();
            let text = last["text"].as_str().unwrap().to_string();
            last["text"] = json!(format!("{text}\n\n{line}"));
        } else if reminder == "messages" {
            let msgs = body["messages"].as_array_mut().unwrap();
            msgs.push(json!({"role":"system","content":[{"type":"text","text":line}]}));
        }
        let t = Telemetry::default();
        let mut c = call(body.to_string().into_bytes(), "req_lapis", "end_turn");
        c.ua_out = "claude-cli/2.1.311 (external, cli)".into();
        c.cred_id = cred_id;
        c.bare = bare;
        c.client_tools = client_tools;
        t.ingest(c);
        let st = t.0.state.lock();
        st.pending[&(cred_id, SESSION.to_string())]
            .events
            .iter()
            .map(|(_, e)| ev_name(e).to_string())
            .collect::<Vec<_>>()
    };
    let lapis = |names: &[String]| {
        names.iter().filter(|n| *n == "tengu_lapis_anchor_from_client_data").count()
    };
    assert_eq!(lapis(&first_turn(93_221, false, false, "")), 1, "完整模拟按钉住的那一档报");
    assert_eq!(
        lapis(&first_turn(93_222, true, false, "")),
        1,
        "billing-only 的第三方 system 本来就没有提醒，与 off 对得上"
    );
    let bare = first_turn(93_225, true, false, "system");
    assert!(bare.iter().any(|n| n == "tengu_mcp_instructions_pool_change"), "启动那段照报");
    assert_eq!(lapis(&bare), 0, "billing-only 下真实 CC 自己照发提醒，与 off 对不上");
    assert_eq!(lapis(&first_turn(93_223, false, true, "")), 1, "透传的体里没有提醒，与 off 对得上");
    assert_eq!(
        lapis(&first_turn(93_224, false, true, "system")),
        0,
        "透传的体里照发提醒，与 off 对不上"
    );
    assert_eq!(
        lapis(&first_turn(93_226, false, true, "messages")),
        0,
        "换了 system 的客户端（Agent SDK / --system-prompt）提醒只在 messages 里，照样对不上"
    );
}

/// 预算与用户轮两项也由 `client_data` 给出时，首次输入那段在档位那条之后按序再各报一条（`{userTurn}`、`{budget}`）。
/// 透传的体里那个最大的数不是下发的预算（客户端照默认 1500 万），预算那条不报。
#[test]
fn lapis_budget_and_user_turn_events_follow_the_mode() {
    let boot = br#"{"client_data":{"tengu_lapis_anchor":"padded-countdown","tengu_lapis_anchor_user_turn":true,"tengu_lapis_anchor_budget":500000}}"#;
    let first_turn = |cred_id: i64, client_tools: bool| {
        crate::proxy::client_data::note_bootstrap(cred_id, "claude-opus-5-5", boot);
        let mut body: Value =
            serde_json::from_slice(&body_for("claude-opus-5-5", Some("medium"), true)).unwrap();
        let last = body["system"].as_array_mut().unwrap().last_mut().unwrap();
        let text = last["text"].as_str().unwrap().to_string();
        let n = if client_tools { 15_000_000 } else { 500_000 };
        last["text"] = json!(format!("{text}\n\n<total_tokens>{n} tokens left</total_tokens>"));
        let t = Telemetry::default();
        let mut c = call(body.to_string().into_bytes(), "req_lapis", "end_turn");
        c.ua_out = "claude-cli/2.1.311 (external, cli)".into();
        c.cred_id = cred_id;
        c.client_tools = client_tools;
        t.ingest(c);
        let st = t.0.state.lock();
        st.pending[&(cred_id, SESSION.to_string())]
            .events
            .iter()
            .map(|(_, e)| e.clone())
            .collect::<Vec<_>>()
    };
    let ev = first_turn(93_227, false);
    let names: Vec<&str> = ev.iter().map(ev_name).collect();
    let at = names.iter().position(|n| *n == "tengu_mcp_instructions_pool_change").unwrap();
    let lapis: Vec<Value> = ev[at + 1..at + 4].iter().map(meta_of).collect();
    assert!(names[at + 1..at + 4].iter().all(|n| *n == "tengu_lapis_anchor_from_client_data"));
    assert_eq!(lapis[0]["mode"], "padded-countdown");
    assert_eq!(lapis[1]["userTurn"], true);
    assert_eq!(lapis[2]["budget"], 500_000);
    assert_eq!(names.iter().filter(|n| **n == "tengu_lapis_anchor_from_client_data").count(), 3);

    let ev = first_turn(93_228, true);
    let metas: Vec<Value> = ev
        .iter()
        .filter(|e| ev_name(e) == "tengu_lapis_anchor_from_client_data")
        .map(meta_of)
        .collect();
    assert_eq!(metas.len(), 2, "预算对不上体里的数");
    assert!(metas.iter().all(|m| m.get("budget").is_none()));
}

/// 遥测里的浮点照 JS 写：值恰好是整数时不带 `.0`（serde_json 默认会写成 `25.0`）。
#[test]
fn floats_are_written_like_json_stringify() {
    assert_eq!(js_number(25.0).to_string(), "25");
    assert_eq!(js_number(-0.0).to_string(), "0");
    assert_eq!(js_number(32.866).to_string(), "32.866");
    assert_eq!(js_number(0.1 + 0.2).to_string(), "0.30000000000000004");
    assert_eq!(js_number(f64::NAN), Value::Null);
    // `!` 命令那条在 `h = 0` 时耗时正好 25ms；命令自己 `&` 起了后台任务时 `left_process_running` 为真。
    let mut meta = json!({"bash_subcommand":"run","has_pipe":false,"interrupted":false,"was_backgrounded":false});
    super::tools::bash_meta_v311(
        &mut meta,
        "npm run dev &",
        Some(super::tools::ShellUsage::typed(0)),
    );
    let text = meta.to_string();
    assert!(text.contains(r#""duration_ms":25,"#), "{text}");
    assert!(!text.contains(".0,") && !text.contains(".0}"), "{text}");
    assert_eq!(meta["left_process_running"], true);
}

/// 2.1.311 的遥测（`cap/auto-2.1.311-20261027-{full,retry,bashfail}`）：
/// - 5xx 仍不带 `error_constructor`，栈哈希不变，顶帧只换块名；
/// - 启动那条 `tengu_init` 在 `rendererEntryPath` 后多 `config_dir_kind: "unset"`；
/// - `env.node_version` 换成 `v26.12.1`（内嵌 Bun 换了构建），2.1.310 仍是 `v26.3.0`；
/// - 事件顶层的会话级 `betas` 不再带 `thinking-token-count`（出站头里照带，API 事件顶层报的完整串也照带）；
/// - Bash 执行事件多 `bash_command_kinds`（`bash_last_argv0` 之后）、`interrupted` 之后的耗时与 CPU 四项、
///   `was_backgrounded` 之后的 `same_as_running_background`；
/// - 这个号这个模型的 bootstrap 下发了 `tengu_lapis_anchor:"off"` 时，首次输入那段在
///   `mcp_instructions_pool_change` 之后紧跟一条 `tengu_lapis_anchor_from_client_data {mode: "off"}`。
#[test]
fn version_2_1_311_telemetry_shape() {
    let ev = retried_sdk_turn(4, false, "claude-cli/2.1.311 (external, sdk-cli)");
    for (n, m) in ev.iter().filter(|(n, _)| n == "tengu_api_retry" || n == "tengu_api_error") {
        assert!(m.get("error_constructor").is_none(), "2.1.311 {n} 不带 error_constructor");
        assert_eq!(
            (&m["error_stack_hash"], &m["error_top_frame"]),
            (&json!("5b32b751db51"), &json!("chunk-ecgnkn41.js:11:1510")),
            "2.1.311 {n}"
        );
    }

    // 2.1.311 的 opus 主线程出站头（`cap/auto-2.1.311-20261027-full/00217`）。
    let opus311 = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,prompt-caching-scope-2026-01-05,per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    let first_turn = |ver: &str, cred_id: i64| {
        let t = Telemetry::default();
        let mut c = call(body_for("claude-opus-5-5", Some("medium"), true), "req_311", "end_turn");
        c.ua_out = format!("claude-cli/{ver} (external, cli)");
        c.cred_id = cred_id;
        c.betas = Some(opus311.into());
        t.ingest(c);
        let st = t.0.state.lock();
        st.pending[&(cred_id, SESSION.to_string())]
            .events
            .iter()
            .map(|(_, e)| e.clone())
            .collect::<Vec<_>>()
    };
    let ev = first_turn("2.1.311", 93_211);
    let init = ev.iter().find(|e| ev_name(e) == "tengu_init").map(meta_of).unwrap();
    let keys: Vec<&str> = init.as_object().unwrap().keys().map(String::as_str).collect();
    let at = keys.iter().position(|k| *k == "rendererEntryPath").unwrap();
    assert_eq!(keys[at + 1], "config_dir_kind");
    assert_eq!(init["config_dir_kind"], "unset");
    for e in &ev {
        assert_eq!(e["event_data"]["env"]["node_version"], "v26.12.1", "{}", ev_name(e));
        // API 事件顶层报这条请求的完整串（与 meta 同值，带 ttc），其余报会话级那份。
        if ev_name(e).starts_with("tengu_api_") {
            continue;
        }
        let betas = e["event_data"]["betas"].as_str().unwrap_or_default();
        assert!(!betas.contains("thinking-token-count"), "{}: {betas}", ev_name(e));
    }
    assert!(ev.iter().any(|e| e["event_data"]["betas"]
        == "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05"));
    assert!(
        !ev.iter().any(|e| ev_name(e) == "tengu_lapis_anchor_from_client_data"),
        "没见过 bootstrap 不报"
    );
    let old = first_turn("2.1.310", 93_212);
    let init = old.iter().find(|e| ev_name(e) == "tengu_init").map(meta_of).unwrap();
    assert!(init.get("config_dir_kind").is_none(), "2.1.310 不带");
    assert!(old.iter().all(|e| e["event_data"]["env"]["node_version"] == "v26.3.0"));
    assert!(old.iter().any(|e| {
        e["event_data"]["betas"].as_str().is_some_and(|b| b.contains("thinking-token-count"))
    }));

    // bootstrap 下发了 off：首次输入那段 `mcp_instructions_pool_change` 之后紧跟一条。
    let boot = br#"{"client_data":{"tengu_lapis_anchor":"off"}}"#;
    crate::proxy::client_data::note_bootstrap(93_213, "claude-opus-5-5", boot);
    let ev = first_turn("2.1.311", 93_213);
    let names: Vec<&str> = ev.iter().map(ev_name).collect();
    let at = names.iter().position(|n| *n == "tengu_mcp_instructions_pool_change").unwrap();
    assert_eq!(names[at + 1], "tengu_lapis_anchor_from_client_data");
    assert_eq!(meta_of(&ev[at + 1])["mode"], "off");
    assert_eq!(names.iter().filter(|n| **n == "tengu_lapis_anchor_from_client_data").count(), 1);

    // Bash：续轮那条 tool_result 报命令执行。
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.text_chars = 0;
    first.started_at = frozen_now() - Duration::from_secs(20);
    first.ua_out = "claude-cli/2.1.311 (external, cli)".into();
    t.ingest(first);
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"git diff | head -5","description":"diff"}}]});
    body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"diff --git a b"}]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.started_at = frozen_now() - Duration::from_secs(10);
    second.ua_out = "claude-cli/2.1.311 (external, cli)".into();
    t.ingest(second);
    let st = t.0.state.lock();
    let bash = st.pending[&key()]
        .events
        .iter()
        .find(|(_, e)| ev_name(e) == "tengu_bash_tool_command_executed")
        .map(|(_, e)| meta_of(e))
        .unwrap();
    let keys: Vec<&str> = bash.as_object().unwrap().keys().map(String::as_str).collect();
    let after = |k: &str| keys[keys.iter().position(|x| *x == k).unwrap() + 1];
    assert_eq!(after("bash_subcommand"), "bash_command_kinds");
    assert_eq!(after("bash_command_kinds"), "has_pipe");
    assert_eq!(bash["bash_command_kinds"], "file_read,vcs", "官方 yte：git diff | head");
    assert_eq!(after("interrupted"), "duration_ms");
    assert_eq!(after("duration_ms"), "command_cpu_user_ms");
    assert_eq!(after("command_cpu_system_ms"), "left_process_running");
    assert_eq!(after("left_process_running"), "zsh_note_in_prompt");
    assert_eq!(after("was_backgrounded"), "same_as_running_background");
    assert_eq!(bash["same_as_running_background"], false);
    assert_eq!(bash["left_process_running"], false);
    let wall = bash["duration_ms"].as_f64().unwrap();
    let cpu = bash["command_cpu_user_ms"].as_f64().unwrap()
        + bash["command_cpu_system_ms"].as_f64().unwrap();
    assert!(wall > 0.0 && cpu > 0.0 && cpu < wall * 1.2, "{wall} {cpu}");
    assert_eq!((wall * 1000.0).round() / 1000.0, wall, "三位小数");
    drop(st);

    // 失败事件（`cap/auto-2.1.311-20261027-bashfail` 那条）：没有 `was_backgrounded`，`same_as_running_background`
    // 排在 `had_sandbox_violation` 之后；耗时与 CPU 照报。
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = "claude-cli/2.1.311 (external, cli)".into();
    t.ingest(first);
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls /nonexistent_dir_luban_311","description":"fail"}}]});
    body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"Exit code 1\nls: /nonexistent_dir_luban_311: No such file or directory"}]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.ua_out = "claude-cli/2.1.311 (external, cli)".into();
    t.ingest(second);
    let st = t.0.state.lock();
    let failed = st.pending[&key()]
        .events
        .iter()
        .find(|(_, e)| ev_name(e) == "tengu_bash_tool_command_failed")
        .map(|(_, e)| meta_of(e))
        .unwrap();
    let keys: Vec<&str> = failed.as_object().unwrap().keys().map(String::as_str).collect();
    let after = |k: &str| keys[keys.iter().position(|x| *x == k).unwrap() + 1];
    assert!(failed.get("was_backgrounded").is_none());
    assert_eq!(after("bash_last_argv0"), "bash_command_kinds");
    assert_eq!(failed["bash_command_kinds"], "file_search", "官方 yte：ls");
    assert_eq!(after("interrupted"), "duration_ms");
    assert_eq!(after("had_sandbox_violation"), "same_as_running_background");
    assert_eq!(after("same_as_running_background"), "tool_use_id");
}

/// 「猜下一句」请求体：主线程上下文、一条回复、猜下一句的提示词；`denied` 是分叉里前面几步调过的工具（每步
/// 一个，都被分叉拒了），接着发的那条末尾是拒绝结果与 total_tokens 附件那条 system 消息（`cap/auto-2.1.307-
/// 20261022-full/00188` 的形态）。
fn suggestion_body(denied: &[&str]) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    let msgs = v["messages"].as_array_mut().unwrap();
    msgs.push(json!({"role":"assistant","content":[{"type":"text","text":"done"}]}));
    msgs.push(json!({"role":"user","content":"[SUGGESTION MODE: Suggest what the user might naturally type next into Claude Code.]\n\nReply with ONLY the suggestion, no quotes or explanation."}));
    for id in denied {
        msgs.push(json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":"true"}}]}));
        msgs.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"is_error":true,"content":"No tools needed for suggestion"}]}));
        msgs.push(
            json!({"role":"system","content":"<total_tokens>14944722 tokens left</total_tokens>"}),
        );
    }
    v.to_string().into_bytes()
}

/// 主线程一轮之后的「猜下一句」分叉：头一条（10 秒发、2 秒）回复调了 Bash，客户端带着拒绝结果在 12.01 秒接着发
/// （2.15 秒、只回了思考）。用量取 `cap/auto-2.1.307-20261022-full` A19 那两条。`retried` 时接着发的那条先吃
/// 一次 503（12.01 秒发、0.2 秒），客户端在 12.4 秒原样重发、照样 14.16 秒收完。返回按时刻排好的事件。
fn suggestion_fork_events(cred: i64, retried: bool) -> Vec<(DateTime<Utc>, String, Value)> {
    let base = frozen_now() - Duration::from_secs(120);
    let ua = "claude-cli/2.1.307 (external, cli)";
    let t = Telemetry::default();
    let mut main = call(cc_body(true), "req_main", "end_turn");
    main.ua_out = ua.into();
    main.cred_id = cred;
    main.started_at = base;
    t.ingest(main);
    let skey = (cred, SESSION.to_string());
    let main_attachments = t.0.state.lock().sessions[&skey].attachment_counts.clone();
    let mut s1 = call(suggestion_body(&[]), "req_s1", "tool_use");
    s1.ua_out = ua.into();
    s1.cred_id = cred;
    s1.started_at = base + Duration::from_secs(10);
    s1.total_ms = 2_000;
    s1.message_id = Some("msg_s1".into());
    s1.text_chars = 0;
    s1.tool_calls = vec![ToolCall {
        id: "toolu_s1".into(),
        name: "Bash".into(),
        input: json!({"command": "true"}),
        verdict: None,
    }];
    s1.tool_use_lens = vec![("Bash".into(), 52)];
    (s1.input_tokens, s1.output_tokens, s1.cache_read_tokens, s1.cache_creation_tokens) =
        (506, 74, 54_638, 60);
    t.ingest(s1);
    let mut s2 = call(suggestion_body(&["toolu_s1"]), "req_s2", "end_turn");
    s2.ua_out = ua.into();
    s2.cred_id = cred;
    s2.started_at = base + Duration::from_millis(12_010);
    s2.total_ms = 2_150;
    if retried {
        let mut first = call(suggestion_body(&["toolu_s1"]), "req_s2_503", "end_turn");
        first.ua_out = ua.into();
        first.cred_id = cred;
        first.started_at = s2.started_at;
        first.total_ms = 200;
        t.ingest(failed(first, Some(503), "api_error", "upstream connect error"));
        s2.started_at = base + Duration::from_millis(12_400);
        s2.total_ms = 1_760;
    }
    s2.message_id = Some("msg_s2".into());
    s2.text_chars = 0;
    s2.saw_thinking = true;
    (s2.input_tokens, s2.output_tokens, s2.cache_read_tokens, s2.cache_creation_tokens) =
        (45, 91, 54_698, 580);
    t.ingest(s2);
    let st = t.0.state.lock();
    assert!(st.sessions[&skey].suggestion_fork.is_none(), "收尾之后分叉清掉");
    assert_eq!(
        st.sessions[&skey].attachment_counts, main_attachments,
        "分叉的附件不进主线程的账本（压缩收尾按它报）"
    );
    let mut ev: Vec<(DateTime<Utc>, String, Value)> = st.pending[&skey]
        .events
        .iter()
        .map(|(dt, e)| (*dt, ev_name(e).to_string(), meta_of(e)))
        .collect();
    ev.sort_by_key(|(dt, _, _)| *dt);
    ev
}

/// 「猜下一句」分叉里模型调了工具（`cap/auto-2.1.307-20261022-full` A19）：分叉自己的 canUseTool 一律拒，那条回复的
/// api_success 之前报 `can_use_tool_rejected`（`other`）与配置拒绝的 `tengu_safety_event`，这条不收尾；带着拒绝
/// 结果接着发的那条仍是猜下一句（同一条链、深度 +1、接上一条的 request-id 与 token 数，此前被当成无链的 helper），
/// 收尾的 fork 统计报整个分叉的时长、用量与消息数。
#[test]
fn suggestion_fork_with_a_denied_tool_spans_two_requests() {
    let ev = suggestion_fork_events(7, false);
    let suggestion = |n: &str| -> Vec<&(DateTime<Utc>, String, Value)> {
        ev.iter().filter(|(_, x, m)| x == n && m["querySource"] == "prompt_suggestion").collect()
    };
    let queries = suggestion("tengu_api_query");
    assert_eq!(queries.len(), 2, "接着发的那条也是猜下一句");
    assert!(ev.iter().all(|(_, n, m)| n != "tengu_api_query" || m["querySource"] != "compact"));
    let (q1, q2) = (&queries[0].2, &queries[1].2);
    assert_eq!(q1["queryChainId"], q2["queryChainId"]);
    let depth = q1["queryDepth"].as_u64().unwrap();
    assert_eq!(q2["queryDepth"], depth + 1);
    assert_eq!(q2["previousRequestId"], "req_s1");
    let successes = suggestion("tengu_api_success");
    assert_eq!(successes.len(), 2);
    assert_eq!(successes[1].2["messageTokens"], 506 + 74 + 54_638 + 60, "接上一条的 token 数");

    let rejected: Vec<_> =
        ev.iter().filter(|(_, n, _)| n == "tengu_tool_use_can_use_tool_rejected").collect();
    assert_eq!(rejected.len(), 1);
    let (t_rej, _, r) = rejected[0];
    for (k, v) in [
        ("messageID", json!("msg_s1")),
        ("toolName", json!("Bash")),
        ("deniedBy", json!("other")),
        ("decisionReasonType", json!("other")),
        ("preToolHookDurationMs", json!(1)),
        ("permissionDurationMs", json!(1)),
        ("queryChainId", q1["queryChainId"].clone()),
        ("queryDepth", json!(depth)),
        ("requestId", json!("req_s1")),
    ] {
        assert_eq!(r[k], v, "{k}");
    }
    assert!(*t_rej < successes[0].0, "流式执行：拒绝在那条的 api_success 之前");
    let safety: Vec<_> = ev.iter().filter(|(_, n, _)| n == "tengu_safety_event").collect();
    assert_eq!(safety.len(), 1);
    assert_eq!(safety[0].0, *t_rej, "与拒绝同一刻");
    let s = &safety[0].2;
    let keys: Vec<&str> = s
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .skip_while(|k| *k != "category")
        .collect();
    assert_eq!(
        keys,
        [
            "category",
            "decision",
            "decision_source",
            "reason_type",
            "reason_code",
            "tool_class",
            "tool_name",
            "tool_source",
            "permission_mode",
            "tool_use_id",
            "sandbox_violation_count",
            "sandbox_enabled"
        ],
        "{s}"
    );
    for (k, v) in [
        ("category", json!("other")),
        ("decision", json!("reject")),
        ("decision_source", json!("config")),
        ("reason_type", json!("other")),
        ("reason_code", json!("other")),
        ("tool_class", json!("bash")),
        ("tool_name", json!("Bash")),
        ("tool_use_id", json!("toolu_s1")),
    ] {
        assert_eq!(s[k], v, "{k}");
    }

    let attachments: Vec<_> = ev
        .iter()
        .filter(|(_, n, m)| n == "tengu_attachments" && m["query_source"] == "prompt_suggestion")
        .collect();
    assert_eq!(attachments.len(), 1, "只有接着发的那条前面攒附件");
    assert_eq!(attachments[0].2["attachment_types"], json!(["total_tokens_reminder"]));
    assert!(attachments[0].0 < queries[1].0 && attachments[0].0 > successes[0].0);

    let ends: Vec<_> = ev
        .iter()
        .filter(|(_, n, m)| n == "tengu_turn_end" && m["query_source"] == "prompt_suggestion")
        .collect();
    assert_eq!(ends.len(), 1, "调工具那条不收尾");
    let forks: Vec<_> = ev
        .iter()
        .filter(|(_, n, m)| n == "tengu_fork_agent_query" && m["forkLabel"] == "prompt_suggestion")
        .collect();
    assert_eq!(forks.len(), 1);
    let f = &forks[0].2;
    // 10 秒发出到 12.01 + 2.15 秒收完，再加收尾那 7ms；消息是 Bash、拒绝、附件、思考四条。
    assert_eq!(
        ["durationMs", "messageCount", "inputTokens", "outputTokens"].map(|k| f[k].clone()),
        [json!(4_167), json!(4), json!(551), json!(165)]
    );
    assert_eq!(
        ["cacheReadInputTokens", "cacheCreationInputTokens"].map(|k| f[k].clone()),
        [json!(109_336), json!(640)]
    );
    assert_eq!(ends[0].2["duration_ms"], 4_167);
    // 分叉的结果只有收尾那一条（主线程那一轮收尾另有它自己的）。
    let results: Vec<&Value> = ev
        .iter()
        .filter(|(dt, n, _)| n == "tengu_prompt_suggestion" && *dt > successes[0].0)
        .map(|(_, _, m)| m)
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(
        (&results[0]["outcome"], &results[0]["reason"]),
        (&json!("suppressed"), &json!("empty"))
    );
}

/// 接着发的那条先吃一次可重试的失败（503）、客户端原样重发：失败那次与重发那次都还在分叉上（同一条链、同一深度、
/// 接着头一条），收尾的 fork 统计仍是整个分叉的合计（输入 / 输出 551 / 165，不是只剩重发那次的 45 / 91）。
#[test]
fn suggestion_fork_survives_a_retried_continuation() {
    let ev = suggestion_fork_events(7, true);
    let of = |n: &str| -> Vec<&Value> {
        ev.iter()
            .filter(|(_, x, m)| x == n && m["querySource"] == "prompt_suggestion")
            .map(|(_, _, m)| m)
            .collect()
    };
    let queries = of("tengu_api_query");
    assert_eq!(queries.len(), 3, "头一条、失败那次、重发那次");
    let chain = &queries[0]["queryChainId"];
    let depth = queries[0]["queryDepth"].as_u64().unwrap();
    for q in &queries[1..] {
        assert_eq!(
            (&q["queryChainId"], &q["queryDepth"], &q["previousRequestId"]),
            (chain, &json!(depth + 1), &json!("req_s1"))
        );
    }
    let errors = of("tengu_api_error");
    assert_eq!(errors.len(), 1);
    assert_eq!((&errors[0]["queryChainId"], &errors[0]["queryDepth"]), (chain, &json!(depth + 1)));
    assert_eq!(of("tengu_api_success").last().unwrap()["messageTokens"], 506 + 74 + 54_638 + 60);
    let forks: Vec<&Value> = ev
        .iter()
        .filter(|(_, n, m)| n == "tengu_fork_agent_query" && m["forkLabel"] == "prompt_suggestion")
        .map(|(_, _, m)| m)
        .collect();
    assert_eq!(forks.len(), 1);
    assert_eq!(
        ["durationMs", "messageCount", "inputTokens", "outputTokens", "cacheReadInputTokens"]
            .map(|k| forks[0][k].clone()),
        [json!(4_167), json!(4), json!(551), json!(165), json!(109_336)]
    );
}

/// 「猜下一句」是后台请求：接着发的那条过载（529）客户端不重发（官方 `X2 && !yTe`），分叉就此结束；503 会重发，
/// 分叉留着等重发那条。
#[test]
fn suggestion_fork_ends_on_a_background_overload() {
    for (status, kept) in [(529, false), (503, true)] {
        let t = Telemetry::default();
        let mut main = call(cc_body(true), "req_main", "end_turn");
        main.ua_out = "claude-cli/2.1.307 (external, cli)".into();
        t.ingest(main);
        let mut s1 = call(suggestion_body(&[]), "req_s1", "tool_use");
        s1.ua_out = "claude-cli/2.1.307 (external, cli)".into();
        s1.tool_calls = vec![ToolCall {
            id: "toolu_s1".into(),
            name: "Bash".into(),
            input: json!({"command": "true"}),
            verdict: None,
        }];
        t.ingest(s1);
        let mut s2 = call(suggestion_body(&["toolu_s1"]), "req_s2", "end_turn");
        s2.ua_out = "claude-cli/2.1.307 (external, cli)".into();
        let etype = if status == 529 { "overloaded_error" } else { "api_error" };
        t.ingest(failed(s2, Some(status), etype, "x"));
        let st = t.0.state.lock();
        assert_eq!(st.sessions[&key()].suggestion_fork.is_some(), kept, "{status}");
    }
}

/// 「猜下一句」的建议看整个分叉（官方 `q()` 逐个候选挑头一个非空正文）：头一条回复带了正文（「run the tests」）
/// 又调了工具、第二条只调了工具、最后一条只回了思考，照样出了建议，不报 `suppressed/empty`；用户接着敲了别的，
/// `ignored` 的 `generationRequestId` 是分叉头一条，字数是那段正文的，建议从分叉收尾那一刻算起挂着。
#[test]
fn suggestion_fork_takes_text_from_an_earlier_reply() {
    let base = frozen_now() - Duration::from_secs(120);
    let t = Telemetry::default();
    let at = |c: &mut ApiCall, millis: u64| {
        c.ua_out = "claude-cli/2.1.307 (external, cli)".into();
        c.started_at = base + Duration::from_millis(millis);
    };
    let mut main = call(cc_body(true), "req_main", "end_turn");
    at(&mut main, 0);
    t.ingest(main);
    let mut s1 = call(suggestion_body(&[]), "req_s1", "tool_use");
    at(&mut s1, 10_000);
    s1.total_ms = 2_000;
    s1.text_chars = 13;
    s1.tool_calls = vec![ToolCall {
        id: "toolu_s1".into(),
        name: "Bash".into(),
        input: json!({"command": "true"}),
        verdict: None,
    }];
    t.ingest(s1);
    let mut s2 = call(suggestion_body(&["toolu_s1"]), "req_s2", "tool_use");
    at(&mut s2, 12_010);
    s2.total_ms = 1_000;
    s2.text_chars = 0;
    s2.tool_calls = vec![ToolCall {
        id: "toolu_s2".into(),
        name: "Bash".into(),
        input: json!({"command": "true"}),
        verdict: None,
    }];
    t.ingest(s2);
    let mut s3 = call(suggestion_body(&["toolu_s1", "toolu_s2"]), "req_s3", "end_turn");
    at(&mut s3, 13_020);
    s3.total_ms = 1_140;
    s3.text_chars = 0;
    s3.saw_thinking = true;
    t.ingest(s3);
    let mut next = call(with_prompt(cc_body(true), PROMPT_2), "req_next", "end_turn");
    at(&mut next, 30_000);
    t.ingest(next);
    let st = t.0.state.lock();
    let results: Vec<(DateTime<Utc>, Value)> = st.pending[&key()]
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_prompt_suggestion")
        .map(|(dt, e)| (*dt, meta_of(e)))
        .collect();
    assert!(results.iter().all(|(_, m)| m["reason"] != "empty"), "{results:?}");
    let ignored: Vec<&(DateTime<Utc>, Value)> =
        results.iter().filter(|(_, m)| m["outcome"] == "ignored").collect();
    assert_eq!(ignored.len(), 1, "{results:?}");
    let (submit, m) = ignored[0];
    assert_eq!(m["generationRequestId"], "req_s1");
    assert_eq!(m["similarity"], json!(11.0 / 13.0), "敲的「hello there」÷ 那段正文");
    let shown: DateTime<Utc> = (base + Duration::from_millis(14_160)).into();
    assert_eq!(m["timeToIgnoreMs"], (*submit - shown).num_milliseconds());
}

/// 回放 `cap/auto-2.1.307-20261022-full` A19 的那个分叉（00186 调 Bash 被拒、00188 收尾），拒绝两条、续发那条与收尾
/// 的 fork 统计逐项对官方 00191 批次。
#[test]
fn suggestion_fork_replay_matches_the_2_1_307_capture() {
    let dir = "auto-2.1.307-20261022-full";
    let (Some((b1, betas1)), Some((b2, betas2))) =
        (cap_request(&format!("{dir}/00186_")), cap_request(&format!("{dir}/00188_")))
    else {
        eprintln!("skipped: cap/{dir} not present");
        return;
    };
    let base = frozen_now() - Duration::from_secs(60);
    let ua = "claude-cli/2.1.307 (external, cli)";
    let t = Telemetry::default();
    let session = parse_shape(&b1).unwrap().session_id.unwrap();
    let auxiliary = || AgentHeaders {
        agent_id: None,
        agent_type: None,
        request_class: Some("auxiliary".into()),
    };
    let mut s1 = call(b1, "req_011CgGUpJb2rDMrUAuobQwT7", "tool_use");
    s1.betas = betas1;
    s1.ua_out = ua.into();
    s1.agent = auxiliary();
    s1.started_at = base;
    s1.total_ms = 1_988;
    s1.message_id = Some("msg_011CgGUpKBEsW5cn2GnVqtZs".into());
    s1.text_chars = 0;
    s1.tool_calls = vec![ToolCall {
        id: "toolu_01Pyy9WP4UHvmEeZTDk6RP5B".into(),
        name: "Bash".into(),
        input: json!({"command": "true", "description": "No-op placeholder"}),
        verdict: None,
    }];
    (s1.input_tokens, s1.output_tokens, s1.cache_read_tokens, s1.cache_creation_tokens) =
        (506, 74, 54_638, 60);
    t.ingest(s1);
    let mut s2 = call(b2, "req_011CgGUpT4mESaLUzQ8hr3Lg", "end_turn");
    s2.betas = betas2;
    s2.ua_out = ua.into();
    s2.agent = auxiliary();
    s2.started_at = base + Duration::from_millis(1_995);
    s2.total_ms = 2_144;
    s2.message_id = Some("msg_011CgGUpThCwGTzLLwJqEuUc".into());
    s2.text_chars = 0;
    s2.saw_thinking = true;
    (s2.input_tokens, s2.output_tokens, s2.cache_read_tokens, s2.cache_creation_tokens) =
        (45, 91, 54_698, 580);
    t.ingest(s2);
    let st = t.0.state.lock();
    let p = &st.pending[&(7, session)];
    let metas = |n: &str| -> Vec<Value> {
        p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
    };
    // 去掉随阶段变的那几项，从官方批次照抄。
    let strip = |m: &Value| -> Value {
        let mut m = m.clone();
        for k in ["renderer_mode", "subscription_type", "cc_prompt_id"] {
            m.as_object_mut().unwrap().remove(k);
        }
        m
    };
    let rejected = metas("tengu_tool_use_can_use_tool_rejected");
    assert_eq!(rejected.len(), 1);
    let mut r = strip(&rejected[0]);
    let chain = r.as_object_mut().unwrap().remove("queryChainId").unwrap();
    let depth = r.as_object_mut().unwrap().remove("queryDepth").unwrap();
    assert_eq!(
        r,
        json!({"messageID": "msg_011CgGUpKBEsW5cn2GnVqtZs", "toolName": "Bash", "deniedBy": "other",
            "decisionReasonType": "other", "preToolHookDurationMs": 1, "permissionDurationMs": 1,
            "requestId": "req_011CgGUpJb2rDMrUAuobQwT7"})
    );
    assert_eq!(
        strip(&metas("tengu_safety_event")[0]),
        json!({"category": "other", "decision": "reject", "decision_source": "config",
            "reason_type": "other", "reason_code": "other", "tool_class": "bash", "tool_name": "Bash",
            "tool_source": "builtin", "permission_mode": "default",
            "tool_use_id": "toolu_01Pyy9WP4UHvmEeZTDk6RP5B", "sandbox_violation_count": 0,
            "sandbox_enabled": false})
    );
    let queries = metas("tengu_api_query");
    assert_eq!(queries.len(), 2);
    assert_eq!((&queries[0]["queryChainId"], &queries[0]["queryDepth"]), (&chain, &depth));
    let q2 = &queries[1];
    assert_eq!(
        ["querySource", "queryChainId", "queryDepth", "messagesLength", "previousRequestId"]
            .map(|k| q2[k].clone()),
        [
            json!("prompt_suggestion"),
            chain.clone(),
            json!(depth.as_u64().unwrap() + 1),
            json!(16),
            json!("req_011CgGUpJb2rDMrUAuobQwT7")
        ]
    );
    assert_eq!(metas("tengu_api_success")[1]["messageTokens"], 55_278);
    assert_eq!(
        strip(&metas("tengu_attachments")[0]),
        json!({"attachment_types": ["total_tokens_reminder"], "attachment_token_estimates": ["23"],
            "query_source": "prompt_suggestion"})
    );
    let f = &metas("tengu_fork_agent_query")[0];
    assert_eq!(
        ["messageCount", "inputTokens", "outputTokens", "cacheReadInputTokens"]
            .map(|k| f[k].clone()),
        [json!(4), json!(551), json!(165), json!(109_336)]
    );
    assert_eq!(f["cacheCreationInputTokens"], 640);
    // 官方 0.9892243524206755；读回 meta 时的浮点解析会差最后一位。
    assert!((f["cacheHitRate"].as_f64().unwrap() - 0.9892243524206755).abs() < 1e-12, "{f}");
    assert_eq!(
        strip(metas("tengu_prompt_suggestion").last().unwrap()),
        json!({"source": "cli", "outcome": "suppressed", "reason": "empty", "prompt_id": "user_intent"})
    );
}

/// 号的服务端开关 `tengu_snoopy_creek` 关了（官方 `$Mn()`，没下发按开）：用户在弹框上拒的、「猜下一句」分叉
/// 拒的，都不报 `tengu_safety_event`，`can_use_tool_rejected` 照报；开着的号照报。
#[test]
fn safety_events_follow_the_accounts_snoopy_creek() {
    let ua = "claude-cli/2.1.307 (external, cli)";
    let eval = |on: bool| json!({"tengu_snoopy_creek": {"value": on, "source": "force"}});
    let (off, on) = (93_307, 93_308);
    crate::proxy::snoopy_creek::note_eval(off, eval(false).as_object().unwrap());
    crate::proxy::snoopy_creek::note_eval(on, eval(true).as_object().unwrap());
    let count = |names: Vec<String>, n: &str| names.iter().filter(|x| *x == n).count();
    for (cred, want) in [(off, 0), (on, 1)] {
        let user: Vec<String> =
            user_rejected_events(ua, cred).into_iter().map(|(n, _)| n).collect();
        let fork: Vec<String> =
            suggestion_fork_events(cred, false).into_iter().map(|(_, n, _)| n).collect();
        for names in [user, fork] {
            assert_eq!(count(names.clone(), "tengu_tool_use_can_use_tool_rejected"), 1);
            assert_eq!(count(names, "tengu_safety_event"), want, "cred {cred}");
        }
    }
}

/// fork 统计的 `messageCount` 是分叉里攒下的消息数（官方 `at.length`，回复的每个内容块一条）：猜下一句只回了
/// 思考、没出建议的报 1（`cap/auto-2.1.303~307-*-full` 共 39 条），思考加正文 2，只有正文 1。
#[test]
fn suggestion_fork_message_count_follows_the_reply_blocks() {
    for (thinking, text, want) in [(true, 0, 1), (true, 12, 2), (false, 12, 1)] {
        let t = Telemetry::default();
        let mut main = call(cc_body(true), "req_main", "end_turn");
        main.ua_out = "claude-cli/2.1.307 (external, cli)".into();
        t.ingest(main);
        let mut s = call(suggestion_body(&[]), "req_s", "end_turn");
        s.ua_out = "claude-cli/2.1.307 (external, cli)".into();
        s.started_at = frozen_now() - Duration::from_secs(1);
        s.saw_thinking = thinking;
        s.text_chars = text;
        t.ingest(s);
        let st = t.0.state.lock();
        let fork = st.pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_fork_agent_query")
            .map(|(_, e)| meta_of(e))
            .unwrap();
        assert_eq!(fork["messageCount"], want, "thinking {thinking}, text {text}");
    }
}

/// 认「猜下一句」分叉里接着发的那条：末条是被拒工具的结果，往回跳过「调工具 → 拒绝结果」那几对，落到的是猜下一句
/// 的提示词；主线程续轮（落到的是用户自己的话）不算。
#[test]
fn suggestion_continuations_are_recognized() {
    let step = |body: Vec<u8>| {
        let s = parse_shape(&body).unwrap();
        (s.suggestion, s.suggestion_step, s.new_prompt, s.tool_uses.len())
    };
    assert_eq!(step(suggestion_body(&[])), (true, 0, true, 0));
    assert_eq!(step(suggestion_body(&["a"])), (true, 1, false, 1));
    assert_eq!(step(suggestion_body(&["a", "b"])), (true, 2, false, 1));
    assert_eq!(step(cc_body(false)), (false, 0, false, 1), "主线程续轮");
    let mut v: Value = serde_json::from_slice(&suggestion_body(&["a"])).unwrap();
    let msgs = v["messages"].as_array_mut().unwrap();
    let at = msgs.len() - 4;
    msgs[at] = json!({"role":"user","content":"run the tests"});
    assert!(!step(v.to_string().into_bytes()).0, "落到的是用户自己的话");
}

/// 2.1.305：`tengu_api_success` 在 `firstContentMs` 之后多 `streamEventCount`（除 ping 外的 SSE 事件数，
/// `cap/auto-2.1.305-20261021-full` 165 条全有）；非流式不报；2.1.303 没有。
#[test]
fn version_2_1_305_telemetry_shape() {
    let run = |ua: &str, events: Option<u32>| {
        let t = Telemetry::default();
        let mut c = call(cc_body(true), "req_1", "end_turn");
        c.ua_out = ua.into();
        c.stream_events = events;
        t.ingest(c);
        let st = t.0.state.lock();
        st.pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_api_success")
            .map(|(_, e)| meta_of(e))
            .unwrap()
    };
    let ok = run("claude-cli/2.1.305 (external, cli)", Some(17));
    let k: Vec<String> = ok.as_object().unwrap().keys().cloned().collect();
    let at = k.iter().position(|k| k == "firstContentMs").unwrap();
    assert_eq!(k[at + 1], "streamEventCount", "{ok}");
    assert_eq!(ok["streamEventCount"], 17);
    assert!(run("claude-cli/2.1.305 (external, cli)", None).get("streamEventCount").is_none());
    assert!(run("claude-cli/2.1.303 (external, cli)", Some(17)).get("streamEventCount").is_none());
}

/// 2.1.303 的遥测（`cap/auto-2.1.303-20261019-full`）：`tengu_init` 在 `mcpClientCount` 之后多
/// `repoMcpAllowList` / `repoMcpDenyList` / `mcpConfigBlockedCount`，`tengu_tool_use_success` 在
/// `messageID` 之后多 `toolUseID`；`-p` 模板多一条 `tengu_fork_subagent_enabled`。2.1.301 都没有。
#[test]
fn version_2_1_303_telemetry_shape() {
    let run = |ua: &str| {
        let t = Telemetry::default();
        let mut first = call(cc_body(true), "req_1", "tool_use");
        first.ua_out = ua.into();
        t.ingest(first);
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"toolu_303","name":"Read","input":{"file_path":"/tmp/a.txt"}}]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_303","content":"hello"}]});
        let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
        second.ua_out = ua.into();
        t.ingest(second);
        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let find = |n: &str| {
            p.events.iter().find(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).unwrap()
        };
        (find("tengu_init"), find("tengu_tool_use_success"))
    };
    let keys = |m: &Value| m.as_object().unwrap().keys().cloned().collect::<Vec<_>>();

    let (init, ok) = run("claude-cli/2.1.303 (external, cli)");
    let k = keys(&init);
    let at = k.iter().position(|k| k == "mcpClientCount").unwrap();
    assert_eq!(k[at + 1..at + 4], ["repoMcpAllowList", "repoMcpDenyList", "mcpConfigBlockedCount"]);
    assert_eq!(
        (&init["repoMcpAllowList"], &init["mcpConfigBlockedCount"]),
        (&json!(false), &json!(0))
    );
    let k = keys(&ok);
    let at = k.iter().position(|k| k == "messageID").unwrap();
    assert_eq!(k[at..at + 3], ["messageID", "toolUseID", "toolName"], "{ok}");
    assert_eq!(ok["toolUseID"], "toolu_303");

    let (init, ok) = run("claude-cli/2.1.301 (external, cli)");
    assert!(init.get("repoMcpAllowList").is_none() && ok.get("toolUseID").is_none(), "{init} {ok}");

    // 失败的 Bash：`tool_use_error` 的栈摘要与顶帧随打包产物变，按版本取各自抓包里的值。
    let failed = |ua: &str| {
        let t = Telemetry::default();
        let mut first = call(cc_body(true), "req_1", "tool_use");
        first.ua_out = ua.into();
        t.ingest(first);
        let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
        body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"false","description":"fail"}}]});
        body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"Exit code 1"}]});
        let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
        second.ua_out = ua.into();
        t.ingest(second);
        let st = t.0.state.lock();
        st.pending[&key()]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_tool_use_error")
            .map(|(_, e)| meta_of(e))
            .unwrap()
    };
    for (ua, hash, frame) in [
        ("claude-cli/2.1.311 (external, cli)", "11211b82294f", "chunk-2ht8z368.js:3554:6820"),
        ("claude-cli/2.1.310 (external, cli)", "b26c68ea74b5", "chunk-31gxg38t.js:3618:6047"),
        ("claude-cli/2.1.309 (external, cli)", "1a36b34122c0", "chunk-kang00hq.js:3682:5744"),
        ("claude-cli/2.1.308 (external, cli)", "40e496cc06bc", "chunk-a1cg2ptw.js:3661:6043"),
        ("claude-cli/2.1.307 (external, cli)", "40e496cc06bc", "chunk-wvsyb4fb.js:3661:6043"),
        ("claude-cli/2.1.306 (external, cli)", "c19e7dcf6637", "chunk-z4fyjkxt.js:3654:6164"),
        ("claude-cli/2.1.305 (external, cli)", "a4913bdffc8a", "chunk-h56h5ggw.js:3654:6164"),
        ("claude-cli/2.1.303 (external, cli)", "99830f66ede0", "chunk-jh3cvmqy.js:3614:6109"),
        ("claude-cli/2.1.299 (external, cli)", "1dada4b9379c", "chunk-4xx9trad.js:3588:6085"),
        ("claude-cli/2.1.293 (external, cli)", "2e671ed03fc0", "chunk-nwqfvmza.js:3493:5989"),
    ] {
        let err = failed(ua);
        assert_eq!(
            (&err["error_stack_hash"], &err["error_top_frame"]),
            (&json!(hash), &json!(frame)),
            "{ua}"
        );
    }

    let fork = |v: &str, sdk: bool| {
        super::template::template_for(v, sdk)
            .startup
            .iter()
            .filter(|t| t.name == "tengu_fork_subagent_enabled")
            .count()
    };
    assert_eq!((fork("2.1.303", true), fork("2.1.303", false)), (1, 1));
    assert_eq!((fork("2.1.301", true), fork("2.1.301", false)), (0, 1));
}

/// 缓存保活：这条主线程回复接替了一条续命成功过的记录（[`ApiCall::cache_keepalive_ok`]）时，收尾补一条
/// `tengu_feature_ok{api_cache_keepalive}`，夹在 `tether_live_outcome` 与 `api_success` 之间、同一刻，
/// Datadog 也有（`cap/auto-2.1.300-20261015-full` A0）；没接替的不报。
#[test]
fn cache_keepalive_feature_follows_the_successor_reply() {
    for ok in [true, false] {
        let t = Telemetry::default();
        let mut c = call(body_for("claude-opus-5-5", Some("medium"), true), "req_ka", "end_turn");
        c.ua_out = "claude-cli/2.1.300 (external, cli)".into();
        c.cache_keepalive_ok = ok;
        t.ingest(c);
        let st = t.0.state.lock();
        let p = &st.pending[&key()];
        let mut evs: Vec<&(DateTime<Utc>, Value)> = p.events.iter().collect();
        evs.sort_by_key(|(t, _)| *t);
        let names: Vec<String> = evs
            .iter()
            .map(|(_, e)| match ev_name(e) {
                "tengu_feature_ok" => {
                    format!("ok:{}", meta_of(e)["feature_name"].as_str().unwrap())
                }
                n => n.to_string(),
            })
            .collect();
        let ka = names.iter().position(|n| n == "ok:api_cache_keepalive");
        assert_eq!(ka.is_some(), ok, "{names:?}");
        if let Some(at) = ka {
            assert_eq!(names[at + 1], "tengu_api_success", "{names:?}");
            assert!(names[..at].iter().any(|n| n == "tengu_tether_live_outcome"));
            assert!(p.dd.iter().any(|d| d.to_string().contains("api_cache_keepalive")));
        }
    }
}

/// 2.1.300 `tengu_agent_summary_stopped` 的开关按子代理拉起时会话的**当前**主模型挑：同一会话 opus 起头、
/// `/model` 换到 fable 之后拉起的 Explore（子代理自己跑 opus），报 fable 族、`tengu_starry_wall`，不是
/// 会话首轮那个 opus。
#[test]
fn agent_summary_stopped_uses_the_main_model_at_spawn() {
    let stopped = summary_stopped_for("claude-cli/2.1.300 (external, cli)");
    assert_eq!(stopped["session_model_family"], "fable", "{stopped}");
    assert_eq!(stopped["config_flag"], "tengu_starry_wall");
    assert_eq!(stopped["agent_model_family"], "opus");
    assert_eq!((&stopped["calls"], &stopped["config_source"]), (&json!(0), &json!("fallback")));
    assert!(stopped.get("mode").is_none(), "2.1.303 之前没有那六项: {stopped}");
}

/// 2.1.303 的 `tengu_agent_summary_stopped` 多六项，键序与取值逐个对 `cap/auto-2.1.303-20261019-full`
/// （六条都是 `full` / `full` / `false` / `2000` / `false` / `0`）。
#[test]
fn agent_summary_stopped_2_1_303_shape() {
    let stopped = summary_stopped_for("claude-cli/2.1.303 (external, cli)");
    let keys: Vec<&str> = stopped
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .skip_while(|k| *k != "mode")
        .collect();
    assert_eq!(
        keys,
        [
            "mode",
            "config_mode",
            "mode_changed",
            "max_input_tokens",
            "config_flag",
            "config_source",
            "session_model_family",
            "agent_model_family",
            "agent_model",
            "agent_model_swapped",
            "is_sdk_host",
            "is_resume",
            "duration_ms",
            "calls",
            "calls_kept_on_full",
            "calls_failed",
            "lines_accepted",
            "first_wait_ms",
            "next_wait_ms"
        ],
        "{stopped}"
    );
    for (k, v) in [
        ("mode", json!("full")),
        ("config_mode", json!("full")),
        ("mode_changed", json!(false)),
        ("max_input_tokens", json!(2000)),
        ("agent_model_swapped", json!(false)),
        ("calls_kept_on_full", json!(0)),
    ] {
        assert_eq!(stopped[k], v, "{k}");
    }
}

/// 2.1.307 在 `agent_model` 与 `agent_model_swapped` 之间多 writer 几项（`cap/auto-2.1.307-20261022-full` 六条都
/// 没配 writer：`writer_model` 同 `agent_model`、`writer_failed` 为 false、`config_writer` 不出现）。
#[test]
fn agent_summary_stopped_2_1_307_writer_keys() {
    let stopped = summary_stopped_for("claude-cli/2.1.307 (external, cli)");
    let keys: Vec<&str> = stopped
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .skip_while(|k| *k != "agent_model")
        .take(4)
        .collect();
    assert_eq!(
        keys,
        ["agent_model", "writer_model", "writer_failed", "agent_model_swapped"],
        "{stopped}"
    );
    assert_eq!(stopped["writer_model"], stopped["agent_model"]);
    assert_eq!(stopped["writer_failed"], false);
    let old = summary_stopped_for("claude-cli/2.1.306 (external, cli)");
    assert!(old.get("writer_model").is_none() && old.get("writer_failed").is_none(), "{old}");
}

/// 2.1.307 的 writer：号给 opus 子代理配了小快模型、摘要按 short 发，收尾报 `writer_model` 是它、`config_writer`
/// 报配的那项；换了 writer 的那条失败过，摘要器之后不再换，收尾报回子代理自己的模型、`writer_failed` 为 true。
/// 客户端的小快模型可以被环境变量换掉：换上过的就是它（配的 sonnet 换上了就报 sonnet，之后配置改来改去、中间
/// 有没换上的也不推翻），配着却没换上的不是它（配的 haiku 没换上就报子代理自己的），一条没发过才按默认的猜。
#[test]
fn agent_summary_writer_follows_the_config_and_failures() {
    let base = frozen_now() - Duration::from_secs(200);
    // `configs` 是号给 opus 配的 writer 及其生效时刻（秒）；`summaries` 是各条 short 摘要的发出时刻、实际用的
    // 模型与各次尝试（隔一秒一次：`None` 成功，状态码是那次失败——503 客户端会重发，后台请求的 529 不会）。
    // 子代理 5 秒起跑、150 秒发最后一条。
    let run = |cred: i64, configs: &[(u64, &str)], summaries: &[(u64, &str, &[Option<u16>])]| {
        for (secs, writer) in configs {
            let cfg = json!({"tengu_groovy_ember": {"value": {"mode": "short",
                "writer": {"opus": writer}}, "source": "experiment"}});
            crate::proxy::agent_summary_config::note_eval_at(
                cred,
                cfg.as_object().unwrap(),
                base + Duration::from_secs(*secs),
            );
        }
        let t = Telemetry::default();
        let at = |c: &mut ApiCall, secs: u64| {
            c.ua_out = "claude-cli/2.1.307 (external, cli)".into();
            c.cred_id = cred;
            c.started_at = base + Duration::from_secs(secs);
        };
        let agent = |class: &str| AgentHeaders {
            agent_id: Some("a1".into()),
            agent_type: Some("general-purpose".into()),
            request_class: Some(class.into()),
        };
        let tool = |id: &str, name: &str| ToolCall {
            id: id.into(),
            name: name.into(),
            input: json!({}),
            verdict: None,
        };
        let mut main = call(body_for("claude-opus-5-5", Some("high"), true), "req_m", "tool_use");
        at(&mut main, 0);
        main.tool_calls = vec![tool("t1", "Agent")];
        t.ingest(main);
        let mut s1 = call(body_for("claude-opus-5-5", Some("high"), true), "req_s1", "tool_use");
        at(&mut s1, 5);
        s1.agent = agent("subagent");
        s1.tool_calls = vec![tool("t2", "Read")];
        t.ingest(s1);
        for (i, (secs, model, attempts)) in summaries.iter().enumerate() {
            let meta: Value =
                serde_json::from_slice::<Value>(&cc_body(true)).unwrap()["metadata"].clone();
            // 每条笔记都不一样（等长：a.rs、b.rs……），官方笔记没变就不发。
            let file = char::from(b'a' + i as u8);
            let notes = format!(
                "<recent_steps>\ndid: Read - {file}.rs\n</recent_steps>\n\nDescribe the agent's most recent action in 3-5 words using present tense (-ing)."
            );
            let short = json!({
                "metadata": meta,
                "model": model,
                "max_tokens": 2108,
                "system": [
                    {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.307.abc; cc_entrypoint=cli; cch=00000;"},
                    {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
                    {"type":"text","text":"You write a 3-5 word progress label for a background agent, from short notes about what it is doing."}
                ],
                "messages": [{"role":"user","content":notes}],
                "tools": []
            });
            for (j, status) in attempts.iter().enumerate() {
                let rid = format!("req_sum{i}_{j}");
                let mut c = call(short.to_string().into_bytes(), &rid, "end_turn");
                at(&mut c, *secs + j as u64);
                c.agent = agent("auxiliary");
                c.text_chars = 18;
                t.ingest(match status {
                    None => c,
                    Some(529) => failed(c, Some(529), "overloaded_error", "Overloaded"),
                    Some(s) => failed(c, Some(*s), "api_error", "upstream connect error"),
                });
            }
        }
        let mut s2 = call(body_for("claude-opus-5-5", Some("high"), false), "req_s2", "end_turn");
        at(&mut s2, 150);
        s2.agent = agent("subagent");
        t.ingest(s2);
        let st = t.0.state.lock();
        st.pending[&(cred, SESSION.to_string())]
            .events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_agent_summary_stopped")
            .map(|(_, e)| meta_of(e))
            .expect("子代理收尾报一条")
    };
    let pick = |m: &Value| {
        ["mode", "writer_model", "config_writer", "writer_failed"].map(|k| m[k].clone())
    };
    let (haiku, sonnet, opus) = ("claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5");
    assert_eq!(
        pick(&run(9_307, &[(0, haiku)], &[(40, haiku, &[None])])),
        [json!("short"), json!(haiku), json!(haiku), json!(false)]
    );
    let failed_once = run(9_308, &[(0, haiku)], &[(40, haiku, &[Some(529)])]);
    assert_eq!(failed_once["writer_model"], failed_once["agent_model"], "退回子代理自己的模型");
    assert_eq!(pick(&failed_once)[2..], [json!(haiku), json!(true)], "配的那项照报，失败过");
    // 客户端把小快模型换成 sonnet、号也把 writer 配成 sonnet：摘要换成 sonnet 写，收尾报 sonnet。
    assert_eq!(
        pick(&run(9_309, &[(0, sonnet)], &[(40, sonnet, &[None])])),
        [json!("short"), json!(sonnet), json!(sonnet), json!(false)]
    );
    // 配的是 haiku-5-5，客户端的小快模型却不是它：摘要仍由子代理自己的模型写，收尾报回它。
    let kept = run(9_310, &[(0, haiku)], &[(40, opus, &[None])]);
    assert_eq!(kept["writer_model"], kept["agent_model"]);
    assert_eq!(pick(&kept)[2..], [json!(haiku), json!(false)]);
    // 一条 short 摘要都没发：看不出客户端的小快模型，按默认的猜。
    assert_eq!(run(9_311, &[(0, haiku)], &[])["writer_model"], haiku);
    // 配 haiku 时换上过 haiku；50 秒改配 sonnet，80 秒那条没换（仍是 opus 写）；100 秒改回 haiku，收尾时摘要器
    // 读到的是 haiku：小快模型早已确认是 haiku，中间没换上那条不推翻它，官方 `fe()` 此时就是 haiku。
    let back = run(
        9_312,
        &[(0, haiku), (50, sonnet), (100, haiku)],
        &[(40, haiku, &[None]), (80, opus, &[None])],
    );
    assert_eq!(pick(&back), [json!("short"), json!(haiku), json!(haiku), json!(false)]);
    // 小快模型被环境变量换成 sonnet 时同样：换上过 sonnet，50 秒改配 haiku 没换上，改回 sonnet 收尾报 sonnet
    // （不能退回按默认的 haiku 去猜）。
    let back_sonnet = run(
        9_314,
        &[(0, sonnet), (50, haiku), (100, sonnet)],
        &[(40, sonnet, &[None]), (80, opus, &[None])],
    );
    assert_eq!(pick(&back_sonnet), [json!("short"), json!(sonnet), json!(sonnet), json!(false)]);
    // 同样的改法但收尾时还配着 sonnet：sonnet 在配着时没换上过，不是小快模型。
    let still =
        run(9_313, &[(0, haiku), (50, sonnet)], &[(40, haiku, &[None]), (80, opus, &[None])]);
    assert_eq!(still["writer_model"], still["agent_model"]);
    assert_eq!(still["config_writer"], sonnet);
    // 换上 haiku 的那条先吃一次 503、客户端重发成功：一次成功的调用，不算失败、writer 也没失败过。
    let retried = run(9_315, &[(0, haiku)], &[(40, haiku, &[Some(503), None])]);
    assert_eq!(pick(&retried), [json!("short"), json!(haiku), json!(haiku), json!(false)]);
    assert_eq!((&retried["calls"], &retried["calls_failed"]), (&json!(1), &json!(0)));
    // 后台请求过载不重发（官方 `X2 && !yTe`）：529 就是整次失败。
    assert_eq!((&failed_once["calls"], &failed_once["calls_failed"]), (&json!(1), &json!(1)));
    let stats = |m: &Value| ["calls", "calls_failed", "writer_failed"].map(|k| m[k].clone());
    // 503 之后没等来重发、子代理就收尾了：扣着的那次先当最终失败结算，再报收尾那条。
    let exhausted = run(9_316, &[(0, haiku)], &[(40, haiku, &[Some(503)])]);
    assert_eq!(stats(&exhausted), [json!(1), json!(1), json!(true)]);
    assert_eq!(exhausted["writer_model"], exhausted["agent_model"]);
    // 上一次（haiku）最终失败后，下一次笔记换成等长的 b.rs、改由子代理自己的 opus 写：是两次调用，不是重发。
    let next = run(9_317, &[(0, haiku)], &[(40, haiku, &[Some(503)]), (80, opus, &[None])]);
    assert_eq!(stats(&next), [json!(2), json!(1), json!(true)]);
    // 模型没换、只是笔记等长地变了，同样是两次调用。
    let same_model = run(9_318, &[], &[(40, opus, &[Some(503)]), (80, opus, &[None])]);
    assert_eq!(stats(&same_model)[..2], [json!(2), json!(1)]);
}

/// 2.1.303 short 模式的子代理摘要：无工具、system 是那句固定指令的侧查询，带子代理的 `agent-id` 与
/// `request-class: auxiliary`。按 `agent_summary` 报（不当成 WebFetch 页面处理），记进支线的摘要统计；
/// 收尾那条的模式按发过的请求与号的配置推。
#[test]
fn short_agent_summaries_are_reported_and_counted() {
    // 主线程 opus 在 0 秒拉起一个子代理：子代理 5 秒起跑、一步工具，第 i 条摘要在 40 + 30i 秒发
    // （`summaries` 里 true 即 short），150 秒子代理发最后一条、约 156.2 秒收尾。摘要器每次计时器触发都
    // 重读配置：没发摘要时起跑后每 30 秒一次；发过的从最后一条摘要结束（发出 + 6.1 秒）起再每 30 秒一次。
    let base = frozen_now() - Duration::from_secs(200);
    let run = |cred: i64, agent_model: &str, summaries: &[bool]| {
        let t = Telemetry::default();
        let ua = "claude-cli/2.1.303 (external, cli)";
        let at = |c: &mut ApiCall, secs: u64| {
            c.ua_out = ua.into();
            c.cred_id = cred;
            c.started_at = base + Duration::from_secs(secs);
        };
        let agent = |class: &str| AgentHeaders {
            agent_id: Some("a1".into()),
            agent_type: Some("general-purpose".into()),
            request_class: Some(class.into()),
        };
        let mut main = call(body_for("claude-opus-5-5", Some("high"), true), "req_m", "tool_use");
        at(&mut main, 0);
        main.tool_calls = vec![ToolCall {
            id: "t1".into(),
            name: "Agent".into(),
            input: json!({}),
            verdict: None,
        }];
        t.ingest(main);
        let mut s1 = call(body_for(agent_model, Some("high"), true), "req_s1", "tool_use");
        at(&mut s1, 5);
        s1.agent = agent("subagent");
        s1.tool_calls = vec![ToolCall {
            id: "t2".into(),
            name: "Read".into(),
            input: json!({}),
            verdict: None,
        }];
        t.ingest(s1);
        for (i, short) in summaries.iter().enumerate() {
            let body = if *short {
                let meta: Value =
                    serde_json::from_slice::<Value>(&cc_body(true)).unwrap()["metadata"].clone();
                json!({
                    "metadata": meta,
                    "model": agent_model,
                    "max_tokens": 2108,
                    "system": [
                        {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.303.abc; cc_entrypoint=cli; cch=00000;"},
                        {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
                        {"type":"text","text":"You write a 3-5 word progress label for a background agent, from short notes about what it is doing."}
                    ],
                    "messages": [{"role":"user","content":"<recent_steps>\ndid: Read - a.rs\n</recent_steps>\n\nDescribe the agent's most recent action in 3-5 words using present tense (-ing)."}],
                    "tools": []
                })
                .to_string()
                .into_bytes()
            } else {
                body_for(agent_model, Some("high"), true)
            };
            let mut c = call(body, &format!("req_sum{i}"), "end_turn");
            at(&mut c, 40 + 30 * i as u64);
            c.agent = agent("auxiliary");
            c.text_chars = 18;
            t.ingest(c);
        }
        let mut s2 = call(body_for(agent_model, Some("high"), false), "req_s2", "end_turn");
        at(&mut s2, 150);
        s2.agent = agent("subagent");
        t.ingest(s2);
        let st = t.0.state.lock();
        let events = &st.pending[&(cred, SESSION.to_string())].events;
        let sources: Vec<Value> = events
            .iter()
            .filter(|(_, e)| ev_name(e) == "tengu_api_success")
            .map(|(_, e)| meta_of(e)["querySource"].clone())
            .collect();
        let stopped = events
            .iter()
            .find(|(_, e)| ev_name(e) == "tengu_agent_summary_stopped")
            .map(|(_, e)| meta_of(e))
            .expect("子代理收尾报一条");
        (sources, stopped)
    };
    let pick = |m: &Value| {
        ["mode", "config_mode", "mode_changed", "calls", "calls_kept_on_full", "lines_accepted"]
            .map(|k| m[k].clone())
    };

    // 两条 short：都报 agent_summary、不报 web_fetch_apply，记两次、都收下；配置由此推成 short。
    // 两条 short：都报 agent_summary、不报 web_fetch_apply，记两次、都收下；配置由此推成 short。
    let (sources, stopped) = run(9_303, "claude-opus-5-5", &[true, true]);
    assert_eq!(sources.iter().filter(|s| *s == "agent_summary").count(), 2, "{sources:?}");
    assert!(!sources.contains(&json!("web_fetch_apply")), "{sources:?}");
    assert_eq!(
        pick(&stopped),
        [json!("short"), json!("short"), json!(false), json!(2), json!(0), json!(2)]
    );

    // 没见过 eval、先 full 后 short：模式换过；发那条 full 时没有配置是 short 的依据，不记 kept_on_full。
    let (_, stopped) = run(9_304, "claude-opus-5-5", &[false, true]);
    assert_eq!(
        pick(&stopped),
        [json!("short"), json!("short"), json!(true), json!(2), json!(0), json!(2)]
    );

    // 号的 eval 按时刻生效：每条摘要按它发出那一刻的配置算，收尾按摘要器最后一次读到的那份报。
    let short_cfg =
        json!({"tengu_groovy_ember": {"value": {"mode": "short"}, "source": "experiment"}});
    let full_cfg = json!({});
    let set_ms = |cred: i64, f: &Value, ms: u64| {
        crate::proxy::agent_summary_config::note_eval_at(
            cred,
            f.as_object().unwrap(),
            base + Duration::from_millis(ms),
        )
    };
    let set = |cred: i64, f: &Value, at: u64| set_ms(cred, f, at * 1_000);
    // full → short（55 秒切）：40 秒那条 full 时配置还是 full，kept_on_full 为 0。
    set(9_306, &full_cfg, 0);
    set(9_306, &short_cfg, 55);
    let (_, stopped) = run(9_306, "claude-opus-5-5", &[false, true]);
    assert_eq!(
        pick(&stopped),
        [json!("short"), json!("short"), json!(true), json!(2), json!(0), json!(2)]
    );
    // short → full：收尾时配置已是 full，早先那条 short 不把 config_mode 拉回 short。
    set(9_307, &short_cfg, 0);
    set(9_307, &full_cfg, 55);
    let (_, stopped) = run(9_307, "claude-opus-5-5", &[true, false]);
    assert_eq!(
        pick(&stopped),
        [json!("full"), json!("full"), json!(true), json!(2), json!(0), json!(2)]
    );
    // full 摘要 40 秒发出、41 秒（回复还没回来）配置切成 short：那条按发出时的 full 算，不记 kept_on_full；
    // 之后的触发（46.1 秒结束后每 30 秒）读到 short，opus 子代理的 `J` 随之变 short，虽然没再发也报
    // short，发过的那条 full 算模式变过。
    set(9_308, &full_cfg, 0);
    set(9_308, &short_cfg, 41);
    let (_, stopped) = run(9_308, "claude-opus-5-5", &[false]);
    assert_eq!(
        pick(&stopped),
        [json!("short"), json!("short"), json!(true), json!(1), json!(0), json!(1)]
    );
    // 反过来：short 摘要 40 秒发出，80 秒配置切回 full，106.1 秒的触发读到 full 但没到期不发：
    // 收尾报 full / full、模式变过（审查的例子），不让最后那条 short 盖过去。
    set(9_311, &short_cfg, 0);
    set(9_311, &full_cfg, 80);
    let (_, stopped) = run(9_311, "claude-opus-5-5", &[true]);
    assert_eq!(
        pick(&stopped),
        [json!("full"), json!("full"), json!(true), json!(1), json!(0), json!(1)]
    );
    // 最后一次读配置之后、收尾之前才切，收尾仍报摘要器读到的 full：没发摘要的支线最后一次在 155 秒，
    // 155.5 秒切；发过一条摘要（40 秒发、46.1 秒结束）的支线计时从结束起算，最后一次在 136.1 秒，140 秒
    // 切也不算（按起跑后 30 秒整点推会误读成 155 秒）。
    set(9_309, &full_cfg, 0);
    set_ms(9_309, &short_cfg, 155_500);
    let (_, stopped) = run(9_309, "claude-opus-5-5", &[]);
    assert_eq!(
        pick(&stopped),
        [json!("full"), json!("full"), json!(false), json!(0), json!(0), json!(0)]
    );
    set(9_310, &full_cfg, 0);
    set(9_310, &short_cfg, 140);
    let (_, stopped) = run(9_310, "claude-opus-5-5", &[false]);
    assert_eq!(
        pick(&stopped),
        [json!("full"), json!("full"), json!(false), json!(1), json!(0), json!(1)]
    );

    // 号的 eval 给 short（opus 那个开关，带输入上限与等待）：haiku 子代理照 full 发，全算 kept_on_full；
    // 一次没发的 opus 子代理按配置报 short。来源、上限、等待都取 eval 里那份。
    let features = json!({"tengu_groovy_ember": {"value": {"mode": "short", "maxInputTokens": 4000,
        "refreshSeconds": [30, 60]}, "source": "experiment"}});
    set(9_305, &features, 0);
    let (_, stopped) = run(9_305, "claude-haiku-5-5", &[false]);
    assert_eq!(
        pick(&stopped),
        [json!("full"), json!("short"), json!(false), json!(1), json!(1), json!(1)]
    );
    assert_eq!(stopped["config_source"], "experiment");
    assert_eq!(stopped["max_input_tokens"], 4000);
    assert_eq!(
        (&stopped["first_wait_ms"], &stopped["next_wait_ms"]),
        (&json!(30_000), &json!(60_000))
    );
    let (_, stopped) = run(9_305, "claude-opus-5-5", &[]);
    assert_eq!(
        pick(&stopped),
        [json!("short"), json!("short"), json!(false), json!(0), json!(0), json!(0)]
    );
}

/// opus 起头、换到 fable 后拉起一个 opus 的 Explore 子代理，返回它收尾那条 `tengu_agent_summary_stopped`。
fn summary_stopped_for(ua: &str) -> Value {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(120);
    let main = |model: &str, rid: &str, at: u64, tools: bool| {
        let mut c = call(body_for(model, Some("high"), true), rid, "tool_use");
        c.ua_out = ua.into();
        c.started_at = base + Duration::from_secs(at);
        if tools {
            c.tool_calls = vec![ToolCall {
                id: "t1".into(),
                name: "Agent".into(),
                input: json!({}),
                verdict: None,
            }];
        } else {
            c.stop_reason = Some("end_turn".into());
        }
        c
    };
    t.ingest(main("claude-opus-5-5", "req_m1", 0, false));
    t.ingest(main("claude-fable-5-1", "req_m2", 20, true));
    let mut sub = call(body_for("claude-opus-5-5", Some("high"), true), "req_sub", "end_turn");
    sub.ua_out = ua.into();
    sub.started_at = base + Duration::from_secs(30);
    sub.agent = AgentHeaders {
        agent_id: Some("a1".into()),
        agent_type: Some("Explore".into()),
        request_class: Some("subagent".into()),
    };
    t.ingest(sub);
    let st = t.0.state.lock();
    st.pending[&key()]
        .events
        .iter()
        .find(|(_, e)| ev_name(e) == "tengu_agent_summary_stopped")
        .map(|(_, e)| meta_of(e))
        .expect("子代理收尾报一条")
}

/// 后台子代理在 opus 下拉起，它的首条回复处理之前主线程已经换到 fable：摘要器照拉起那一刻的 opus 报
/// （`tengu_groovy_ember`），不看处理到它时的主模型。
#[test]
fn agent_summary_stopped_keeps_the_model_from_spawn_time() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(120);
    let ua = "claude-cli/2.1.300 (external, cli)";
    let main = |model: &str, rid: &str, at: u64, tools: bool| {
        let mut c = call(body_for(model, Some("high"), true), rid, "tool_use");
        c.ua_out = ua.into();
        c.started_at = base + Duration::from_secs(at);
        if tools {
            c.tool_calls = vec![ToolCall {
                id: "t1".into(),
                name: "Agent".into(),
                input: json!({}),
                verdict: None,
            }];
        } else {
            c.stop_reason = Some("end_turn".into());
        }
        c
    };
    t.ingest(main("claude-opus-5-5", "req_m1", 0, true));
    // 子代理第 10 秒发出，但主线程第 20 秒发出的 fable 那条先处理完。
    t.ingest(main("claude-fable-5-1", "req_m2", 20, false));
    let mut sub = call(body_for("claude-opus-5-5", Some("high"), true), "req_sub", "end_turn");
    sub.ua_out = ua.into();
    sub.started_at = base + Duration::from_secs(10);
    sub.agent = AgentHeaders {
        agent_id: Some("a2".into()),
        agent_type: Some("Explore".into()),
        request_class: Some("subagent".into()),
    };
    t.ingest(sub);
    let st = t.0.state.lock();
    let stopped = st.pending[&key()]
        .events
        .iter()
        .find(|(_, e)| ev_name(e) == "tengu_agent_summary_stopped")
        .map(|(_, e)| meta_of(e))
        .expect("子代理收尾报一条");
    assert_eq!(stopped["session_model_family"], "opus", "{stopped}");
    assert_eq!(stopped["config_flag"], "tengu_groovy_ember");
}

/// [`Telemetry::seed_org_uuid`] 只垫底：已有的不覆盖（不论是先垫的还是响应头学到的），空白的
/// 不算数、也不让后来的那次被快路径挡掉。
#[test]
fn seed_org_uuid_only_fills_a_missing_one() {
    let t = Telemetry::default();
    t.seed_org_uuid(1, Some("org-a"));
    t.seed_org_uuid(1, Some("org-b"));
    assert_eq!(t.org_uuid(1).as_deref(), Some("org-a"));
    t.seed_org_uuid(2, Some("  "));
    assert_eq!(t.org_uuid(2), None);
    t.seed_org_uuid(2, Some("org-c"));
    assert_eq!(t.org_uuid(2).as_deref(), Some("org-c"));
    // 响应头学到的（状态里已有、快路径集合里还没有）：垫底照样不覆盖。
    t.0.state.lock().org_uuid.insert(3, "from-header".into());
    t.seed_org_uuid(3, Some("from-profile"));
    assert_eq!(t.org_uuid(3).as_deref(), Some("from-header"));
}

/// 模拟路径不补工具（`fill_absent_tools` 关，线上默认）时，第三方单轮请求出站是 `request-class: main`、
/// 零个工具。官方只有辅助调用才不带工具（它们的 `request-class` 是 `auxiliary`）；头上明说 `main`
/// 的就是主线程（`claude -p --tools ""` 之类同样如此），不能因为零工具判成无工具 helper、报成
/// `querySource: compact`——那与同一条请求的出站头、billing header 里的 `cc_prompt_id` 自相矛盾。
#[test]
fn a_main_class_request_without_tools_is_reported_as_main() {
    let t = Telemetry::default();
    let mut v: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    v.as_object_mut().unwrap().remove("tools");
    let mut c = call(v.to_string().into_bytes(), "req_notools", "end_turn");
    c.ua_out = "claude-cli/2.1.303 (external, cli)".into();
    c.agent = AgentHeaders { agent_id: None, agent_type: None, request_class: Some("main".into()) };
    t.ingest(c);
    let st = t.0.state.lock();
    let p = &st.pending[&key()];
    let ok = p
        .events
        .iter()
        .find(|(_, e)| ev_name(e) == "tengu_api_success")
        .map(|(_, e)| meta_of(e))
        .expect("主线程那条 api_success");
    assert_eq!(ok["querySource"], "repl_main_thread", "{ok}");
}

/// 无工具的主线程对话（模拟路径不补工具时的第三方多轮）：首轮 `create` 是全量，要记成线程底本，
/// 第二轮 `continue` 的增量（只有 billing header 一块 system、只发新增消息）才补得回全量——
/// 底本此前只在「工具数 > 0」时记，零工具的对话续轮报出来的 system 长度、消息数都是增量那份。
#[test]
fn a_tool_less_main_thread_continue_is_filled_from_its_create() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(60);
    let mut first: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    first.as_object_mut().unwrap().remove("tools");
    first["thread"] = json!({"type": "create"});
    let mut c1 = call(first.to_string().into_bytes(), "req_c1", "end_turn");
    c1.ua_out = "claude-cli/2.1.303 (external, cli)".into();
    c1.started_at = base;
    c1.agent =
        AgentHeaders { agent_id: None, agent_type: None, request_class: Some("main".into()) };
    let msg_id = c1.message_id.clone();
    t.ingest(c1);
    // 续轮：system 只剩 billing header，messages 只有新增那条。
    let billing = first["system"][0].clone();
    let second = json!({
        "model": first["model"], "max_tokens": first["max_tokens"], "thinking": first["thinking"],
        "output_config": first["output_config"], "metadata": first["metadata"], "stream": true,
        "system": [billing],
        "thread": {"type": "continue", "previous_message_id": msg_id},
        "messages": [{"role": "user", "content": [{"type": "text", "text": "and now in German"}]}],
    });
    let mut c2 = call(second.to_string().into_bytes(), "req_c2", "end_turn");
    c2.ua_out = "claude-cli/2.1.303 (external, cli)".into();
    c2.started_at = base + Duration::from_secs(20);
    c2.agent =
        AgentHeaders { agent_id: None, agent_type: None, request_class: Some("main".into()) };
    t.ingest(c2);
    let st = t.0.state.lock();
    let ok: Vec<Value> = st.pending[&key()]
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_api_success")
        .map(|(_, e)| meta_of(e))
        .collect();
    assert_eq!(ok.len(), 2);
    assert_eq!(ok[1]["querySource"], "repl_main_thread", "{}", ok[1]);
    assert_eq!(ok[1]["systemCharLength"], ok[0]["systemCharLength"], "续轮按底本补回完整 system");
}

/// 空文件的 Read：结果整段是那句提醒（后面可能还追着别的提醒），剥完提醒后照样认得出空文件，
/// `file_operation.contentHash` 报空串的 SHA-256。
#[test]
fn an_empty_file_read_keeps_its_content_hash() {
    let warning =
        "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>";
    for result in [warning.to_string(), format!("{warning}\n<system-reminder>x</system-reminder>")]
    {
        let msg =
            json!({"content": [{"type": "tool_result", "tool_use_id": "t1", "content": result}]});
        let r = &tool_results_of(&msg)[0];
        assert_eq!(super::files::read_content(&r.text).as_deref(), Some(""), "{result}");
        assert_eq!(r.stripped_bytes, 0);
        assert_eq!(r.len, 87, "长度按那句提醒算：{result}");
    }
}

/// Bash 命令里提到的扩展名（官方 `hjr`）：小写、按 `\b` 收尾、去重后按字母序。
#[test]
fn bash_mentions_follow_the_official_pattern() {
    use super::files::bash_mentioned_exts;
    assert_eq!(bash_mentioned_exts("tail -5 README.md | cat > out.TXT && cat a.md"), ["md", "txt"]);
    assert_eq!(
        bash_mentioned_exts("open report.docx notes.doc deck.pptx"),
        ["doc", "docx", "pptx"]
    );
    // `.mdx`、`.jsonl`、`.htm` 不算；`.json.bak` 的 json 后面是点，算。
    assert_eq!(bash_mentioned_exts("cat a.mdx b.jsonl c.htm d.json.bak"), ["json"]);
    assert!(bash_mentioned_exts("ls -la").is_empty());
}

/// Bash 的 `fileExtension`（官方 `o8s` + `g6`）：期望值是 node 跑官方原函数的结果；抓包里全部 137 条
/// Bash 命令也逐条对过。
#[test]
fn bash_file_ext_follows_the_official_o8s() {
    use super::files::bash_command_file_ext as ext;
    assert_eq!(ext("tail -20 README.md").as_deref(), Some("md"));
    // 开头的空白让首词变成空串；`..`、`...`、点开头的文件都没有扩展名，超过 10 个字符记 other。
    assert_eq!(ext("  cat a.py"), None);
    assert_eq!(ext("cat .."), None);
    assert_eq!(ext("cat ..."), None);
    assert_eq!(ext("cat .bashrc x.TOOLONGEXTENSION1").as_deref(), Some("other"));
    // 引号照留，按出现先后去重；结尾的 `/` 不影响。
    assert_eq!(ext("rm -rf /tmp/a.b/ && mv x.Py y.py;cp 'q.md' z").as_deref(), Some("b,py,md'"));
    assert_eq!(ext("grep -n foo a.rs|wc -l").as_deref(), Some("rs"));
    assert_eq!(ext("head\u{a0}a.md").as_deref(), Some("md"));
    assert_eq!(ext("/bin/cat dir/x.JSON -n b.c").as_deref(), Some("json,c"));
    assert_eq!(ext("python3 calc.py"), None);
}

/// Bash 那条 `tengu_tool_use_success`：`fileExtension` 2.1.277 起就有（`cap/2.1.277` 的 `sed … .rs` 报 `rs`），
/// `bashCommandFileExtensions` 2.1.285 起才有；两项都在 `toolInputSizeBytes` 与 `bashCommandLen` 之间。
#[test]
fn bash_success_extensions_follow_their_versions() {
    let tu = |command: &str| ToolUse {
        name: "Bash".into(),
        input: json!({ "command": command }),
        command_len: command.len(),
        ..Default::default()
    };
    let keys = |command: &str, v277: bool, v285: bool| {
        let mut success = json!({ "toolInputSizeBytes": 40 });
        tool_success_extras(&mut success, &tu(command), &mut HashMap::new(), v277, v285);
        let keys: Vec<String> = success.as_object().unwrap().keys().take(4).cloned().collect();
        (keys, success)
    };
    let (k, s) = keys("tail -20 README.md", true, true);
    assert_eq!(
        k,
        ["toolInputSizeBytes", "fileExtension", "bashCommandFileExtensions", "bashCommandLen"]
    );
    assert_eq!(
        (&s["fileExtension"], &s["bashCommandFileExtensions"]),
        (&json!("md"), &json!("md"))
    );
    let (k, s) = keys("sed -n 1,5p src/main.rs", true, false);
    assert_eq!(k, ["toolInputSizeBytes", "fileExtension", "bashCommandLen", "bash_command_class"]);
    assert_eq!(s["fileExtension"], "rs");
    let (k, _) = keys("sed -n 1,5p src/main.rs", false, false);
    assert_eq!(k[1], "bashCommandLen", "2.1.277 之前两项都没有");
}

/// Datadog 那份 `tengu_feature_ok` 按服务端抽样表（`tengu_agile_glade`）抽：2.1.293 起表里的只留掷中的百分之一，
/// `sample_rate` 紧跟 `feature_name`、在 `user_bucket` 之前（`cap/auto-2.1.299-20261014-full/00139`）；表外的、
/// 别的事件、更早的版本原样放行。
#[test]
fn datadog_feature_ok_follows_the_server_sampling_table() {
    use super::template::sample_dd_at;
    let ctx = EventCtx { model: "claude-opus-5", betas: "", prompt_id: "p", uptime_secs: 1.0 };
    let entry = |version: &str, message: &str, extra: Value| {
        let id = Identity { version: version.into(), ..identity() };
        id.dd_entry(message, &ctx, "claude-opus-5", extra)
    };
    let ok = |version: &str, name: &str| {
        let props = json!({ "delivery": "turn_end" });
        entry(version, "tengu_feature_ok", feature_with(name, props, true))
    };
    assert!(!sample_dd_at(&mut ok("2.1.306", "queued_message_delivered"), 0.5), "没掷中不发");
    let mut kept = ok("2.1.306", "queued_message_delivered");
    assert!(sample_dd_at(&mut kept, 0.005));
    let keys: Vec<&str> = kept.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys[keys.len() - 4..], ["delivery", "feature_name", "sample_rate", "user_bucket"]);
    assert_eq!(kept["sample_rate"], json!(0.01));
    // 表外的 feature、别的事件、2.1.291（那时客户端还不抽）：原样发。
    for mut e in [
        ok("2.1.306", "history_save"),
        entry("2.1.306", "tengu_api_success", json!({ "model": "claude-opus-5" })),
        ok("2.1.291", "queued_message_delivered"),
    ] {
        let before = e.to_string();
        assert!(sample_dd_at(&mut e, 0.5), "{before}");
        assert_eq!(e.to_string(), before);
    }
}

/// `-p` 的一轮：新输入那条调三个工具（MCP、ToolSearch、Bash），续轮收尾。
fn sdk_turn(t: &Telemetry, end: impl FnOnce(ApiCall) -> ApiCall) {
    sdk_turn_with(t, |_, c| c, end);
}

/// 同 [`sdk_turn`]，首条也能改（先拿到 `Telemetry`，可以在它之前送几次失败的尝试）。
fn sdk_turn_with(
    t: &Telemetry,
    start: impl FnOnce(&Telemetry, ApiCall) -> ApiCall,
    end: impl FnOnce(ApiCall) -> ApiCall,
) {
    let sdk = |body: Value| body.to_string().replace("cc_entrypoint=cli", "cc_entrypoint=sdk-cli");
    let base = frozen_now() - Duration::from_secs(60);
    let first: Value = serde_json::from_slice(&cc_body(true)).unwrap();
    let mut c1 = call(sdk(first.clone()).into_bytes(), "req_1", "tool_use");
    c1.ua_out = "claude-cli/2.1.306 (external, sdk-cli)".into();
    c1.started_at = base;
    c1.ttft_ms = Some(1_000);
    c1.total_ms = 3_000;
    let names = ["mcp__s__x", "ToolSearch", "Bash"];
    c1.tool_calls = names
        .iter()
        .enumerate()
        .map(|(i, n)| ToolCall {
            id: format!("t{i}"),
            name: (*n).into(),
            input: json!({}),
            verdict: None,
        })
        .collect();
    let c1 = start(t, c1);
    t.ingest(c1);
    let mut cont = first;
    let uses: Vec<Value> = names
        .iter()
        .enumerate()
        .map(|(i, n)| json!({"type":"tool_use","id":format!("t{i}"),"name":n,"input":{}}))
        .collect();
    let results: Vec<Value> = (0..names.len())
        .map(|i| json!({"type":"tool_result","tool_use_id":format!("t{i}"),"content":"ok"}))
        .collect();
    let msgs = cont["messages"].as_array_mut().unwrap();
    msgs.push(json!({"role":"assistant","content":uses}));
    msgs.push(json!({"role":"user","content":results}));
    let mut c2 = call(sdk(cont).into_bytes(), "req_2", "end_turn");
    c2.ua_out = "claude-cli/2.1.306 (external, sdk-cli)".into();
    c2.started_at = base + Duration::from_secs(4);
    c2.total_ms = 2_000;
    t.ingest(end(c2));
}

fn sdk_events(t: &Telemetry) -> Vec<(String, Value)> {
    let st = t.0.state.lock();
    st.pending[&key()].events.iter().map(|(_, e)| (ev_name(e).to_string(), meta_of(e))).collect()
}

/// `tengu_sdk_result` / `tengu_sdk_ttft`：时长从本轮开始算、首字量到第一条回复收完，工具按名字分
/// MCP / ToolSearch / 内置（官方 `ux`）。
#[test]
fn sdk_result_counts_tools_and_times_from_the_turn() {
    let t = Telemetry::default();
    sdk_turn(&t, |c| c);
    let ev = sdk_events(&t);
    let get = |n: &str| ev.iter().find(|(x, _)| x == n).map(|(_, m)| m.clone()).unwrap();
    let r = get("tengu_sdk_result");
    assert_eq!(r["subtype"], "success");
    assert_eq!(r["num_turns"], 2);
    let counts = ["tool_use_count", "mcp_tool_calls", "toolsearch_calls", "builtin_tool_calls"];
    assert_eq!(counts.map(|k| r[k].as_u64().unwrap()), [3, 1, 1, 1], "{r}");
    // 本轮开始 = 首条发出前 15 + 42ms；首字 = 首条回复收完前 5ms；结果在末条收尾后 4ms。
    assert_eq!(get("tengu_sdk_ttft")["ttft_ms"], 3_000 - 5 + 57);
    assert_eq!(r["duration_ms"], 4_000 + 2_000 + 4 + 57);
}

/// `-p` 这一轮请求失败报 `success` + `is_error`、被中止报 `terminated`，都不报首字；中止也不报界面上的
/// Esc 与撤回输入。
#[test]
fn sdk_result_reports_failures_and_aborts() {
    let failed_t = Telemetry::default();
    sdk_turn(&failed_t, |c| failed(c, Some(529), "overloaded_error", "Overloaded"));
    give_up_retries(&failed_t);
    let ev = sdk_events(&failed_t);
    let r = ev.iter().find(|(n, _)| n == "tengu_sdk_result").map(|(_, m)| m).unwrap();
    // 请求失败官方报的仍是 `success`，`is_error` 为真，另带最终失败的状态码
    // （`cap/auto-2.1.306-20261021-retry` R2）。
    assert_eq!((r["subtype"].as_str(), r["is_error"].as_bool()), (Some("success"), Some(true)));
    assert_eq!(r["api_error_status"], 529);
    assert!(!ev.iter().any(|(n, _)| n == "tengu_sdk_ttft"));

    let aborted_t = Telemetry::default();
    sdk_turn(&aborted_t, |mut c| {
        c.aborted = true;
        c.stop_reason = None;
        c
    });
    let ev = sdk_events(&aborted_t);
    let r = ev.iter().find(|(n, _)| n == "tengu_sdk_result").map(|(_, m)| m).unwrap();
    assert_eq!((r["subtype"].as_str(), r["is_error"].as_bool()), (Some("terminated"), Some(true)));
    for absent in ["tengu_sdk_ttft", "tengu_cancel", "tengu_conversation_rewind"] {
        assert!(!ev.iter().any(|(n, _)| n == absent), "{absent}");
    }
}

/// 图片的字节数扣掉 base64 填充、像素数按文件头读出宽高（`cap/auto-2.1.306-20261021-full` A0：
/// 32×32 的 PNG、121 字节）。
#[test]
fn image_stats_follow_the_official_decoder() {
    let png = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAQElEQVR4nGOQizqBFf3HgUhVzzBqwagFQ8ACahmES/2oBaMWDAULqGUQLvWjFoxaMBQsoJZBuNSPWjBqwRCwAACst8JbyMsFeQAAAABJRU5ErkJggg==";
    assert_eq!(super::shape::image_dims(png), Some((32, 32)));
    let mut shape = RequestShape::default();
    let content = json!([{"type":"tool_result","tool_use_id":"t","content":[
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":png}}
    ]}]);
    super::shape::walk_content(&content, &mut shape);
    assert_eq!((shape.image_blocks, shape.image_bytes, shape.image_pixels), (1, 121, 1024));
}

/// 模型调了这一轮没声明的工具（Glob）：只报 `feature_bad{tool_not_found}` 与 `tool_use_error{NO_SUCH_TOOL}`，
/// 不报授权与成功（`cap/auto-2.1.306-20261021-full/00387`）。
#[test]
fn an_undeclared_tool_reports_no_such_tool() {
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(first);
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Glob","input":{"pattern":"*"}}]});
    body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,
        "content":"<tool_use_error>Error: No such tool available: Glob. Glob is not available in this session</tool_use_error>"}]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(second);
    let st = t.0.state.lock();
    let ev: Vec<(String, Value)> = st.pending[&key()]
        .events
        .iter()
        .map(|(_, e)| (ev_name(e).to_string(), meta_of(e)))
        .collect();
    let tool_ev = |n: &str| ev.iter().filter(|(x, m)| x == n && m["toolName"] == "Glob").count();
    for absent in [
        "tengu_tool_use_success",
        "tengu_tool_use_granted_in_config",
        "tengu_tool_use_can_use_tool_allowed",
    ] {
        assert_eq!(tool_ev(absent), 0, "{absent}");
    }
    let err = ev.iter().find(|(n, _)| n == "tengu_tool_use_error").map(|(_, m)| m).unwrap();
    assert_eq!(err["error"], "No such tool available: registered_base_tool");
    assert_eq!(err["errorCode"], "NO_SUCH_TOOL");
    assert_eq!(err["toolNameHash"], "18d00f7df062");
    let bad = ev.iter().find(|(n, _)| n == "tengu_feature_bad").map(|(_, m)| m).unwrap();
    assert_eq!(
        (&bad["feature_name"], &bad["error_code"]),
        (&json!("tool_glob"), &json!("tool_not_found"))
    );
}

/// 没注册的 MCP 工具：官方脱敏成 `mcp_tool`，文案 `No such tool available: mcp_tool`，功能名另叫
/// `tool_unregistered_mcp`（2.1.306 `qYr` / `IJ`）。
#[test]
fn an_unregistered_mcp_tool_reports_mcp_tool() {
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(first);
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"mcp__my_local_server__lookup","input":{}}]});
    body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,
        "content":"<tool_use_error>Error: No such tool available: mcp__my_local_server__lookup</tool_use_error>"}]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(second);
    let st = t.0.state.lock();
    let ev: Vec<(String, Value)> = st.pending[&key()]
        .events
        .iter()
        .map(|(_, e)| (ev_name(e).to_string(), meta_of(e)))
        .collect();
    let err = ev.iter().find(|(n, _)| n == "tengu_tool_use_error").map(|(_, m)| m).unwrap();
    assert_eq!(
        (&err["toolName"], &err["error"], &err["isMcp"]),
        (&json!("mcp_tool"), &json!("No such tool available: mcp_tool"), &json!(true))
    );
    let bad = ev.iter().find(|(n, _)| n == "tengu_feature_bad").map(|(_, m)| m).unwrap();
    assert_eq!(bad["feature_name"], "tool_unregistered_mcp");
}

/// 字符匹配照 JS 正则：`\s` 是 ECMAScript 的空白集合（U+FEFF 算、U+0085 不算），`/i` 没有 `u` 标志时非 ASCII 字符
/// 不折成 ASCII 字母（开尔文符号不算 `k`）。期望哈希是官方 2.1.305 原函数跑出来的。
#[test]
fn scrub_follows_js_whitespace_and_case_rules() {
    use super::tools::scrub_error_message as dze;
    let feff = "Read failed for /tmp/a\u{FEFF}b/file.txt";
    assert_eq!(dze(feff), "Read failed for <path>\u{FEFF}b/file.txt");
    assert_eq!(&sha256_hex(dze(feff).as_bytes())[..12], "69bedba00c59");
    let nel = "Read failed for /tmp/a\u{0085}b/file.txt";
    assert_eq!(dze(nel), "Read failed for <path>", "U+0085 在 JS 里不是空白");
    // `s` 后面跟开尔文符号（U+212A）：Unicode 折叠会把它当 `k`，JS 不会。
    assert_eq!(
        dze("token s\u{212A}-abcdefghijk"),
        "token s\u{212A}-abcdefghijk",
        "开尔文符号不折成 k"
    );
    assert_eq!(dze("token SK-abcdefghijk"), "token <key>", "ASCII 大小写照样不分");
}

/// 截断照官方按 UTF-16 码元：第 500 个码元落在 emoji 代理对中间时，留下的半个代理编成 UTF-8 是 U+FFFD，
/// 而不是整个 emoji 都丢掉。
#[test]
fn scrub_truncation_splits_a_surrogate_pair_like_js() {
    use super::tools::scrub_error_message as dze;
    // 填充用带空格的短词，避开 base64 之类的规整规则。
    let fill = |n: usize| "a ".repeat(n / 2) + if n % 2 == 1 { "a" } else { "" };
    let msg = format!("{}😀tail", fill(499));
    assert_eq!(dze(&msg), format!("{}\u{FFFD}", fill(499)));
    let fits = format!("{}😀tail", fill(498));
    assert_eq!(dze(&fits), format!("{}😀", fill(498)), "整个装得下就留着");
}

/// 报错文案脱敏与官方 `dZe` 逐条一致（期望值是把 2.1.306 里那个函数原样拿到 node 里跑出来的）。
#[test]
fn error_message_scrub_matches_the_official_function() {
    use super::tools::scrub_error_message as dze;
    for (input, want) in [
        (
            "File does not exist. Note: your current working directory is /Users/x/proj. Did you mean /Users/x/proj/a.py?",
            "File does not exist. Note: your current working directory is <path> Did you mean <path>",
        ),
        (
            "String to replace not found in file.\nString: foo 12345 bar",
            "String to replace not found in file.\nString: foo <num> bar",
        ),
        (
            "see https://a.b/c?x=1 and me@x.io key sk-ant-abcdefghijkl",
            "see <url> and <email> key <key>",
        ),
        ("C:\\Users\\x y \\\\srv\\share a\\b\\c", "<path> y <path> <path>"),
        (
            "id 3c1f0a4e-5c4f-4a8b-9d2e-7f0a1b2c3d4e req_011CgEiRQ6PstaucYdYEZ6Kd deadbeefdeadbeef00 ip 10.0.0.1 n 2026 x 123",
            "id <id> <id> <id> ip <ip> n <num> x 123",
        ),
        ("QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo0MTIzNDU2Nzg5MA== end", "<b64> end"),
        ("中文/路径/a.txt ok", "中文<path> ok"),
    ] {
        assert_eq!(dze(input), want, "{input}");
    }
}

/// Edit 改不存在的文件：验证这一步就被拦下，报 `feature_sad{tool_validate_input_rejected}` 与
/// `tool_use_error{ValidateInputError, errorCode: 4}`，不报授权与成功。
#[test]
fn a_rejected_validation_reports_validate_input_error() {
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(first);
    let msg = "File does not exist. Note: your current working directory is /Users/x/proj.";
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit",
        "input":{"file_path":"/Users/x/proj/nope.py","old_string":"a","new_string":"b"}}]});
    body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,
        "content":format!("<tool_use_error>{msg}</tool_use_error>")}]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(second);
    let st = t.0.state.lock();
    let ev: Vec<(String, Value)> = st.pending[&key()]
        .events
        .iter()
        .map(|(_, e)| (ev_name(e).to_string(), meta_of(e)))
        .collect();
    let edit = |n: &str| ev.iter().filter(|(x, m)| x == n && m["toolName"] == "Edit").count();
    for absent in [
        "tengu_tool_use_success",
        "tengu_tool_use_granted_in_config",
        "tengu_tool_use_can_use_tool_allowed",
        "tengu_file_operation",
    ] {
        assert_eq!(edit(absent), 0, "{absent}");
    }
    let err = ev.iter().find(|(n, _)| n == "tengu_tool_use_error").map(|(_, m)| m).unwrap();
    assert_eq!((&err["error"], &err["errorCode"]), (&json!("ValidateInputError"), &json!(4)));
    let hash = &sha256_hex(super::tools::scrub_error_message(msg).as_bytes())[..12];
    assert_eq!(err["error_message_hash"], *hash);
    let sad = ev.iter().find(|(n, _)| n == "tengu_feature_sad").map(|(_, m)| m).unwrap();
    assert_eq!(sad["error_code"], "tool_validate_input_rejected");
    assert_eq!(sad["feature_name"], "tool_edit");
}

/// 校验失败的错误码补齐 2.1.305 / 2.1.306 的几种：Edit 前后相同 1、Write 子代理写报告文件 5、NotebookEdit 结构不对 6、
/// Edit / NotebookEdit 遇到非 UTF-8 文本 15（官方 `VSt` 三种开头）。
#[test]
fn validate_error_codes_cover_the_remaining_cases() {
    use super::tools::validate_error_code as code;
    let enc = |head: &str| {
        format!(
            "File {head}. These tools write UTF-8 (or UTF-16LE behind its byte-order mark) and never convert a file's encoding on their own, so nothing was written."
        )
    };
    for (tool, msg, want) in [
        ("Edit", "No changes to make: old_string and new_string are exactly the same.".to_string(), 1),
        ("Write", "Subagents should return findings as text, not write report files. Include this content in your final response instead.".to_string(), 5),
        ("NotebookEdit", "Notebook file is not a valid Jupyter notebook (top-level \"cells\" must be an array of cell objects, each with a string or string-array \"source\").".to_string(), 6),
        ("Edit", enc("is not valid UTF-8: it uses a legacy encoding such as Windows-1252, Shift-JIS or GBK, or it is binary"), 15),
        ("Edit", enc("has NUL bytes in its first 8 KB: binary data, UTF-16/UTF-32 without a byte-order mark, or text with embedded NULs"), 15),
        ("NotebookEdit", enc("is UTF-16 big-endian (it starts FE FF), which these tools do not write"), 15),
    ] {
        assert_eq!(code(tool, &msg), Some(want), "{tool}: {msg}");
    }
    // 原有的照旧。
    assert_eq!(code("Edit", "File does not exist."), Some(4));
}

/// Edit 前后字符串相同：验证这一步就拦下，报 1 号 `ValidateInputError`，不走授权、不执行。
#[test]
fn a_no_op_edit_is_rejected_at_validation() {
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(first);
    let msg = "No changes to make: old_string and new_string are exactly the same.";
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit",
        "input":{"file_path":"/Users/x/proj/a.py","old_string":"a","new_string":"a"}}]});
    body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,
        "content":format!("<tool_use_error>{msg}</tool_use_error>")}]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(second);
    let st = t.0.state.lock();
    let ev: Vec<(String, Value)> = st.pending[&key()]
        .events
        .iter()
        .map(|(_, e)| (ev_name(e).to_string(), meta_of(e)))
        .collect();
    let edit = |n: &str| ev.iter().filter(|(x, m)| x == n && m["toolName"] == "Edit").count();
    for absent in ["tengu_tool_use_granted_in_config", "tengu_tool_use_success"] {
        assert_eq!(edit(absent), 0, "{absent}");
    }
    let err = ev.iter().find(|(n, _)| n == "tengu_tool_use_error").map(|(_, m)| m).unwrap();
    assert_eq!((&err["error"], &err["errorCode"]), (&json!("ValidateInputError"), &json!(1)));
}

/// `-p` 首条请求重发过两次才成功、之后工具续轮正常收尾：`saw_retry` / `retry_status` 按整轮累计（取各次重试
/// 状态码的最大值），两个时长从头一次尝试发出算起。
#[test]
fn sdk_result_keeps_the_turn_retries() {
    let t = Telemetry::default();
    sdk_turn_with(
        &t,
        |t, c1| {
            // 同一条请求先失败两次（503、529），各隔 1.5 秒重发。
            for (back_ms, status) in [(3_000, 503), (1_500, 529)] {
                let mut a = call(c1.body.to_vec(), "req_0", "end_turn");
                a.ua_out = c1.ua_out.clone();
                a.started_at = c1.started_at - Duration::from_millis(back_ms);
                a.total_ms = 300;
                t.ingest(failed(a, Some(status), "overloaded_error", "Overloaded"));
            }
            c1
        },
        |c| c,
    );
    let ev = sdk_events(&t);
    let get = |n: &str| ev.iter().find(|(x, _)| x == n).map(|(_, m)| m.clone()).unwrap();
    let r = get("tengu_sdk_result");
    assert_eq!((&r["saw_retry"], &r["retry_status"]), (&json!(true), &json!(529)), "{r}");
    assert_eq!(r["duration_ms"], 4_000 + 2_000 + 4 + 57 + 3_000);
    assert_eq!(get("tengu_sdk_ttft")["ttft_ms"], 3_000 - 5 + 57 + 3_000);
}

/// 入参没过 schema（Edit 缺 `old_string`）：报 `ZOD_VALIDATION` 与按文案认出的 `zodIssueCodes`，不报成功。
#[test]
fn a_schema_failure_reports_zod_validation() {
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(first);
    let detail =
        "Edit failed due to the following issue:\nThe required parameter `old_string` is missing";
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit",
        "input":{"file_path":"/p/a.py","new_string":"b"}}]});
    body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,
        "content":format!("<tool_use_error>InputValidationError: {detail}</tool_use_error>")}]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(second);
    let st = t.0.state.lock();
    let ev: Vec<(String, Value)> = st.pending[&key()]
        .events
        .iter()
        .map(|(_, e)| (ev_name(e).to_string(), meta_of(e)))
        .collect();
    assert!(!ev.iter().any(|(n, m)| n == "tengu_tool_use_success" && m["toolName"] == "Edit"));
    let err = ev.iter().find(|(n, _)| n == "tengu_tool_use_error").map(|(_, m)| m).unwrap();
    assert_eq!(
        (&err["error"], &err["errorCode"]),
        (&json!("InputValidationError"), &json!("ZOD_VALIDATION"))
    );
    assert_eq!(err["zodIssueCodes"], "invalid_type");
    assert_eq!(err["errorDetailsHash"], json!(&sha256_hex(detail.as_bytes())[..12]));
    let sad = ev.iter().find(|(n, _)| n == "tengu_feature_sad").map(|(_, m)| m).unwrap();
    assert_eq!(sad["error_code"], "tool_input_validation_failed");
}

/// 认不出的执行失败（WebFetch 抛错）也不报成成功：报 `tool_use_error`，不报 `tool_web_fetch` 的 feature_ok。
#[test]
fn an_unrecognized_tool_failure_is_not_a_success() {
    let t = Telemetry::default();
    let mut first = call(cc_body(true), "req_1", "tool_use");
    first.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(first);
    let mut body: Value = serde_json::from_slice(&cc_body(false)).unwrap();
    body["messages"][1] = json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"WebFetch",
        "input":{"url":"https://example.invalid","prompt":"x"}}]});
    body["messages"][2] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,
        "content":"<tool_use_error>Error: getaddrinfo ENOTFOUND example.invalid</tool_use_error>"}]});
    let mut second = call(body.to_string().into_bytes(), "req_2", "end_turn");
    second.ua_out = "claude-cli/2.1.306 (external, cli)".into();
    t.ingest(second);
    let st = t.0.state.lock();
    let ev: Vec<(String, Value)> = st.pending[&key()]
        .events
        .iter()
        .map(|(_, e)| (ev_name(e).to_string(), meta_of(e)))
        .collect();
    assert!(!ev.iter().any(|(n, m)| n == "tengu_tool_use_success" && m["toolName"] == "WebFetch"));
    assert!(
        !ev.iter().any(|(n, m)| n == "tengu_feature_ok" && m["feature_name"] == "tool_web_fetch")
    );
    let err = ev.iter().find(|(n, _)| n == "tengu_tool_use_error").map(|(_, m)| m).unwrap();
    assert_eq!((&err["toolName"], &err["error"]), (&json!("WebFetch"), &json!("Error")));
}

/// `cap/auto-2.1.308-20261022-gaps` 里的一条：文件编号、停止原因、回复里的工具调用（工具名 → 入参字符数）。
type GapStep = (&'static str, &'static str, &'static [(&'static str, usize)]);

/// `cap/auto-2.1.308-20261022-gaps` 里的一条请求照抓包做成 [`ApiCall`]（停止原因与工具调用由调用方给）；抓包目录
/// 不在就是 `None`。
fn gaps_call(step: &GapStep, cred_id: i64, at: SystemTime) -> Option<ApiCall> {
    let (f, stop, tools) = *step;
    let rel = format!("auto-2.1.308-20261022-gaps/{f}_");
    let (body, betas) = cap_request(&rel)?;
    let shape = parse_shape(&body).unwrap();
    let mut c = call(body, &format!("req_{f}"), stop);
    c.cred_id = cred_id;
    c.betas = betas;
    c.ua_out = "claude-cli/2.1.308 (external, cli)".into();
    c.resp_model = Some(shape.model.clone());
    c.message_id = Some(format!("msg_{f}"));
    c.started_at = at;
    c.tool_use_lens = tools.iter().map(|(n, l)| (n.to_string(), *l)).collect();
    c.client_request_id = cap_header(&rel, "x-client-request-id");
    c.agent = AgentHeaders {
        agent_id: cap_header(&rel, "x-claude-code-agent-id"),
        agent_type: cap_header(&rel, "x-claude-code-agent-type"),
        request_class: cap_header(&rel, "x-claude-code-request-class"),
    };
    Some(c)
}

/// 回放 `cap/auto-2.1.308-20261022-gaps` 里一段会话（各条的停止原因与工具调用由调用方给），返回会话 id。
fn replay_2_1_308_gaps(t: &Telemetry, cred_id: i64, steps: &[GapStep]) -> Option<String> {
    let base = frozen_now() - Duration::from_secs(600);
    let mut session = None;
    for (i, step) in steps.iter().enumerate() {
        let c = gaps_call(step, cred_id, base + Duration::from_secs(10 * i as u64))?;
        session = parse_shape(&c.body).unwrap().session_id;
        t.ingest(c);
    }
    session
}

/// `modelHeldStateless` 跟服务端清单 `tengu_curious_tower_stateless_models` 走（`cap/auto-2.1.308-20261022-gaps`
/// 的 fable-5-1 默认权限模式：`00037` 首轮调 Read、`00039` 续轮）：
/// - 没见过这个号的 eval：2.1.291 起按空串（这一版起抓包里一律是空串），两条都是 false，首轮 `create`、续轮
///   `continue`——官方就是这样，此前写死「fable-5-1 一律 true」与实际建线程自相矛盾；
/// - 号的 eval 里清单列了 fable-5-1：照官方报 true；清单是空串时即使是 2.1.285 这种老版本也报 false。
#[test]
fn model_held_stateless_follows_the_server_list() {
    const STEPS: &[GapStep] =
        &[("00037", "tool_use", &[("Read", 135)]), ("00039", "end_turn", &[])];
    let decisions = |cred_id: i64| -> Option<Vec<(Value, Value, Value)>> {
        let t = Telemetry::default();
        let session = replay_2_1_308_gaps(&t, cred_id, STEPS)?;
        let st = t.0.state.lock();
        let p = st.pending.get(&(cred_id, session)).expect("queued");
        let metas = |n: &str| -> Vec<Value> {
            p.events.iter().filter(|(_, e)| ev_name(e) == n).map(|(_, e)| meta_of(e)).collect()
        };
        let dec = metas("tengu_tether_decision");
        let live = metas("tengu_tether_live_outcome");
        assert_eq!((dec.len(), live.len()), (2, 2));
        Some(
            dec.iter()
                .zip(&live)
                .map(|(d, l)| {
                    assert_eq!(d["modelHeldStateless"], l["modelHeldStateless"]);
                    (
                        d["modelHeldStateless"].clone(),
                        d["decision"].clone(),
                        l["sentThreadType"].clone(),
                    )
                })
                .collect(),
        )
    };
    let Some(unseen) = decisions(7) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    assert_eq!(
        unseen,
        [
            (json!(false), json!("create"), json!("create")),
            (json!(false), json!("continue"), json!("continue"))
        ]
    );
    let listed = 9_308_001;
    crate::proxy::note_eval_features(
        listed,
        br#"{"features":{"tengu_curious_tower_stateless_models":{"value":"fable-5-1"}}}"#,
    );
    assert!(decisions(listed).unwrap().iter().all(|(held, _, _)| held == &json!(true)));
    // 版本兜底只在没见过 eval 时用：清单是空串的号，老版本照样报 false。
    let empty = 9_308_002;
    crate::proxy::note_eval_features(empty, br#"{"features":{}}"#);
    assert!(!super::kind::model_held_stateless("claude-fable-5-1", Some(""), false));
    assert!(
        super::kind::model_held_stateless("claude-fable-5-1", None, false),
        "2.1.291 之前沿用旧规则"
    );
    assert!(!super::kind::model_held_stateless("claude-fable-5-1", None, true));
    // 清单按规范名写、请求带 provider id 的也算（官方 `O8e` 还比 `qe()` 映射后的规范名）。
    assert!(super::kind::model_held_stateless(
        "claude-sonnet-4-20250514",
        Some("claude-sonnet-4-0"),
        true
    ));
    assert!(decisions(empty).unwrap().iter().all(|(held, _, _)| held == &json!(false)));
}

/// 子代理事件顶层 `betas` 按事件类别取（`cap/auto-2.1.308-20261022-gaps`：opus 主线程 `00116` 派 claude-code-guide、
/// `00118` 收尾；子代理跑 haiku-5-5，`00119` 调 WebFetch、`00123` 跑 Bash、`00124` 收尾）：
/// - 它自己的 `api_query` / `api_success` 是这条请求完整的 beta 串（16 项，与 meta 同值）；
/// - `agent_tool_selected` / `agent_tool_completed` 与它里面那条 Bash（事件与 Datadog）是它自己那份会话级 beta
///   （haiku 那 6 项）；
/// - 支线上其余事件是主线程那份（opus 的 7 项）。
#[test]
fn subagent_event_betas_follow_the_event_kind() {
    const STEPS: &[GapStep] = &[
        ("00116", "tool_use", &[("Agent", 270)]),
        ("00118", "end_turn", &[]),
        ("00119", "tool_use", &[("WebFetch", 257)]),
        ("00123", "tool_use", &[("Bash", 416)]),
        ("00124", "end_turn", &[]),
    ];
    const OWN: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
        thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05";
    const MAIN: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
        redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,\
        prompt-caching-scope-2026-01-05";
    let t = Telemetry::default();
    let Some(session) = replay_2_1_308_gaps(&t, 7, STEPS) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    let full: Vec<String> = ["00119", "00123", "00124"]
        .iter()
        .map(|f| cap_request(&format!("auto-2.1.308-20261022-gaps/{f}_")).unwrap().1.unwrap())
        .collect();
    assert_eq!(full[0].split(',').count(), 16);
    let agent = cap_header("auto-2.1.308-20261022-gaps/00119_", "x-claude-code-agent-id").unwrap();
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).expect("queued");
    let top = |e: &Value| e["event_data"]["betas"].as_str().unwrap_or_default().to_string();
    let of_agent = |e: &Value| e["event_data"]["agent_id"] == agent.as_str();

    for n in ["tengu_api_query", "tengu_api_success"] {
        let got: Vec<String> = p
            .events
            .iter()
            .filter(|(_, e)| ev_name(e) == n && of_agent(e))
            .map(|(_, e)| top(e))
            .collect();
        assert_eq!(got, full, "{n}：完整请求串");
        for (_, e) in p.events.iter().filter(|(_, e)| ev_name(e) == n && of_agent(e)) {
            assert_eq!(meta_of(e)["betas"].as_str().unwrap(), top(e), "{n}：顶层与 meta 同值");
        }
    }
    let one = |n: &str| -> Value {
        p.events
            .iter()
            .find(|(_, e)| ev_name(e) == n)
            .map(|(_, e)| e.clone())
            .unwrap_or_else(|| panic!("{n}"))
    };
    assert_eq!(top(&one("tengu_agent_tool_selected")), OWN);
    assert_eq!(top(&one("tengu_agent_tool_completed")), OWN);
    assert_eq!(top(&one("tengu_bash_tool_command_executed")), OWN);
    let dd_bash = p.dd.iter().find(|d| d["message"] == "tengu_bash_tool_command_executed").unwrap();
    assert_eq!(dd_bash["betas"], OWN, "Datadog 那条同样是它自己那份");
    // 支线上其余事件：主线程那份。
    let tool_ok: Vec<String> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_tool_use_success" && of_agent(e))
        .map(|(_, e)| top(e))
        .collect();
    assert!(!tool_ok.is_empty() && tool_ok.iter().all(|b| b == MAIN), "{tool_ok:?}");
}

/// 主线程（fable）那 7 项与 haiku-5-5 自己那 6 项会话级 beta。
const GAPS_MAIN_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
    redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,\
    prompt-caching-scope-2026-01-05";
const GAPS_HAIKU_BETAS: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
    redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,\
    prompt-caching-scope-2026-01-05";

/// 一个会话里某几条事件的（顶层 `model`，顶层 `betas`）。
fn top_model_betas(p: &Pending, name: &str) -> Vec<(String, String)> {
    p.events
        .iter()
        .filter(|(_, e)| ev_name(e) == name)
        .map(|(_, e)| {
            let d = &e["event_data"];
            (
                d["model"].as_str().unwrap_or_default().into(),
                d["betas"].as_str().unwrap_or_default().into(),
            )
        })
        .collect()
}

/// 标题这类侧查询处理时报的事件，顶层是主线程那份会话级 beta（`cap/auto-2.1.308-20261022-gaps` G0：haiku-5-5 的标题
/// `00036` 先到、被扣住，fable 主线程 `00037` 处理完才补处理）：`session_title_generated` / `tool_schema_sizes` /
/// `tool_search_mode_decision` 都是主线程那 7 项；meta 报标题自己模型的 `sleepy_snowflake_applied` 是 haiku 那 6 项。
#[test]
fn side_query_events_report_the_main_session_betas() {
    const STEPS: &[GapStep] = &[
        ("00036", "end_turn", &[]),
        ("00037", "tool_use", &[("Read", 135)]),
        ("00039", "end_turn", &[]),
    ];
    let t = Telemetry::default();
    let Some(session) = replay_2_1_308_gaps(&t, 7, STEPS) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).expect("queued");
    for n in [
        "tengu_session_title_generated",
        "tengu_tool_schema_sizes",
        "tengu_tool_search_mode_decision",
    ] {
        let got = top_model_betas(p, n);
        assert!(!got.is_empty() && got.iter().all(|(_, b)| b == GAPS_MAIN_BETAS), "{n}: {got:?}");
    }
    let sleepy: Vec<(String, String)> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_sleepy_snowflake_applied")
        .map(|(_, e)| {
            let b = e["event_data"]["betas"].as_str().unwrap_or_default();
            (meta_of(e)["model"].as_str().unwrap_or_default().to_string(), b.to_string())
        })
        .collect();
    assert!(sleepy.contains(&("claude-haiku-5-5".into(), GAPS_HAIKU_BETAS.into())), "{sleepy:?}");
    assert!(sleepy.contains(&("claude-fable-5-1".into(), GAPS_MAIN_BETAS.into())), "{sleepy:?}");
}

/// 主线程那条一直没来、扣满时限的标题（`cap/auto-2.1.308-20261022-gaps` 的 `00115`，单独处理）：按会话主模型推
/// 主线程那份——设备记得默认模型（先处理过同一台机器上 fable 那个会话的 `00037`）就按它，顶层 `model` 与 `betas`
/// 都是 fable；设备也没见过、会话由标题自己建的，两样都只能用标题自己的（haiku）。
#[test]
fn a_lone_title_falls_back_to_the_session_main_model() {
    let titles = |seed: bool| -> Option<Vec<(String, String)>> {
        let t = Telemetry::default();
        let base = frozen_now() - Duration::from_secs(600);
        if seed {
            t.ingest(gaps_call(&("00037", "tool_use", &[("Read", 135)]), 7, base)?);
        }
        let title = gaps_call(&("00115", "end_turn", &[]), 7, base + Duration::from_secs(60))?;
        let session = parse_shape(&title.body).unwrap().session_id.unwrap();
        t.ingest(title);
        let hold = Duration::from_secs(crate::config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1);
        t.gc(std::time::Instant::now() + hold);
        let st = t.0.state.lock();
        Some(top_model_betas(
            st.pending.get(&(7, session)).expect("queued"),
            "tengu_session_title_generated",
        ))
    };
    let Some(seeded) = titles(true) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    assert_eq!(seeded, [("claude-fable-5-1".to_string(), GAPS_MAIN_BETAS.to_string())]);
    assert_eq!(
        titles(false).unwrap(),
        [("claude-haiku-5-5".to_string(), GAPS_HAIKU_BETAS.to_string())]
    );
}

/// 孤立标题扣满时限补处理之后，指标导出那条 `tengu_feature_ok{internal_metrics_export}` 的顶层 `model` 与 `betas`
/// 同源：设备默认主模型是 fable（先处理过 `00037`），两样都报 fable 那份，不能一个 haiku（标题自己的）一个 fable。
#[test]
fn a_lone_title_exports_metrics_with_the_session_main_model() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(600);
    let Some(seed) = gaps_call(&("00037", "tool_use", &[("Read", 135)]), 7, base) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    t.ingest(seed);
    let title = gaps_call(&("00115", "end_turn", &[]), 7, base + Duration::from_secs(60)).unwrap();
    let session = parse_shape(&title.body).unwrap().session_id.unwrap();
    t.ingest(title);
    let hold = Duration::from_secs(crate::config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1);
    let now = std::time::Instant::now() + hold;
    t.gc(now);
    let later = now + Duration::from_secs(crate::config::TELEMETRY_METRICS_FLUSH_SECS + 1);
    let exports: Vec<(String, String)> = t
        .take_due(later)
        .iter()
        .filter(|f| f.session_id == session)
        .flat_map(|f| f.events.iter())
        .filter(|e| {
            ev_name(e) == "tengu_feature_ok"
                && meta_of(e)["feature_name"] == "internal_metrics_export"
        })
        .map(|e| {
            let d = &e["event_data"];
            (
                d["model"].as_str().unwrap_or_default().into(),
                d["betas"].as_str().unwrap_or_default().into(),
            )
        })
        .collect();
    assert_eq!(exports, [("claude-fable-5-1".to_string(), GAPS_MAIN_BETAS.to_string())]);
}

/// 来访模型名可以是任意 UTF-8：一个 `claude-模型` 的主线程请求（400）被记成设备默认模型后，同设备只有标题的新会话
/// 扣满时限补处理，按它推主线程那份 beta（[`crate::proxy::cc_main_beta_at`]）不能切进多字节字符 panic——那会让
/// 生产里的发送任务退出。
#[test]
fn a_lone_title_survives_a_non_ascii_device_default_model() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(600);
    let Some(mut seed) = gaps_call(&("00037", "end_turn", &[]), 7, base) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    let body = String::from_utf8(seed.body.to_vec()).unwrap();
    assert!(body.contains("\"claude-fable-5-1\""));
    seed.body = Bytes::from(body.replace("\"claude-fable-5-1\"", "\"claude-模型\""));
    seed.resp_model = Some("claude-模型".into());
    t.ingest(failed(seed, Some(400), "invalid_request_error", "model: 模型"));
    let title = gaps_call(&("00115", "end_turn", &[]), 7, base + Duration::from_secs(60)).unwrap();
    let session = parse_shape(&title.body).unwrap().session_id.unwrap();
    t.ingest(title);
    let hold = Duration::from_secs(crate::config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1);
    t.gc(std::time::Instant::now() + hold);
    let st = t.0.state.lock();
    let got = top_model_betas(
        st.pending.get(&(7, session)).expect("queued"),
        "tengu_session_title_generated",
    );
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, "claude-模型");
}

/// 2.1.308 haiku-5-5\[1m] 会话（`cap/auto-2.1.308-20261022-gaps` G3：标题 `00078` 先发、主线程 `00079` 随后）只有一条
/// `tengu_sleepy_snowflake_applied`，`model` 是标题那条的 `claude-haiku-5-5`：官方剥 `[1m]` 去重。标题被扣住、主线程先
/// 处理，主线程也不能抢走这个键。
#[test]
fn sleepy_dedups_haiku_1m_against_its_earlier_title() {
    const STEPS: &[GapStep] = &[
        ("00078", "end_turn", &[]),
        ("00079", "tool_use", &[("Read", 135)]),
        ("00081", "end_turn", &[]),
    ];
    let t = Telemetry::default();
    let Some(session) = replay_2_1_308_gaps(&t, 7, STEPS) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    let st = t.0.state.lock();
    let p = st.pending.get(&(7, session)).expect("queued");
    let sleepy: Vec<Value> = p
        .events
        .iter()
        .filter(|(_, e)| ev_name(e) == "tengu_sleepy_snowflake_applied")
        .map(|(_, e)| meta_of(e)["model"].clone())
        .collect();
    assert_eq!(sleepy, [json!("claude-haiku-5-5")]);
}

/// 转发路径发出这条请求时的登记（[`Telemetry::note_dispatch`]）。生产里登记与 `ApiCall::started_at`（经
/// `Capture::started_at`）取的都是这条请求入站那一刻的墙钟（`proxy::ReqStart::wall`），故这里传 `c.started_at`。
fn dispatch(t: &Telemetry, c: &ApiCall) {
    let mut h = axum::http::HeaderMap::new();
    if let Some(b) = &c.betas {
        h.insert("anthropic-beta", b.parse().unwrap());
    }
    t.note_dispatch(c.cred_id, &h, &c.body, c.started_at);
}

/// 一个会话所有 `tengu_sleepy_snowflake_applied` 的 `model`（已刷出的批次加上还在待发队列里的）。
fn sleepy_models_of(t: &Telemetry, flushed: &[Flush], session: &str) -> Vec<Value> {
    let st = t.0.state.lock();
    flushed
        .iter()
        .filter(|f| f.session_id == session)
        .flat_map(|f| f.events.iter())
        .chain(
            st.pending
                .get(&(7, session.to_string()))
                .into_iter()
                .flat_map(|p| p.events.iter().map(|(_, e)| e)),
        )
        .filter(|e| ev_name(e) == "tengu_sleepy_snowflake_applied")
        .map(|e| meta_of(e)["model"].clone())
        .collect()
}

/// 同一个 G3 会话，回包顺序反过来：标题 `00078` 先发、却比主线程 `00079` 晚返回。官方在发起前记首次应用，仍只有标题
/// 那条 `claude-haiku-5-5`——标题发出时登记过，主线程处理时让给它；哪怕主线程那批先刷出去、标题 45 秒后才回来也一样。
#[test]
fn sleepy_goes_to_the_earlier_title_even_when_it_returns_later() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(600);
    let Some(mut title) = gaps_call(&("00078", "end_turn", &[]), 7, base) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    title.total_ms = 45_000;
    let main =
        gaps_call(&("00079", "tool_use", &[("Read", 135)]), 7, base + Duration::from_secs(1));
    let session = parse_shape(&title.body).unwrap().session_id.unwrap();
    dispatch(&t, &title);
    t.ingest(main.unwrap());
    // 主线程那批先刷出去（事件 30 秒一批）。
    let later = Instant::now() + Duration::from_secs(config::TELEMETRY_EVENT_FLUSH_SECS + 1);
    let flushed = t.take_due(later);
    t.ingest(title);
    t.ingest(gaps_call(&("00081", "end_turn", &[]), 7, base + Duration::from_secs(50)).unwrap());
    assert_eq!(sleepy_models_of(&t, &flushed, &session), [json!("claude-haiku-5-5")]);
    assert!(t.0.state.lock().dispatched_titles.is_empty(), "标题处理完登记就去掉");
}

/// 生产时序：标题 0ms 入站、20ms（准备完）才发出，主线程 10ms 入站、30ms 发出，主线程先返回。登记用入站时刻（与
/// `ApiCall::started_at` 同源），不是发出那一刻：标题仍算先发，sleepy 归它；它处理完登记也正好清掉，不残留。
#[test]
fn sleepy_ordering_uses_the_inbound_start_not_the_send_time() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(600);
    let Some(title) = gaps_call(&("00078", "end_turn", &[]), 7, base) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    let main =
        gaps_call(&("00079", "tool_use", &[("Read", 135)]), 7, base + Duration::from_millis(10))
            .unwrap();
    let session = parse_shape(&title.body).unwrap().session_id.unwrap();
    // 20ms 时标题发出：登记的是它的入站时刻（0ms），不是 20ms。
    let mut h = axum::http::HeaderMap::new();
    h.insert("anthropic-beta", title.betas.as_deref().unwrap().parse().unwrap());
    t.note_dispatch(7, &h, &title.body, title.started_at);
    t.ingest(main);
    t.ingest(title);
    t.ingest(gaps_call(&("00081", "end_turn", &[]), 7, base + Duration::from_secs(10)).unwrap());
    assert_eq!(sleepy_models_of(&t, &[], &session), [json!("claude-haiku-5-5")]);
    assert!(t.0.state.lock().dispatched_titles.is_empty(), "登记与 started_at 同源，处理完即清");
}

/// 主线程首发 529（`t0`）、标题随后发出（`t1`）、主线程重发成功（`t5`）先处理：首次应用按逻辑起点属于 `t0` 的主线程，
/// 留主线程那条 `claude-haiku-5-5[1m]`，晚到的标题不报。
#[test]
fn sleepy_stays_with_a_main_request_that_started_before_the_title_and_retried() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(600);
    let main: GapStep = ("00079", "tool_use", &[("Read", 135)]);
    let Some(first) = gaps_call(&main, 7, base) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    let mut first = failed(first, Some(529), "overloaded_error", "Overloaded");
    first.total_ms = 8;
    let session = parse_shape(&first.body).unwrap().session_id.unwrap();
    t.ingest(first);
    let title = gaps_call(&("00078", "end_turn", &[]), 7, base + Duration::from_secs(1)).unwrap();
    dispatch(&t, &title);
    let mut retry = gaps_call(&main, 7, base + Duration::from_secs(5)).unwrap();
    retry.dispatch = Some("v2p".into());
    t.ingest(retry);
    t.ingest(title);
    t.ingest(gaps_call(&("00081", "end_turn", &[]), 7, base + Duration::from_secs(20)).unwrap());
    assert_eq!(sleepy_models_of(&t, &[], &session), [json!("claude-haiku-5-5[1m]")]);
}

/// 主线程那条正扣在重试链里（`00116` 头一次 529）时，扣住的标题（`00115`）跟着等，不到时限就放；重发成功后先处理
/// 主线程那条，标题随后补处理，报主线程那份会话级 beta。
#[test]
fn a_title_waits_for_a_main_request_held_in_retry() {
    let t = Telemetry::default();
    let base = frozen_now() - Duration::from_secs(600);
    let main: GapStep = ("00116", "tool_use", &[("Agent", 270)]);
    let Some(title) = gaps_call(&("00115", "end_turn", &[]), 7, base) else {
        eprintln!("skipped: cap/auto-2.1.308-20261022-gaps not present");
        return;
    };
    let session = parse_shape(&title.body).unwrap().session_id.unwrap();
    t.ingest(title);
    let mut first = failed(
        gaps_call(&main, 7, base + Duration::from_secs(1)).unwrap(),
        Some(529),
        "overloaded_error",
        "Overloaded",
    );
    first.total_ms = 8;
    t.ingest(first);
    let hold = Duration::from_secs(crate::config::TELEMETRY_SIDE_QUERY_HOLD_SECS + 1);
    t.gc(std::time::Instant::now() + hold);
    assert!(
        t.0.state.lock().presession.contains_key(&(7, session.clone())),
        "主线程还扣在重试里，标题接着等"
    );
    let mut retry = gaps_call(&main, 7, base + Duration::from_secs(2)).unwrap();
    retry.dispatch = Some("v2p".into());
    t.ingest(retry);
    let st = t.0.state.lock();
    assert!(!st.presession.contains_key(&(7, session.clone())));
    let got = top_model_betas(
        st.pending.get(&(7, session)).expect("queued"),
        "tengu_session_title_generated",
    );
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].1, GAPS_MAIN_BETAS);
}

/// 按会话主模型推出来的主线程那份会话级 beta（[`crate::proxy::cc_main_beta_at`] 再过 [`session_betas`]）与
/// `cap/auto-2.1.308-20261022-full` 各族真实主线程请求头过滤出来的逐字相同：opus `00469`、sonnet `00280`、
/// haiku-4.5 `00423`、haiku-5-5 `00380`、fable `00613`（auto 模式，多出的两项不进会话级）、opus\[1m] `00237`。
#[test]
fn derived_main_session_betas_match_the_2_1_308_captures() {
    for (f, model) in [
        ("00469", "claude-opus-5-5"),
        ("00280", "claude-sonnet-5-5"),
        ("00423", "claude-haiku-4-5"),
        ("00380", "claude-haiku-5-5"),
        ("00613", "claude-fable-5-1"),
        ("00237", "claude-opus-5-5[1m]"),
    ] {
        let Some((_, Some(official))) = cap_request(&format!("auto-2.1.308-20261022-full/{f}_"))
        else {
            eprintln!("skipped: cap/auto-2.1.308-20261022-full not present");
            return;
        };
        let derived = crate::proxy::cc_main_beta_at(model, Some((2, 1, 308)));
        assert_eq!(
            session_betas(&derived, "2.1.308"),
            session_betas(&official, "2.1.308"),
            "{model}（{f}）"
        );
    }
}

/// billing-only 模拟（[`ApiCall::bare`]）体里没有环境说明、附件与 `thread`：tether 三条、`tengu_attachments`、
/// `tengu_context_announcement`、`tengu_context_rendering_recorded` 一概不报，其余照旧。
/// 2026-10-24 billing-only 第三方实测：首轮报了六类附件与 session_context / date 宣告，两轮都报 tether。
#[test]
fn bare_simulation_reports_nothing_it_did_not_inject() {
    const SID: &str = "4dc73702-d904-4887-809d-17b93cc5357c";
    let run = |bare: bool| -> Vec<String> {
        let t = Telemetry::default();
        let base = frozen_now() - Duration::from_secs(120);
        for (i, last_text) in [true, false, true].into_iter().enumerate() {
            let mut c = call(cc_body(last_text), &format!("req_{i}"), "end_turn");
            c.bare = bare;
            c.ua_out = "claude-cli/2.1.309 (external, cli)".into();
            c.started_at = base + Duration::from_secs(20 * i as u64);
            t.ingest(c);
        }
        let st = t.0.state.lock();
        let p = st.pending.get(&(7, SID.to_string())).expect("queued");
        let mut names: Vec<String> = p.events.iter().map(|(_, e)| ev_name(e).to_string()).collect();
        names.extend(p.dd.iter().filter_map(|d| {
            let m = d["message"].as_str()?;
            Some(match d["feature_name"].as_str() {
                Some(f) => format!("{m}:{f}"),
                None => m.to_string(),
            })
        }));
        names
    };
    let full = run(false);
    let bare = run(true);
    let gone = [
        "tengu_tether_decision",
        "tengu_tether_echo_audit",
        "tengu_tether_live_outcome",
        "tengu_attachments",
        "tengu_context_announcement",
        "tengu_context_rendering_recorded",
    ];
    for n in gone {
        assert!(!bare.iter().any(|x| x == n), "bare 不报 {n}");
    }
    assert!(full.iter().any(|x| x == "tengu_tether_decision"), "完整模拟照报 tether");
    assert!(full.iter().any(|x| x == "tengu_attachments"), "完整模拟照报附件");
    // 其余照旧（整份比不了：模板里有按种子抽样的项，两次运行本来就不同）。
    for n in [
        "tengu_input_prompt",
        "tengu_api_query",
        "tengu_api_success",
        "tengu_turn_end",
        "tengu_reminder_fold_replayed",
        "tengu_wire_tool_input_echo_replayed",
        "tengu_started",
    ] {
        assert!(bare.iter().any(|x| x == n), "bare 照报 {n}");
    }
}

/// 真实 CC 透传（[`ApiCall::client_tools`]）只在出站头带 message-threads 时报 tether：经网关的客户端不带，官方那时
/// 一条都不报（`cap/auto-2.1.309-20261023-api`）。2026-10-24 billing-only 实测：真实 CC 不带这项也报了 tether。
#[test]
fn passthrough_tether_follows_the_thread_beta() {
    const SID: &str = "4dc73702-d904-4887-809d-17b93cc5357c";
    let tethers = |betas: &str| -> usize {
        let t = Telemetry::default();
        let mut c = call(cc_body(true), "req_0", "end_turn");
        c.client_tools = true;
        c.betas = Some(betas.into());
        c.ua_out = "claude-cli/2.1.309 (external, cli)".into();
        t.ingest(c);
        let st = t.0.state.lock();
        let p = st.pending.get(&(7, SID.to_string())).expect("queued");
        p.events.iter().filter(|(_, e)| ev_name(e).starts_with("tengu_tether_")).count()
            + p.dd
                .iter()
                .filter(|d| d["message"].as_str().is_some_and(|m| m.starts_with("tengu_tether_")))
                .count()
    };
    assert_eq!(tethers("claude-code-20250219,effort-2025-11-24"), 0, "网关形态不报");
    let with = format!(
        "claude-code-20250219,effort-2025-11-24,{}",
        crate::config::CC_BETA_MESSAGE_THREADS
    );
    assert!(tethers(&with) > 0, "带线程 beta 照报");
}
