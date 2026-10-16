//! 已知**必定**被上游判成第三方应用的出站特征，命中就本地拒绝、不出门（开关
//! `reject_known_third_party`，完整模拟与 billing-only 都生效，见
//! [`crate::store::ForwardFlags::reject_known_third_party`]）。
//!
//! 判据全部来自线上流水与本地对拍（2026-10-16）：
//! - `system` 正文：OpenClaw 提示词「Assistant Output Directives」一节的两个原文片段
//!   `[[reply_to_current]]` 与 `stripped before rendering` **同时**出现即 400（各自单独、或放进
//!   用户消息都放行；去一个字符、改大小写都放行），haiku / sonnet 一致，7 发 7 中；
//! - 工具名：以 `mcp_`（单下划线）开头的客户端工具名，单独一个就 400
//!   （[`crate::proxy::is_mcp_underscore_name`]）。
//!
//! 看的是**出站体**：挪位（`relocate_client_system`）把超长 system 搬进用户消息、工具名混淆把
//! `mcp_` 名改成 `mcp__luban__*` 之后不再命中——那两步已经把特征藏起来了，这里拦的是它们
//! 管不到的（不到 1900 字节的短提示词、混淆关着）。

/// 本地拒绝时回给客户端的文案：与上游 2026-10-16 的原话逐字相同（`usage_logs` 里 120 条一致），
/// 客户端那边看不出是谁拒的。
pub(crate) const THIRD_PARTY_REJECT_MESSAGE: &str = "Third-party apps now draw from extra usage, \
not plan limits. Ask your workspace admin to add more and keep going.";

/// `system` 正文里**同时**出现才触发的词组，每组一个标签（进日志与流水的 `rejected_locally:<标签>`）。
/// 匹配区分大小写、逐字：实测大写、少一个方括号都不触发。
const SYSTEM_SIGNATURES: &[(&str, &[&str])] =
    &[("openclaw", &["[[reply_to_current]]", "stripped before rendering"])];

/// 工具名命中 [`crate::proxy::is_mcp_underscore_name`] 时的标签。
const MCP_UNDERSCORE_TOOL: &str = "mcp_-tool";

/// 出站体是否带已知的第三方特征；带就返回标签。
pub(crate) fn known_third_party_signature(v: &serde_json::Value) -> Option<&'static str> {
    if v.get("tools")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .any(crate::proxy::is_mcp_underscore_name)
    {
        return Some(MCP_UNDERSCORE_TOOL);
    }
    let system: String = match v.get("system") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    SYSTEM_SIGNATURES
        .iter()
        .find(|(_, phrases)| phrases.iter().all(|p| system.contains(p)))
        .map(|(label, _)| *label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(system: serde_json::Value, tools: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "model": "claude-haiku-5-5",
            "system": system,
            "tools": tools.iter().map(|n| serde_json::json!({ "name": n })).collect::<Vec<_>>(),
            "messages": [{ "role": "user", "content": "hi" }],
        })
    }

    /// OpenClaw 那两个片段要**同时**在 system 里：拆开、放进消息、少个字符都不算。
    #[test]
    fn openclaw_pair_must_co_occur_in_system() {
        let pair = "[[reply_to_current]]\nstripped before rendering";
        assert_eq!(known_third_party_signature(&body(pair.into(), &["read"])), Some("openclaw"));
        let split = serde_json::json!([
            { "type": "text", "text": "- Native quote/reply: first token `[[reply_to_current]]`" },
            { "type": "text", "text": "Supported directives are stripped before rendering" },
        ]);
        assert_eq!(known_third_party_signature(&body(split, &[])), Some("openclaw"), "跨块也算");
        for miss in [
            "[[reply_to_current]]",
            "stripped before rendering",
            "[reply_to_current]\nstripped before rendering",
            "[[REPLY_TO_CURRENT]]\nstripped before rendering",
            "[[reply_to_current]]\nremoved before rendering",
        ] {
            assert_eq!(known_third_party_signature(&body(miss.into(), &["read"])), None, "{miss}");
        }
        let in_user = serde_json::json!({
            "system": "(see conversation)",
            "messages": [{ "role": "user", "content": pair }],
        });
        assert_eq!(known_third_party_signature(&in_user), None, "挪进用户消息就不算");
        assert_eq!(known_third_party_signature(&serde_json::json!({ "messages": [] })), None);
    }

    /// `mcp_` 单下划线工具名单独就触发；官方 `mcp__` 命名空间与混淆后的假名不触发。
    #[test]
    fn mcp_underscore_tool_name_alone_triggers() {
        assert_eq!(
            known_third_party_signature(&body("x".into(), &["read", "mcp_call_tool"])),
            Some(MCP_UNDERSCORE_TOOL)
        );
        assert_eq!(
            known_third_party_signature(&body(
                "x".into(),
                &["mcp__srv__x", "mcp__luban__mcp_call_tool", "mcpcall", "mcp_"]
            )),
            None
        );
    }
}
