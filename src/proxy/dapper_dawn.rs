//! 服务端开关 `tengu_dapper_dawn`：开着的号，2.1.301 起的交互式会话与 `-p` 报
//! `tengu_cache_heartbeat_shadow`（会话活动心跳，见 `crate::telemetry` 的 `heartbeat`）。
//!
//! 值是布尔，客户端读不到时按 `false`。同一个号 10-19 拉到的是 `defaultValue: false`，10-21 是
//! `source: force` 的 `true`（`cap/auto-2.1.303-20261019-full/00012` 与 `cap/auto-2.1.305-20261021-full/00006`
//! 的 eval），服务端按规则下发、本地算不出来，只能按号从 eval 里记（保活与会话启动握手拉 eval 时顺带记）。
//! 没见过 eval 的号按关算。只存在内存里。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 开关名。
pub(super) const DAPPER_DAWN_FEATURE: &str = "tengu_dapper_dawn";

static ENABLED: LazyLock<parking_lot::RwLock<HashMap<i64, bool>>> = LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的开关；eval 里没有这一项按关记。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let on =
        features.get(DAPPER_DAWN_FEATURE).and_then(|f| f.get("value")).and_then(|v| v.as_bool())
            == Some(true);
    ENABLED.write().insert(cred_id, on);
}

/// 这个号的心跳开没开。遥测在会话首条请求时读一次、整个会话钉住。
pub(crate) fn account_enabled(cred_id: i64) -> bool {
    ENABLED.read().get(&cred_id).copied().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v["features"].as_object().unwrap().clone()
    }

    #[test]
    fn follows_the_accounts_eval() {
        let id = -93_001;
        assert!(!account_enabled(id), "没见过 eval 按关");
        note_eval(
            id,
            &features(
                serde_json::json!({"features": {"tengu_dapper_dawn": {"value": true, "source": "force"}}}),
            ),
        );
        assert!(account_enabled(id));
        note_eval(
            id,
            &features(
                serde_json::json!({"features": {"tengu_dapper_dawn": {"value": false, "source": "defaultValue"}}}),
            ),
        );
        assert!(!account_enabled(id));
        note_eval(id, &features(serde_json::json!({"features": {}})));
        assert!(!account_enabled(id), "eval 里没有这一项按关");
    }
}
