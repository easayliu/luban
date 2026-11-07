//! 2.1.319 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.319 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.319 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **除无工具 helper 与 `/model` 预热那两行外，各行除版本号外与 2.1.318（[`CC_PROFILES_2_1_318`]）完全相同**。2.1.319 起一次性侧查询的
/// 两个公共入口（`mNr` / `r0e`）没显式给 `sendSideQueryBeta` 时按 `g4()` 取：firstParty、OAuth、走官方地址（或 diskless）
/// 就带 `claude-code-side-query-2026-09-29`，位置同标题请求、紧跟 `mid-conversation-system-clear-at`。于是 WebFetch 页面处理
/// （`cap/auto-2.1.319-20261106-full/00110`，2.1.318 那条没有）与给对话起 kebab-case 名（schema 要 `name`，`00204`；
/// 与标题同属 [`CcProfileKind::SessionTitleHaiku`]，那行 2.1.318 起就带）也带了；WebSearch 子调用（带工具，不走这两个入口）
/// 与额度探测照旧不带。经自定义 `ANTHROPIC_BASE_URL` 的客户端不带，转发时补回，见 [`crate::proxy::merge_beta_for`]。
/// 依据 `cap/auto-2.1.319-20261106-full`（场景同 2.1.318-full，测试项目换了干净的 `scratchpad/cal10`）。
pub const CC_PROFILES: &[CcProfile] = &[
    at_319(CC_PROFILES_2_1_318[0]),
    at_319(CC_PROFILES_2_1_318[1]),
    at_319(CC_PROFILES_2_1_318[2]),
    at_319(CC_PROFILES_2_1_318[3]),
    at_319(CC_PROFILES_2_1_318[4]),
    CcProfile {
        version: "2.1.319",
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
               inline-tools-2026-09-15,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,\
               mid-conversation-system-clear-at-2026-08-21,claude-code-side-query-2026-09-29,\
               effort-2025-11-24,dangerous-tool-use-2026-09-03,cache-diagnosis-2026-04-07,\
               message-threads-2026-08-12",
        ..CC_PROFILES_2_1_318[5]
    },
    at_319(CC_PROFILES_2_1_318[6]),
    at_319(CC_PROFILES_2_1_318[7]),
    // `/model` 预热（`cap/auto-2.1.319-20261106-model/00061` opus、`00065` sonnet、`00070` fable、`00077` haiku-5-5）：
    // 同走那两个侧查询入口，2.1.293 那串末尾多 `claude-code-side-query`（2.1.318 `cap/auto-2.1.318-20261104-model/00064`
    // 还没有）；haiku 照旧少 `claude-code`（[`cc_model_beta`]）。额度探测（同包 `00019`）不走那两个入口，不带。
    CcProfile {
        version: "2.1.319",
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               redact-thinking-2026-02-12,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,claude-code-side-query-2026-09-29",
        ..CC_PROFILES_2_1_318[8]
    },
];

const fn at_319(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.319", ..p }
}

/// **2.1.319 还缺的抓包**：同 [`cc_2_1_318_missing_samples`]。这一版抓了 full / model / retry 与 `-bashfail`
/// （`cap/auto-2.1.319-20261106-{full,model,retry,bashfail}`，bashfail 里有同一次跑的 2.1.318 对照），另补了一份 MCP 工具
/// 与 `/clear`、`--continue` 的对照（`cap/auto-2.1.319-20261106-confirm`）。`connector-downloads` 目录在时 sandy-plover 补
/// `ReadMcpResourceTool` 那支没有样本。gaps 与 API-key 端没有重抓，沿用 2.1.309 的结论。TLS ClientHello 没有重测。
pub mod cc_2_1_319_missing_samples {}
