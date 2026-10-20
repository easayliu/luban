//! 与 API Key 直连对齐的几条本地 400 与报错下标修正：订阅通道放行、或出站改写让上游换了说法的
//! 请求形态，按直连那句原文就地回给客户端（渠道测试工具拿「直连 400、中转 200」识别中转）。
//!
//! 判的都是**来访原件**与来访自己那串 `anthropic-beta`：luban 出站补的 beta（`thinking-display-updates`、
//! `interleaved-thinking`）正是两边结果分叉的原因，不能拿出站那份来判。
//! 全部 2026-10-18 对 api.anthropic.com 实测，原文逐字取自直连的响应。

use std::borrow::Cow;

use axum::body::Bytes;

use super::has_beta;
use super::simulation::{BlockFate, billing_only_block_fates};
use super::thinking::adaptive_only_model;

/// 缺 `anthropic-version` 头时直连的 400 原文。上游最先校验它（体再错也先报这条）；订阅通道
/// 收到的是 luban 补了头的请求，永远回不出这句。
pub(crate) const VERSION_REQUIRED_MESSAGE: &str = "anthropic-version: header is required";

/// `budget_tokens` 不小于 `max_tokens` 时直连的 400 原文。来访带 `interleaved-thinking` 时直连
/// 也放行；luban 出站恒带它，订阅通道因此一律放行。
pub(crate) const BUDGET_EXCEEDS_MAX_TOKENS_MESSAGE: &str = "`max_tokens` must be greater than \
`thinking.budget_tokens`. Please consult our documentation at \
https://platform.claude.com/docs/en/build-with-claude/extended-thinking#max-tokens-and-context-window-size-with-extended-thinking";

const BETA_DISPLAY_UPDATES: &str = "thinking-display-updates-2026-08-18";
const BETA_INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";

/// 来访没带 `thinking-display-updates` 却写了直连不收的 `thinking.display` → 直连那句 400。
///
/// 直连只认 `summarized` / `omitted`（`null` 等于没写），`updates` 要那项 beta；luban 出站补了它，
/// 订阅通道于是把 `updates` 放行、把非法值报成三选一。实测这条排在缺 `max_tokens`、未知顶层字段
/// 与采样参数冲突**之前**，所以不必让位。来访带了那项 beta 的交给上游：出站同样带，两边说法一致。
/// 只判 `adaptive` / `enabled`：`between_tools` 带 `display` 两边都是同一句「多余字段」。
pub(crate) fn client_display_rejection(
    body: Option<&serde_json::Value>,
    inbound_betas: &[String],
) -> Option<String> {
    let th = body?.get("thinking")?.as_object()?;
    let ty = th.get("type")?.as_str().filter(|t| matches!(*t, "adaptive" | "enabled"))?;
    let display = th.get("display").filter(|d| !d.is_null())?;
    if matches!(display.as_str(), Some("summarized" | "omitted"))
        || has_beta(inbound_betas, BETA_DISPLAY_UPDATES)
    {
        return None;
    }
    Some(format!("thinking.{ty}.display: Input should be 'summarized', 'omitted'"))
}

/// 来访没带 `interleaved-thinking`、`thinking.budget_tokens` 却不小于 `max_tokens` → 直连那句 400
/// （[`BUDGET_EXCEEDS_MAX_TOKENS_MESSAGE`]）。
///
/// 只判收 `enabled` 的模型（[`adaptive_only_model`] 之外）：新一代对 `enabled` 回的是另一句，
/// 由 [`super::client_enabled_thinking_rejection`] 管。**排在这条之前的校验让给上游**（实测）：
/// `budget_tokens` 缺 / 非整数 / 不足 1024、`display` 非法、与 thinking 冲突的 `temperature` /
/// `top_k` / `top_p` 各回各的那句，两条通道一致，照发。
pub(crate) fn client_budget_rejection(
    body: Option<&serde_json::Value>,
    model: Option<&str>,
    inbound_betas: &[String],
) -> bool {
    let Some(body) = body else { return false };
    let Some(th) = body.get("thinking").and_then(|t| t.as_object()) else { return false };
    if th.get("type").and_then(|t| t.as_str()) != Some("enabled")
        || model.is_none_or(adaptive_only_model)
        || has_beta(inbound_betas, BETA_INTERLEAVED_THINKING)
    {
        return false;
    }
    let Some(budget) = th.get("budget_tokens").and_then(|b| b.as_u64()).filter(|&b| b >= 1024)
    else {
        return false;
    };
    let Some(max_tokens) = body.get("max_tokens").and_then(|m| m.as_u64()) else { return false };
    if budget < max_tokens {
        return false;
    }
    if th.get("display").is_some_and(|d| {
        !d.is_null() && !matches!(d.as_str(), Some("summarized" | "omitted" | "updates"))
    }) {
        return false;
    }
    !(body.get("temperature").is_some_and(|t| t.as_f64() != Some(1.0))
        || body.get("top_k").is_some()
        || body.get("top_p").is_some_and(|p| !p.as_f64().is_some_and(|p| p >= 0.95)))
}

/// billing-only 改写后出站 `system` 的块与来访块的对应：`map[k]` 是出站 `system[k + 1]` 来自的
/// 来访下标（出站 `system[0]` 是 luban 那块 billing header，不在表里）。上游报错里的 `system.N`
/// 按它还原成客户端那份的下标（[`unshift_system_index`]）。
///
/// 按 billing-only 的同一套规则重算（[`billing_only_block_fates`]：块首抄来的 billing header 行去掉、
/// 只剩空白的块不出站），再逐块核对出站正文与之一致，对得上才给表：客户端首块混着旧 billing
/// header 与业务指令时块数照样对得上；system 是字符串（直连那边没有块下标）、整段挪进了首条
/// 用户消息、或出站不是这套改写的结果时返回 `None`，不改。
pub(crate) fn billing_system_index_map(
    inbound: Option<&serde_json::Value>,
    outbound: &serde_json::Value,
) -> Option<Vec<usize>> {
    fn text(b: &serde_json::Value) -> Option<&str> {
        b.get("text").and_then(|t| t.as_str())
    }
    let out = outbound.get("system")?.as_array()?;
    let inb = inbound?.get("system")?.as_array()?;
    if !out.first().and_then(text).is_some_and(|t| t.starts_with("x-anthropic-billing-header:")) {
        return None;
    }
    let kept: Vec<(usize, Option<Cow<'_, str>>)> = inb
        .iter()
        .zip(billing_only_block_fates(inb))
        .enumerate()
        .filter_map(|(i, (b, fate))| match fate {
            BlockFate::Drop => None,
            BlockFate::Keep => Some((i, text(b).map(Cow::Borrowed))),
            BlockFate::Rewrite(t) => Some((i, Some(Cow::Owned(t)))),
        })
        .collect();
    if kept.len() + 1 != out.len() {
        return None;
    }
    let consistent = kept
        .iter()
        .zip(&out[1..])
        .all(|((_, want), got)| want.as_deref().is_none_or(|w| text(got) == Some(w)));
    consistent.then(|| kept.into_iter().map(|(i, _)| i).collect())
}

/// 把上游报错 `error.message` 里的 `system.N`（N ≥ 1）按 [`billing_system_index_map`] 改回客户端
/// 那份的下标；一处都没变就返回 `None`。`system.0` 指的是 luban 自己那块 billing header，不动。
/// 只改 `message`，体的其余字节不碰。
pub(crate) fn unshift_system_index(err: &Bytes, map: &[usize]) -> Option<Bytes> {
    let mut v: serde_json::Value = serde_json::from_slice(err).ok()?;
    let msg = v.pointer_mut("/error/message")?;
    let fixed = remap_system_paths(msg.as_str()?, map)?;
    *msg = serde_json::Value::String(fixed);
    serde_json::to_vec(&v).ok().map(Bytes::from)
}

/// [`unshift_system_index`] 的纯文本部分：逐处找 `system.<数字>`，前一个字符不能是字母数字或
/// `_`（免得改到 `xsystem.1` 这类别的名字）。出站下标 N ≥ 1 且在表内的换成 `map[N - 1]`。
fn remap_system_paths(msg: &str, map: &[usize]) -> Option<String> {
    const KEY: &str = "system.";
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    let mut changed = false;
    while let Some(at) = rest.find(KEY) {
        // 紧挨在 `system.` 前面的那个字符：在本段里就取本段的，段首则取已写出部分的末尾。
        let boundary = rest[..at].chars().last().or_else(|| out.chars().last());
        let after = &rest[at + KEY.len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        out.push_str(&rest[..at + KEY.len()]);
        rest = after;
        if digits == 0 || boundary.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let client = after[..digits]
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|k| map.get(k).copied());
        if let Some(client) = client {
            out.push_str(&client.to_string());
            rest = &after[digits..];
            changed |= after[..digits] != client.to_string();
        }
    }
    out.push_str(rest);
    changed.then_some(out)
}

#[cfg(test)]
mod tests;
