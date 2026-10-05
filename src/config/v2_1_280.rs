//! 2.1.280 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.280 的 profile 表：2.1.280 ~ 2.1.284 来访的 beta 参照（[`cc_profile_at`]）。beta 串逐字取自
/// `cap/2.1.280`，去掉 `oauth` 与动态的 `afk-mode`。模拟路径已换到 2.1.285（[`CC_PROFILES`]）。
///
/// **按非 auto 权限模式那段取**（`00065` 之后）：模拟的是一个非 auto 会话。同一会话前半段是
/// auto 模式（`00021` ~ `00046`），opus / fable / sonnet 在那段多一个动态的 `afk-mode`、体里多
/// `safeguards`，却**少**队尾的 `message-threads`；其余每一项逐字相同。
///
/// | profile | 抓包（非 auto / auto） | system | tools | thinking | effort | class |
/// |---|---|---|---:|---|---|---|
/// | `MainOpus` | `00065` / `00021`（opus-5-5） | 4 块 | 20 | adaptive+updates | medium | main |
/// | `MainFable` | `00068` / `00029` | 4 块 | 19 | adaptive+updates | high | main |
/// | `MainSonnet` | `00073` / `00033` | 4 块 | 19 | adaptive+updates | high | main |
/// | `MainHaiku` | `00038`（`thread: create`） | 4 块 | 19 | enabled+updates | — | main |
/// | `QuotaProbe` | `00008` | 无 | — | 无 | — | auxiliary |
///
/// 相对 2.1.277（[`CC_PROFILES_2_1_277`]）改了什么，四族主线程逐条核过：
///
/// - **beta**：四族新增 [`cc_beta_dangerous_tool_use`]，四族都不再发 `fallback-credit`（新项正好
///   占它那一格）；opus 也带上了 `per-turn-control`（2.1.277 只有 fable 带）；`message-threads`
///   非 auto 模式下四族都在队尾（2.1.277 的 fable 不带），auto 模式下只剩 haiku；体里的
///   `thread` 只有 sonnet / haiku 的首轮写 `create`，opus / fable 带 beta 却不写 `thread`；
/// - **体**：auto 模式下 opus / fable / sonnet 主线程顶层多一个 `safeguards`（危险工具分类上下文，
///   键序见 [`CC_BODY_ORDER_MAIN_2_1_280`]），非 auto 模式不发，模拟也不造；opus 的
///   `output_config.effort` 是 `medium`（两种模式都是，同一会话 fable / sonnet 仍是 `high`，
///   故不是用户调了全局 effort）；
/// - **非主线程的 fable 请求**（`00070`，`request-class: auxiliary`，带工具、21 条消息）顶层有
///   `fallbacks: "default"`（字符串，不是 2.1.260 那种数组），头上随之多
///   `server-side-fallback-2026-07-01` 与 `fallback-credit-2026-06-01`。模拟路径不发这类请求，
///   没编行；[`crate::proxy::merge_beta_for`] 的非主线程判据认不认它见 [`cc_2_1_280_missing_samples`]；
/// - **system**：基座 1588 字节与 2.1.277 逐字节相同；第四块整段换了（[`CC_SYSTEM_REST`]），
///   四族仍是同一份；
/// - **工具**：内建 14 个里只有 `Artifact` 的描述改了一句，其余 13 个逐字节相同；服务端工具
///   `advisor` 这回只有 opus 带（fable 不带了），模拟照旧不注；
/// - **billing header**：后缀是 `bc5`，四族相同——但那不是 profile 固有值，而是
///   [`crate::proxy::cc_version_suffix`] 对会话首条输入（`hilew`）算出来的，见那个函数；
/// - 请求头除 UA 外逐字节相同，`x-claude-code-request-class` 照旧。
///
/// SDK 子代理、标题生成、无工具 helper 与安全分类在 2.1.280 里没有样本，不编行：
/// [`cc_profile`] 依次落回 [`CC_PROFILES_2_1_277`]、[`CC_PROFILES_2_1_260`]。模拟路径只发
/// 主线程四族，不受影响。
pub const CC_PROFILES_2_1_280: &[CcProfile] = &[
    CcProfile {
        kind: CcProfileKind::MainOpus,
        version: "2.1.280",
        // `cap/2.1.280/00065`（opus-5-5，1M 上下文，非 auto 模式）；auto 模式的 `00021` 少队尾的
        // `message-threads`、多 `afk-mode`，其余逐字相同。
        beta: "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_280,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        // `00021`（auto）与 `00065`（非 auto）都是 `{"effort":"medium"}`；两条都是 opus-5-5——
        // opus-5 在 2.1.280 上是不是也是 medium 没有证据，按族取这一个值。
        effort: Some("medium"),
    },
    CcProfile {
        kind: CcProfileKind::MainFable,
        version: "2.1.280",
        // `cap/2.1.280/00068`（非 auto）：与 opus 那串只差一个 `context-1m`。auto 模式的 `00029`
        // 同样少 `message-threads`。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        // 官方形态里仍没有 `fallbacks`；字面量只给默认关的 `fable_refusal_fallback` 开关用，
        // 理由同 [`CC_PROFILES_2_1_277`] 那一行。
        fallbacks: Some(r#"[{"model":"claude-opus-5"}]"#),
        body_key_order: CC_BODY_ORDER_MAIN_2_1_280,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainSonnet,
        version: "2.1.280",
        // `cap/2.1.280/00073`（非 auto，`thread: create`）：没有 `per-turn-control` /
        // `mid-conversation-tool-changes`。auto 模式的 `00033` 少 `message-threads`。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
               dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07,message-threads-2026-08-12",
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
        version: "2.1.280",
        // `cap/2.1.280/00038`（haiku 这一段的首轮，`thread: create`）。`claude-code` 仍在第 5 位；
        // 它是四族里唯一在 auto 模式下也带 `message-threads` 的（haiku 不走 auto 模式）。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        subagent: false,
        system: CcSystemShape::Identity,
        // `{"budget_tokens":31999,"type":"enabled","display":"updates"}`，`max_tokens` 32000。
        thinking: CcThinking::EnabledUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_280,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::QuotaProbe,
        version: "2.1.280",
        // `cap/2.1.280/00008`，与 2.1.260 ~ 2.1.277 逐字相同。
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

/// **2.1.280 还缺的抓包**（`cap/2.1.280` 有四族主线程——opus / fable / sonnet 各有 auto 与非 auto
/// 两种、haiku 只有 auto 段——、fable / sonnet / haiku 的 auxiliary 带工具请求、一条 sonnet thread
/// 续轮与额度探测）。
///
/// 1. SDK 子代理、标题生成、无工具 helper、安全分类——[`cc_profile`] 落回 2.1.277 / 2.1.260
///    那几行，beta 串有没有也长出 `dangerous-tool-use` 没有证据。
/// 2. opus-5（非 5.5）的主线程——`MainOpus` 的 `effort: medium` 只来自 opus-5-5 那一条。
/// 3. 非 auto 模式下的 haiku 主线程——表里那行取自 auto 段的 `00038`；haiku 在 auto 段本来就
///    不带 `afk-mode` / `safeguards`、带 `message-threads`，与其余三族非 auto 段的形态一致，
///    预计相同，但没抓到。
/// 4. API-key 端任何一族，理由同 [`cc_2_1_277_missing_samples`] 第 1 条。
/// 5. auxiliary 那几条（`00039`、`00046` haiku，`00070` fable，`00077` sonnet）的用途不明——
///    看着是主线程的分叉（同样的 system 与工具、更长的消息，billing 没有 `cc_prompt_id`），
///    没编 profile；fable 那条带 `fallbacks: "default"` 与 `server-side-fallback`，真 CC 来访时
///    [`crate::proxy::merge_beta_for`] 按主线程处理（它有 `effort` / `advisor-tool`），参照串里没有
///    这两项、也就不补不删，原样透传。
pub mod cc_2_1_280_missing_samples {}
