//! 2.1.316 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.316 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.315 来访的
/// beta 参照（[`cc_profile_at`]）。2.1.315 发布了但没抓：它与 2.1.316 的可执行文件同一个 Bun 构建，JS 层只差
/// 一个 macOS plist 串，落在这一档。
///
/// **各行除版本号外与 2.1.314（[`CC_PROFILES_2_1_314`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.316-20261031-full`（场景同 2.1.314-full，测试项目换了干净的
/// `scratchpad/cal7`）。
pub const CC_PROFILES: &[CcProfile] = &[
    at_316(CC_PROFILES_2_1_314[0]),
    at_316(CC_PROFILES_2_1_314[1]),
    at_316(CC_PROFILES_2_1_314[2]),
    at_316(CC_PROFILES_2_1_314[3]),
    at_316(CC_PROFILES_2_1_314[4]),
    at_316(CC_PROFILES_2_1_314[5]),
    at_316(CC_PROFILES_2_1_314[6]),
    at_316(CC_PROFILES_2_1_314[7]),
    at_316(CC_PROFILES_2_1_314[8]),
];

const fn at_316(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.316", ..p }
}

/// **2.1.316 还缺的抓包**：同 [`cc_2_1_314_missing_samples`]。这一版抓了 full / model / retry 与 `-bashfail`
/// （`cap/auto-2.1.316-20261031-{full,model,retry,bashfail}`）；2.1.314 用的是 10-30 那份，没有同日对照；2.1.315 没抓。
/// gaps 与 API-key 端没有重抓，沿用 2.1.309 的结论。TLS ClientHello 没有重测（Bun 换了构建）。
pub mod cc_2_1_316_missing_samples {}
