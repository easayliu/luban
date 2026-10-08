//! 2.1.291 的 profile 表（2.1.291 ~ 2.1.292 来访的 beta 参照；模拟路径已换到 2.1.293），及这一版还缺的抓包。

use super::*;

/// 2.1.291 的 profile 表——2.1.291 ~ 2.1.292 来访的 beta 参照（[`cc_profile_at`]），也是 2.1.293
/// 表没编的 kind 的兜底。模拟路径已换到 2.1.293（[`CC_PROFILES`]）。beta 串逐字取自 `cap/auto-2.1.291-20261006-full`，去掉 `oauth`。
///
/// 这一版的四族主线程按**默认权限模式**的抓包定（`00340` opus-5-5、`00253` sonnet-5-5、`00303`
/// haiku-4.5，三条都不带 `safeguards`；同批 auto 会话里进规划模式的 `00163` 也是这串）：模拟请求从来不写 `safeguards` / `afk-mode`，遥测报的
/// 权限模式也是 `default`（`crate::telemetry` 从请求体判），而 `dangerous-tool-use` 只跟 auto 模式
/// 走——同一批抓包里 auto 模式的主线程（`00032`、`00395`、`00464`、`00514`）都带它和 `afk-mode`，
/// 默认模式的一条都不带。2.1.285 表取的是 auto 模式的串（[`CC_PROFILES_2_1_285`]），带着
/// `dangerous-tool-use` 却没有 `afk-mode`，是官方不产生的组合，这一版顺带改正。
///
/// | 模型 | 抓包 | 权限模式 | effort | `max_tokens` | 内建工具 |
/// |---|---|---|---|---:|---|
/// | opus-5-5 | `00340`（`00216` 是 1M） | default | high | 128000 | 14 + ToolSearch，[`crate::proxy::cc_tools_core`] |
/// | sonnet-5-5 | `00253` | default | medium¹ | 128000 | 同 opus |
/// | fable-5-1 | `00464` | auto² | high | 64000 | 同 opus（Bash 是 auto 那版） |
/// | haiku-4.5 | `00303`、`00553` | default | — | 32000 | 长描述那一套，见 [`CC_SYSTEM_BASE_HAIKU`] |
///
/// ¹ 官方默认 medium；模拟路径三族一律按 high 发，理由同 [`CC_PROFILES_2_1_285`]。
/// ² fable 这一版没抓到默认模式的主线程。它在 auto 模式下的串与 opus 逐字相同（`00464` ↔ `00032`），
///   `-p`（`00809` ↔ `00695`）也逐字相同，故默认模式按 opus 那条（`00340`）记。
///
/// 相对 2.1.285（[`CC_PROFILES_2_1_285`]）改了什么：
///
/// - **beta**：sonnet-5-5 多了 `mid-conversation-tool-changes`（2.1.285 时三代 sonnet 都没有），
///   与 opus / fable 成了同一串；四族都去掉了 `dangerous-tool-use`（见上，模式问题，不是这一版
///   才变）。标题生成、无工具 helper、额度探测与 2.1.285 的默认模式抓包逐字相同；
/// - **请求头**：`X-Stainless-Package-Version` 0.127.0 → 0.128.0；同一轮里工具续轮的主线程与
///   子代理请求多了 `anthropic-usage-limit: extended`，见 [`CC_USAGE_LIMIT_HEADER`]；
/// - **system**：opus / sonnet 的基座不变，第四块记忆一节去掉了 `<cc-memory>` 引用写法那句；
///   fable 第四块换成另一版（多了 Fable 自我介绍、`# Delivering work`、`# Writing for the user`，
///   见 [`CC_SYSTEM_REST_FABLE`]）；haiku 整套换成长版——基座 11050 字节、第四块 17172 字节，
///   工具描述也是长版（见 [`CC_SYSTEM_BASE_HAIKU`]）。四族第四块末尾都多了一段 `WebSearch takes
///   a mode` 的说明（模拟路径不注 WebSearch，不收，理由同 EndConversation 那段）；
/// - **工具**：`Artifact` 改了一句（34386 → 34399 字节），`Bash` 默认模式那版 3303 → 3018；
///   延迟池里的 `WebSearch` / `WebFetch` 也改了（模拟路径不注它们）。
pub const CC_PROFILES_2_1_291: &[CcProfile] = &[
    CcProfile {
        kind: CcProfileKind::MainOpus,
        version: "2.1.291",
        // `cap/auto-2.1.291-20261006-full/00340`（opus-5-5，默认模式，200K）；1M 会话（`00216`）只在
        // `claude-code` 后面多一个 `context-1m`，见 [`crate::proxy::simulated_beta`]。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
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
        version: "2.1.291",
        // 与 opus 同一串，见表头注 ²。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
               message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        // 官方形态里仍没有 `fallbacks`；字面量只给默认关的 `fable_refusal_fallback` 开关用。
        fallbacks: Some(r#"[{"model":"claude-opus-5"}]"#),
        body_key_order: CC_BODY_ORDER_MAIN_2_1_280,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainSonnet,
        version: "2.1.291",
        // `cap/auto-2.1.291-20261006-full/00253`（sonnet-5-5，默认模式）：比 2.1.285 多了
        // `mid-conversation-tool-changes`，与 opus 逐字相同。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
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
        version: "2.1.291",
        // `cap/auto-2.1.291-20261006-full/00303`（默认模式），与 `00553`（auto 模式会话里的 haiku——
        // haiku 没有 auto 模式可用，同样不带 `safeguards`）逐字相同。
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
        version: "2.1.291",
        // `cap/auto-2.1.291-20261006-full/00063`（WebFetch 之后的无工具 helper），与 2.1.285 那行逐字相同。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
               dangerous-tool-use-2026-09-03,cache-diagnosis-2026-04-07,\
               message-threads-2026-08-12",
        subagent: true,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Disabled,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        eager_tools: CcEagerTools::Unknown,
        request_class: "auxiliary",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::SessionTitleHaiku,
        version: "2.1.291",
        // `cap/auto-2.1.291-20261006-full/00031` 等 13 条会话标题生成，逐字相同，与 2.1.285 默认模式
        // 那几条（`cap/auto-2.1.285-20260930/00030`、`00234`）也相同：不带 `dangerous-tool-use` 与
        // `message-threads`。2.1.285 表那行取自 auto 模式会话里的 `cap/2.1.285/00038`，带着这两项；
        // 规划模式收尾的会话起名（`00151`）仍带它们。
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
        version: "2.1.291",
        // `cap/auto-2.1.291-20261006-full/00018`，与 2.1.260 ~ 2.1.285 逐字相同。
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

/// **2.1.291 还缺的抓包**（`cap/auto-2.1.291-20261006-full`：auto / default / plan 权限模式、
/// `--continue`、各档 effort、1M、四族主线程、Explore / general-purpose / Plan 子代理、
/// WebSearch / WebFetch、`/compact`、`-p`；`cap/auto-2.1.291-20261006`：四族主线程用环境变量
/// 关掉 Artifact / ListAgents / SendFeedback 前后各一条）。
///
/// 1. **fable 的默认权限模式主线程**——按 opus 那串记（见 [`CC_PROFILES_2_1_291`] 表头注 ²）。
/// 2. **SDK 子代理（claude-code-guide）**——这一版没触发；Explore 子代理在 haiku 主线程下
///    （`00558`）比 2.1.285 的 claude-code-guide 多 `advanced-tool-use` 与 `extended-cache-ttl`
///    以外的项都一样，但不是同一种子代理，[`cc_profile_at`] 对 `SdkSubagentHaiku` 仍落回 2.1.285。
/// 3. 安全分类与 API-key 端任何一族，理由同 [`cc_2_1_280_missing_samples`]。
pub mod cc_2_1_291_missing_samples {}
