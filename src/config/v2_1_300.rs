//! 2.1.300 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.300 的 profile 表：2.1.300 来访的 beta 参照（[`cc_profile_at`]）。2.1.301 的表（[`CC_PROFILES`]）
/// 逐行同这张，只换版本号。
///
/// **各行除版本号外与 2.1.299（[`CC_PROFILES_2_1_299`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.300-20261015-full`（场景同 2.1.299-full）：全部 `/v1/messages` 的
/// `anthropic-beta` 去掉 [`CC_BETA_CACHE_KEEPALIVE`] 后共 24 种，与 2.1.299 抓包的 24 种逐字相同；
/// 可执行文件里的 beta 注册表也与 2.1.299 逐字相同。
///
/// 抓包里交互式主线程多出来的 [`CC_BETA_CACHE_KEEPALIVE`] 不是这一版带来的：发不发看服务端开关
/// `tengu_sequential_cloud`，同一个号 10-15 用 2.1.299 拉 eval 也已是强制开。它按号加在模拟路径的
/// 主线程串上（[`crate::proxy::simulation`] 的 `with_cache_keepalive`），不写进这张表。
///
/// 这一版改的在别处：system 第三块 `<pasted_content>` 那条加了一句、第四块末尾多一段 Artifact `edits`
/// 的说明，`Artifact` 工具多了 `edits` 参数，`Bash` 说明多一句，遥测几条事件多了键。
pub const CC_PROFILES_2_1_300: &[CcProfile] = &[
    at_300(CC_PROFILES_2_1_299[0]),
    at_300(CC_PROFILES_2_1_299[1]),
    at_300(CC_PROFILES_2_1_299[2]),
    at_300(CC_PROFILES_2_1_299[3]),
    at_300(CC_PROFILES_2_1_299[4]),
    at_300(CC_PROFILES_2_1_299[5]),
    at_300(CC_PROFILES_2_1_299[6]),
    at_300(CC_PROFILES_2_1_299[7]),
    at_300(CC_PROFILES_2_1_299[8]),
];

const fn at_300(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.300", ..p }
}

/// **2.1.300 还缺的抓包**：同 [`cc_2_1_299_missing_samples`]——fable 默认权限模式主线程、haiku-5-5
/// 的 1M 会话、SDK 子代理、安全分类与 API-key 端。
pub mod cc_2_1_300_missing_samples {}
