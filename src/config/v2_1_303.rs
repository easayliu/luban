//! 2.1.303 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.303 的 profile 表：2.1.303 ~ 2.1.304 来访的 beta 参照（[`cc_profile_at`]）。2.1.305 的表
/// （[`CC_PROFILES_2_1_305`]）逐行同这张，只换版本号。
///
/// **各行除版本号外与 2.1.301（[`CC_PROFILES_2_1_301`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.303-20261019-full`（场景同 2.1.301-full，2.1.302 没抓）：请求头除 UA
/// 外逐字相同，beta、`metadata.user_id` 与 billing 字段及次序也都没变，全部 `/v1/messages` 配对比下来
/// beta 没有一处不同。
///
/// 这一版在请求上改的在别处：第四块末尾那段 Artifact `edits` 说明挪到了基座（global 缓存那块）末尾
/// （见 [`CC_SYSTEM_ARTIFACT_EDITS`]），`Artifact` 工具说明加了 dashboard 那几句，`-p` 默认开 fork
/// （Agent 说明随之变，模拟路径不发 `-p` 形态）。billing header 新增的 `cc_project` 字段在这一版没有
/// 调用方传值，抓包里一次没出现，不跟。
pub const CC_PROFILES_2_1_303: &[CcProfile] = &[
    at_303(CC_PROFILES_2_1_301[0]),
    at_303(CC_PROFILES_2_1_301[1]),
    at_303(CC_PROFILES_2_1_301[2]),
    at_303(CC_PROFILES_2_1_301[3]),
    at_303(CC_PROFILES_2_1_301[4]),
    at_303(CC_PROFILES_2_1_301[5]),
    at_303(CC_PROFILES_2_1_301[6]),
    at_303(CC_PROFILES_2_1_301[7]),
    at_303(CC_PROFILES_2_1_301[8]),
];

const fn at_303(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.303", ..p }
}

/// **2.1.303 还缺的抓包**：同 [`cc_2_1_301_missing_samples`]——fable 默认权限模式主线程、haiku-5-5
/// 的 1M 会话、SDK 子代理、安全分类与 API-key 端；2.1.302 整版没抓。
pub mod cc_2_1_303_missing_samples {}
