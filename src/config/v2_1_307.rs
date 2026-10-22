//! 2.1.307 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.307 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.307 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.306（[`CC_PROFILES_2_1_306`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.307-20261022-full`（场景同 2.1.306-full）：请求头除 UA 外逐字相同，
/// beta、`metadata.user_id` 与 billing 字段及次序都没变，system 各块逐字不变。
///
/// 这一版在请求上改的在别处：`tengu_lucky_cerf` 开关连同短版正文一起删了，精简提示词的模型一律用 cerf
/// 那版 `Agent` 说明（`proxy::lucky_cerf::reads_list`，opus 也由短版换成这一版）。另外
/// `prompt-caching-scope` 改成只在直连官方（`ANTHROPIC_BASE_URL` 未设或就是 `api.anthropic.com`）时发，
/// 模拟路径按直连官方的形态发，照旧带。
pub const CC_PROFILES: &[CcProfile] = &[
    at_307(CC_PROFILES_2_1_306[0]),
    at_307(CC_PROFILES_2_1_306[1]),
    at_307(CC_PROFILES_2_1_306[2]),
    at_307(CC_PROFILES_2_1_306[3]),
    at_307(CC_PROFILES_2_1_306[4]),
    at_307(CC_PROFILES_2_1_306[5]),
    at_307(CC_PROFILES_2_1_306[6]),
    at_307(CC_PROFILES_2_1_306[7]),
    at_307(CC_PROFILES_2_1_306[8]),
];

const fn at_307(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.307", ..p }
}

/// **2.1.307 还缺的抓包**：同 [`cc_2_1_306_missing_samples`]——fable 默认权限模式主线程、haiku-5-5
/// 的 1M 会话、SDK 子代理、安全分类与 API-key 端。
pub mod cc_2_1_307_missing_samples {}
