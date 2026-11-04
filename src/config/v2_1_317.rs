//! 2.1.317 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.317 的 profile 表：2.1.317 来访的 beta 参照（[`cc_profile_at`]）。2.1.318 的表（[`CC_PROFILES`]）各行除版本号外
/// 与这张相同。
///
/// **各行除版本号外与 2.1.316（[`CC_PROFILES_2_1_316`]）完全相同**：beta 串、各类请求的 beta 集合、请求体键都没变
/// （新增的 `claude-code-side-query-2026-09-29` 只给 Artifact 评论那几类侧查询，`program-use-case-ids-2026-09-17`
/// 只在服务端开了 `tengu_federated_possum`（默认关）或设了 `CLAUDE_CODE_PROGRAM_USE_CASE_ID` 时带，抓包里都没有）。
/// 依据 `cap/auto-2.1.317-20261103-full`（场景同 2.1.316-full，测试项目换了干净的 `scratchpad/cal8`）。
pub const CC_PROFILES_2_1_317: &[CcProfile] = &[
    at_317(CC_PROFILES_2_1_316[0]),
    at_317(CC_PROFILES_2_1_316[1]),
    at_317(CC_PROFILES_2_1_316[2]),
    at_317(CC_PROFILES_2_1_316[3]),
    at_317(CC_PROFILES_2_1_316[4]),
    at_317(CC_PROFILES_2_1_316[5]),
    at_317(CC_PROFILES_2_1_316[6]),
    at_317(CC_PROFILES_2_1_316[7]),
    at_317(CC_PROFILES_2_1_316[8]),
];

const fn at_317(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.317", ..p }
}

/// **2.1.317 还缺的抓包**：同 [`cc_2_1_316_missing_samples`]。这一版抓了 full / model / retry 与 `-bashfail`
/// （`cap/auto-2.1.317-20261103-{full,model,retry,bashfail}`，bashfail 里有同一次跑的 2.1.316 对照）。重复命令统计按会话
/// 抽样（`tengu_vivid_peach`，默认 10%），这次全量里没有一个会话抽中，抽中后那组键的取值没有 317 样本。gaps 与 API-key
/// 端没有重抓，沿用 2.1.309 的结论。TLS ClientHello 没有重测（Bun 换了构建）。
pub mod cc_2_1_317_missing_samples {}
