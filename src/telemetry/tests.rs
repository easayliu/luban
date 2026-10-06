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
            text_chars: 87,
            reply_input_chars: 87,
            thinking_chars: 0,
            saw_thinking: false,
            tool_use_lens: Vec::new(),
            tool_calls: Vec::new(),
            cost_usd: Some(0.18),
            speed: None,
            failure: None,
            aborted: false,
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
        agent: AgentHeaders::default(),
        organization_id: None,
        started_at: frozen_now() - Duration::from_secs(3),
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
        agent: AgentHeaders::default(),
        organization_id: Some("09520b85-f6b6-432f-97e2-6ecb804a083f".into()),
        started_at: frozen_now() - Duration::from_secs(2),
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
        session_betas(header),
        "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07",
        "cap/2.1.258/00020 那条 opus 事件的 betas"
    );
    // haiku 主线程（`cap/2.1.285/00051` 的出站头）→ 同批事件里的会话级 betas（`00060`）。
    let haiku = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    assert_eq!(
        session_betas(haiku),
        "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05"
    );
    // opus-4-6（`cap/2.1.285/00083`）少 `mid-conversation-system`，会话级那份跟着少（`00090`）。
    let opus46 = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    assert_eq!(
        session_betas(opus46),
        "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05"
    );
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
    let t = Telemetry::default();
    let s = |c: &Value, k: &str| c[k].as_str().map(str::to_string);
    for c in &calls {
        let mut a = call(c["body"].as_str().unwrap().as_bytes().to_vec(), "x", "end_turn");
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
        a.text_chars = c["text_chars"].as_u64().unwrap_or(0) as usize;
        a.reply_input_chars = a.text_chars;
        a.thinking_chars = c["thinking_chars"].as_u64().unwrap_or(0) as usize;
        a.saw_thinking = c["saw_thinking"].as_bool().unwrap_or(false);
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
