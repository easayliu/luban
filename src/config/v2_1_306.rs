//! 2.1.306 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.306 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.306 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.305（[`CC_PROFILES_2_1_305`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.306-20261021-full`（场景同 2.1.305-full）：请求头除 UA 外逐字相同，
/// beta、`metadata.user_id` 与 billing 字段及次序都没变，system 各块逐字不变；遥测事件与 meta 键
/// 也没变（两版内嵌 JS 逐模块比过，除版本号 / 构建时间外只改了两处：远程会话凭据文件的读取条件，
/// 和直连官方时延迟工具 400 去掉 deferred tools 重发要求报错里带 `defer_loading`——走代理的
/// 客户端设了 `ANTHROPIC_BASE_URL`，行为同 2.1.305）。
pub const CC_PROFILES: &[CcProfile] = &[
    at_306(CC_PROFILES_2_1_305[0]),
    at_306(CC_PROFILES_2_1_305[1]),
    at_306(CC_PROFILES_2_1_305[2]),
    at_306(CC_PROFILES_2_1_305[3]),
    at_306(CC_PROFILES_2_1_305[4]),
    at_306(CC_PROFILES_2_1_305[5]),
    at_306(CC_PROFILES_2_1_305[6]),
    at_306(CC_PROFILES_2_1_305[7]),
    at_306(CC_PROFILES_2_1_305[8]),
];

const fn at_306(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.306", ..p }
}

/// **2.1.306 还缺的抓包**：同 [`cc_2_1_305_missing_samples`]——fable 默认权限模式主线程、haiku-5-5
/// 的 1M 会话、SDK 子代理、安全分类与 API-key 端。
pub mod cc_2_1_306_missing_samples {}
