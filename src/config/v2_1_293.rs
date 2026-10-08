//! 2.1.293 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.293 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.293 来访的
/// beta 参照（[`cc_profile_at`]）。beta 串逐字取自 `cap/auto-2.1.293-20261008-full`，去掉 `oauth`。
///
/// 与 2.1.291 一样按**默认权限模式**的抓包定（不带 `safeguards` / `afk-mode` / `dangerous-tool-use`）：
///
/// | 模型 | 抓包 | 权限模式 | effort | `max_tokens` | 内建工具 |
/// |---|---|---|---|---:|---|
/// | opus-5-5 | `00419`（`00219` 是 1M） | default | high | 128000 | 14 + ToolSearch，[`crate::proxy::cc_tools_core`] |
/// | sonnet-5-5 | `00256` | default | medium¹ | 128000 | 同 opus |
/// | fable-5-1 | `00546` | auto² | high | 64000 | 同 opus（Bash 是 auto 那版） |
/// | haiku-5-5 | `00344`（`00305` 是 `--model haiku`） | default | medium¹ | 128000 | 同 opus |
/// | haiku-4.5 | `00383` | default | — | 32000 | 长描述那一套，见 [`CC_SYSTEM_BASE_HAIKU`] |
///
/// ¹ 官方默认 medium；模拟路径一律按 high 发，理由同 [`CC_PROFILES_2_1_285`]。
/// ² fable 这一版仍没抓到默认模式的主线程，auto 模式下的串与 opus 逐字相同（`00546` ↔ `00032`），
///   默认模式按 opus 那条（`00419`）记，同 2.1.291。
///
/// 相对 2.1.291（[`CC_PROFILES_2_1_291`]）改了什么：
///
/// - **haiku-5-5 新出现，成了 `haiku` 别名指向的模型**（可执行文件 `haiku:{default:"claude-haiku-5-5"}`，
///   `--model haiku` 起的会话 `00305` 就是它）。它不走 haiku-4.5 那套长基座 / 长工具描述，而是与
///   opus / sonnet 同一套短基座与工具（`00344` 与 `00419` 的工具逐字节相同），`thinking` 是
///   adaptive + updates、带 `effort`、写 `thread`、`max_tokens` 128000——故单列一个
///   [`CcProfileKind::MainHaiku55`]，`MainHaiku` 仍是 haiku-4.5；
/// - **beta**：opus / sonnet / fable 在 `mid-conversation-tool-changes` 之后多了
///   [`CC_BETA_INLINE_TOOLS`]（MCP 工具改成写进首轮那条 system 消息的 `tool_addition`，不再进
///   `tools`）；haiku-4.5 主线程与额度探测逐字未变；
/// - **标题生成 / 无工具 helper / WebSearch 子调用换成 haiku-5-5**：不再发 `thinking:disabled` 与
///   `temperature`，改发 `output_config.effort`（helper、标题 medium，WebSearch high），
///   `max_tokens` 32000 → 128000，beta 多了 `mid-conversation-system` 一系、`per-turn-control`、
///   `mid-conversation-tool-changes`、`inline-tools` 与 `effort`；WebSearch 的 `tool_choice` 从强制
///   `web_search` 改成 `auto`（haiku-5-5 不收强制工具）；
/// - **system**：基座首句 `You are an interactive agent that helps users with software engineering
///   tasks.` 换成 `You are an agent working with the user toward their goals, using your own judgment
///   along the way.`（haiku-4.5 的长基座同一句）；第四块 `# Environment` 的模型列表里 `Haiku 4.5:
///   'claude-haiku-4-5-20251001'` 换成 `Haiku 5.5: 'claude-haiku-5-5'`；haiku-5-5 的第四块是 opus 那份
///   把 EndConversation 那段换成几段「effort 不改变要做完多少」的说明，见 [`CC_SYSTEM_REST_HAIKU_5_5`]；
/// - **工具**：`Agent` 多了 `effort` 参数，`Artifact` 改了一段措辞、`asset_ids` 上限 50 → 200。
pub const CC_PROFILES: &[CcProfile] = &[
    CcProfile {
        kind: CcProfileKind::MainOpus,
        version: "2.1.293",
        // `cap/auto-2.1.293-20261008-full/00419`（opus-5-5，默认模式，200K）；1M 会话（`00219`）只在
        // `claude-code` 后面多一个 `context-1m`，见 [`crate::proxy::simulated_beta`]。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               inline-tools-2026-09-15,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
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
        version: "2.1.293",
        // 与 opus 同一串，见表头注 ²。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               inline-tools-2026-09-15,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
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
        version: "2.1.293",
        // `cap/auto-2.1.293-20261008-full/00256`（sonnet-5-5，默认模式），与 opus 逐字相同。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               inline-tools-2026-09-15,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
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
        version: "2.1.293",
        // `cap/auto-2.1.293-20261008-full/00344`（`--model claude-haiku-5-5`，默认模式），与
        // `00305`（`--model haiku`）逐字相同。项与 opus 那串一样，只是 `claude-code` 不在开头、
        // 落在 `mid-conversation-system` 之后——`oauth` 于是排第一（[`crate::proxy::simulated_beta`]
        // 的落位规则：开头不是 `claude-code` 时 `oauth` 插在最前）。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               mid-conversation-system-2026-04-07,claude-code-20250219,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               inline-tools-2026-09-15,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
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
        version: "2.1.293",
        // `cap/auto-2.1.293-20261008-full/00383`（`--model claude-haiku-4-5`，默认模式），与 2.1.291
        // 那条（`00303`）逐字相同。
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
        version: "2.1.293",
        // `cap/auto-2.1.293-20261008-full/00063`（WebFetch 之后的无工具 helper，haiku-5-5）：没有
        // `thinking` 与 `temperature`，`output_config: {"effort":"medium"}`，`max_tokens` 128000。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               inline-tools-2026-09-15,advisor-tool-2026-03-01,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               dangerous-tool-use-2026-09-03,cache-diagnosis-2026-04-07,\
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
        version: "2.1.293",
        // `cap/auto-2.1.293-20261008-full/00030` 等 17 条会话标题生成（haiku-5-5），逐字相同；
        // 规划模式收尾的会话起名（`00156`）同一串。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               inline-tools-2026-09-15,advisor-tool-2026-03-01,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               structured-outputs-2025-12-15,cache-diagnosis-2026-04-07",
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
        version: "2.1.293",
        // `cap/auto-2.1.293-20261008-full/00017`，与 2.1.260 ~ 2.1.291 逐字相同（仍是 haiku-4.5）。
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

/// **2.1.293 还缺的抓包**（`cap/auto-2.1.293-20261008-full`：场景同 2.1.291，另加 haiku-5-5 的
/// 默认 / auto / `-p` 主线程、haiku-4-5 主线程与 haiku-5-5 的 Explore 子代理）。
///
/// 1. **fable 的默认权限模式主线程**——按 opus 那串记（见 [`CC_PROFILES`] 表头注 ²）；
/// 2. **haiku-5-5 的 1M 会话**——它原生 1M（`native_1m`），没抓 `[1m]` 后缀的会话；
/// 3. **SDK 子代理（claude-code-guide）**——仍没触发，[`cc_profile_at`] 对 `SdkSubagentHaiku` 落回 2.1.285；
/// 4. 安全分类与 API-key 端任何一族，理由同 [`cc_2_1_280_missing_samples`]。
pub mod cc_2_1_293_missing_samples {}
