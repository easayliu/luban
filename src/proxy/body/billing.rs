//! billing header 与 `diagnostics`：会话链字段的补齐。

use super::*;

/// 给 `system[0]` 的 `x-anthropic-billing-header` 补上 `cch=<值>`，对齐订阅客户端。
///
/// 官方客户端只在订阅(OAuth)模式下发这个字段，API-key 模式（接入 luban 的形态）不发，
/// 于是「OAuth token + 无 cch」是个确定性判据。抓包实测补齐后与真实客户端形态一致：
/// `…cc_version=2.1.260.222; cc_entrypoint=cli; cch=f850a;`
///
/// 只在该块确实是 billing header、且尚无 `cch=` 时改写；其余情况返回 `false` 不动结构。
///
/// **这条路只服务真实 CC 来访。** 模拟路径的 billing header 由
/// [`simulated_billing_header_text`] 一次拼好（cch 也在里面），走不到这里。
///
/// `system[0]` 位于第一个缓存断点之前，直觉上「每请求变的 cch 会打爆 prompt cache」——
/// **抓包证否了这一点**，上游不把 billing header 算进缓存键，见 [`cch_value`]。
pub(in crate::proxy) fn ensure_billing_cch(v: &mut serde_json::Value) -> bool {
    let blk = match v.get_mut("system").and_then(|s| s.as_array_mut()).and_then(|a| a.first_mut()) {
        Some(b) => b,
        None => return false,
    };
    let text = match blk.get("text").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    if !text.starts_with("x-anthropic-billing-header:") || text.contains("cch=") {
        return false;
    }
    let mut s = text.trim_end().to_string();
    if !s.ends_with(';') {
        s.push(';');
    }
    s.push_str(&format!(" cch={};", cch_value()));
    match blk.get_mut("text") {
        Some(t) => {
            *t = serde_json::Value::String(s);
            true
        }
        None => false,
    }
}

/// 真 CC 主线程的 billing header 在 `cc_prompt_id` 之后还要补哪几项，见 [`append_billing_link`]。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct TurnFields {
    /// `cc_turn_origin=human`（2.1.277 起）。
    pub(super) origin: bool,
    /// `cc_prompt_index=N; cc_turn_index=N;`（2.1.285 起）。
    pub(super) index: bool,
}

/// `system[0]` 是不是一条 billing header。
pub(in crate::proxy) fn has_billing_header(v: &serde_json::Value) -> bool {
    v.get("system")
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| t.starts_with("x-anthropic-billing-header:"))
}

/// 给**真实 CC 来访**那条 billing header 追加会话关联字段：`cc_prev_req` 与
/// `cc_prompt_id`，落在 `cch` 之后（官方段序，见 [`simulated_billing_header_text`]）。
///
/// API-key 端的 CC 一个都不发，而订阅端官方每条主线程请求都有；luban 拿 OAuth token 转出去
/// 之后缺着，就是「OAuth 请求没有会话关联」这个官方不产生的形态。要不要补由
/// [`client_session_link`] 判，这里只管拼串。
///
/// 客户端**自己已经写了**哪一项就不动那一项——它比我们更清楚自己的链。
///
/// `turn` 决定 `cc_prompt_id` 之后再补不补 `cc_turn_origin=human` 与
/// `cc_prompt_index` / `cc_turn_index`，见 [`TurnFields`]。只在这条已经有 `cc_prompt_id`（客户端
/// 自己的或刚补的）时补：官方这几项与它同条件出现。`cc_turn_origin` 恒写 `human`——后台任务
/// 通知那种轮次（`task_notification`，`cap/2.1.285/00149`）代理分辨不出。
pub(super) fn append_billing_link(
    v: &mut serde_json::Value,
    link: &CcSessionLink,
    turn: TurnFields,
) -> bool {
    let blk = match v.get_mut("system").and_then(|s| s.as_array_mut()).and_then(|a| a.first_mut()) {
        Some(b) => b,
        None => return false,
    };
    let text = match blk.get("text").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    if !text.starts_with("x-anthropic-billing-header:") {
        return false;
    }
    let mut s = text.trim_end().to_string();
    let mut changed = false;
    if !s.ends_with(';') {
        s.push(';');
    }
    if let Some(prev) = &link.prev_req
        && !s.contains("cc_prev_req=")
    {
        s.push_str(&format!(" cc_prev_req={prev};"));
        changed = true;
    }
    if let Some(pid) = &link.prompt_id
        && !s.contains("cc_prompt_id=")
    {
        s.push_str(&format!(" cc_prompt_id={pid};"));
        changed = true;
    }
    if s.contains("cc_prompt_id=") {
        if turn.origin && !s.contains("cc_turn_origin=") {
            s.push_str(" cc_turn_origin=human;");
            changed = true;
        }
        if turn.index && link.prompt_index > 0 && !s.contains("cc_prompt_index=") {
            let n = link.prompt_index;
            s.push_str(&format!(" cc_prompt_index={n}; cc_turn_index={n};"));
            changed = true;
        }
    }
    if !changed {
        return false;
    }
    match blk.get_mut("text") {
        Some(t) => {
            *t = serde_json::Value::String(s);
            true
        }
        None => false,
    }
}

/// 补 `diagnostics.previous_message_id`：同会话上一条回复的 `message.id`，会话第一条写
/// `null`。
///
/// **字段恒在，值可为 null**——`cap/2.1.260-2/00013`、`00025`、`00057` 三份首轮全是
/// `{"previous_message_id":null}`，而不是不发 `diagnostics`。少这个字段与写错值同样是
/// 一个稳定差异。
///
/// 官方位置在 `output_config` 之后、`stream` 之前；`insert_top_level` 找不到锚点时追加，
/// 随后 [`align_cc_top_level_order`] 会按 profile 的键序归位，故这里的锚点只是省一次搬动。
///
/// 客户端自己带了 `diagnostics` 就不动——那是它自己的字段，替它改属于越权。
/// 官方不发这个字段的 profile（`cap/2.1.260-2/00063` 那条「猜下一句」、额度探测、
/// 标题生成、安全分类）由调用方按 profile 判，不在这里判。
pub(super) fn ensure_diagnostics(v: &mut serde_json::Value, link: &CcSessionLink) -> bool {
    if v.get("diagnostics").is_some() {
        return false;
    }
    let value = match &link.prev_message_id {
        Some(id) => serde_json::json!({ "previous_message_id": id }),
        None => serde_json::json!({ "previous_message_id": serde_json::Value::Null }),
    };
    insert_top_level(
        v,
        "diagnostics",
        value,
        &["output_config", "context_management", "thinking", "max_tokens", "metadata", "model"],
    );
    true
}
