//! 服务端开关 `tengu_glowing_lark`：启动握手那两条 `GET /v1/mcp_servers` 的 `anthropic-mcp-client-capabilities` 里
//! 带不带 `extensions: {"io.modelcontextprotocol/tasks": {}}`。
//!
//! 官方 `wgn()`：MCP SDK 走 v2（`wM()`：没设 `MCP_SDK_GENERATION` 时看 `tengu_brindle_causeway`，读不到按开，下发
//! `false` 才退回 v1）且 `tengu_glowing_lark` 开着（读不到按关）才加这一项。10-22 的 eval 里 `tengu_glowing_lark` 还是
//! 默认的关（`cap/auto-2.1.307-20261022-full`、`cap/auto-2.1.308-20261022-full`，那两版的头里没有），10-23 起
//! `force: true`（`cap/auto-2.1.309-20261023-full` 起，`cap/auto-2.1.312-20261028-full` 25 条都带）——是服务端开关，
//! 不是版本改动。能确认有这段代码的最早是 2.1.309，更早的会话照旧不带。两个开关按号从 eval 里记，只存在内存里。
//!
//! 2.1.317 起客户端默认值变了（`ZT()`）：没设 `CLAUDE_CODE_DISABLE_MCP_TASKS` 时读
//! `k("tengu_glowing_lark", entrypoint === "cli" && !CLAUDE_CODE_REMOTE && !diskless)`——交互式终端会话读不到也按开。
//! 握手一律按交互式（bootstrap 带 `entrypoint=cli`）发，所以 ≥2.1.317 没下发这一项时按开，下发了照下发。
//!
//! 2.1.323 删掉了 v1 的 MCP 客户端（`MCP_SDK_GENERATION=v1` 也只记一笔 `v1Compat`，照样走 v2），也就不再读
//! `tengu_brindle_causeway`：只看 `tengu_glowing_lark`。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 一个号的两个开关：`tengu_glowing_lark` 下发的值（没下发是 `None`）、`tengu_brindle_causeway`。
type Flags = (Option<bool>, bool);

/// 号 → [`Flags`]。
static FLAGS: LazyLock<parking_lot::RwLock<HashMap<i64, Flags>>> = LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的两个开关：一份 eval 是整份特性缓存，没有的按默认。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let served = |k: &str| features.get(k).and_then(|f| f.get("value")).and_then(|v| v.as_bool());
    let glowing = served("tengu_glowing_lark");
    let sdk_v2 = served("tengu_brindle_causeway") != Some(false);
    FLAGS.write().insert(cred_id, (glowing, sdk_v2));
}

/// 这个号上、`version` 这一版的官方客户端在 MCP 客户端声明里带不带 tasks 扩展。没见过 eval 的号、或 eval 里没有
/// `tengu_glowing_lark` 的按客户端默认：2.1.317 之前关，之后开（`tengu_brindle_causeway` 缺省按开）。
pub(crate) fn tasks_extension(cred_id: i64, version: Option<(u64, u64, u64)>) -> bool {
    if version.is_some_and(|v| v < (2, 1, 309)) {
        return false;
    }
    let default_on = version.is_none_or(|v| v >= (2, 1, 317));
    let (glowing, sdk_v2) = FLAGS.read().get(&cred_id).copied().unwrap_or((None, true));
    // 2.1.323 起 SDK 恒为 v2，`tengu_brindle_causeway` 不再读。
    let sdk_v2 = sdk_v2 || version.is_none_or(|v| v >= (2, 1, 323));
    glowing.unwrap_or(default_on) && sdk_v2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v["features"].as_object().unwrap().clone()
    }

    #[test]
    fn follows_both_flags_from_2_1_309() {
        let id = -93_318;
        assert!(!tasks_extension(id, Some((2, 1, 312))), "没见过 eval 按关");
        assert!(tasks_extension(id, Some((2, 1, 317))), "2.1.317 起客户端默认按开");
        assert!(tasks_extension(id, None), "读不出版本按最新那版");
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_glowing_lark": {"value": true}}})),
        );
        assert!(tasks_extension(id, Some((2, 1, 312))));
        assert!(tasks_extension(id, Some((2, 1, 309))));
        assert!(!tasks_extension(id, Some((2, 1, 308))), "2.1.309 之前不带");
        note_eval(
            id,
            &features(serde_json::json!({"features": {
                "tengu_glowing_lark": {"value": true}, "tengu_brindle_causeway": {"value": false}
            }})),
        );
        assert!(!tasks_extension(id, Some((2, 1, 312))), "SDK 退回 v1 就不带");
        assert!(!tasks_extension(id, Some((2, 1, 317))), "2.1.317 也一样");
        assert!(tasks_extension(id, Some((2, 1, 323))), "2.1.323 删了 v1，不再看 brindle_causeway");
        note_eval(id, &features(serde_json::json!({"features": {}})));
        assert!(
            !tasks_extension(id, Some((2, 1, 312))),
            "eval 里没有 glowing_lark：2.1.316 及以前按关"
        );
        assert!(tasks_extension(id, Some((2, 1, 317))), "2.1.317 起按开");
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_glowing_lark": {"value": false}}})),
        );
        assert!(!tasks_extension(id, Some((2, 1, 317))), "显式下发 false 照关");
    }
}
