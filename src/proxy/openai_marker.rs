/// 请求体里一处 OpenAI 格式转换残留，见 [`find_openai_marker`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OpenAiMarker {
    /// JSON 路径，如 `messages.0.content.1`、`tool_choice`、`n`。
    pub(super) location: String,
    /// 残留类别（进日志按类聚合）：`image_url` / `system_role` / `foreign_role` /
    /// `message_field` / `tool_call_id` / `tool_choice` / `tool_schema` / `top_level_field` /
    /// `content_type`。
    pub(super) kind: &'static str,
    /// 给客户端那句话的主体（不含路径前缀）。
    reason: String,
}

impl OpenAiMarker {
    fn new(location: String, kind: &'static str, reason: impl Into<String>) -> Self {
        Self { location, kind, reason: reason.into() }
    }

    /// 回给客户端的 `error.message`：路径 + 原因 + 一句总括，让它知道该修客户端而不是换号重试。
    pub(super) fn message(&self) -> String {
        format!(
            "{}: {}. This request looks like it was converted from the OpenAI Chat Completions \
             format; only native Anthropic Messages API requests are accepted here",
            self.location, self.reason
        )
    }
}

/// OpenAI 专属的顶层字段。Anthropic Messages API 一个都不收（`Extra inputs are not permitted`），
/// 出现即说明请求是 OpenAI 格式转过来的、转换器没把它们删干净。
///
/// 只列**确定属于 OpenAI 方言**的：`service_tier`、`stop_sequences`、`metadata`、`top_k`
/// 这些两边都有或 Anthropic 独有的一律不算。
const OPENAI_TOP_LEVEL_FIELDS: &[&str] = &[
    "n",
    "stop",
    "presence_penalty",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "top_logprobs",
    "seed",
    "response_format",
    "stream_options",
    "functions",
    "function_call",
    "max_completion_tokens",
    "parallel_tool_calls",
    "user",
    "reasoning_effort",
    "modalities",
    "audio",
    "prediction",
    "store",
    "web_search_options",
];

/// OpenAI 的内容块 `type`（Chat Completions 的 `image_url`，Responses API 的 `input_*` /
/// `output_text` / `function_call*` / `refusal`）。Anthropic 只认 `text` / `image` / `document` /
/// `tool_use` / `tool_result` / `thinking` / `redacted_thinking` 等。
const OPENAI_CONTENT_TYPES: &[&str] = &[
    "image_url",
    "input_text",
    "input_image",
    "input_file",
    "input_audio",
    "output_text",
    "function_call",
    "function_call_output",
    "refusal",
];

/// 在请求体里找 OpenAI 格式转换残留，找到第一处就返回。
///
/// 到 luban 手里的请求 body 已经是 `/v1/messages` 的形态，转换器（litellm、one-api、
/// claude-code-router 之类）不会在头上声明「我是转过来的」，能认的只有它留下的痕迹：
///
/// | 类别 | 判据 | Anthropic 的对应 |
/// |---|---|---|
/// | `image_url` | 内容块 `type:"image_url"`（含 tool_result 内嵌） | `{"type":"image","source":{…}}` |
/// | `content_type` | 内容块 type 在 [`OPENAI_CONTENT_TYPES`] | `text` / `image` / `document`… |
/// | `system_role` | `messages[i].role == "system"` | 顶层 `system` 字段 |
/// | `foreign_role` | `role` 是 `tool` / `function` / `developer` | `user` 轮里的 `tool_result` 块 / 顶层 `system` |
/// | `message_field` | 消息上有 `name` / `tool_calls` / `tool_call_id` / `function_call` | `tool_use` / `tool_result` 内容块 |
/// | `tool_call_id` | `tool_use.id` / `tool_result.tool_use_id` 以 `call_` 开头 | 上游签发的 `toolu_…` |
/// | `tool_choice` | 字串形态，或 `{"type":"function",…}` | `{"type":"auto"|"any"|"tool"|"none"}` |
/// | `tool_schema` | `tools[i]` 带 `function` / `parameters`，或 `type:"function"` | `{"name","description","input_schema"}` |
/// | `top_level_field` | 顶层键在 [`OPENAI_TOP_LEVEL_FIELDS`] | 各有各的（`stop`→`stop_sequences`…） |
///
/// `image_url` 一项**无条件**查（上游恒 400，本地拒省一次往返，是加开关之前就有的行为）；
/// 其余全部由 `strict` 拨——对应网页上的 `reject_openai_shape` 开关。
///
/// 两处放宽：
/// - `cc_shaped`（`system` 里有 CC 身份声明）的请求**不查** `system_role`：CC 自己在
///   messages 里合法使用 `role:"system"`（deferred tools），见 [`hoist_system_role_messages`]
///   调用处的同一取舍。
/// - `tool_call_id` 只认 `call_` 前缀。上游对 id 的要求只是 `^[a-zA-Z0-9_-]+$`，`call_x` 本身
///   能过，所以这一条不是「省一次 400」，是纯粹的转换指纹：Anthropic 侧签发的 id 恒为
///   `toolu_` 开头，`call_` 只可能来自 OpenAI 的 `tool_calls[].id` 被原样回填。
pub(super) fn find_openai_marker(
    body: Option<&serde_json::Value>,
    cc_shaped: bool,
    strict: bool,
) -> Option<OpenAiMarker> {
    let v = body?;
    let obj = v.as_object()?;

    // 1) 内容块 type：`image_url` 无条件；其余 OpenAI type 仅 strict。
    let content_type_marker = |ty: &str, loc: String| -> Option<OpenAiMarker> {
        if ty == "image_url" {
            return Some(OpenAiMarker::new(
                loc,
                "image_url",
                "content type 'image_url' is not supported by the Anthropic API; use \
                 {\"type\":\"image\",\"source\":{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"...\"}} \
                 or {\"type\":\"image\",\"source\":{\"type\":\"url\",\"url\":\"...\"}}",
            ));
        }
        (strict && OPENAI_CONTENT_TYPES.contains(&ty)).then(|| {
            OpenAiMarker::new(
                loc,
                "content_type",
                format!("content type '{ty}' is an OpenAI content type, not an Anthropic one"),
            )
        })
    };

    if let Some(msgs) = obj.get("messages").and_then(|m| m.as_array()) {
        for (mi, msg) in msgs.iter().enumerate() {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or_default();
            if strict {
                if role == "system" && !cc_shaped {
                    return Some(OpenAiMarker::new(
                        format!("messages.{mi}.role"),
                        "system_role",
                        "role 'system' is not a valid message role in the Anthropic API; put \
                         system instructions in the top-level 'system' field",
                    ));
                }
                if matches!(role, "tool" | "function" | "developer") {
                    return Some(OpenAiMarker::new(
                        format!("messages.{mi}.role"),
                        "foreign_role",
                        format!(
                            "role '{role}' is an OpenAI message role; the Anthropic API only has \
                             'user' and 'assistant' (tool results go in a 'tool_result' block of a \
                             'user' turn)"
                        ),
                    ));
                }
                for key in ["name", "tool_calls", "tool_call_id", "function_call"] {
                    if msg.get(key).is_some() {
                        return Some(OpenAiMarker::new(
                            format!("messages.{mi}.{key}"),
                            "message_field",
                            format!(
                                "'{key}' is an OpenAI message field; Anthropic messages carry only \
                                 'role' and 'content' (tool calls are 'tool_use' / 'tool_result' \
                                 content blocks)"
                            ),
                        ));
                    }
                }
            }
            let Some(content) = msg.get("content").and_then(|c| c.as_array()) else { continue };
            for (ci, block) in content.iter().enumerate() {
                let ty = block.get("type").and_then(|t| t.as_str()).unwrap_or_default();
                let loc = format!("messages.{mi}.content.{ci}");
                if let Some(m) = content_type_marker(ty, loc.clone()) {
                    return Some(m);
                }
                if strict {
                    let id_key = match ty {
                        "tool_use" => Some("id"),
                        "tool_result" => Some("tool_use_id"),
                        _ => None,
                    };
                    if let Some(k) = id_key
                        && block
                            .get(k)
                            .and_then(|i| i.as_str())
                            .is_some_and(|i| i.starts_with("call_"))
                    {
                        return Some(OpenAiMarker::new(
                            format!("{loc}.{k}"),
                            "tool_call_id",
                            "tool call id starts with 'call_', the OpenAI tool_calls id form; \
                             Anthropic tool_use ids are issued by the API as 'toolu_...'",
                        ));
                    }
                }
                // tool_result 内嵌的 content 数组也要扫。
                if ty == "tool_result"
                    && let Some(inner) = block.get("content").and_then(|c| c.as_array())
                {
                    for (ki, inner_block) in inner.iter().enumerate() {
                        let ity =
                            inner_block.get("type").and_then(|t| t.as_str()).unwrap_or_default();
                        if let Some(m) = content_type_marker(ity, format!("{loc}.content.{ki}")) {
                            return Some(m);
                        }
                    }
                }
            }
        }
    }

    if !strict {
        return None;
    }

    // 2) tool_choice 的 OpenAI 方言。
    match obj.get("tool_choice") {
        Some(serde_json::Value::String(s)) => {
            return Some(OpenAiMarker::new(
                "tool_choice".into(),
                "tool_choice",
                format!(
                    "tool_choice is the string \"{s}\", the OpenAI form; the Anthropic API takes \
                     an object such as {{\"type\":\"auto\"}} / {{\"type\":\"any\"}} / \
                     {{\"type\":\"tool\",\"name\":\"...\"}}"
                ),
            ));
        }
        Some(tc) if tc.get("type").and_then(|t| t.as_str()) == Some("function") => {
            return Some(OpenAiMarker::new(
                "tool_choice.type".into(),
                "tool_choice",
                "tool_choice type 'function' is the OpenAI form; the Anthropic API uses \
                 {\"type\":\"tool\",\"name\":\"...\"}",
            ));
        }
        _ => {}
    }

    // 3) tools 条目的 OpenAI schema。
    if let Some(tools) = obj.get("tools").and_then(|t| t.as_array()) {
        for (ti, tool) in tools.iter().enumerate() {
            if tool.get("function").is_some() || tool.get("parameters").is_some() {
                let key = if tool.get("function").is_some() { "function" } else { "parameters" };
                return Some(OpenAiMarker::new(
                    format!("tools.{ti}.{key}"),
                    "tool_schema",
                    "tool definition is in the OpenAI function-calling shape; the Anthropic API \
                     takes {\"name\":\"...\",\"description\":\"...\",\"input_schema\":{...}}",
                ));
            }
            if tool.get("type").and_then(|t| t.as_str()) == Some("function") {
                return Some(OpenAiMarker::new(
                    format!("tools.{ti}.type"),
                    "tool_schema",
                    "tool type 'function' is the OpenAI form; Anthropic custom tools have no \
                     'type' (or 'custom')",
                ));
            }
        }
    }

    // 4) OpenAI 专属顶层字段。
    if let Some(key) = OPENAI_TOP_LEVEL_FIELDS.iter().find(|k| obj.contains_key(**k)) {
        let hint = match *key {
            "stop" => "; use 'stop_sequences'",
            "max_completion_tokens" => "; use 'max_tokens'",
            "user" => "; the Anthropic API carries the caller identity in 'metadata.user_id'",
            "reasoning_effort" => "; use 'output_config.effort'",
            "functions" | "function_call" => "; use 'tools' / 'tool_choice'",
            _ => "",
        };
        return Some(OpenAiMarker::new(
            (*key).to_string(),
            "top_level_field",
            format!(
                "'{key}' is an OpenAI Chat Completions parameter, not accepted by the Anthropic API{hint}"
            ),
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use crate::proxy::{config, find_openai_marker};
    /// 剥字段走的是 [`crate::proxy::rewrite_body`] 这条统一路径，且开关关掉即原样透传。
    #[test]
    fn openai_marker_image_url_is_unconditional() {
        let body = serde_json::json!({
            "model": "claude-sonnet-5", "max_tokens": 10,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi"},
                {"type": "image_url", "image_url": {"url": "https://x/y.png"}}
            ]}]
        });
        for strict in [false, true] {
            let m = find_openai_marker(Some(&body), false, strict).expect("image_url rejected");
            assert_eq!(m.kind, "image_url");
            assert_eq!(m.location, "messages.0.content.1");
        }
        // tool_result 内嵌的也查。
        let nested = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                    {"type": "image_url", "image_url": {"url": "data:..."}}
                ]}
            ]}]
        });
        let m = find_openai_marker(Some(&nested), false, false).unwrap();
        assert_eq!(m.location, "messages.0.content.0.content.0");
    }

    #[test]
    fn openai_marker_native_anthropic_request_passes() {
        // 一条地道的 Anthropic 请求：非 CC 形态、非 CC UA 都不算残留——这正是模拟路径要接管的。
        let body = serde_json::json!({
            "model": "claude-sonnet-5", "max_tokens": 1024,
            "system": "You are helpful.",
            "metadata": {"user_id": "abc"},
            "stop_sequences": ["END"], "top_k": 5, "service_tier": "auto",
            "tool_choice": {"type": "auto"},
            "tools": [{"name": "get_weather", "description": "d", "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01", "content": "sunny"}
                ]}
            ]
        });
        assert_eq!(find_openai_marker(Some(&body), false, true), None);
        assert_eq!(find_openai_marker(None, false, true), None);
    }

    #[test]
    fn openai_marker_strict_only_residue() {
        let cases: Vec<(serde_json::Value, &str, &str)> = vec![
            (
                serde_json::json!({"messages": [{"role": "system", "content": "be nice"}]}),
                "system_role",
                "messages.0.role",
            ),
            (
                serde_json::json!({"messages": [{"role": "tool", "content": "x", "tool_call_id": "call_1"}]}),
                "foreign_role",
                "messages.0.role",
            ),
            (
                serde_json::json!({"messages": [{"role": "user", "content": "x", "name": "bob"}]}),
                "message_field",
                "messages.0.name",
            ),
            (
                serde_json::json!({"messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_abc", "name": "f", "input": {}}
                ]}]}),
                "tool_call_id",
                "messages.0.content.0.id",
            ),
            (
                serde_json::json!({"messages": [{"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_abc", "content": "ok"}
                ]}]}),
                "tool_call_id",
                "messages.0.content.0.tool_use_id",
            ),
            (
                serde_json::json!({"messages": [{"role": "user", "content": [
                    {"type": "input_text", "text": "hi"}
                ]}]}),
                "content_type",
                "messages.0.content.0",
            ),
            (
                serde_json::json!({"messages": [], "tool_choice": "auto"}),
                "tool_choice",
                "tool_choice",
            ),
            (
                serde_json::json!({"messages": [], "tool_choice": {"type": "function", "function": {"name": "f"}}}),
                "tool_choice",
                "tool_choice.type",
            ),
            (
                serde_json::json!({"messages": [], "tools": [
                    {"type": "function", "function": {"name": "f", "parameters": {}}}
                ]}),
                "tool_schema",
                "tools.0.function",
            ),
            (serde_json::json!({"messages": [], "n": 1}), "top_level_field", "n"),
            (serde_json::json!({"messages": [], "stop": ["x"]}), "top_level_field", "stop"),
            (serde_json::json!({"messages": [], "user": "u-1"}), "top_level_field", "user"),
        ];
        for (body, kind, loc) in cases {
            // strict 关：一律放行（退回修补路径）。
            assert_eq!(
                find_openai_marker(Some(&body), false, false),
                None,
                "{kind} must pass when not strict"
            );
            let m = find_openai_marker(Some(&body), false, true)
                .unwrap_or_else(|| panic!("{kind} not caught"));
            assert_eq!(m.kind, kind);
            assert_eq!(m.location, loc);
            // 客户端读到的那句话带路径与总括。
            let msg = m.message();
            assert!(msg.starts_with(&format!("{loc}: ")), "{msg}");
            assert!(msg.contains("OpenAI Chat Completions"), "{msg}");
        }
    }

    #[test]
    fn openai_marker_cc_shaped_may_use_system_role() {
        // CC 自己在 messages 里合法使用 role:"system"（deferred tools）——CC 形态不查这一条。
        let body = serde_json::json!({
            "system": [{"type": "text", "text": format!("{}xyz", config::CC_SYSTEM_IDENTITY_PREFIX)}],
            "messages": [{"role": "system", "content": "deferred"}, {"role": "user", "content": "hi"}]
        });
        assert_eq!(find_openai_marker(Some(&body), true, true), None);
        // 但其它残留照查：CC 形态挡不住 `call_` id。
        let body = serde_json::json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "f", "input": {}}
            ]}]
        });
        assert_eq!(find_openai_marker(Some(&body), true, true).unwrap().kind, "tool_call_id");
    }
}
