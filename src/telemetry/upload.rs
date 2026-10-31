//! 上报

use super::*;

/// 一次 HTTP 上报的结果：`Some(status)`；网络层失败为 `None`。
pub async fn post_event_logging(
    client: &wreq::Client,
    access_token: &str,
    version: &str,
    events: &[Value],
) -> Option<u16> {
    event_logging_request(client, config::UPSTREAM_BASE_URL, access_token, version, events)
        .send()
        .await
        .ok()
        .map(|r| r.status().as_u16())
}

/// [`post_event_logging`] 发往 `base` 的那条，形态跟会话版本：2.1.314 起走 Bun 的长连接会话（`Connection: keep-alive`、
/// Messages API 那份 `Accept-Encoding`，头序 [`config::EVENT_LOGGING_FETCH_ORDER`]），更早的是 axios 那套。
pub(crate) fn event_logging_request(
    client: &wreq::Client,
    base: &str,
    access_token: &str,
    version: &str,
    events: &[Value],
) -> wreq::RequestBuilder {
    let url = format!("{base}{}", config::KEEPALIVE_EVENT_LOGGING);
    let fetch = crate::proxy::parse_version(version)
        .is_some_and(|v| v >= config::EVENT_LOGGING_FETCH_SINCE);
    let req = client
        .post(&url)
        .header("Accept", config::AXIOS_ACCEPT)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", format!("claude-code/{version}"))
        .header("anthropic-beta", config::OAUTH_BETA_HEADER)
        .header("x-service-name", "claude-code")
        .json(&json!({ "events": events }));
    if !fetch {
        return crate::oauth::axios(req, "event_logging");
    }
    let req = req
        .header("Connection", "keep-alive")
        .header("Accept-Encoding", config::CC_ACCEPT_ENCODING)
        .orig_headers(crate::oauth::orig_headers(config::EVENT_LOGGING_FETCH_ORDER))
        .timeout(crate::oauth::SIDE_REQUEST_TIMEOUT);
    crate::oauth::side_dump(&req, "event_logging");
    req
}

/// Datadog 日志摄入。真实客户端用 axios 直发，不带 Authorization。
pub async fn post_datadog(client: &wreq::Client, entries: &[Value]) -> Option<u16> {
    crate::oauth::axios(
        client
            .post(config::DATADOG_INTAKE_URL)
            .header("Accept", config::AXIOS_ACCEPT)
            .header("Content-Type", "application/json")
            .header("DD-API-KEY", config::DATADOG_API_KEY)
            .header("User-Agent", config::DATADOG_USER_AGENT)
            .json(&entries),
        "datadog",
    )
    .send()
    .await
    .ok()
    .map(|r| r.status().as_u16())
}

/// OTel 指标。
pub async fn post_metrics(
    client: &wreq::Client,
    access_token: &str,
    version: &str,
    body: &Value,
) -> Option<u16> {
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, config::KEEPALIVE_METRICS);
    crate::oauth::axios(
        client
            .post(&url)
            .header("Accept", config::AXIOS_ACCEPT)
            .header("Content-Type", "application/json")
            .header("User-Agent", format!("claude-code/{version}"))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("anthropic-beta", config::OAUTH_BETA_HEADER)
            .json(body),
        "metrics",
    )
    .send()
    .await
    .ok()
    .map(|r| r.status().as_u16())
}

/// 把一批调用聚合成 OTel 指标请求体（形态取自 `cap/2.1.258/00030`）。
///
/// 官方还带 `user.email` 与 `user.account_id`——凭证里没有这两项，缺省。
pub(super) fn metrics_body(
    calls: &[CallMetric],
    version: &str,
    subscription_type: &str,
    org: Option<&str>,
) -> Value {
    let ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let attrs = |c: &CallMetric| {
        let mut a = Map::new();
        a.insert("user.id".into(), json!(c.device_id));
        a.insert("session.id".into(), json!(c.session_id));
        if let Some(org) = org {
            a.insert("organization.id".into(), json!(org));
        }
        a.insert("user.account_uuid".into(), json!(c.account_uuid));
        a.insert("terminal.type".into(), json!("vscode"));
        a
    };
    let point =
        |a: Map<String, Value>, v: Value| json!({ "attributes": a, "value": v, "timestamp": &ts });

    // session.count：每个新会话一条。
    let mut sessions: Vec<Value> = Vec::new();
    for c in calls.iter().filter(|c| c.new_session) {
        let mut a = attrs(c);
        a.insert(
            "start_type".into(),
            json!(if c.continued {
                "continue"
            } else if c.resumed {
                "resume"
            } else {
                "fresh"
            }),
        );
        sessions.push(point(a, json!(1)));
    }
    // cost / token：按 (session, model, category, effort) 聚合。
    let mut cost: Vec<(&CallMetric, f64)> = Vec::new();
    let mut tokens: Vec<(&CallMetric, [i64; 4])> = Vec::new();
    let mut active: Vec<(&CallMetric, (f64, f64))> = Vec::new();
    let same = |a: &CallMetric, b: &CallMetric| {
        a.session_id == b.session_id
            && a.model == b.model
            && a.category == b.category
            && a.effort == b.effort
            && a.agent_name == b.agent_name
    };
    for c in calls {
        if c.usage {
            match cost.iter_mut().find(|(k, _)| same(k, c)) {
                Some((_, v)) => *v += c.cost,
                None => cost.push((c, c.cost)),
            }
            match tokens.iter_mut().find(|(k, _)| same(k, c)) {
                Some((_, v)) => {
                    v[0] += c.input;
                    v[1] += c.output;
                    v[2] += c.cache_read;
                    v[3] += c.cache_creation;
                }
                None => tokens.push((c, [c.input, c.output, c.cache_read, c.cache_creation])),
            }
        }
        // 只有辅助调用（离开回顾、标题这类）的那一批不报活跃时长（`cap/2.1.285/00104`、
        // `00161`，`cap/2.1.280/00089`）：用户没在用、客户端也没在跑一轮。
        if c.category == "auxiliary" {
            continue;
        }
        match active.iter_mut().find(|(k, _)| k.session_id == c.session_id) {
            Some((_, v)) => {
                v.0 += c.user_secs;
                v.1 += c.cli_secs;
            }
            None => active.push((c, (c.user_secs, c.cli_secs))),
        }
    }
    let with_model = |c: &CallMetric| {
        let mut a = attrs(c);
        a.insert("model".into(), json!(c.model));
        a.insert("query_source".into(), json!(c.category));
        if let Some(e) = &c.effort {
            a.insert("effort".into(), json!(e));
        }
        if let Some(n) = &c.agent_name {
            a.insert("agent.name".into(), json!(n));
        }
        a
    };
    let cost_points: Vec<Value> =
        cost.iter().map(|(c, v)| point(with_model(c), js_number(*v))).collect();
    let mut token_points: Vec<Value> = Vec::new();
    for (c, v) in &tokens {
        for (i, ty) in ["input", "output", "cacheRead", "cacheCreation"].iter().enumerate() {
            let mut a = with_model(c);
            a.insert("type".into(), json!(ty));
            token_points.push(point(a, json!(v[i])));
        }
    }
    let mut active_points: Vec<Value> = Vec::new();
    for (c, (user, cli)) in &active {
        for (ty, v) in [("user", *user), ("cli", *cli)] {
            let mut a = attrs(c);
            a.insert("type".into(), json!(ty));
            active_points.push(point(a, js_number((v * 100.0).round() / 100.0)));
        }
    }
    // 改动的行数：按（会话, 模型）各一对 `added` / `removed`，删了 0 行也报那个点（`cap/auto-2.1.305-20261021-full`
    // 五批全是成对的）。编辑类工具的权限决定：按（会话, 决定, 来源, 工具, 语言）计数。
    let mut loc: Vec<(&CallMetric, usize, usize)> = Vec::new();
    let mut decisions: Vec<(&CallMetric, &EditDecision, u64)> = Vec::new();
    for c in calls {
        if !c.loc.is_empty() {
            let (a, d) = c.loc.iter().fold((0, 0), |(x, y), (a, d)| (x + a, y + d));
            match loc
                .iter_mut()
                .find(|(k, _, _)| k.session_id == c.session_id && k.loc_model == c.loc_model)
            {
                Some((_, x, y)) => {
                    *x += a;
                    *y += d;
                }
                None => loc.push((c, a, d)),
            }
        }
        for e in &c.decisions {
            match decisions.iter_mut().find(|(k, x, _)| k.session_id == c.session_id && *x == e) {
                Some((_, _, n)) => *n += 1,
                None => decisions.push((c, e, 1)),
            }
        }
    }
    let mut loc_points: Vec<Value> = Vec::new();
    for (c, added, removed) in &loc {
        for (ty, v) in [("added", *added), ("removed", *removed)] {
            let mut a = attrs(c);
            a.insert("type".into(), json!(ty));
            a.insert("model".into(), json!(c.loc_model));
            loc_points.push(point(a, json!(v)));
        }
    }
    let decision_points: Vec<Value> = decisions
        .iter()
        .map(|(c, e, n)| {
            let mut a = attrs(c);
            a.insert("decision".into(), json!(e.decision));
            a.insert("source".into(), json!(e.source));
            a.insert("tool_name".into(), json!(e.tool));
            if let Some(l) = &e.language {
                a.insert("language".into(), json!(l));
            }
            point(a, json!(n))
        })
        .collect();
    // 数组里的先后照官方：session → lines_of_code → cost → token → code_edit_tool → active_time。
    let mut metrics = Vec::new();
    if !sessions.is_empty() {
        metrics.push(json!({
            "name": "claude_code.session.count",
            "description": "Count of CLI sessions started",
            "unit": "",
            "data_points": sessions
        }));
    }
    if !loc_points.is_empty() {
        metrics.push(json!({
            "name": "claude_code.lines_of_code.count",
            "description": "Count of lines of code modified, with the 'type' attribute indicating whether lines were added or removed and the 'model' attribute indicating which model made the change",
            "unit": "",
            "data_points": loc_points
        }));
    }
    // 一批里全是失败请求时这两项没有任何数据点——官方那种批次里它们压根不出现，
    // 别发一个空数组。
    if !cost_points.is_empty() {
        metrics.push(json!({
            "name": "claude_code.cost.usage",
            "description": "Cost of the Claude Code session",
            "unit": "USD",
            "data_points": cost_points
        }));
    }
    if !token_points.is_empty() {
        metrics.push(json!({
            "name": "claude_code.token.usage",
            "description": "Number of tokens used",
            "unit": "tokens",
            "data_points": token_points
        }));
    }
    if !decision_points.is_empty() {
        metrics.push(json!({
            "name": "claude_code.code_edit_tool.decision",
            "description": "Count of code editing tool permission decisions (accept/reject) for Edit, Write, and NotebookEdit tools",
            "unit": "",
            "data_points": decision_points
        }));
    }
    if !active_points.is_empty() {
        metrics.push(json!({
            "name": "claude_code.active_time.total",
            "description": "Total active time in seconds",
            "unit": "s",
            "data_points": active_points
        }));
    }
    json!({
        "resource_attributes": {
            "service.name": "claude-code",
            "service.version": version,
            "os.type": "darwin",
            "os.version": crate::config::CC_OS_RELEASE,
            "host.arch": "arm64",
            "aggregation.temporality": "delta",
            "user.customer_type": "claude_ai",
            "user.subscription_type": subscription_type
        },
        "metrics": metrics
    })
}

/// 定时把攒下的遥测发出去。每 5 秒看一眼到期的；发送用该凭证自己的出站客户端（配了代理
/// 走代理）与新鲜的 access_token。发失败只记日志——遥测丢一批不影响任何转发。
pub async fn run_flusher(
    t: Telemetry,
    store: Arc<crate::store::CredentialStore>,
    clients: Arc<crate::clients::ClientPool>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let now = Instant::now();
        t.gc(now);
        let due = t.take_due(now);
        // `api_telemetry` 关掉之后，队列里攒着的那批也不发：关掉就是不发，不等它排空。
        // 队列里只有逐请求的 API 遥测（保活那一半不走这里）。
        if !store.forward_flags().api_telemetry {
            continue;
        }
        // 各批之间并发发（有上限），一批之内照旧按序：一个号的代理慢，不该让别的号的那批
        // 跟着排队，也不该拖住下一 tick 的 `gc`。单条请求有时限（`oauth::axios`），这一轮
        // 总会在有限时间内结束。
        futures_util::StreamExt::for_each_concurrent(
            futures_util::stream::iter(due),
            FLUSH_CONCURRENCY,
            |f| flush_one(&store, &clients, f),
        )
        .await;
    }
}

/// [`run_flusher`] 一轮里同时在发的批数上限。
const FLUSH_CONCURRENCY: usize = 8;

/// 取该批所属凭证的客户端与新鲜 token，发出去；号不可用或拿不到 token 就丢掉这一批。
async fn flush_one(
    store: &crate::store::CredentialStore,
    clients: &Arc<crate::clients::ClientPool>,
    f: Flush,
) {
    let Ok(Some(cred)) = store.get(f.cred_id).await else { return };
    // 订阅未生效暂停中的号同样不发：上游对它每一发都是 403。
    if cred.is_banned() || cred.is_subscription_paused() {
        return;
    }
    let Ok(client) = clients.for_credential(&cred) else { return };
    let token = match crate::store::ensure_fresh_token(store, clients, &cred).await {
        Ok(crate::store::TokenAttempt::Ready(t)) => t,
        _ => {
            tracing::debug!(cred_id = cred.id, "telemetry: no fresh token, dropping this batch");
            return;
        }
    };
    send_flush(&client, &token, &cred, f).await;
}

/// 发一张凭证的这一批。
pub async fn send_flush(
    client: &wreq::Client,
    token: &str,
    cred: &crate::credentials::Credential,
    f: Flush,
) {
    // 会话 id 只展示前 8 位，与转发日志里 `device=` 的脱敏口径一致。
    let session: String = f.session_id.chars().take(8).collect();
    for chunk in f.events.chunks(config::TELEMETRY_BATCH_MAX) {
        let st = post_event_logging(client, token, &f.version, chunk).await;
        report(cred, &session, "event_logging", chunk.len(), st);
    }
    for chunk in f.dd.chunks(config::TELEMETRY_BATCH_MAX) {
        let st = post_datadog(client, chunk).await;
        report(cred, &session, "datadog", chunk.len(), st);
    }
    if let Some(body) = &f.metrics {
        let st = post_metrics(client, token, &f.version, body).await;
        report(cred, &session, "metrics", 1, st);
    }
}

pub(super) fn report(
    cred: &crate::credentials::Credential,
    session: &str,
    what: &str,
    n: usize,
    status: Option<u16>,
) {
    match status {
        Some(s) if s < 400 => {
            tracing::debug!(cred_id = cred.id, cred = %cred.label, session, what, n, status = s, "telemetry sent")
        }
        Some(s) => {
            tracing::warn!(cred_id = cred.id, cred = %cred.label, session, what, n, status = s, "telemetry rejected upstream")
        }
        None => {
            tracing::warn!(cred_id = cred.id, cred = %cred.label, session, what, n, "telemetry request failed")
        }
    }
}
