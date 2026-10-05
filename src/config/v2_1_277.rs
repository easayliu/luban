//! 2.1.277 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.277 的 profile 表：2.1.277 ~ 2.1.279 来访的 beta 参照（[`cc_profile_at`]），以及 2.1.280
/// 表里没编的 SDK 子代理与标题生成的兜底（[`cc_profile`]）。beta 串逐字取自 `cap/2.1.277`，
/// 去掉 `oauth` 与动态的 `afk-mode`。
///
/// | profile | 抓包 | system | tools | thinking | class |
/// |---|---|---|---:|---|---|
/// | `MainOpus` | `00357` | 4 块 | 20 | adaptive+updates | main |
/// | `MainFable` | `00023`、`00026` | 4 块 | 20 | adaptive+updates | main |
/// | `MainSonnet` | `00031` | 4 块 | 19 | adaptive+updates | main |
/// | `MainHaiku` | `00046` | 4 块 | 19 | enabled+updates | main |
/// | `SdkSubagentHaiku` | `00049` | 3 块 | 9 | enabled+updates | subagent |
/// | `SessionTitleHaiku` | `00022` | 3 块 | 0 | disabled | auxiliary |
/// | `QuotaProbe` | `00005` | 无 | — | 无 | auxiliary |
///
/// 相对 2.1.260 这一版改了什么（四族主线程逐条核过）：
///
/// - billing 后缀四族主线程都是 `d56`、子代理 `385`、标题 `e18`——当时以为是逐 profile 定死的，
///   2.1.280 核实是 [`crate::proxy::cc_version_suffix`] 对会话首条输入派生的；
/// - **fable 不再带 `# Reporting outcomes` 块**，四族都是 `[billing, 身份, 基座, 其余]` 四块，
///   基座与其余段四族**同一份**（[`CC_SYSTEM_BASE`]、[`CC_SYSTEM_REST`]）；
/// - billing header 多了 `cc_turn_origin=human`，紧跟 `cc_prompt_id`（有 `cc_prompt_id` 的轮次
///   才有；工具续轮只有 `cc_prev_req`）；
/// - 每条请求多一个 `x-claude-code-request-class` 头（[`CcProfile::request_class`]）；
/// - opus / fable / sonnet 主线程顶层多 `output_config: {"effort":"high"}`（[`CcProfile::effort`]）；
/// - beta：四族新增 `mid-conversation-system-clear-at` 与 `thinking-binding-controls`，opus / fable
///   另有 `mid-conversation-tool-changes`；`server-side-fallback` 四族都不发了；opus / sonnet /
///   haiku 队尾是 `message-threads`（fable 不带）；haiku 主线程与子代理的 `claude-code` 只在
///   会话首轮（`thread: create`）出现，续轮不带——取首轮那份；
/// - 主线程工具 20 / 19 个：内建 14 个（[`crate::proxy::cc_tools_core`] 注入的那份）加
///   `ToolSearch` / `DeferredToolPlaceholder` 那一对延迟机制、用户自己的 MCP 工具，以及 opus /
///   fable 独有的服务端工具 `advisor`（`type: advisor_20260301`，模拟不注）；内建工具全带
///   `eager_input_streaming: true`，fable 也带了（2.1.260 时不带）；
/// - 会话续轮走 **message threads**：`thread: {type: continue, previous_message_id}`，只发新增
///   消息，`system` 只剩 billing header 一块、不带 `tools`（`00035`、`00051` 等 30 余条）。
///   模拟路径每轮仍发完整上下文、不写 `thread`；透传路径对这种续轮整条放行，见
///   [`crate::proxy::is_thread_continuation`]。
///
/// 无工具 helper 与安全分类在 2.1.277 里没有样本，不编行：[`cc_profile`] 对这两个 kind 落回
/// [`CC_PROFILES_2_1_260`]。
pub const CC_PROFILES_2_1_277: &[CcProfile] = &[
    CcProfile {
        kind: CcProfileKind::MainOpus,
        version: "2.1.277",
        // `cap/2.1.277/00357`。
        beta: "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               mid-conversation-tool-changes-2026-07-01,advisor-tool-2026-03-01,\
               advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
               effort-2025-11-24,fallback-credit-2026-06-01,thinking-binding-controls-2026-08-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_270,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainFable,
        version: "2.1.277",
        // `cap/2.1.277/00023`、`00026`（首轮与续轮逐字相同）。仍用 `per-turn-control`，且是四族里
        // 唯一不带 `message-threads` 的。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               fallback-credit-2026-06-01,thinking-binding-controls-2026-08-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        subagent: false,
        // 2.1.277 的 fable 不再单独带 reporting 块（`00023` 四块）。
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        // `00023` / `00026` 都**没有** `fallbacks` 字段，`server-side-fallback` beta 四族也都不发了
        // ——2.1.277 的官方形态里没有这一项。这里仍留着 2.1.260 那份字面量
        // （`cap/2.1.260/00018`），只给 `fable_refusal_fallback` 开关（默认关）用：用户主动要
        // 「fable 拒答时服务端改用 opus-5 重跑」时才写进体里并在头上补 beta，见
        // [`crate::proxy::refusal_fallbacks_for`]；开关关着一个字节都不发，与官方一致。
        fallbacks: Some(r#"[{"model":"claude-opus-5"}]"#),
        body_key_order: CC_BODY_ORDER_MAIN_2_1_270,
        // `00023` 内建 15 个全带（2.1.260 时 fable 不带）。
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainSonnet,
        version: "2.1.277",
        // `cap/2.1.277/00031`（会话首轮，`thread: create`）。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               fallback-credit-2026-06-01,thinking-binding-controls-2026-08-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_270,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainHaiku,
        version: "2.1.277",
        // `cap/2.1.277/00046`（会话首轮）。haiku 不发 `mid-conversation-*` / `effort`，
        // `claude-code` 仍在第 5 位；续轮（`00050`、`00349`）连 `claude-code` 都不带。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               fallback-credit-2026-06-01,thinking-binding-controls-2026-08-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        // `{"budget_tokens":31999,"type":"enabled","display":"updates"}`，`max_tokens` 32000。
        thinking: CcThinking::EnabledUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_270,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        // haiku 主线程没有 `output_config`。
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::SdkSubagentHaiku,
        version: "2.1.277",
        // `cap/2.1.277/00049`（子代理首轮，`thread: create`）：相对主线程 haiku 少
        // `fallback-credit` 与 `extended-cache-ttl`，多不了什么；比 2.1.260 的子代理多了
        // `advisor-tool` / `advanced-tool-use` / `thinking-binding-controls` / `message-threads`。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
               cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        subagent: true,
        system: CcSystemShape::Identity,
        thinking: CcThinking::EnabledUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_270,
        // `00049` 的 8 个真工具全带。
        eager_tools: CcEagerTools::On,
        request_class: "subagent",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::SessionTitleHaiku,
        version: "2.1.277",
        // `cap/2.1.277/00022`：相对 2.1.260 去掉了 `fallback-credit`。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
               structured-outputs-2025-12-15,cache-diagnosis-2026-04-07",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Disabled,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        eager_tools: CcEagerTools::Unknown,
        request_class: "auxiliary",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::QuotaProbe,
        version: "2.1.277",
        // `cap/2.1.277/00005`，与 2.1.260 / 2.1.270 逐字相同。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05",
        subagent: false,
        system: CcSystemShape::None,
        thinking: CcThinking::Absent,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_QUOTA,
        eager_tools: CcEagerTools::Unknown,
        request_class: "auxiliary",
        effort: None,
    },
];

/// **2.1.277 还缺的抓包**（`cap/2.1.277` 有四族主线程、SDK 子代理、标题生成、额度探测，以及
/// 三十多条 message-threads 续轮）。
///
/// 1. **API-key 端**任何一族——不知道 API-key 端的 2.1.277 发不发 `mid-conversation-system-clear-at`
///    / `thinking-binding-controls` / `message-threads`、发不发 `cc_turn_origin`、走不走 thread
///    续轮，[`crate::proxy::merge_beta_for`] 因此对 2.1.277 只沿用 2.1.258 那五项的补法，参照串换成
///    [`CC_PROFILES`]；`cc_turn_origin` 只在模拟路径写。
/// 2. 无工具 helper 与安全分类——没有 2.1.277 样本，[`cc_profile`] 落回 2.1.260 那两行。
/// 3. 2.1.277 的子代理 beta 带 `advisor-tool` / `advanced-tool-use`，
///    [`crate::proxy::is_official_non_main_beta`] 那条「有 display-updates 却没有主线程标记」的
///    子代理判据认不出它，一条 API-key 端的 2.1.277 子代理会被当主线程补齐——补进去的是
///    `fallback-credit` 与 `extended-cache-ttl`。抓到 API-key 端子代理样本前不改判据。
pub mod cc_2_1_277_missing_samples {}
