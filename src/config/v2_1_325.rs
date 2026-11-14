//! 2.1.325 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.325 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.325 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.323（[`CC_PROFILES_2_1_323`]）完全相同**（2.1.324 没有发布，更新日志里也没有这一条）：两版的 beta
/// 名字清单、`x-`/`anthropic-` 头名、`/v1` 端点逐个相同，luban 资产（system 各段、工具描述与 schema）里 323 能找到的句子
/// 325 里一句不少。`cap/auto-2.1.325-20261114-full` 按场景逐条对 `cap/auto-2.1.323-20261113-full`：opus / sonnet /
/// haiku-5-5 / haiku-4-5 / fable-5-1 主线程 system 各块（测试项目路径归一后）、工具定义、头顺序、beta、body 键都没变，
/// 只有 sandy-plover 随场景用到的工具在变；`-model` 那份同样。原生层 `__text` 变了但 cch 种子常量还在，全量 cch 回归
/// （含 325）全中。这一版对 luban 的改动只在遥测（错误栈、几个事件键），见 [`crate::telemetry`] 里的 `v325`。
pub const CC_PROFILES: &[CcProfile] = &[
    at_325(CC_PROFILES_2_1_323[0]),
    at_325(CC_PROFILES_2_1_323[1]),
    at_325(CC_PROFILES_2_1_323[2]),
    at_325(CC_PROFILES_2_1_323[3]),
    at_325(CC_PROFILES_2_1_323[4]),
    at_325(CC_PROFILES_2_1_323[5]),
    at_325(CC_PROFILES_2_1_323[6]),
    at_325(CC_PROFILES_2_1_323[7]),
    at_325(CC_PROFILES_2_1_323[8]),
];

const fn at_325(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.325", ..p }
}

/// **2.1.325 还缺的抓包**：同 [`cc_2_1_323_missing_samples`]。这一版抓了 full / model / retry / `-bashfail`
/// （`cap/auto-2.1.325-20261114-{full,model,retry,bashfail}`，bashfail 里有同一次跑的 2.1.323 对照；原来那句项目外的
/// `ls /nonexistent_dir_luban_312` 两版都被权限拦下不执行，改成项目内的 `ls ./nonexistent_dir_luban_325`，被拦那次留在
/// `bashfail/_blocked`）。测试项目 `cal15` 是从 323 抓包还原的同一份 calc 项目。Fable 5.2 交互式、定义更新（redefine）、
/// SDK 恢复、gaps 与 API-key 端没有重抓，TLS ClientHello 没有重测。
pub mod cc_2_1_325_missing_samples {}
