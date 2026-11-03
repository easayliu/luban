//! 服务端开关 `tengu_vivid_peach`：2.1.317 起 Bash 执行 / 失败事件末尾那组重复命令统计按会话抽样的比例
//! （`crate::telemetry` 里 `bash_repeat::metered`）。
//!
//! 官方 `JYo(sessionId)`：读 `k("tengu_vivid_peach", undefined)`，读不到按 0.1，读到的是 `[0, 1]` 里的数就用它，别的
//! 一律当 0（一个都不抽）。`cap/auto-2.1.317-20261103-full` 的 eval 里没有这一项。按号从 eval 里记（保活与会话启动握手拉
//! eval 时顺带记），没见过 eval 的号、或 eval 里没有这一项按 0.1。只存在内存里。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 开关名。
const FEATURE: &str = "tengu_vivid_peach";

/// 读不到时的比例（官方 `n = 0.1`）。
const DEFAULT_RATE: f64 = 0.1;

/// 号 → eval 里下发的比例（已按官方规则归好：不在 `[0, 1]` 或不是数是 0）；eval 里没有这一项不记。
static RATE: LazyLock<parking_lot::RwLock<HashMap<i64, f64>>> = LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的比例；eval 里没有这一项就清掉，退回默认。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    match features.get(FEATURE).and_then(|f| f.get("value")) {
        Some(v) => {
            let rate = v.as_f64().filter(|r| (0.0..=1.0).contains(r)).unwrap_or(0.0);
            RATE.write().insert(cred_id, rate);
        }
        None => {
            RATE.write().remove(&cred_id);
        }
    }
}

/// 这个号的抽样比例。
pub(crate) fn rate(cred_id: i64) -> f64 {
    RATE.read().get(&cred_id).copied().unwrap_or(DEFAULT_RATE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v["features"].as_object().unwrap().clone()
    }

    #[test]
    fn follows_the_accounts_eval_with_the_official_fallbacks() {
        let id = -93_717;
        assert_eq!(rate(id), 0.1, "没见过 eval 按 0.1");
        note_eval(id, &features(serde_json::json!({"features": {FEATURE: {"value": 0.5}}})));
        assert_eq!(rate(id), 0.5);
        note_eval(id, &features(serde_json::json!({"features": {FEATURE: {"value": 1}}})));
        assert_eq!(rate(id), 1.0);
        // 超出范围、不是数的都当 0。
        note_eval(id, &features(serde_json::json!({"features": {FEATURE: {"value": 1.5}}})));
        assert_eq!(rate(id), 0.0);
        note_eval(id, &features(serde_json::json!({"features": {FEATURE: {"value": "0.5"}}})));
        assert_eq!(rate(id), 0.0);
        note_eval(id, &features(serde_json::json!({"features": {}})));
        assert_eq!(rate(id), 0.1, "eval 里没有这一项退回默认");
    }
}
