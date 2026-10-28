//! 服务端开关 `tengu_snoopy_creek`：开着的号 2.1.307 起报 `tengu_safety_event`（工具在弹框上被拒、被配置拦下时
//! 那条安全事件，见 `crate::telemetry`）。
//!
//! 客户端读不到时的默认值随版本变：2.1.307 ~ 2.1.311 按**开**（`k("tengu_snoopy_creek", true)`），2.1.312 起按
//! **关**（`k("tengu_snoopy_creek", false)`），每次报之前现读。服务端下发了布尔值才按下发的算，按号从 eval 里记
//! （保活与会话启动握手拉 eval 时顺带记）；没见过 eval 的号、eval 里没有这一项、值不是布尔的都按那一版的默认。
//! 10-27 的 eval 里还没有这一项，10-28 起显式下发 `defaultValue: false`（`cap/auto-2.1.311-20261027-full` 与
//! `cap/auto-2.1.312-20261028-full` 的 eval 回包）。只存在内存里。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 开关名。
pub(super) const SNOOPY_CREEK_FEATURE: &str = "tengu_snoopy_creek";

/// 号 → 服务端下发的值；`None` 是见过 eval、但里面没有这一项（或不是布尔），按版本默认算。
static ENABLED: LazyLock<parking_lot::RwLock<HashMap<i64, Option<bool>>>> =
    LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的开关：一份 eval 是整份特性缓存，没有这一项就记成「按默认」。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let served =
        features.get(SNOOPY_CREEK_FEATURE).and_then(|f| f.get("value")).and_then(|v| v.as_bool());
    ENABLED.write().insert(cred_id, served);
}

/// 这个号上、`version` 这一版的客户端现在报不报 `tengu_safety_event`。
pub(crate) fn account_enabled(cred_id: i64, version: &str) -> bool {
    let served = ENABLED.read().get(&cred_id).copied().flatten();
    served.unwrap_or_else(|| crate::proxy::parse_version(version).is_none_or(|v| v < (2, 1, 312)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v["features"].as_object().unwrap().clone()
    }

    #[test]
    fn follows_the_accounts_eval_and_the_versions_default() {
        let id = -93_101;
        assert!(account_enabled(id, "2.1.311"), "没见过 eval：2.1.311 按开");
        assert!(!account_enabled(id, "2.1.312"), "没见过 eval：2.1.312 起按关");
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snoopy_creek": {"value": false}}})),
        );
        assert!(!account_enabled(id, "2.1.311"));
        assert!(!account_enabled(id, "2.1.312"));
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snoopy_creek": {"value": true}}})),
        );
        assert!(account_enabled(id, "2.1.311"));
        assert!(account_enabled(id, "2.1.312"), "下发了开就按开");
        note_eval(id, &features(serde_json::json!({"features": {}})));
        assert!(account_enabled(id, "2.1.311"), "eval 里没有这一项按那一版的默认：开");
        assert!(!account_enabled(id, "2.1.312"), "eval 里没有这一项按那一版的默认：关");
    }
}
