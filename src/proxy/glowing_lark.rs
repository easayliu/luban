//! 服务端开关 `tengu_glowing_lark`：启动握手那两条 `GET /v1/mcp_servers` 的 `anthropic-mcp-client-capabilities` 里
//! 带不带 `extensions: {"io.modelcontextprotocol/tasks": {}}`。
//!
//! 官方 `wgn()`：MCP SDK 走 v2（`wM()`：没设 `MCP_SDK_GENERATION` 时看 `tengu_brindle_causeway`，读不到按开，下发
//! `false` 才退回 v1）且 `tengu_glowing_lark` 开着（读不到按关）才加这一项。10-22 的 eval 里 `tengu_glowing_lark` 还是
//! 默认的关（`cap/auto-2.1.307-20261022-full`、`cap/auto-2.1.308-20261022-full`，那两版的头里没有），10-23 起
//! `force: true`（`cap/auto-2.1.309-20261023-full` 起，`cap/auto-2.1.312-20261028-full` 25 条都带）——是服务端开关，
//! 不是版本改动。能确认有这段代码的最早是 2.1.309，更早的会话照旧不带。两个开关按号从 eval 里记，只存在内存里。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 号 → （`tengu_glowing_lark`，`tengu_brindle_causeway`）。
static FLAGS: LazyLock<parking_lot::RwLock<HashMap<i64, (bool, bool)>>> =
    LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的两个开关：一份 eval 是整份特性缓存，没有的按默认。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let served = |k: &str| features.get(k).and_then(|f| f.get("value")).and_then(|v| v.as_bool());
    let glowing = served("tengu_glowing_lark") == Some(true);
    let sdk_v2 = served("tengu_brindle_causeway") != Some(false);
    FLAGS.write().insert(cred_id, (glowing, sdk_v2));
}

/// 这个号上、`version` 这一版的官方客户端在 MCP 客户端声明里带不带 tasks 扩展。没见过 eval 的号按两个开关的默认：不带。
pub(crate) fn tasks_extension(cred_id: i64, version: Option<(u64, u64, u64)>) -> bool {
    version.is_none_or(|v| v >= (2, 1, 309))
        && FLAGS.read().get(&cred_id).is_some_and(|&(glowing, sdk_v2)| glowing && sdk_v2)
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
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_glowing_lark": {"value": true}}})),
        );
        assert!(tasks_extension(id, Some((2, 1, 312))));
        assert!(tasks_extension(id, Some((2, 1, 309))));
        assert!(tasks_extension(id, None), "读不出版本按最新那版");
        assert!(!tasks_extension(id, Some((2, 1, 308))), "2.1.309 之前不带");
        note_eval(
            id,
            &features(serde_json::json!({"features": {
                "tengu_glowing_lark": {"value": true}, "tengu_brindle_causeway": {"value": false}
            }})),
        );
        assert!(!tasks_extension(id, Some((2, 1, 312))), "SDK 退回 v1 就不带");
        note_eval(id, &features(serde_json::json!({"features": {}})));
        assert!(!tasks_extension(id, Some((2, 1, 312))), "eval 里没有 glowing_lark 按关");
    }
}
