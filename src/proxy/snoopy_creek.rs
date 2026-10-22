//! 服务端开关 `tengu_snoopy_creek`：开着的号 2.1.307 起报 `tengu_safety_event`（工具在弹框上被拒、被配置拦下时
//! 那条安全事件，见 `crate::telemetry`）。
//!
//! 客户端读不到时按**开**（2.1.307 `$Mn()` = `k("tengu_snoopy_creek", true)`，每次报之前现读）。服务端下发了布尔值
//! 才按下发的算，按号从 eval 里记（保活与会话启动握手拉 eval 时顺带记）；没见过 eval 的号、eval 里没有这一项、
//! 值不是布尔的都按开。只存在内存里。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 开关名。
pub(super) const SNOOPY_CREEK_FEATURE: &str = "tengu_snoopy_creek";

static ENABLED: LazyLock<parking_lot::RwLock<HashMap<i64, bool>>> = LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的开关：一份 eval 是整份特性缓存，没有这一项即按默认（开）记。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let on = features
        .get(SNOOPY_CREEK_FEATURE)
        .and_then(|f| f.get("value"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    ENABLED.write().insert(cred_id, on);
}

/// 这个号现在报不报 `tengu_safety_event`。
pub(crate) fn account_enabled(cred_id: i64) -> bool {
    ENABLED.read().get(&cred_id).copied().unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v["features"].as_object().unwrap().clone()
    }

    #[test]
    fn defaults_on_and_follows_the_accounts_eval() {
        let id = -93_101;
        assert!(account_enabled(id), "没见过 eval 按开");
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snoopy_creek": {"value": false}}})),
        );
        assert!(!account_enabled(id));
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snoopy_creek": {"value": true}}})),
        );
        assert!(account_enabled(id));
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snoopy_creek": {"value": false}}})),
        );
        note_eval(id, &features(serde_json::json!({"features": {}})));
        assert!(account_enabled(id), "eval 里没有这一项按默认开");
    }
}
