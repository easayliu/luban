//! 2.1.301 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.301 的 profile 表：2.1.301 ~ 2.1.302 来访的 beta 参照（[`cc_profile_at`]）。2.1.303 的表
/// （[`CC_PROFILES`]）逐行同这张，只换版本号。
///
/// **各行除版本号外与 2.1.300（[`CC_PROFILES_2_1_300`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.301-20261016-full`（场景同 2.1.300-full）：可执行文件里的 beta 注册表
/// （51 项）、请求头名单、Stainless 版本、`metadata.user_id` 与 billing 字段都与 2.1.300 逐字相同，
/// 全部 `/v1/messages` 配对比下来 beta 也没有一处不同。
///
/// 这一版在请求上只改了 `Artifact` 工具 `edits` 参数说明里的一句（去掉 `` `root` ``），见
/// [`crate::proxy::cc_tools_core_for`]。可执行文件新加的 leanRewrite 提示词（模型能力 / 开关
/// `temporal_hare`）默认不开，服务端也没给这个号下发，模拟路径不跟。
pub const CC_PROFILES_2_1_301: &[CcProfile] = &[
    at_301(CC_PROFILES_2_1_300[0]),
    at_301(CC_PROFILES_2_1_300[1]),
    at_301(CC_PROFILES_2_1_300[2]),
    at_301(CC_PROFILES_2_1_300[3]),
    at_301(CC_PROFILES_2_1_300[4]),
    at_301(CC_PROFILES_2_1_300[5]),
    at_301(CC_PROFILES_2_1_300[6]),
    at_301(CC_PROFILES_2_1_300[7]),
    at_301(CC_PROFILES_2_1_300[8]),
];

const fn at_301(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.301", ..p }
}

/// **2.1.301 还缺的抓包**：同 [`cc_2_1_300_missing_samples`]——fable 默认权限模式主线程、haiku-5-5
/// 的 1M 会话、SDK 子代理、安全分类与 API-key 端。
pub mod cc_2_1_301_missing_samples {}
