//! 2.1.258 的 profile 表（低于 2.1.260 与读不出版本时用）。

use super::*;

/// 2.1.258 的主线程四族 profile，**给真实 2.1.258 来访用**。
///
/// 留着它不是为了怀旧：[`crate::proxy::merge_beta_for`] 要按来访**自报的版本**决定补哪几项。
/// 拿 2.1.260 那张表去处理一个自报 2.1.258 的客户端，会给它补上 `thinking-display-updates`
/// 并剥掉 `redact-thinking`——那是 2.1.260 才有的形态，拼在一条 2.1.258 的请求上就是
/// 「同一条请求里混了两个版本」，比不补更容易被认出来。
///
/// 相对 2.1.260 的三处差异（就是这一版升级改的那三样）：
/// - opus / sonnet / haiku 发 `redact-thinking`、不发 `thinking-display-updates`；
/// - `server-side-fallback` 是 `2026-07-01`，且 opus 也发；
/// - fable 发的是 `advisor-tool` 而不是 `per-turn-control`。
///
/// 后缀四族统一 `1e2`（`cap/2.1.258` 五份对话抓包全是这个值）。只列主线程四族：2.1.258
/// 没有抓到辅助请求的样本，编不出来的行就不编。
pub const CC_PROFILES_2_1_258: &[CcProfile] = &[
    CcProfile {
        kind: CcProfileKind::MainOpus,
        version: "2.1.258",
        // `cap/2.1.258/00025`（opus-5 直连，无 afk-mode 的那次）。
        beta: "claude-code-20250219,context-1m-2025-08-07,\
               interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
               server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Adaptive,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        // 2.1.258 四族 OAuth 主线程全带（`cap/2.1.258/00012` / `00013` / `00026` / `00031`）。
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::MainFable,
        version: "2.1.258",
        // `cap/2.1.258/00013`（fable-5-1 直连）。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
               server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        subagent: false,
        system: CcSystemShape::IdentityReporting,
        thinking: CcThinking::AdaptiveUpdates,
        // 2.1.258 发的是字符串 `"default"`（`cap/2.1.258/00013`）。
        fallbacks: Some(r#""default""#),
        body_key_order: CC_BODY_ORDER_MAIN,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::MainSonnet,
        version: "2.1.258",
        // `cap/2.1.258/00026`（sonnet-5 直连）。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
               advanced-tool-use-2025-11-20,effort-2025-11-24,\
               server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Adaptive,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::MainHaiku,
        version: "2.1.258",
        // `cap/2.1.258/00031`（haiku-4.5 直连）。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,claude-code-20250219,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Enabled,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: None,
    },
];
