//! 2.1.318 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.318 的 profile 表：2.1.318 来访的 beta 参照（[`cc_profile_at`]）。2.1.319 的表（[`CC_PROFILES`]）除 helper 那行外
/// 各行除版本号外与这张相同。
///
/// **除标题生成那行外，各行除版本号外与 2.1.317（[`CC_PROFILES_2_1_317`]）完全相同**。标题生成（`generate_session_title`）
/// 2.1.318 起在官方地址、OAuth 登录时多带 `claude-code-side-query-2026-09-29`，紧跟 `mid-conversation-system-clear-at`
/// （`cap/auto-2.1.318-20261104-full` 16 条标题请求条条都有，2.1.317 那份 16 条都没有；经自定义 `ANTHROPIC_BASE_URL`
/// 的客户端不带，转发时补回，见 [`crate::proxy::merge_beta_for`]）。
/// 新增的 `tool-provenance-2026-10-26` 只在官方地址、OAuth 登录、服务端 `tengu_snappy_noodle` 开着时带（只给 `mcp__`
/// 工具挂 `ownership_metadata`），`cap/auto-2.1.318-20261104-full` 的 eval 下发它为 `false`，全量里一条都没有。
/// 依据 `cap/auto-2.1.318-20261104-full`（场景同 2.1.317-full，测试项目换了干净的 `scratchpad/cal9`）。
pub const CC_PROFILES_2_1_318: &[CcProfile] = &[
    at_318(CC_PROFILES_2_1_317[0]),
    at_318(CC_PROFILES_2_1_317[1]),
    at_318(CC_PROFILES_2_1_317[2]),
    at_318(CC_PROFILES_2_1_317[3]),
    at_318(CC_PROFILES_2_1_317[4]),
    at_318(CC_PROFILES_2_1_317[5]),
    CcProfile {
        version: "2.1.318",
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               inline-tools-2026-09-15,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,\
               mid-conversation-system-clear-at-2026-08-21,claude-code-side-query-2026-09-29,\
               effort-2025-11-24,structured-outputs-2025-12-15,cache-diagnosis-2026-04-07",
        ..CC_PROFILES_2_1_317[6]
    },
    at_318(CC_PROFILES_2_1_317[7]),
    at_318(CC_PROFILES_2_1_317[8]),
];

const fn at_318(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.318", ..p }
}

/// **2.1.318 还缺的抓包**：同 [`cc_2_1_317_missing_samples`]。这一版抓了 full / model / retry 与 `-bashfail`
/// （`cap/auto-2.1.318-20261104-{full,model,retry,bashfail}`，bashfail 里有同一次跑的 2.1.317 对照）。
/// `tool-provenance` 那条 beta 与 MCP 工具的 `ownership_metadata` 服务端没开，没有样本。gaps 与 API-key 端没有重抓，
/// 沿用 2.1.309 的结论。TLS ClientHello 没有重测（Bun 换了构建）。
pub mod cc_2_1_318_missing_samples {}
