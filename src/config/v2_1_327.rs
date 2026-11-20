//! 2.1.327 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.327 的 profile 表：2.1.327 来访的 beta 参照（[`cc_profile_at`]）。2.1.328 的表（[`CC_PROFILES`]）各行
/// 除版本号外与这张相同。
///
/// **各行除版本号外与 2.1.326（[`CC_PROFILES_2_1_326`]）完全相同**：`cap/auto-max-2.1.327-20261118-full`（个人 Max 号）
/// 按场景逐条对 `cap/auto-max-2.1.326-20261117-full`，主线程 system 各块（测试项目路径归一后）、beta、body 键、端点都没变，
/// `-model` 那份零差异。请求面的改动只在工具资产：Bash 的 git 段多一行「Do not commit files that likely contain secrets…」，
/// ListAgents 描述末尾多一句分页说明、入参多 `cursor`（`cc_tools_core.json` / `cc_tools_core_haiku.json`）。原生层 cch 种子
/// 常量还在，全量 cch 回归（含 327）全中。
///
/// 代码里新出现、但要服务端按号下发才发的两样，这个号都没有：WebSearch 侧查询的 `web-search-results-only-2026-11-15`
/// beta（`tengu_nifty_quasar`）与线程路由提示头（头名由 `tengu_tingly_flute` 下发）。
pub const CC_PROFILES_2_1_327: &[CcProfile] = &[
    at_327(CC_PROFILES_2_1_326[0]),
    at_327(CC_PROFILES_2_1_326[1]),
    at_327(CC_PROFILES_2_1_326[2]),
    at_327(CC_PROFILES_2_1_326[3]),
    at_327(CC_PROFILES_2_1_326[4]),
    at_327(CC_PROFILES_2_1_326[5]),
    at_327(CC_PROFILES_2_1_326[6]),
    at_327(CC_PROFILES_2_1_326[7]),
    at_327(CC_PROFILES_2_1_326[8]),
];

const fn at_327(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.327", ..p }
}

/// **2.1.327 还缺的抓包**：这一版只抓了个人 Max 号的 full / model / retry / `-bashfail`（`cap/auto-max-2.1.327-20261118-*`，
/// bashfail 里有同一次跑的 2.1.326 对照；测试项目是同一份 calc 项目 `cal15`）。团队号、fork、Fable 5.2 交互式、定义更新
/// （redefine）、SDK 恢复、gaps 与 API-key 端没有重抓，TLS ClientHello 没有重测。
pub mod cc_2_1_327_missing_samples {}
