//! 2.1.285 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.285 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.285 来访的
/// beta 参照（[`cc_profile_at`]）。beta 串逐字取自 `cap/2.1.285`，去掉 `oauth` 与动态的 `afk-mode`。
///
/// 这批抓包是**同一个会话里用 `/model` 轮流切了 11 个模型**，每个模型一轮主线程：
///
/// | 模型 | 抓包 | 代际（[`CcModelTier`]） | effort | `max_tokens` | 工具 |
/// |---|---|---|---|---:|---:|
/// | opus-5-5 | `00030` | `Latest` | high¹ | 128000 | 20（含 `advisor`） |
/// | fable-5-1 | `00039` | `Latest` | high | 64000 | 19 |
/// | sonnet-5-5 | `00045` | `Latest` | medium | 128000 | 19 |
/// | haiku-4.5 | `00051` | — | — | 32000 | 19 |
/// | sonnet-5 | `00055` | `Gen5` | high | 64000 | 19 |
/// | opus-5 | `00061` | `Gen5` | high | 64000 | 20（含 `advisor`） |
/// | fable-5 | `00067` | `Gen5` | high | 64000 | 19 |
/// | opus-4-8 | `00072` | `Gen5` | high | 64000 | 20（含 `advisor`） |
/// | opus-4-7 | `00077` | `Legacy` | high | 64000 | 19 |
/// | opus-4-6 | `00083` | `Legacy` | high | 64000 | 19 |
/// | sonnet-4-6 | `00088` | `Legacy` | high | 32000 | 19 |
///
/// ¹ 抓包时用户自己调成了 high（默认是 medium，同 2.1.280 的 `00021` / `00065`）。模拟路径
/// 四族一律按 high 发，见 [`CcProfile::effort`]。
///
/// **同一族内 beta 按模型代际不同**，这是这一版头一回有老模型的样本才看出来的：每族取最新
/// 那一代的串当全集（下表各行），老一代在它上面按 [`CcModelTier`] 去掉几项
/// （[`cc_model_beta`]），11 个模型逐字节还原。其余每一项——thinking（四族非 haiku 全是
/// adaptive + updates）、`system` 四块（基座、第四块 sha256 全部相同）、14 个内建工具（逐字节
/// 相同、全带 eager）、`context_management`、键序——**不随模型变**。
///
/// 相对 2.1.280（[`CC_PROFILES_2_1_280`]）改了什么：
///
/// - **beta**：四族最新一代的串与 2.1.280 逐字相同；新增 sonnet-5-5，它比 sonnet-5 多一个
///   `per-turn-control`（仍没有 `mid-conversation-tool-changes`）。auto 模式下 `message-threads`
///   也在队尾了（2.1.280 的 auto 段不带）；
/// - **请求头**：每条 messages 多一个 `anthropic-dispatch-id: v2d`（[`CC_DISPATCH_ID`]），额度探测
///   不带；带 `cc_prompt_id` 的请求另多一个同值的 `x-claude-code-prompt-id`（[`CC_HEADER_ORDER`]）；
///   `X-Stainless-Package-Version` 从 0.112.1 升到 0.127.0；
/// - **billing header**：`cc_turn_origin` 之后多 `cc_prompt_index=N; cc_turn_index=N;`，见
///   [`crate::proxy::simulated_billing_header_text`]；
/// - **system**：基座不变；第四块的记忆一节整段换了写法（`# auto memory` 改回 `# Memory`，
///   frontmatter 换成 `metadata.type`、去掉 `pinned`），模型列表里 Sonnet 5 换成 Sonnet 5.5；
/// - **工具**：`Bash` 与 `Artifact` 的描述 / schema 各改了几句，其余 12 个逐字节相同；
/// - **标题生成**多了 `dangerous-tool-use` 与队尾的 `message-threads`（`00038`）。
///
/// 第二批抓包（`00097` 起，一段带工具与子代理的会话）另补了三类：SDK 子代理（claude-code-guide，
/// `00120`）、无工具 helper（`00125` 等）两行编进表里；子代理的进度摘要（`00135`、`00147`，
/// `auxiliary`、没有 `claude-code` beta）与主线程分叉的 auxiliary（`00097`、`00158`，后者带
/// `fallbacks: "default"` 与 `server-side-fallback` / `fallback-credit`）没有对应的 kind，
/// 透传时由 [`crate::proxy::merge_beta_for`] 原样留着（前者不带 `claude-code`、后者参照串里
/// 没有那两项，都不补不删）。安全分类仍没有样本，[`cc_profile`] 落回 2.1.260 表。模拟路径只发
/// 主线程四族，不受影响。
pub const CC_PROFILES: &[CcProfile] = &[
    CcProfile {
        kind: CcProfileKind::MainOpus,
        version: "2.1.285",
        // opus 族全集：`cap/2.1.285/00030`（opus-5-5，auto 模式，去掉 `afk-mode`）与 `00113`（非 auto）
        // 逐字相同。不带 `context-1m`：2.1.285 的 opus 各代（`00030`、`00061`、`00072`、`00077`、
        // `00083`、`00113`）都是默认的 200K 上下文，一条都没有它；2.1.280 的 `00065` 带它是因为
        // 那个会话选了 1M。第三方来访自己带了 `context-1m` 的，[`crate::proxy::simulated_beta`]
        // 把它插回官方位置（`oauth` 之后，`cap/auto-2.1.285-20260930/00235`）。
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
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_280,
        eager_tools: CcEagerTools::On,
        request_class: "main",
        // 官方默认 medium（2.1.280 的 opus-5-5），`00030` 的 high 是用户调的；模拟路径按 high 发。
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainFable,
        version: "2.1.285",
        // fable 族全集：`cap/2.1.285/00039`（fable-5-1），与 2.1.280 的 `00068` 逐字相同。
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
        version: "2.1.285",
        // sonnet 族全集：`cap/2.1.285/00045`（sonnet-5-5，这一版新出现）。比 sonnet-5（`00055`，与
        // 2.1.280 的 `00073` 逐字相同）多一个 `per-turn-control`；两代都没有
        // `mid-conversation-tool-changes`。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,advisor-tool-2026-03-01,\
               advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
               effort-2025-11-24,dangerous-tool-use-2026-09-03,\
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
        // sonnet-5-5 的 `00045` 是 medium（默认值），sonnet-5 等老一代是 high；模拟路径按 high 发。
        effort: Some("high"),
    },
    CcProfile {
        kind: CcProfileKind::MainHaiku,
        version: "2.1.285",
        // `cap/2.1.285/00051`，与 2.1.280 的 `00038` 逐字相同。
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
        kind: CcProfileKind::SdkSubagentHaiku,
        version: "2.1.285",
        // `cap/2.1.285/00120`（claude-code-guide 子代理首轮，`thread: create`；续轮 `00127` 等五条
        // beta 逐字相同）：相对 2.1.277（`00049`）少了 `advanced-tool-use`，也没长出主线程那个
        // `dangerous-tool-use`。billing 只有 `cc_is_subagent` / `cc_prev_req` / `cc_prompt_id`，
        // 不写 `cc_turn_origin` 与轮次。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               claude-code-20250219,advisor-tool-2026-03-01,thinking-binding-controls-2026-08-01,\
               thinking-display-updates-2026-08-18,cache-diagnosis-2026-04-07,\
               message-threads-2026-08-12",
        subagent: true,
        system: CcSystemShape::Identity,
        thinking: CcThinking::EnabledUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN_2_1_270,
        // `00120` 的 4 个工具（Bash / Read / WebFetch / WebSearch）全带。
        eager_tools: CcEagerTools::On,
        request_class: "subagent",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::HelperSubagentHaiku,
        version: "2.1.285",
        // `cap/2.1.285/00125`、`00134`、`00140`、`00144`（子代理 WebFetch 之后的无工具 helper，
        // 四条 beta 逐字相同）：2.1.260 之后头一回有样本。相对 2.1.260（`00024`）去掉了
        // `server-side-fallback` / `fallback-credit`，多了 `advisor-tool` / `dangerous-tool-use` /
        // `message-threads`。两块 system（子代理 billing + SDK 身份句），`temperature: 1`，头上
        // `x-claude-code-request-class` 是 `auxiliary`（不是 `subagent`）。
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
        version: "2.1.285",
        // `cap/2.1.285/00038`：相对 2.1.277（`00022`）多了 `dangerous-tool-use` 与队尾的
        // `message-threads`（体里仍没有 `thread`）。三块 system，`temperature: 1`。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
               structured-outputs-2025-12-15,dangerous-tool-use-2026-09-03,\
               cache-diagnosis-2026-04-07,message-threads-2026-08-12",
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
        version: "2.1.285",
        // `cap/2.1.285/00017`，与 2.1.260 ~ 2.1.280 逐字相同；这一条不带 `anthropic-dispatch-id`。
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

/// **2.1.285 还缺的抓包**（`cap/2.1.285` 是一个会话里 `/model` 切了 11 个模型，每个模型一轮
/// 主线程，外加标题生成与额度探测；一次工具调用都没有）。
///
/// 1. ~~工具续轮~~——第二批抓包补上了：thread 续轮（`00115`、`00121`）照写
///    `cc_prompt_id; cc_turn_origin=human; cc_prompt_index=N; cc_turn_index=N;`，N 不加；后台任务
///    通知那一轮（`00149`）写 `cc_turn_origin=task_notification`，`cc_prompt_index` 不加、
///    `cc_turn_index` 加一。模拟路径每轮都是 human，照前者。
/// 2. **1M 上下文的 opus 会话**——这次 opus 各代都没带 `context-1m`，模拟路径跟着不带；1M 会话里
///    它落在 `oauth` 之后（`cap/2.1.280/00065`），模拟路径对来访自带的那项仍是追加在队尾。
/// 3. **非 auto 模式**——opus-5-5 / fable-5-1 / sonnet-5-5 三条是 auto 模式（`afk-mode`、
///    `safeguards`），其余八个模型没有 auto 模式可选。2.1.280 已证非 auto 与 auto 只差这两样。
/// 4. 安全分类与 API-key 端任何一族，理由同 [`cc_2_1_280_missing_samples`]。SDK 子代理与无工具
///    helper 第二批抓包补上了（[`CC_PROFILES`]）。
/// 5. 更老的模型（opus-4-5、sonnet-4-5 及以前）——[`cc_model_tier`] 把它们归进 `Legacy`，
///    是按 4.6 那一代外推的。
pub mod cc_2_1_285_missing_samples {}
