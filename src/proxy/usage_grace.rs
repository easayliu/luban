//! 「额度用尽后的宽限」请求头 `anthropic-usage-limit: extended`（2.1.291 起）要的按账号的事实。
//!
//! 写不写这个头见 [`config::CC_USAGE_LIMIT_HEADER`]：除了「续轮、主线程用途」这种按请求判的
//! 条件，还要几样只有上游知道、按账号变的事实——
//!
//! - **服务端开关 `tengu_lantern_spool`**：按账号分流的实验，eval 响应里能读到
//!   （[`note_eval_features`]，保活与会话启动握手都会拉 eval）。
//! - **额外用量停用**：上游响应头 `anthropic-ratelimit-unified-overage-disabled-reason` 有值。官方
//!   客户端把最近一次限流器跑过的响应（带 `anthropic-ratelimit-unified-status`）里这个值缓存下来
//!   （`cachedExtraUsageDisabledReason`）；这里同样只在那种响应上更新（[`note_overage`]）。额外用量
//!   **开着**的号另有一条路：服务端开关 `tengu_smooth_harbor` 开着也算（2.1.291 `function Far`：
//!   `cachedExtraUsageDisabledReason === null && !tengu_smooth_harbor` 才拦），它同样从 eval 读。
//!
//! 没见过的一律按「不成立」处理——宁可少一个头，也不给一个没进实验的号发实验头。只存在内存里：
//! 重启后等下一次 eval 与下一条带限流头的响应重新学到，期间不写这个头。

use std::collections::HashMap;

use super::RateLimitInfo;
use crate::config;

#[derive(Default, Clone, Copy)]
struct Grace {
    /// `tengu_lantern_spool` 的取值，没拉到 eval 之前是 `None`。
    lantern_spool: Option<bool>,
    /// `tengu_smooth_harbor` 的取值（eval 里没有这个键时不改）。
    smooth_harbor: Option<bool>,
    /// 最近一次限流器跑过的响应里额外用量是否停用，没见过之前是 `None`。
    overage_disabled: Option<bool>,
}

static GRACE: std::sync::LazyLock<parking_lot::RwLock<HashMap<i64, Grace>>> =
    std::sync::LazyLock::new(Default::default);

/// 记下一条上游响应的额外用量状态。没带 `anthropic-ratelimit-unified-status` 的响应限流器
/// 没跑过，不算数（官方同样只认那种响应）。
pub(super) fn note_overage(cred_id: i64, info: &RateLimitInfo) {
    if info.unified_status.is_none() {
        return;
    }
    let disabled = info.overage_disabled_reason.is_some();
    GRACE.write().entry(cred_id).or_default().overage_disabled = Some(disabled);
}

/// 从 eval 响应体里记下 `tengu_lantern_spool` 与 `tengu_smooth_harbor`：
/// `{"features": {"tengu_lantern_spool": {"value": true, …}, …}}`。
///
/// 一份带 `features` 对象的 eval 是**整份**特性缓存：官方拿它整个替换旧的，缺的开关按默认值
/// （这两个都是 false）算——先给 `smooth_harbor = true`、下一份不再提它，就是关了。体解析不出来、
/// 或者没有 `features` 对象的不算数，什么都不改。
pub fn note_eval_features(cred_id: i64, body: &[u8]) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else { return };
    let Some(features) = v.get("features").and_then(|f| f.as_object()) else { return };
    let flag = |k: &str| {
        features.get(k).and_then(|f| f.get("value")).and_then(|b| b.as_bool()).unwrap_or(false)
    };
    let mut map = GRACE.write();
    let g = map.entry(cred_id).or_default();
    g.lantern_spool = Some(flag("tengu_lantern_spool"));
    g.smooth_harbor = Some(flag("tengu_smooth_harbor"));
}

/// 这个号现在该不该带 `anthropic-usage-limit: extended`（只看按账号的事实，按请求的条件见
/// [`usage_limit_wanted`]）：进了 `tengu_lantern_spool` 实验，且额外用量停用着、或
/// `tengu_smooth_harbor` 开着。
fn account_eligible(cred_id: i64) -> bool {
    GRACE.read().get(&cred_id).is_some_and(|g| {
        g.lantern_spool == Some(true)
            && (g.overage_disabled == Some(true) || g.smooth_harbor == Some(true))
    })
}

/// 模拟路径这条请求要不要写 `anthropic-usage-limit: extended`：主线程用途；同一轮里带着工具结果
/// 的续轮（官方 `queryTracking.depth > 0`），或者后台任务通知那一轮（官方 `Far` 的第三个参数
/// `v7n`：末条用户消息是会话任务的通知——`cap/auto-2.1.291-20261006-full/00174`、`00564` 末条只有
/// 通知那段 text、没有工具结果，也带这个头）；且这个号按账号的事实成立。
pub(super) fn usage_limit_wanted(
    v: &serde_json::Value,
    profile: &config::CcProfile,
    cred_id: i64,
) -> bool {
    profile.request_class == "main"
        && (is_tool_continuation(v) || is_task_notification_turn(v))
        && account_eligible(cred_id)
}

/// 末条非 system 消息。
fn last_message(v: &serde_json::Value) -> Option<&serde_json::Value> {
    v.get("messages").and_then(|m| m.as_array()).and_then(|m| {
        m.iter().rev().find(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"))
    })
}

/// 末条非 system 消息是带 `tool_result` 的用户消息、且不是一次新的用户输入——官方的
/// `queryTracking.depth > 0`。
fn is_tool_continuation(v: &serde_json::Value) -> bool {
    if crate::telemetry::last_is_new_prompt_body(v) {
        return false;
    }
    let Some(last) = last_message(v) else { return false };
    last.get("role").and_then(|r| r.as_str()) == Some("user")
        && last.get("content").and_then(|c| c.as_array()).is_some_and(|blocks| {
            blocks.iter().any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        })
}

/// 这一轮是**纯**后台任务通知：末条非 system 消息是用户消息，里面有官方那段
/// `[SYSTEM NOTIFICATION - NOT USER INPUT]` 提醒（裹着 `<task-notification>`，与
/// `crate::telemetry` 认通知的口径相同），且除了 `<system-reminder>` 之外没有别的正文。
///
/// 通知送到时用户正好敲了下一句，两样会挤在同一条消息里（`cap/auto-2.1.291-20261006-full/00078`、
/// `00086`：通知那块后面跟着用户的新输入，billing 写 `cc_turn_origin=human`）——那是一次新的用户
/// 输入，官方不带这个头。
fn is_task_notification_turn(v: &serde_json::Value) -> bool {
    let Some(last) = last_message(v) else { return false };
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    let is_notice = |t: &str| {
        t.contains("[SYSTEM NOTIFICATION - NOT USER INPUT]") && t.contains("<task-notification>")
    };
    let texts: Vec<&str> = match last.get("content") {
        Some(serde_json::Value::String(t)) => vec![t.as_str()],
        Some(serde_json::Value::Array(blocks)) => {
            if blocks.iter().any(|b| b.get("type").and_then(|t| t.as_str()) != Some("text")) {
                return false;
            }
            blocks.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect()
        }
        _ => return false,
    };
    texts.iter().any(|t| is_notice(t)) && texts.iter().all(|t| only_reminders(t))
}

/// 去掉所有 `<system-reminder>…</system-reminder>` 区段之后只剩空白。只看开头不够：同一个文本块
/// （或 content 字符串）里，闭合标签之后还可能跟着用户的新输入。没闭合的区段不算数。
fn only_reminders(text: &str) -> bool {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            return true;
        }
        let Some(body) = trimmed.strip_prefix(OPEN) else { return false };
        let Some(end) = body.find(CLOSE) else { return false };
        rest = &body[end + CLOSE.len()..];
    }
}

#[cfg(test)]
fn reset_for_test(cred_id: i64) {
    GRACE.write().remove(&cred_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(on: bool) -> Vec<u8> {
        format!(r#"{{"features":{{"tengu_lantern_spool":{{"value":{on},"on":{on},"source":"experiment"}}}}}}"#)
            .into_bytes()
    }

    fn limited(disabled: bool) -> RateLimitInfo {
        let mut h = axum::http::HeaderMap::new();
        h.insert("anthropic-ratelimit-unified-status", "allowed".parse().unwrap());
        if disabled {
            h.insert(
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "org_level_disabled".parse().unwrap(),
            );
        }
        RateLimitInfo::from_headers(&h)
    }

    /// `anthropic-usage-limit: extended` 只给工具续轮、主线程、进了实验且额外用量停用着的号：
    /// `cap/auto-2.1.291-20261006-full` 每轮首条（`00032`、`00033`）不带，续轮（`00036` 起）带；
    /// 按账号的事实缺一样都不写，没带限流头的响应不改已记的状态。
    #[test]
    fn usage_limit_header_needs_continuation_and_both_account_facts() {
        let main = config::cc_profile(config::CcProfileKind::MainOpus);
        let title = config::cc_profile(config::CcProfileKind::SessionTitleHaiku);
        let cont: serde_json::Value = serde_json::json!({"messages": [
            {"role": "user", "content": "run ls"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]}
        ]});
        let fresh: serde_json::Value =
            serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        let id = 9_999_001;
        reset_for_test(id);
        assert!(!usage_limit_wanted(&cont, main, id), "什么都没见过");
        note_eval_features(id, &eval(true));
        assert!(!usage_limit_wanted(&cont, main, id), "额外用量状态还没见过");
        note_overage(id, &limited(true));
        assert!(usage_limit_wanted(&cont, main, id));
        assert!(!usage_limit_wanted(&fresh, main, id), "新输入那条不带");
        assert!(!usage_limit_wanted(&cont, title, id), "auxiliary 不带");
        // 没带限流头的响应不算数。
        note_overage(id, &RateLimitInfo::from_headers(&axum::http::HeaderMap::new()));
        assert!(usage_limit_wanted(&cont, main, id));
        note_overage(id, &limited(false));
        assert!(!usage_limit_wanted(&cont, main, id), "额外用量没停用、smooth_harbor 没见过");
        note_overage(id, &limited(true));
        note_eval_features(id, &eval(false));
        assert!(!usage_limit_wanted(&cont, main, id), "没进实验");
        note_eval_features(id, b"not json");
        assert!(!usage_limit_wanted(&cont, main, id), "体解析不出来不改已记的");
        reset_for_test(id);
    }

    /// 额外用量开着的号，`tengu_smooth_harbor` 开着也发（`function Far` 那条放行）；后台任务通知
    /// 那一轮末条没有工具结果，也发（`00174`、`00564`）。
    #[test]
    fn usage_limit_header_smooth_harbor_and_task_notifications() {
        let main = config::cc_profile(config::CcProfileKind::MainOpus);
        let notice: serde_json::Value = serde_json::json!({"messages": [
            {"role": "user", "content": "run it in the background"},
            {"role": "assistant", "content": "started"},
            {"role": "user", "content": [{"type": "text", "text": "<system-reminder>\n[SYSTEM NOTIFICATION - NOT USER INPUT]\nThis is an automated background-task event\n<task-notification>\n<status>completed</status>\n</task-notification>\n</system-reminder>"}]}
        ]});
        let fresh: serde_json::Value =
            serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        let id = 9_999_002;
        reset_for_test(id);
        note_eval_features(
            id,
            br#"{"features":{"tengu_lantern_spool":{"value":true},"tengu_smooth_harbor":{"value":true}}}"#,
        );
        note_overage(id, &limited(false));
        assert!(usage_limit_wanted(&notice, main, id), "额外用量开着、smooth_harbor 开着照发");
        assert!(!usage_limit_wanted(&fresh, main, id), "新的用户输入仍不发");
        note_eval_features(
            id,
            br#"{"features":{"tengu_lantern_spool":{"value":true},"tengu_smooth_harbor":{"value":false}}}"#,
        );
        assert!(!usage_limit_wanted(&notice, main, id), "两条路都不通");
        note_overage(id, &limited(true));
        assert!(usage_limit_wanted(&notice, main, id), "额外用量停用那条路");

        // 通知与用户的新输入挤在同一条消息里（`00078`、`00086`）：是新输入，不带。
        let mut mixed = notice.clone();
        mixed["messages"][2]["content"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type": "text", "text": "launch a subagent to count lines"}));
        assert!(!usage_limit_wanted(&mixed, main, id), "夹带通知的新输入");

        // 同一个文本块里、闭合标签之后跟着用户正文：同样是新输入。
        let notice_text = notice["messages"][2]["content"][0]["text"].as_str().unwrap().to_string();
        let mut one_block = notice.clone();
        one_block["messages"][2]["content"][0]["text"] =
            serde_json::json!(format!("{notice_text}\nlaunch a subagent to count lines"));
        assert!(!usage_limit_wanted(&one_block, main, id), "同一块里夹带新输入");
        // content 写成字符串：纯通知照带，后面跟了正文就不带。
        let mut as_string = notice.clone();
        as_string["messages"][2]["content"] = serde_json::json!(notice_text.clone());
        assert!(usage_limit_wanted(&as_string, main, id), "字符串形态的纯通知");
        as_string["messages"][2]["content"] =
            serde_json::json!(format!("{notice_text}\n\nultrathink: is div() safe?"));
        assert!(!usage_limit_wanted(&as_string, main, id), "字符串形态夹带新输入");
        // 前面是别的提醒、后面是通知，全是提醒区段：仍是纯通知。
        let mut two = notice.clone();
        two["messages"][2]["content"][0]["text"] = serde_json::json!(format!(
            "<system-reminder>\nother\n</system-reminder>\n{notice_text}"
        ));
        assert!(usage_limit_wanted(&two, main, id), "几段提醒连在一起");
        reset_for_test(id);
    }

    /// 一份 eval 是整份特性缓存：后来那份不再提 `tengu_smooth_harbor`，就按 false 算，不沿用上一份
    /// 的 true；不是 eval 形态的体（没有 `features`）不改。
    #[test]
    fn eval_features_replace_the_previous_snapshot() {
        let main = config::cc_profile(config::CcProfileKind::MainOpus);
        let cont: serde_json::Value = serde_json::json!({"messages": [
            {"role": "user", "content": "run ls"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]}
        ]});
        let id = 9_999_003;
        reset_for_test(id);
        note_overage(id, &limited(false));
        note_eval_features(
            id,
            br#"{"features":{"tengu_lantern_spool":{"value":true},"tengu_smooth_harbor":{"value":true}}}"#,
        );
        assert!(usage_limit_wanted(&cont, main, id));
        note_eval_features(id, br#"{"features":{"tengu_lantern_spool":{"value":true}}}"#);
        assert!(!usage_limit_wanted(&cont, main, id), "smooth_harbor 被撤掉就是 false");
        note_eval_features(
            id,
            br#"{"features":{"tengu_lantern_spool":{"value":true},"tengu_smooth_harbor":{"value":true}}}"#,
        );
        note_eval_features(id, br#"{"error":"x"}"#);
        assert!(usage_limit_wanted(&cont, main, id), "没有 features 的体不算一份 eval");
        reset_for_test(id);
    }
}
