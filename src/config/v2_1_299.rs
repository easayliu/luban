//! 2.1.299 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.299 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.299 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// 相对 2.1.296（[`CC_PROFILES_2_1_296`]）**只改了 beta**：`mid-conversation-system-2026-04-07`
/// 转为正式能力，客户端不再发这个头（可执行文件里的注册项改成 `header: null`，`ANTHROPIC_BETAS` 里
/// 写了也会被丢掉）。依据 `cap/auto-2.1.299-20261014-full`（场景同 2.1.296-full）：全部 `/v1/messages`
/// 的 `anthropic-beta` 共 24 种，正好是 2.1.296 那 24 种各去掉这一项，别的项与顺序都没变。不含它的
/// 三行（haiku-4.5 主线程、额度探测、预热）逐字未变。
///
/// 其余与 2.1.296 相同：system 各块、首轮环境说明、顶层键序、effort 与 billing header 的字段组合都
/// 逐字一致。工具只有 `Agent` 的 cerf 那段说明改了措辞，见 [`crate::proxy::cc_tools_core_for`]。
///
/// 2.1.297 / 2.1.298 官方没发布（发布清单里没有这两个版本）。
pub const CC_PROFILES: &[CcProfile] = &[
    CcProfile {
        kind: CcProfileKind::MainOpus,
        version: "2.1.299",
        // `cap/auto-2.1.299-20261014-full/00418`（opus-5-5，默认模式）；与 2.1.296 那串只差去掉了
        // `mid-conversation-system`。auto 模式（`00160`）与 sonnet（`00246`）同一串。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,per-turn-control-2026-07-01,\
               mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
               message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_280,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainFable,
        version: "2.1.299",
        // 与 opus 同一串，同 [`CC_PROFILES_2_1_293`] 表头注 ²。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,per-turn-control-2026-07-01,\
               mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
               message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: Some(r#"[{"model":"claude-opus-5"}]"#),
        body_key_order: CC_BODY_ORDER_MAIN_2_1_280,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainSonnet,
        version: "2.1.299",
        // `cap/auto-2.1.299-20261014-full/00246`（sonnet-5-5，默认模式），与 opus 逐字相同。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,per-turn-control-2026-07-01,\
               mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
               message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_280,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainHaiku55,
        version: "2.1.299",
        // `cap/auto-2.1.299-20261014-full/00294`（`--model haiku`，默认模式）、`00329`（`--model
        // claude-haiku-5-5`）。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               claude-code-20250219,per-turn-control-2026-07-01,\
               mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
               message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_280,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainHaiku,
        version: "2.1.299",
        // `cap/auto-2.1.299-20261014-full/00364`（`--model claude-haiku-4-5`），与 2.1.293 起逐字相同。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
               message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::EnabledUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_280,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::HelperSubagentHaiku,
        version: "2.1.299",
        // `cap/auto-2.1.299-20261014-full/00052`（WebSearch 子调用）、`00061`（WebFetch 之后的 helper）。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,per-turn-control-2026-07-01,\
               mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,\
               advisor-tool-2026-03-01,mid-conversation-system-clear-at-2026-08-21,\
               effort-2025-11-24,dangerous-tool-use-2026-09-03,cache-diagnosis-2026-04-07,\
               message-threads-2026-08-12",
        subagent: true,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Absent,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        eager_tools: CcEagerTools::Unknown,
        request_class: "auxiliary",
        effort: Some("medium"),
    },
    CcProfile {
        kind: CcProfileKind::SessionTitleHaiku,
        version: "2.1.299",
        // `cap/auto-2.1.299-20261014-full/00029` 等 16 条会话标题生成，逐字相同。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,per-turn-control-2026-07-01,\
               mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,\
               advisor-tool-2026-03-01,mid-conversation-system-clear-at-2026-08-21,\
               effort-2025-11-24,structured-outputs-2025-12-15,cache-diagnosis-2026-04-07",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Absent,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        eager_tools: CcEagerTools::Unknown,
        request_class: "auxiliary",
        effort: Some("medium"),
    },
    CcProfile {
        kind: CcProfileKind::QuotaProbe,
        version: "2.1.299",
        // `cap/auto-2.1.299-20261014-full/00014` 等 16 条，与 2.1.260 起逐字相同。
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
    CcProfile {
        kind: CcProfileKind::Prewarm,
        version: "2.1.299",
        // `cap/auto-2.1.299-20261014-model/00030`（opus）、`00033`（sonnet）、`00037`（fable）三条逐字相同，
        // 与 2.1.296 同串；haiku-5-5（`00042`）少 `claude-code`，`[1m]` 那条（`00039`）多 `context-1m`。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               redact-thinking-2026-02-12,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Absent,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_PREWARM,
        eager_tools: CcEagerTools::Unknown,
        request_class: "auxiliary",
        effort: None,
    },
];

/// **2.1.299 还缺的抓包**：同 [`cc_2_1_293_missing_samples`]——fable 默认权限模式主线程、haiku-5-5
/// 的 1M 会话、SDK 子代理、安全分类与 API-key 端。
pub mod cc_2_1_299_missing_samples {}
