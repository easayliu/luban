//! 2.1.329 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.329 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.329 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.328（[`CC_PROFILES_2_1_328`]）完全相同**：`cap/auto-max-2.1.329-20261120-full`（个人 Max 号）
/// 按场景逐条对 `cap/auto-max-2.1.328-20261119-full`，主线程 system 各块（记忆路径归一后逐字同）、beta、头名、body 键都没变，
/// `-model` 那份零差异；luban 资产里的句子 329 一句没少。唯一变了的工具描述是延迟池里的 DesignSync（luban 没有它的资产）。
/// 原生层 cch 种子常量还在，全量 cch 回归（含 329）全中。
///
/// 请求面以外的版本改动：`policy_limits` 拉取加了 `anthropic-client-capabilities`（见
/// [`crate::oauth`] 的握手头序）、遥测 `tengu_api_success` 加 `mcpToolsCharLength` 与新错误栈。
pub const CC_PROFILES: &[CcProfile] = &[
    at_329(CC_PROFILES_2_1_328[0]),
    at_329(CC_PROFILES_2_1_328[1]),
    at_329(CC_PROFILES_2_1_328[2]),
    at_329(CC_PROFILES_2_1_328[3]),
    at_329(CC_PROFILES_2_1_328[4]),
    at_329(CC_PROFILES_2_1_328[5]),
    at_329(CC_PROFILES_2_1_328[6]),
    at_329(CC_PROFILES_2_1_328[7]),
    at_329(CC_PROFILES_2_1_328[8]),
];

const fn at_329(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.329", ..p }
}

/// **2.1.329 还缺的抓包**：这一版只抓了个人 Max 号的 full / model / retry / `-bashfail`（`cap/auto-max-2.1.329-20261120-*`，
/// bashfail 里有同一次跑的 2.1.328 对照；测试项目是同一份 calc 项目 `cal15`）。团队号（`policy_limits` 新头只在团队号上发）、
/// fork、Fable 5.2 交互式、定义更新（redefine）、SDK 恢复、gaps 与 API-key 端没有重抓，TLS ClientHello 没有重测。
pub mod cc_2_1_329_missing_samples {}
