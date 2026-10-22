//! 2.1.305 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.305 的 profile 表：2.1.305 来访的 beta 参照（[`cc_profile_at`]）。2.1.306 的表
/// （[`CC_PROFILES_2_1_306`]）逐行同这张，只换版本号。
///
/// **各行除版本号外与 2.1.303（[`CC_PROFILES_2_1_303`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.305-20261021-full`（场景同 2.1.303-full，2.1.304 没抓）：请求头除 UA
/// 外逐字相同，beta、`metadata.user_id` 与 billing 字段及次序也都没变，system 各块逐字不变。
///
/// 这一版在请求上改的在别处：服务端下发 cerf 正文（`tengu_lucky_cerf_text`）的机制删了，cerf 那版
/// `Agent` 说明一律用内置正文（`proxy::lucky_cerf::reads_text`）。billing header 的 `cc_project` 这一版
/// 真有了调用方，但只在订阅登录且直连 `api.anthropic.com` 时写，模拟路径不跟。
pub const CC_PROFILES_2_1_305: &[CcProfile] = &[
    at_305(CC_PROFILES_2_1_303[0]),
    at_305(CC_PROFILES_2_1_303[1]),
    at_305(CC_PROFILES_2_1_303[2]),
    at_305(CC_PROFILES_2_1_303[3]),
    at_305(CC_PROFILES_2_1_303[4]),
    at_305(CC_PROFILES_2_1_303[5]),
    at_305(CC_PROFILES_2_1_303[6]),
    at_305(CC_PROFILES_2_1_303[7]),
    at_305(CC_PROFILES_2_1_303[8]),
];

const fn at_305(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.305", ..p }
}

/// **2.1.305 还缺的抓包**：同 [`cc_2_1_303_missing_samples`]——fable 默认权限模式主线程、haiku-5-5
/// 的 1M 会话、SDK 子代理、安全分类与 API-key 端；2.1.304 整版没抓。
pub mod cc_2_1_305_missing_samples {}
