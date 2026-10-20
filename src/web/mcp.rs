//! 排查用的 MCP 端点 `POST /mcp`：按请求 id 或会话查阅流水，供 Claude Code 等 MCP 客户端直接接入：
//! `claude mcp add --transport http luban <域名>/mcp --header "Authorization: Bearer lbm-…"`。
//!
//! **协议**：取 Streamable HTTP 的无状态子集。一个 JSON-RPC 请求对应一次 POST，以
//! `application/json` 作答；不签发 `Mcp-Session-Id`，不开 SSE（GET 回 405）；通知与客户端发来的
//! 响应回 202。只实现 `initialize` / `ping` / `tools/list` / `tools/call`，不收批量（数组）。
//!
//! **安全**：
//! - 只认 MCP Key（`lbm-`，见 [`auth::mcp_actor`]），会话 token 与上号 Key 一概不认；
//! - 全程只读：工具只查流水，不写库（Key 的最近使用时间除外）；
//! - 可见范围与请求查询页一致：admin 看全部；代理和用户只看本人名下的号，字段经 [`for_owner`]
//!   抹去下游身份与改写细节；
//! - 带 `Origin` 且与本站不同源的一律 403。这只是纵深防御：真正的门槛是 Bearer，浏览器里的
//!   跨站页面拿不到它；DNS rebinding 下 `Origin` 与 `Host` 同为攻击者域名，此项拦不住，也无须拦；
//! - 每次工具调用记一行 info（谁、哪个工具、什么参数），事后可追溯谁查过什么。
//!
//! **性能**：流水表是转发热路径持续写入的大表，此处不得拖累它。
//! - 只走有索引的筛选（请求 id、会话、号、模型、时间），一律 `LIMIT`，不做 `COUNT`/`SUM`；
//! - 同时执行的工具调用至多 [`MAX_CONCURRENT`] 个，排不上即报忙，连接池留给转发；
//! - 每把 Key 每分钟至多 [`RATE_LIMIT_PER_MIN`] 次；
//! - 请求体上限 [`MAX_BODY_BYTES`]；列表只回精简字段，大字段仅见于单条详情。

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;
use crate::store::UsageLog;

/// 请求体上限。一次工具调用不过寥寥几个参数，几 KB 已绰绰有余。
const MAX_BODY_BYTES: usize = 64 * 1024;

/// 同时执行的工具调用上限。每个调用占一条库连接，多了会挤占转发的连接池。
///
/// 名额一直持有到查询返回，**不另设调用时限**：超时丢弃 future 并不会叫停库里正在跑的语句，
/// 名额却已提前归还，库上的实际并发就会越过这个上限。查询都走索引且带 `LIMIT`，客户端自有超时。
const MAX_CONCURRENT: usize = 4;

/// 等待执行名额的时限，逾时回「忙」，不无限挂起。
const QUEUE_WAIT: Duration = Duration::from_secs(10);

/// 每把 Key 每分钟最多调几次（所有方法合计）。
const RATE_LIMIT_PER_MIN: u32 = 120;

/// 列表一次最多回几条、默认几条。
const LIST_MAX: i64 = 50;
const LIST_DEFAULT: i64 = 20;

/// 单条详情附带同一会话此前的几条：默认与上限。
const CONTEXT_DEFAULT: i64 = 5;
const CONTEXT_MAX: i64 = 20;

/// 同一请求 id 至多回几条（来访的 `X-Request-Id` 由客户端给出，不保证唯一）。
const SAME_ID_MAX: i64 = 5;

/// 列表中错误文案截取的字符数，全文见单条详情。
const LIST_ERROR_CHARS: usize = 300;

/// 支持的协议版本，新者在前。客户端所请在列即照回，否则回第一个。
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

static PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_CONCURRENT);

/// 每把 Key 当前一分钟窗口的起点与已用次数。条目数以 MCP Key 的把数为限，无须淘汰。
static RATE: LazyLock<parking_lot::Mutex<HashMap<i64, (Instant, u32)>>> =
    LazyLock::new(Default::default);

/// 记一次调用；本分钟额度已满则回 false。
fn take_rate(key_id: i64) -> bool {
    let mut map = RATE.lock();
    let now = Instant::now();
    let slot = map.entry(key_id).or_insert((now, 0));
    if now.duration_since(slot.0) >= Duration::from_secs(60) {
        *slot = (now, 0);
    }
    if slot.1 >= RATE_LIMIT_PER_MIN {
        return false;
    }
    slot.1 += 1;
    true
}

/// 未带 `Origin` 或与本站同源才放行。比对 `Host` 与 `X-Forwarded-Host`（应对反代改写 Host）。
/// MCP 客户端本不带 `Origin`，带了的多半出自浏览器。
fn same_origin(headers: &header::HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else { return true };
    let Some(authority) = origin.to_str().ok().and_then(|o| o.split_once("://")).map(|(_, a)| a)
    else {
        return false;
    };
    [header::HOST.as_str(), "x-forwarded-host"].iter().any(|h| {
        headers.get(*h).and_then(|v| v.to_str().ok()).is_some_and(|v| {
            v.split(',').next().unwrap_or("").trim().eq_ignore_ascii_case(authority)
        })
    })
}

/// `/mcp` 的入口。
pub(super) async fn handle(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    if req.method() != axum::http::Method::POST {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response();
    }
    if !same_origin(req.headers()) {
        return (StatusCode::FORBIDDEN, "cross-origin requests are not allowed").into_response();
    }
    let (actor, key_id) = match auth::mcp_actor(&state, req.headers()).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if !take_rate(key_id) {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded, retry in a minute")
            .into_response();
    }
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
    };
    let msg: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return rpc_error(Value::Null, -32700, &format!("parse error: {e}")),
    };
    let Some(obj) = msg.as_object() else {
        return rpc_error(Value::Null, -32600, "expected a single JSON-RPC object");
    };
    // 通知（无 id）与客户端发来的响应（无 method）均无须作答。
    let (Some(method), Some(id)) = (obj.get("method").and_then(Value::as_str), obj.get("id"))
    else {
        return StatusCode::ACCEPTED.into_response();
    };
    let id = id.clone();
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => rpc_result(id, initialize(&params)),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tool_specs() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            if !TOOLS.contains(&name.as_str()) {
                return rpc_error(id, -32602, &format!("unknown tool: {name}"));
            }
            tracing::info!(
                key_id,
                user = %actor.username,
                tool = %name,
                args = %truncate_chars(&args.to_string(), 300),
                "mcp tool call"
            );
            rpc_result(id, call_tool(&state, &actor, &name, &args).await)
        }
        _ => rpc_error(id, -32601, &format!("method not found: {method}")),
    }
}

fn rpc_result(id: Value, result: Value) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        .into_response()
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
    let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "luban", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Read-only access to luban's request log, which keeps the last 30 days. \
            Request ids look like `req_…`; luban returns them in the `X-Oneapi-Request-Id` header \
            and in error bodies, and New API records them as `upstream_request_id`. \
            Start with get_request; use search_requests to browse a conversation, an account or a model.",
    })
}

const TOOLS: &[&str] = &["get_request", "search_requests"];

fn tool_specs() -> Value {
    let read_only = json!({ "readOnlyHint": true, "openWorldHint": false });
    json!([
        {
            "name": "get_request",
            "title": "Get request",
            "description": "Fetch a request from luban's log by its luban request id (`req_…`). \
                Returns the full record — status, account, model, latency, tokens, cost, rate-limit \
                headers, upstream error, simulation and rewrite details, inbound headers and body \
                shape — together with the preceding requests of the same conversation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_id": { "type": "string", "description": "luban request id, e.g. req_Q3k9ZpL2mNv7Xb1c" },
                    "context": {
                        "type": "integer", "minimum": 0, "maximum": CONTEXT_MAX,
                        "description": format!("Number of preceding requests from the same conversation to include (default {CONTEXT_DEFAULT})")
                    }
                },
                "required": ["request_id"],
                "additionalProperties": false
            },
            "annotations": read_only,
        },
        {
            "name": "search_requests",
            "title": "Search requests",
            "description": "List requests from luban's log, newest first, as compact rows; call \
                get_request for a full record. Every filter is optional and they combine with AND. \
                To page, pass the returned `next_before_id` as `before_id`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Conversation uuid, matching either the client's own or the one sent upstream" },
                    "session_key": { "type": "string", "description": "luban simulated-session key (`lb:v2:…`)" },
                    "cred_id": { "type": "integer", "description": "Account id" },
                    "model": { "type": "string", "description": "Exact model name" },
                    "hours": { "type": "integer", "minimum": 1, "maximum": store::USAGE_LOG_RETENTION_SECS / 3600, "description": "Restrict to the last N hours" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": LIST_MAX, "description": format!("Maximum rows to return (default {LIST_DEFAULT})") },
                    "before_id": { "type": "integer", "description": "Return only rows with a smaller id (for paging)" }
                },
                "additionalProperties": false
            },
            "annotations": read_only,
        }
    ])
}

/// 执行一个工具，回 `tools/call` 的 result。参数有误、查无此条、繁忙都作为工具错误（`isError`）
/// 返回，让模型看清缘由、自行调整，而非报协议错误。
async fn call_tool(state: &AppState, actor: &Actor, name: &str, args: &Value) -> Value {
    let outcome = match tokio::time::timeout(QUEUE_WAIT, PERMITS.acquire()).await {
        Ok(Ok(_permit)) => match name {
            "get_request" => get_request(state, actor, args).await,
            _ => search_requests(state, actor, args).await,
        },
        _ => Err("luban is busy with other lookups; please retry shortly".into()),
    };
    match outcome {
        Ok(v) => json!({ "content": [{ "type": "text", "text": v.to_string() }] }),
        Err(msg) => json!({ "content": [{ "type": "text", "text": msg }], "isError": true }),
    }
}

type ToolResult = Result<Value, String>;

/// 库错误只进日志，回给客户端的是一句泛称：SQL 与库内细节不该经由此接口外泄。
fn query_failed(e: anyhow::Error) -> String {
    tracing::error!(error = %format!("{e:#}"), "mcp lookup query failed");
    "the lookup failed on the server; see luban's log".into()
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.len() > 256 {
                return Err(format!("`{key}` is too long"));
            }
            Ok((!s.is_empty()).then_some(s))
        }
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn arg_int(args: &Value, key: &str, min: i64, max: i64) -> Result<Option<i64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_i64() {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(format!("`{key}` must be an integer between {min} and {max}")),
        },
    }
}

/// 代理和用户只看本人名下的号，与请求查询页同一口径（见 `usage::list_usage`）。
fn owner_of(actor: &Actor) -> Option<i64> {
    actor.scope().owner()
}

async fn get_request(state: &AppState, actor: &Actor, args: &Value) -> ToolResult {
    let request_id = arg_str(args, "request_id")?.ok_or("`request_id` is required")?;
    if !proxy::acceptable_request_id(request_id) {
        return Err("`request_id` is not a valid request id".into());
    }
    let context = arg_int(args, "context", 0, CONTEXT_MAX)?.unwrap_or(CONTEXT_DEFAULT);
    let owner = owner_of(actor);
    let hits = state
        .store
        .query_usage_logs(store::UsageLogQuery {
            request_id: Some(request_id.to_owned()),
            owner_id: owner,
            limit: SAME_ID_MAX,
            ..Default::default()
        })
        .await
        .map_err(query_failed)?;
    let Some(first) = hits.first() else {
        return Err(format!(
            "no request with id {request_id} (the log keeps 30 days{})",
            if owner.is_some() { "; only your own accounts are visible" } else { "" }
        ));
    };
    // 同一会话此前的几条：依次取来访会话、模拟会话、出站会话中第一个有值的。
    //
    // 来访会话在前：标题、分类等辅助请求与主请求同属一个来访会话，模拟会话（session_key）却
    // 各不相同，先按 session_key 查会漏掉同一对话里的主请求。
    let f = &first.forensics;
    let mut earlier = Vec::new();
    if context > 0
        && (f.session_key.is_some() || f.session_id_in.is_some() || f.session_id.is_some())
    {
        let (session_key, session_id) = match (&f.session_id_in, &f.session_key) {
            (Some(sid), _) => (None, Some(sid.clone())),
            (None, Some(k)) => (Some(k.clone()), None),
            (None, None) => (None, f.session_id.clone()),
        };
        earlier = state
            .store
            .query_usage_logs(store::UsageLogQuery {
                session_key,
                session_id,
                until_id: Some(first.id - 1),
                owner_id: owner,
                limit: context,
                ..Default::default()
            })
            .await
            .map_err(query_failed)?;
    }
    let view = |log: UsageLog| if owner.is_some() { for_owner(log) } else { log };
    Ok(json!({
        "requests": hits.into_iter().map(|l| detail(view(l))).collect::<Vec<_>>(),
        "earlier_in_session": earlier.into_iter().map(|l| compact(&view(l))).collect::<Vec<_>>(),
    }))
}

async fn search_requests(state: &AppState, actor: &Actor, args: &Value) -> ToolResult {
    let max_hours = store::USAGE_LOG_RETENTION_SECS / 3600;
    let limit = arg_int(args, "limit", 1, LIST_MAX)?.unwrap_or(LIST_DEFAULT);
    let hours = arg_int(args, "hours", 1, max_hours)?;
    let before_id = arg_int(args, "before_id", 1, i64::MAX)?;
    let owner = owner_of(actor);
    let logs = state
        .store
        .query_usage_logs(store::UsageLogQuery {
            cred_id: arg_int(args, "cred_id", 1, i64::MAX)?,
            session_id: arg_str(args, "session_id")?.map(str::to_owned),
            session_key: arg_str(args, "session_key")?.map(str::to_owned),
            model: arg_str(args, "model")?.map(str::to_owned),
            since: hours.map(|h| chrono::Utc::now().timestamp() - h * 3600),
            until_id: before_id.map(|id| id - 1),
            owner_id: owner,
            limit,
            ..Default::default()
        })
        .await
        .map_err(query_failed)?;
    let next = (logs.len() as i64 == limit).then(|| logs.last().map(|l| l.id)).flatten();
    let rows: Vec<Value> = logs
        .into_iter()
        .map(|l| compact(&if owner.is_some() { for_owner(l) } else { l }))
        .collect();
    Ok(json!({ "requests": rows, "next_before_id": next }))
}

/// 本地时区的 RFC 3339 时刻。带着偏移，身处何地读来都不致误解。
fn local_time(ts: i64) -> Option<String> {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(ts, 0).single().map(|t| t.to_rfc3339())
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// 剔除值为 null 的字段，免得每条都拖着一串 `"x": null`。
fn prune(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, prune(v))).collect(),
        ),
        other => other,
    }
}

/// 列表中的一行：只留辨认请求、判断结果所需的字段；请求体结构、来访头、限流原文等大字段不带。
fn compact(l: &UsageLog) -> Value {
    let f = &l.forensics;
    prune(json!({
        "id": l.id,
        "time": local_time(l.ts),
        "request_id": l.request_id,
        "upstream_request_id": l.upstream_request_id,
        "cred_id": l.cred_id,
        "cred_label": l.cred_label,
        "model": l.model,
        "status": l.status,
        "ttft_ms": l.ttft_ms,
        "total_ms": l.total_ms,
        "input_tokens": l.input_tokens,
        "output_tokens": l.output_tokens,
        "cache_write_tokens": l.cache_creation_tokens,
        "cache_read_tokens": l.cache_read_tokens,
        "cost_usd": l.cost_usd,
        "simulated": f.simulated.then_some(true),
        "sim_reason": f.sim_reason,
        "third_party": f.third_party.then_some(true),
        "rewrites": f.rewrites,
        "error_type": f.error_type,
        "error_message": f.error_message.as_deref().map(|m| truncate_chars(m, LIST_ERROR_CHARS)),
        "session_id_in": f.session_id_in,
        "session_id": f.session_id,
    }))
}

/// 单条详情：整条流水。以 JSON 文本存储的几个字段展开为对象，省得模型再隔一层转义去读。
fn detail(l: UsageLog) -> Value {
    let time = local_time(l.ts);
    let mut v = serde_json::to_value(&l).unwrap_or(Value::Null);
    if let Some(m) = v.as_object_mut() {
        m.insert("time".into(), json!(time));
        // 取证字段在 `UsageLog` 中是 flatten 的，与其余字段同处一层。
        for k in ["ratelimit_raw", "shape", "inbound_shape", "inbound_headers", "response_excerpt"]
        {
            expand_json(m, k);
        }
    }
    prune(v)
}

fn expand_json(m: &mut serde_json::Map<String, Value>, key: &str) {
    if let Some(Value::String(s)) = m.get(key)
        && let Ok(parsed) = serde_json::from_str::<Value>(s)
        && (parsed.is_object() || parsed.is_array())
    {
        m.insert(key.into(), parsed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_must_match_the_host() {
        let mut h = header::HeaderMap::new();
        h.insert(header::HOST, "luban.example.com".parse().unwrap());
        assert!(same_origin(&h), "没带 Origin 放行");
        h.insert(header::ORIGIN, "https://luban.example.com".parse().unwrap());
        assert!(same_origin(&h));
        h.insert(header::ORIGIN, "https://evil.example.com".parse().unwrap());
        assert!(!same_origin(&h));
        h.insert("x-forwarded-host", "evil.example.com".parse().unwrap());
        assert!(same_origin(&h), "反代改过 Host 时认 X-Forwarded-Host");
        h.insert(header::ORIGIN, "null".parse().unwrap());
        assert!(!same_origin(&h));
    }

    #[test]
    fn rate_limit_is_per_key_and_per_minute() {
        let key = -42;
        for _ in 0..RATE_LIMIT_PER_MIN {
            assert!(take_rate(key));
        }
        assert!(!take_rate(key));
        assert!(take_rate(key - 1), "别的 Key 不受影响");
    }

    #[test]
    fn prune_drops_nulls_and_truncate_keeps_char_boundaries() {
        assert_eq!(prune(json!({"a": null, "b": {"c": null, "d": 1}})), json!({"b": {"d": 1}}));
        assert_eq!(truncate_chars("你好世界", 2), "你好…");
        assert_eq!(truncate_chars("ab", 2), "ab");
    }
}
