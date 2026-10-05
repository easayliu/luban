//! 2.1.260 的 profile 表（各版本的最后兜底），及这一版还缺的抓包。

use super::*;

/// 2.1.260 的 profile 全表。beta 串逐字取自抓包，去掉 `oauth` 与 `afk-mode`。现在只做两件事：
/// 2.1.260 ~ 2.1.276 来访的 beta 参照（[`cc_profile_at`]），以及 2.1.277 表里没编的 kind
/// （无工具 helper、安全分类）的兜底（[`cc_profile`]）。
///
/// | profile | 抓包 | 后缀 | system | tools | thinking |
/// |---|---|---|---|---:|---|
/// | `MainOpus` | `2.1.260-2/00025` | `222` | 4 块 | 16 | adaptive+updates |
/// | `MainFable` | `2.1.260/00018` | `bcd` | 5 块 | 13 | adaptive+updates |
/// | `MainSonnet` | *无*（外推） | `1e2` | 4 块 | — | adaptive+updates |
/// | `MainHaiku` | *无*（外推） | `1e2` | 4 块 | — | enabled+updates |
/// | `SdkSubagentHaiku` | `2.1.260/00020` | `660` | 3 块 | 4 | enabled+updates |
/// | `HelperSubagentHaiku` | `2.1.260/00024` | `d95` | 2 块 | 0 | disabled |
/// | `SessionTitleHaiku` | `2.1.260-2/00058` | `ced` | 3 块 | 0 | disabled |
/// | `SecurityClassifierSonnet` | `2.1.260/00019` | `3de` | 3 块 | — | disabled |
/// | `QuotaProbe` | `2.1.260-2/00004` | — | 无 | — | 无 |
///
/// **`MainSonnet` / `MainHaiku` 是外推的，不是抓包。** 2.1.260 只抓到了 opus 与 fable 的
/// 主线程（[`crate::config::cc_2_1_260_missing_samples`]）。外推只用了两条在**全部六份**
/// 2.1.260 抓包上都成立的规则：
///
/// 1. `thinking-display-updates` 与 `redact-thinking` 互斥——前者在（主线程 opus/fable、
///    SDK 子代理）则后者必不在，反之亦然（helper、标题、分类）。主线程要显示思考过程，
///    故 2.1.260 的 sonnet/haiku 主线程按「有 display-updates、无 redact-thinking」推。
/// 2. `server-side-fallback` 出现时日期一律是 `2026-06-01`（fable 主线程、helper）。
///
/// 剩下那一处**没有证据**：opus 主线程在 2.1.260 整项不发 `server-side-fallback`（2.1.258
/// 时发）。sonnet/haiku 是跟着 opus 一起不发了，还是像 fable 那样留着换了日期，抓包答不
/// 上。这里取后者——它们在 2.1.258 自己就发这一项，「留着换日期」比「整项消失」离各自的
/// 上一版更近。抓到样本前这两行都不能算已证。
pub const CC_PROFILES_2_1_260: &[CcProfile] = &[
    CcProfile {
        kind: CcProfileKind::MainOpus,
        version: "2.1.260",
        // `cap/2.1.260-2/00025`：相对 2.1.258 删了 `redact-thinking` 与
        // `server-side-fallback`，加了 `thinking-display-updates`。
        beta: "claude-code-20250219,context-1m-2025-08-07,\
               interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
               advanced-tool-use-2025-11-20,effort-2025-11-24,fallback-credit-2026-06-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        // `cap/2.1.260-2/00013` 等 7 条全带。
        eager_tools: CcEagerTools::On,
        request_class: "main",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::MainFable,
        version: "2.1.260",
        // `cap/2.1.260/00018`：相对 2.1.258 把 `advisor-tool` 换成了 `per-turn-control`，
        // `server-side-fallback` 的日期从 07-01 回到 06-01。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
               server-side-fallback-2026-06-01,fallback-credit-2026-06-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        subagent: false,
        system: CcSystemShape::IdentityReporting,
        thinking: CcThinking::AdaptiveUpdates,
        // 2.1.258 时是字符串 `"default"`，2.1.260 换成了数组。**语义随之变了**：这是在替
        // 用户声明「本模型不可用时服务端改用 opus-5 跑」，模型换了计价也跟着换。故模拟
        // 路径默认**不发**它，见 [`crate::proxy::ensure_fallbacks`]——表里留着是因为它是
        // 官方形态的一部分，形态与要不要替用户拨这个开关是两件事。
        fallbacks: Some(r#"[{"model":"claude-opus-5"}]"#),
        body_key_order: CC_BODY_ORDER_MAIN,
        // `cap/2.1.260/00018` 等 3 条全不带。
        eager_tools: CcEagerTools::Off,
        request_class: "main",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::MainSonnet,
        version: "2.1.260",
        // 外推：`cap/2.1.258/00026` 去掉 `redact-thinking`、把 `server-side-fallback` 换成
        // 06-01、在 `fallback-credit` 之后补 `thinking-display-updates`。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
               server-side-fallback-2026-06-01,fallback-credit-2026-06-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        // 没有 2.1.260 的 sonnet 主线程样本，后缀沿用 2.1.258 那个四族通用值。它一定不对，
        // 但比抄 opus 的 `222`（那是「opus 主线程」的标记）更少造出错误关联。
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        // 没有样本；模拟路径跟随 opus 那份资产。
        eager_tools: CcEagerTools::Unknown,
        request_class: "main",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::MainHaiku,
        version: "2.1.260",
        // 外推：`cap/2.1.258/00031` 同上三处改动。haiku 不发 `effort`/`mid-conversation-system`，
        // 且 `claude-code` 排在第 6 位而非队首——这个位置在 2.1.260 的三份 haiku 抓包里没变。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               server-side-fallback-2026-06-01,fallback-credit-2026-06-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::EnabledUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        // 没有样本；模拟路径跟随 opus 那份资产。
        eager_tools: CcEagerTools::Unknown,
        request_class: "main",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::SdkSubagentHaiku,
        version: "2.1.260",
        // `cap/2.1.260/00020`：比主线程 haiku 短得多——没有 advisor-tool / advanced-tool-use /
        // server-side-fallback / fallback-credit / extended-cache-ttl。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               claude-code-20250219,thinking-display-updates-2026-08-18,\
               cache-diagnosis-2026-04-07",
        subagent: true,
        system: CcSystemShape::Identity,
        thinking: CcThinking::EnabledUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        // `cap/2.1.260/00020` 等 4 条全不带。
        eager_tools: CcEagerTools::Off,
        request_class: "subagent",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::HelperSubagentHaiku,
        version: "2.1.260",
        // `cap/2.1.260/00024`。没有 `claude-code` beta，却是官方 2.1.260 请求。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,server-side-fallback-2026-06-01,\
               fallback-credit-2026-06-01,cache-diagnosis-2026-04-07",
        subagent: true,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Disabled,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
        eager_tools: CcEagerTools::Unknown,
        request_class: "subagent",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::SessionTitleHaiku,
        version: "2.1.260",
        // `cap/2.1.260-2/00058`。同样没有 `claude-code` beta；`structured-outputs` 配的是
        // body 里的 `output_config.format=json_schema`。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
               structured-outputs-2025-12-15,fallback-credit-2026-06-01,\
               cache-diagnosis-2026-04-07",
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
        kind: CcProfileKind::SecurityClassifierSonnet,
        version: "2.1.260",
        // `cap/2.1.260/00019`。注意它**没有** `thinking-token-count` 与 `cache-diagnosis`，
        // 是六个 profile 里唯一少这两项的。
        beta: "claude-code-20250219,context-1m-2025-08-07,\
               interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               mid-conversation-system-2026-04-07,auto-mode-classifier-2026-07-16,\
               extended-cache-ttl-2025-04-11",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Disabled,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_CLASSIFIER,
        eager_tools: CcEagerTools::Unknown,
        request_class: "auxiliary",
        effort: None,
    },
    CcProfile {
        kind: CcProfileKind::QuotaProbe,
        version: "2.1.260",
        // `cap/2.1.260-2/00004`。整条请求只有四个顶层键，`system` 与 billing header 都没有。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05",
        // 没有 billing header，这个值用不上；留空串免得被误当成真后缀写出去。
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

/// **2.1.260 还缺的抓包**（记在案，别把外推当成已证）。
///
/// 1. 2.1.260 的 API-key 端四族成对抓包——没有它就无法证明「API-key → OAuth」的差分在
///    2.1.260 上仍是 2.1.258 那套（[`crate::proxy::merge_beta_for`] 的落位规则依赖这一点）。
/// 2. 2.1.260 的普通主线程 **sonnet-5** 请求。
/// 3. 2.1.260 的普通主线程 **haiku-4.5** 请求。
/// 4. TLS ClientHello / JA3 / JA4 原始字节，见 [`known_fingerprint_gaps`] 第 3 条。
///
/// 前三项缺着时，[`CC_PROFILES`] 里 `MainSonnet` / `MainHaiku` 两行是外推值，四模型族的
/// 差分矩阵不能宣称完整。
pub mod cc_2_1_260_missing_samples {}
