//! 请求参数补齐：thinking、context_management、output_config。

use super::*;

/// [`ensure_thinking`] 补 `thinking` 的 `max_tokens` 下限：再小的请求，思考预算本身就塞不进去，
/// 不值得加，见该函数文档的「三种情况不补」。
pub(in crate::proxy) const THINKING_MIN_MAX_TOKENS: u64 = 1024;

/// 模拟路径下补 `thinking`，形态取自 profile（[`config::CcThinking`]，2.1.260 抓包）：
///
/// - `EnabledUpdates`（haiku 族）：`{"budget_tokens": N, "type": "enabled",
///   "display": "updates"}`（`cap/2.1.260/00020`，`budget_tokens` 在前），
///   `N = max_tokens - 1`（`max_tokens: 32000` → `31999`）；
/// - `AdaptiveUpdates`（opus / fable / sonnet 主线程）：
///   `{"type": "adaptive", "display": "updates"}`（`cap/2.1.260-2/00025`、`cap/2.1.260/00018`）。
///   2.1.258 时只有 fable 带 `display`，2.1.260 起 opus 也带了；
/// - `Disabled` / `Absent`：不补——helper / 标题 / 分类那几个 profile 官方就是
///   `{"type":"disabled"}` 或整个不发，而模拟路径只造主线程形态，走不到这里。
///
/// 别给 opus-5 / sonnet-5 / fable 发 `enabled + budget_tokens`：这几个模型上 `budget_tokens`
/// 直接 400。
///
/// 三种情况不补：
/// - 客户端自己带了 `thinking`（`disabled`/`null`/`enabled` 都算——那是它自己的选择）；
/// - `tool_choice` 强制工具调用（上游不允许两者并存）；
/// - `max_tokens` 太小（< 1024）：thinking 本身要消耗 token 预算，探测级请求不值得加。
pub(in crate::proxy) fn ensure_thinking(
    v: &mut serde_json::Value,
    profile: &config::CcProfile,
) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    if obj.contains_key("thinking") {
        return false;
    }
    // tool_choice 强制工具调用时上游不允许 thinking，不注入。
    // 客户端明确要强制工具，thinking 是我们补的，客户端优先。
    if obj
        .get("tool_choice")
        .and_then(|tc| tc.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| t == "tool")
    {
        return false;
    }
    let max_tokens = obj.get("max_tokens").and_then(|m| m.as_u64()).unwrap_or(32000);
    if max_tokens < THINKING_MIN_MAX_TOKENS {
        return false;
    }
    let value = match profile.thinking {
        config::CcThinking::Enabled | config::CcThinking::EnabledUpdates => {
            let budget = max_tokens.saturating_sub(1).max(1);
            // 官方 key 序是 `budget_tokens` → `type` → `display`，手工插入以保住顺序
            // （`cap/2.1.260/00020`）。
            let mut m = serde_json::Map::new();
            m.insert("budget_tokens".into(), serde_json::Value::Number(budget.into()));
            m.insert("type".into(), "enabled".into());
            if profile.thinking == config::CcThinking::EnabledUpdates {
                m.insert("display".into(), "updates".into());
            }
            serde_json::Value::Object(m)
        }
        config::CcThinking::Adaptive => serde_json::json!({"type": "adaptive"}),
        config::CcThinking::AdaptiveUpdates => {
            serde_json::json!({"type": "adaptive", "display": "updates"})
        }
        // 模拟路径只造主线程 profile，这两支走不到；真走到了就是「客户端没写、官方也不写」，
        // 不补才是对的。
        config::CcThinking::Disabled | config::CcThinking::Absent => return false,
    };
    insert_top_level(
        v,
        "thinking",
        value,
        &["max_tokens", "metadata", "tools", "system", "messages", "model"],
    );
    true
}

/// CC 请求补 `thinking.display:"updates"`：订阅端官方发的是
/// `{"type":"adaptive","display":"updates"}`（2.1.258 只有 fable-5-1 这样，`00013`；
/// 2.1.260 起 opus 主线程也是，`cap/2.1.260-2/00025`），API-key 端发裸 `adaptive`。
/// 只在 `thinking.type == "adaptive"` 且客户端没写 `display` 时补；模型族由
/// [`cc_profile_for`] 判（该族的官方串里有 `thinking-display-updates` 的才算）。
///
/// **调用方必须先确认出站头里真有那项 beta**（[`rewrite_body`] 的 `display_beta`）：`updates`
/// 是 beta 才认的取值，头上没声明时上游回 400 `Input should be 'summarized', 'omitted'`。
/// 2026-09-02 一条 `claude-vscode, agent-sdk/0.3.258` 的 fable-5-1 请求就是这样被拒的——它的
/// beta 串没有 `advisor-tool`，[`merge_beta_for`] 按老世代处理不补，体里却写了。
///
/// 那个前提也是**唯一**的门槛：本函数不再自己按模型族判一遍。哪一族在哪一版发这项 beta
/// 是 [`merge_beta_for`] 的事（它按来访自报的版本查 [`config::cc_profile_at`]），在这里再判
/// 一次只会两处口径分头漂移——2.1.260 起 opus 主线程也发 `display:"updates"`，
/// 原来那句「只有 fable」的判断当场就成了错的。
pub(in crate::proxy) fn fill_thinking_display(v: &mut serde_json::Value) -> bool {
    let Some(th) = v.get_mut("thinking").and_then(|t| t.as_object_mut()) else { return false };
    if th.get("type").and_then(|t| t.as_str()) != Some("adaptive") || th.contains_key("display") {
        return false;
    }
    th.insert("display".into(), "updates".into());
    true
}

/// 补上官方客户端恒发的 `context_management`，落在官方位置（`thinking` 之后、
/// `output_config`/`stream` 之前）。已经有这个字段就原样不动，返回 `false`。
///
/// **依据**：`cap/raw` 八份抓包（四份直连、四份经 luban）的顶层 `context_management`
/// **逐字节相同**——`{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}`，
/// 四个模型族无一例外，连 haiku 那两份也一样。这与 `thinking`/`output_config` 那种逐族不同
/// 的字段不是一类，不存在「补哪一份」的选择问题。
///
/// **为什么该补**：这个字段要 `context-management-2025-06-27` 认，而 [`config::CC_PROFILES`]
/// 里**每一个** profile 的 beta 串都带着它。
/// 不补就是「头上声明了 context-management、体里零个 `edits`」——与
/// [`ensure_beta_query`] 要消灭的那个组合同一个形状，只是落在体上。
///
/// `keep:"all"` 意为「一条都不清」，故补它不改变本次请求的语义，也不动计价：与
/// `known_fingerprint_gaps` 第 7 条的 `fallbacks`（补上等于替用户决定换模型）正相反，
/// 那条不补的理由在这里不成立。
///
/// **但它不是独立字段——依赖 `thinking`**。上游对「有 `clear_thinking` 却没开 thinking」
/// 的请求直接回 400：
///
/// ```text
/// `clear_thinking_20251015` strategy requires `thinking` to be enabled or adaptive
/// ```
///
/// 抓包看不出这层依赖：八份**全都**开着 thinking（opus/sonnet/fable 是 `{"type":"adaptive"}`，
/// haiku 是 `{"budget_tokens":31999,"type":"enabled"}`），于是 8/8 共现让它看着像个独立字段。
/// 这是一次「共现不等于无依赖」的教训——v0.2.51 上线后普通请求即因此 400。
///
/// **不替客户端补 `thinking`** 来满足这个依赖，三条理由都写在抓包里：
/// 1. haiku 那份是 `budget_tokens:31999` 配 `max_tokens:32000`，budget 必须小于 max_tokens。
///    客户端发 `max_tokens:1024` 时这个值根本塞不进去，要么改它的 max_tokens（改掉它明确
///    要的上限与费用天花板），要么自己算一个 budget——两条都是替它做决定。
/// 2. 开了 thinking，响应里就多出 thinking 块，客户端未必认得，直接把它弄坏。
/// 3. thinking token 按输出计费，等于未经同意加钱。
///
/// 故只在客户端**自己已经开着** thinking 时才补，其余情形一个字节都不动。
///
/// **注意**：模拟路径下 [`ensure_thinking`] 会先补上 `thinking`，然后本函数就能自然补上
/// `context_management`，两者配合才完整。
pub(in crate::proxy) fn ensure_context_management(v: &mut serde_json::Value) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    // 客户端自己带了就不动——那是它自己的编辑策略，替它改属于越权（同 [`ensure_beta_query`]
    // 对客户端自带 `beta=` 的口径）。
    if obj.contains_key("context_management") {
        return false;
    }
    // 没开 thinking 就不补：`clear_thinking` 依赖它，硬补上游直接 400（见函数文档）。
    // 上游认的是 `enabled`/`adaptive` 两种，`disabled` 与字段缺失都不算。
    let thinking_on = obj
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| matches!(t, "enabled" | "adaptive"));
    if !thinking_on {
        return false;
    }
    let value = serde_json::json!({
        "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }]
    });
    // 官方顺序是 `… max_tokens, thinking, context_management, output_config, stream`。
    // 走到这里必有 `thinking`，故锚点首选它，落位与官方一致。
    insert_top_level(
        v,
        "context_management",
        value,
        &["thinking", "max_tokens", "metadata", "tools", "system", "messages", "model"],
    );
    true
}

/// 模拟路径下按 profile 补顶层 `output_config`：2.1.277 起 opus / fable / sonnet 主线程每条都是
/// `{"effort":"high"}`（`cap/2.1.277/00023`、`00031`、`00357`，首轮与工具续轮都带；opus-5-5 与
/// sonnet-5-5 的官方默认是 `medium`，`cap/2.1.280/00021`、`cap/2.1.285/00045`，模拟路径照旧按
/// `high` 发），haiku
/// 主线程与全部辅助请求不带（[`config::CcProfile::effort`] 为 `None`，这里什么都不做）。
///
/// 客户端自己带了 `output_config`（不论写的是 `effort` 还是 `format`）就不动——那是它自己的
/// 输出策略，替它改是越权。位置按官方线序落在 `context_management` 之后、`thread` /
/// `diagnostics` / `stream` 之前（[`config::CC_BODY_ORDER_MAIN_2_1_270`]）。
///
/// **`effort` 要配 `effort-2025-11-24` beta**：三个带它的 profile 的 beta 串里都有这一项，
/// haiku 的没有——这也是 haiku 那行 `effort: None` 的另一层理由，别只看抓包里有没有字段。
pub(in crate::proxy) fn ensure_output_config(
    v: &mut serde_json::Value,
    profile: &config::CcProfile,
) -> bool {
    let Some(effort) = profile.effort else { return false };
    let Some(obj) = v.as_object() else { return false };
    if obj.contains_key("output_config") {
        return false;
    }
    insert_top_level(
        v,
        "output_config",
        serde_json::json!({ "effort": effort }),
        &[
            "context_management",
            "thinking",
            "max_tokens",
            "metadata",
            "tools",
            "system",
            "messages",
            "model",
        ],
    );
    true
}
