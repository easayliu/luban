//! 2.1.326 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.326 的 profile 表：2.1.326 来访的 beta 参照（[`cc_profile_at`]）。2.1.327 的表（[`CC_PROFILES`]）各行
/// 除版本号外与这张相同。
///
/// **各行除版本号外与 2.1.325（[`CC_PROFILES_2_1_325`]）完全相同**：两版的 beta 名字清单与 `/v1` 端点逐个相同。
/// `cap/auto-team-2.1.326-20261117-full`（团队号）按场景逐条对 `cap/auto-2.1.325-20261114-full`、
/// `cap/auto-max-2.1.326-20261117-full`（个人 Max 号）对 `cap/auto-max-2.1.325-20261116-full`：主线程 system 各块（测试项目
/// 与配置目录路径归一后）、beta、body 键都没变。请求面的改动只有两处：Workflow 的 `resumeFromRunId` 说明整段重写
/// （工具资产 `cc_tools_core.json` / `cc_tools_core_haiku.json`），`x-claude-1p-request-class` 换了一套分法
/// （[`cc_first_party_request_class`]）。原生层 cch 种子常量还在，全量 cch 回归（含 326）全中。
///
/// 同一天 Max 号上多出来的 `Yield` 工具（与 Agent / Bash 描述里那句 "To wait on background work, call Yield."）
/// 与 `anthropic-usage-limit: extended` **不是这一版的改动**：两者 2.1.325 的代码里就有，分别由服务端按号下发的
/// `tengu_dazzling_acorn`、`tengu_lantern_spool` 管，团队号同版一样都没有。
pub const CC_PROFILES_2_1_326: &[CcProfile] = &[
    at_326(CC_PROFILES_2_1_325[0]),
    at_326(CC_PROFILES_2_1_325[1]),
    at_326(CC_PROFILES_2_1_325[2]),
    at_326(CC_PROFILES_2_1_325[3]),
    at_326(CC_PROFILES_2_1_325[4]),
    at_326(CC_PROFILES_2_1_325[5]),
    at_326(CC_PROFILES_2_1_325[6]),
    at_326(CC_PROFILES_2_1_325[7]),
    at_326(CC_PROFILES_2_1_325[8]),
];

const fn at_326(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.326", ..p }
}

/// **2.1.326 还缺的抓包**：这一版个人 Max 号与团队号各抓了 full / model / retry / `-bashfail`
/// （`cap/auto-max-2.1.326-20261117-*`、`cap/auto-team-2.1.326-20261117-*`，bashfail 里各有同一次跑的 2.1.325 对照；测试项目
/// 是从 323 抓包还原的同一份 calc 项目 `cal15`）。团队号头一回跑到一半号被冻结（`account_on_hold`），那份挪在各目录
/// `_held_0845/`，不作数；换号重抓的那份干净。Fable 5.2 交互式、定义更新（redefine）、SDK 恢复、gaps 与 API-key 端没有
/// 重抓，TLS ClientHello 没有重测。
pub mod cc_2_1_326_missing_samples {}
