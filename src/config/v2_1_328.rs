//! 2.1.328 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.328 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.328 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.327（[`CC_PROFILES_2_1_327`]）完全相同**：`cap/auto-max-2.1.328-20261119-full`（个人 Max 号）
/// 按场景逐条对 `cap/auto-max-2.1.327-20261118-full`，主线程 system 各块（测试项目路径归一后）、beta、body 键、端点都没变，
/// `-model` 那份零差异。请求面的改动在资产：环境段那句 curl `--form-string`（327 要服务端 `tengu_dazzling_platypus`，
/// 328 起无条件，见 [`CC_ENV_HEAD`]）、ListAgents 描述（云端会话那段重写、加一句名字只是标签）、ReportFindings（`summary`
/// 描述改短、加 `severity`）与技能清单里 code-review 的描述。原生层 cch 种子常量还在，全量 cch 回归（含 328）全中。
///
/// 代码里新出现、但要服务端按号下发才发的：`resumption-2026-10-08` beta（`tengu_peaceful_wigderson`，替代
/// thinking-resumption），这个号没有。web-fetch 内置子代理跑 haiku 时首条 `max_tokens` 8192、WebSearch 侧查询的
/// `web_search` 带 `search_profile`，都在模拟路径不发的子代理 / 侧查询上。
pub const CC_PROFILES: &[CcProfile] = &[
    at_328(CC_PROFILES_2_1_327[0]),
    at_328(CC_PROFILES_2_1_327[1]),
    at_328(CC_PROFILES_2_1_327[2]),
    at_328(CC_PROFILES_2_1_327[3]),
    at_328(CC_PROFILES_2_1_327[4]),
    at_328(CC_PROFILES_2_1_327[5]),
    at_328(CC_PROFILES_2_1_327[6]),
    at_328(CC_PROFILES_2_1_327[7]),
    at_328(CC_PROFILES_2_1_327[8]),
];

const fn at_328(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.328", ..p }
}

/// **2.1.328 还缺的抓包**：这一版只抓了个人 Max 号的 full / model / retry / `-bashfail`（`cap/auto-max-2.1.328-20261119-*`，
/// bashfail 里有同一次跑的 2.1.327 对照；测试项目是同一份 calc 项目 `cal15`）。团队号、fork、Fable 5.2 交互式、定义更新
/// （redefine）、SDK 恢复、gaps 与 API-key 端没有重抓，TLS ClientHello 没有重测。
pub mod cc_2_1_328_missing_samples {}
