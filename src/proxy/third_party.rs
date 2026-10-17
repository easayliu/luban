//! 已知**必定**被上游判成第三方应用的出站特征，命中就本地拒绝、不出门（开关
//! `reject_known_third_party`，完整模拟与 billing-only 都生效，见
//! [`crate::store::ForwardFlags::reject_known_third_party`]）。
//!
//! 判据全部来自线上流水与本地对拍（2026-10-16）：
//! - `system` 正文：OpenClaw 提示词「Assistant Output Directives」一节的两个原文片段
//!   `[[reply_to_current]]` 与 `stripped before rendering` **同时**出现即 400（各自单独、或放进
//!   用户消息都放行；去一个字符、改大小写都放行），haiku / sonnet 一致，7 发 7 中；
//! - `system` 正文：pi（badlogic/pi-mono 编码代理）提示词里的 `docs/custom-provider.md` 与
//!   `pi packages (docs/packages.md)` **同时**出现即 400（顺序无关、中间可夹其他文字；各自单独、
//!   少一个字符、`Pi` 大写、放进用户消息都放行），haiku / sonnet 一致，线上 10-16 四种变体 100% 复现；
//! - `system` 正文：OpenCode（1.19.3）环境段的三行 `Here is some useful information about the
//!   environment you are running in:`、`Workspace root folder:`、`Is directory a git repo:` **同时**出现
//!   即 400（顺序无关、行首空格与冒号后的取值无关、中间可夹 3KB 正文；少任一行、全小写、`some` 去掉
//!   成官方原句、`Workspace root folder` 换成 `Working directory`、放进用户消息都放行），2026-10-17
//!   本地 ddmin 38 发 + 10 发变体对拍，sonnet-5-5。这三行由 OpenCode 的环境段模板固定生成，每条主请求
//!   都带，与提示词变体（anthropic / gemini / beast 等）无关；它的标题生成请求没有环境段，上游也放行；
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
const SYSTEM_SIGNATURES: &[(&str, &[&str])] = &[
    ("openclaw", &["[[reply_to_current]]", "stripped before rendering"]),
    ("pi", &["docs/custom-provider.md", "pi packages (docs/packages.md)"]),
    (
        "opencode",
        &[
            "Here is some useful information about the environment you are running in:",
            "Workspace root folder:",
            "Is directory a git repo:",
        ],
    ),
];

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

    /// pi 那两个片段同时在 system 里才触发：顺序无关、可夹文字；拆开、改大小写、放进消息都不算。
    #[test]
    fn pi_pair_must_co_occur_in_system() {
        for hit in [
            "(docs/custom-provider.md pi packages (docs/packages.md)",
            "pi packages (docs/packages.md) docs/custom-provider.md",
            "See docs/custom-provider.md for providers. Also read pi packages (docs/packages.md) first.",
        ] {
            assert_eq!(known_third_party_signature(&body(hit.into(), &[])), Some("pi"), "{hit}");
        }
        for miss in [
            "docs/custom-provider.md",
            "pi packages (docs/packages.md)",
            "custom-provider.md pi packages (docs/packages.md)",
            "docs/custom-provider.md pi packages docs/packages.md",
            "docs/custom-provider.md Pi packages (docs/packages.md)",
        ] {
            assert_eq!(known_third_party_signature(&body(miss.into(), &[])), None, "{miss}");
        }
        let in_user = serde_json::json!({
            "system": "You are a concise assistant.",
            "messages": [{ "role": "user", "content": "docs/custom-provider.md pi packages (docs/packages.md)" }],
        });
        assert_eq!(known_third_party_signature(&in_user), None, "挪进用户消息就不算");
    }

    /// OpenCode 环境段三行同时在 system 里才触发：顺序、行首空格、取值无关；少一行、改成官方原句、
    /// 换行名、全小写、放进消息都不算。
    #[test]
    fn opencode_env_triplet_must_co_occur_in_system() {
        let a = "Here is some useful information about the environment you are running in:";
        let b = "  Workspace root folder: /";
        let c = "  Is directory a git repo: no";
        for hit in [
            format!("{a}\n{b}\n{c}"),
            format!("{c}\n{b}\n{a}"),
            format!("{a}\n{}\n{}", b.trim(), c.trim()),
            format!("{a}\n  Workspace root folder: /Users/me/proj\n  Is directory a git repo: yes"),
            format!("{a}\n{}\n{b}\n{c}", "x".repeat(3000)),
        ] {
            assert_eq!(
                known_third_party_signature(&body(hit.clone().into(), &[])),
                Some("opencode"),
                "{hit}"
            );
        }
        for miss in [
            format!("{a}\n{b}"),
            format!("{b}\n{c}"),
            format!("{a}\n{c}"),
            format!(
                "Here is useful information about the environment you are running in:\n{b}\n{c}"
            ),
            format!("{a}\n  Working directory: /tmp/x\n{c}"),
            format!("{a}\n{b}\n{c}").to_lowercase(),
            // 官方 Claude Code 自己的环境段（cap/2.1.260/00029 原文）：首句没有 `some`、没有
            // `Workspace root folder`，只命中第三句，不能误伤。
            "Here is useful information about the environment you are running in:\n<env>\n\
             Working directory: /tmp/p\nIs directory a git repo: No\nPlatform: darwin\n</env>"
                .to_string(),
        ] {
            assert_eq!(
                known_third_party_signature(&body(miss.clone().into(), &[])),
                None,
                "{miss}"
            );
        }
        let in_user = serde_json::json!({
            "system": "You are a helpful assistant.",
            "messages": [{ "role": "user", "content": format!("{a}\n{b}\n{c}") }],
        });
        assert_eq!(known_third_party_signature(&in_user), None, "挪进用户消息就不算");
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
