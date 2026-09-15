//! 请求/响应摘要与脱敏辅助：把出站报文压成不含用户正文的形态摘要，供第三方拒绝、
//! 裸 429 等日志路径复用；也被 thinking 块取证复用（`turn_label`/`block_label`）。

use axum::http::HeaderMap;

/// 摘要里每段文本最多保留的字符数。
pub(super) const DUMP_TEXT_HEAD: usize = 200;

/// 截断到 `n` 个字符，并在尾部标出被吃掉多少，避免「看着是全文其实是截断」。
/// 按 `char` 截而不是按字节切：请求体里有中文，按字节切会 panic。
pub(super) fn head(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    format!("{}…(+{})", s.chars().take(n).collect::<String>(), total - n)
}

/// 不能进日志的头。取值本身有鉴权效力，打出来等于把凭证写进日志文件。
/// 保留头名与位置（值换成 `<redacted>`），因为**头序**本身也是要看的东西。
pub(super) fn is_secret_header(name: &str) -> bool {
    matches!(name, "authorization" | "x-api-key" | "cookie" | "proxy-authorization")
}

/// 把出站 `HeaderMap` 格式化成一行，对鉴权头脱敏。
/// 多处日志共用（第三方 400、裸 429……），避免某条路径漏掉脱敏。
pub(super) fn redact_headers(headers: &HeaderMap) -> String {
    headers
        .iter()
        .map(|(k, v)| {
            let name = k.as_str();
            let value = if is_secret_header(name) {
                "<redacted>".to_string()
            } else {
                v.to_str()
                    .map(|s| head(s, DUMP_TEXT_HEAD))
                    .unwrap_or_else(|_| "<non-ascii>".to_string())
            };
            format!("{name}: {value}")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// 出站请求体的结构摘要，见 [`super::ban::log_third_party_rejection`] 的取舍说明。
/// 非对象（理论上到不了这儿）原样返回。
pub(super) fn request_digest(v: &serde_json::Value) -> serde_json::Value {
    let Some(obj) = v.as_object() else { return v.clone() };
    let mut out = serde_json::Map::new();
    for (k, val) in obj {
        let digest = match k.as_str() {
            "messages" => messages_digest(val),
            "system" => system_digest(val),
            "tools" => tools_digest(val),
            _ => val.clone(),
        };
        out.insert(k.clone(), digest);
    }
    serde_json::Value::Object(out)
}

/// `messages` 的摘要：只留「第几条、谁说的、里面是些什么块」，不留任何正文。
/// `tool_use` 例外——它的 `name` 正是第三方判定最可能盯的维度，必须打出来。
pub(super) fn messages_digest(v: &serde_json::Value) -> serde_json::Value {
    let Some(arr) = v.as_array() else { return v.clone() };
    let turns = arr.iter().map(turn_label).collect::<Vec<_>>();
    serde_json::json!({ "count": arr.len(), "turns": turns })
}

/// 单条消息在摘要里的写法：`角色:块,块,…`。见 [`block_label`]。
pub(super) fn turn_label(m: &serde_json::Value) -> String {
    let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
    let blocks = match m.get("content") {
        Some(serde_json::Value::Array(bs)) => {
            bs.iter().map(block_label).collect::<Vec<_>>().join(",")
        }
        Some(serde_json::Value::String(s)) => format!("text(len={})", s.len()),
        _ => "?".to_string(),
    };
    format!("{role}:{blocks}")
}

/// 单个内容块在摘要里的写法：`text` 只记长度，`tool_use` 记名字，其余只记类型。
pub(super) fn block_label(b: &serde_json::Value) -> String {
    let t = b.get("type").and_then(|t| t.as_str()).unwrap_or("?");
    match t {
        "tool_use" => {
            format!("tool_use({})", b.get("name").and_then(|n| n.as_str()).unwrap_or("?"))
        }
        "text" => format!(
            "text(len={})",
            b.get("text").and_then(|t| t.as_str()).map(str::len).unwrap_or(0)
        ),
        // 空 thinking 块有没有签名是「each thinking block must contain thinking」
        // 排障的核心判据，摘要里必须能看出来。
        "thinking" => format!(
            "thinking(len={},sig_len={})",
            b.get("thinking").and_then(|t| t.as_str()).map(str::len).unwrap_or(0),
            b.get("signature").and_then(|t| t.as_str()).map(str::len).unwrap_or(0)
        ),
        // 密文长度是「这一块有没有被动过」的唯一可打指标（`data` 本身几 KB，没有信息量）。
        "redacted_thinking" => format!(
            "redacted_thinking(data_len={})",
            b.get("data").and_then(|d| d.as_str()).map(str::len).unwrap_or(0)
        ),
        other => other.to_string(),
    }
}

/// `system` 的摘要：每块记长度、前 [`DUMP_TEXT_HEAD`] 字符与 `cache_control` 原样。
/// 块数与断点位置是形态对齐的核心判据（见 [`super::align_system_shape`]），必须能一眼数出来。
pub(super) fn system_digest(v: &serde_json::Value) -> serde_json::Value {
    let blocks: Vec<serde_json::Value> = match v {
        serde_json::Value::String(s) => {
            vec![serde_json::json!({ "len": s.len(), "head": head(s, DUMP_TEXT_HEAD) })]
        }
        serde_json::Value::Array(bs) => bs
            .iter()
            .map(|b| {
                let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                let mut o = serde_json::Map::new();
                o.insert("len".to_string(), serde_json::json!(text.len()));
                o.insert("head".to_string(), serde_json::json!(head(text, DUMP_TEXT_HEAD)));
                if let Some(cc) = b.get("cache_control") {
                    o.insert("cache_control".to_string(), cc.clone());
                }
                serde_json::Value::Object(o)
            })
            .collect(),
        other => return other.clone(),
    };
    serde_json::Value::Array(blocks)
}

/// `tools` 的摘要：`name`/`type` **原样**（第三方判定最可能盯的就是这两项），
/// `description` 截断，`input_schema` 只留顶层参数名。见过的键之外若还有别的，
/// 把键名列出来——摘要不该悄悄吃掉一个没见过的字段。
pub(super) fn tools_digest(v: &serde_json::Value) -> serde_json::Value {
    const KNOWN: &[&str] = &["name", "type", "description", "input_schema"];
    let Some(arr) = v.as_array() else { return v.clone() };
    let out = arr
        .iter()
        .map(|t| {
            let mut o = serde_json::Map::new();
            for key in ["name", "type"] {
                if let Some(x) = t.get(key) {
                    o.insert(key.to_string(), x.clone());
                }
            }
            if let Some(d) = t.get("description").and_then(|d| d.as_str()) {
                o.insert("desc".to_string(), serde_json::json!(head(d, 80)));
            }
            if let Some(props) =
                t.get("input_schema").and_then(|s| s.get("properties")).and_then(|p| p.as_object())
            {
                o.insert(
                    "schema_props".to_string(),
                    serde_json::json!(props.keys().collect::<Vec<_>>()),
                );
            }
            let extra: Vec<&String> = t
                .as_object()
                .map(|m| m.keys().filter(|k| !KNOWN.contains(&k.as_str())).collect())
                .unwrap_or_default();
            if !extra.is_empty() {
                o.insert("extra_keys".to_string(), serde_json::json!(extra));
            }
            serde_json::Value::Object(o)
        })
        .collect::<Vec<_>>();
    serde_json::Value::Array(out)
}

#[cfg(test)]
mod tests {
    use crate::proxy::digest::{block_label, head, is_secret_header};
    use crate::proxy::request_digest;
    /// 摘要要留住形态判据（工具名与类型、system 块数与断点、顶层 key 顺序），
    /// 同时不把用户对话正文带进日志。
    #[test]
    fn request_digest_keeps_shape_and_drops_user_text() {
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "system": [
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude.",
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            ],
            "tools": [
                {"name": "delegate_task", "description": "hand work to a subagent",
                 "input_schema": {"type": "object", "properties": {"prompt": {"type": "string"}}}},
                {"type": "web_search_20250305", "name": "web_search"},
            ],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "我的银行卡号是 1234"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "name": "delegate_task", "input": {"prompt": "secret"}},
                ]},
            ],
            "stream": true});
        let dumped = request_digest(&body).to_string();

        // 形态判据留住了。
        assert!(dumped.contains("delegate_task"), "工具名是要查的那个维度: {dumped}");
        assert!(dumped.contains("web_search_20250305"), "server tool 的 type 要能看见: {dumped}");
        assert!(dumped.contains("ephemeral"), "缓存断点要原样留着: {dumped}");
        assert!(dumped.contains("claude-opus-5") && dumped.contains("\"stream\":true"));
        assert!(dumped.contains("tool_use(delegate_task)"), "历史里的工具名同样要看: {dumped}");

        // 用户正文没进去。
        assert!(!dumped.contains("1234"), "用户对话正文不得进日志: {dumped}");
        assert!(!dumped.contains("secret"), "工具入参不得进日志: {dumped}");

        // 顶层 key 顺序原样（preserve_order），顺序本身也是判据。
        let parsed: serde_json::Value = serde_json::from_str(&dumped).unwrap();
        let keys: Vec<String> = parsed.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["model", "system", "tools", "messages", "stream"]);
    }

    /// 出站头摘要不得带出鉴权值——日志文件会被随手贴出来排查。
    #[test]
    fn header_dump_redacts_credentials() {
        assert!(is_secret_header("authorization"));
        assert!(is_secret_header("x-api-key"));
        assert!(!is_secret_header("anthropic-beta"));
    }

    /// 截断按字符不按字节：请求体里有中文，按字节切会 panic。
    #[test]
    fn head_truncates_by_char() {
        assert_eq!(head("abc", 10), "abc");
        assert_eq!(head("中文中文中", 2), "中文…(+3)");
    }

    #[test]
    fn block_label_shows_thinking_len_and_signature_len() {
        let b = serde_json::json!({"type": "thinking", "thinking": "abc", "signature": "xy"});
        assert_eq!(block_label(&b), "thinking(len=3,sig_len=2)");
        let b = serde_json::json!({"type": "thinking", "thinking": ""});
        assert_eq!(block_label(&b), "thinking(len=0,sig_len=0)");
    }
}
