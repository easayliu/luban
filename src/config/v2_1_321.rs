//! 2.1.321 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.321 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.321 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.319（[`CC_PROFILES_2_1_319`]）完全相同**：两版可执行文件里的 beta 名字清单逐个相同（55 个），
/// `cap/auto-2.1.321-20261110-full` 按场景逐条对 `cap/auto-2.1.319-20261106-full`，头顺序、beta、body 键、工具定义都没变。
/// 这一版改的是提示词（见 [`CC_SYSTEM_REST`]：多一段隐私说明、模型清单换成 Fable 5.2、fast mode 那句改写）与模型表
/// （新增 `claude-fable-5-2` / `claude-mythos-5-2`，默认 effort `medium`）。2.1.320 发布了但没抓，来访按 2.1.319 的表认。
pub const CC_PROFILES: &[CcProfile] = &[
    at_321(CC_PROFILES_2_1_319[0]),
    at_321(CC_PROFILES_2_1_319[1]),
    at_321(CC_PROFILES_2_1_319[2]),
    at_321(CC_PROFILES_2_1_319[3]),
    at_321(CC_PROFILES_2_1_319[4]),
    at_321(CC_PROFILES_2_1_319[5]),
    at_321(CC_PROFILES_2_1_319[6]),
    at_321(CC_PROFILES_2_1_319[7]),
    at_321(CC_PROFILES_2_1_319[8]),
];

const fn at_321(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.321", ..p }
}

/// **2.1.321 还缺的抓包**：同 [`cc_2_1_319_missing_samples`]。这一版抓了 full / model / retry 与 `-bashfail`
/// （`cap/auto-2.1.321-20261110-{full,model,retry,bashfail}`，bashfail 里有同一次跑的 2.1.319 对照），另有 Fable 5.2 的
/// 交互式会话（`cap/auto-2.1.321-20261110-fable52`）与 `-p` 一条（`-bashfail-fable`）。gaps 与 API-key 端没有重抓，沿用
/// 2.1.309 的结论。TLS ClientHello 没有重测。
pub mod cc_2_1_321_missing_samples {}
