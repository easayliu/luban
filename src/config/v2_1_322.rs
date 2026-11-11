//! 2.1.322 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.322 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.322 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.321（[`CC_PROFILES_2_1_321`]）完全相同**：两版原生层逐节相同（cch 不变），JS 层的改动全在
/// Vertex 上的 fast mode（可用性按 provider 分 `fast_mode` / `vertex_fast_mode`、Vertex 区域限制、被拒冷却重试、
/// 环境段那句 fast mode 在 Vertex 下改说法），官方直连这条路上没有变化。`cap/auto-2.1.322-20261110-full` 按场景逐条对
/// `cap/auto-2.1.321-20261110-full`，system 各块（测试项目路径归一后）、头顺序、beta、body 键、工具定义都没变。
pub const CC_PROFILES: &[CcProfile] = &[
    at_322(CC_PROFILES_2_1_321[0]),
    at_322(CC_PROFILES_2_1_321[1]),
    at_322(CC_PROFILES_2_1_321[2]),
    at_322(CC_PROFILES_2_1_321[3]),
    at_322(CC_PROFILES_2_1_321[4]),
    at_322(CC_PROFILES_2_1_321[5]),
    at_322(CC_PROFILES_2_1_321[6]),
    at_322(CC_PROFILES_2_1_321[7]),
    at_322(CC_PROFILES_2_1_321[8]),
];

const fn at_322(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.322", ..p }
}

/// **2.1.322 还缺的抓包**：同 [`cc_2_1_321_missing_samples`]。这一版抓了 full / model / retry / `-bashfail`
/// （`cap/auto-2.1.322-20261110-{full,model,retry,bashfail}`，bashfail 里有同一次跑的 2.1.321 对照）与 Fable 5.2 的交互式会话
/// （`cap/auto-2.1.322-20261110-fable52`，场景同 321 那份；system 各块路径归一后、beta、工具逐项相同）。gaps 与 API-key 端
/// 没有重抓（这一版官方直连路径没改），TLS ClientHello 没有重测。
pub mod cc_2_1_322_missing_samples {}
