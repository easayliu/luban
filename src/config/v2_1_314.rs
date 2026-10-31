//! 2.1.314 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.314 的 profile 表：2.1.314 来访的 beta 参照（[`cc_profile_at`]）。2.1.316 的表（[`CC_PROFILES`]）
/// 各行除版本号外与这张相同。2.1.313 没有发布过（npm、`downloads.claude.ai`、内置与线上 CHANGELOG 都是
/// 2.1.312 直接到 2.1.314），落在 2.1.312 那一档。
///
/// **各行除版本号外与 2.1.312（[`CC_PROFILES_2_1_312`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.314-20261030-full`（场景同 2.1.312-full，测试项目换了干净的
/// `scratchpad/cal6`）：按场景配对的全部 `/v1/messages` 的 beta、请求头、请求体键、`thinking` /
/// `output_config` / `context_management`、工具定义都与 2.1.312 逐字相同；预热
/// （`cap/auto-2.1.314-20261030-model/00064` 等）除版本与 cch 外同 2.1.312。原生层换了一版 Bun 构建（版本串仍
/// 1.4.4，运行时仍报 `v26.12.1`），cch 那个函数归一后不变，全量回归照中。
///
/// 这一版改在别处：opus-5-5 的第四块多一段原创游戏的说明（[`crate::proxy::original_games`]）；内置模型目录里
/// opus-5-5、sonnet-5-5 去掉了 `total_tokens_reminder`，没有下发档位时这两个模型不发 `<total_tokens>`
/// （[`crate::proxy::lapis_anchor`]）；压缩后第一条主线程请求的 billing header 多 `cc_prev_msg` 等三项（API Key
/// 接入的真实 CC 由 luban 补会话链字段时一并补）；`event_logging` 改走 Bun 的长连接 fetch，头序与取值都换了
/// （[`EVENT_LOGGING_FETCH_ORDER`]）；遥测里三个事件的键与错误栈指纹，见 [`crate::telemetry`]。
///
/// 抓包里与版本无关的差异：eval 打开了 `tengu_org_config_required`（team / enterprise、25%），组织配置闸的影子判定
/// 换了取值、另多一条真判定事件；`tengu_read_dedup_killswitch` 等七个开关转开；MCP 与插件环境。
pub const CC_PROFILES_2_1_314: &[CcProfile] = &[
    at_314(CC_PROFILES_2_1_312[0]),
    at_314(CC_PROFILES_2_1_312[1]),
    at_314(CC_PROFILES_2_1_312[2]),
    at_314(CC_PROFILES_2_1_312[3]),
    at_314(CC_PROFILES_2_1_312[4]),
    at_314(CC_PROFILES_2_1_312[5]),
    at_314(CC_PROFILES_2_1_312[6]),
    at_314(CC_PROFILES_2_1_312[7]),
    at_314(CC_PROFILES_2_1_312[8]),
];

const fn at_314(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.314", ..p }
}

/// **2.1.314 还缺的抓包**：同 [`cc_2_1_312_missing_samples`]。这一版抓了 full / model / retry 与 `-bashfail`
/// （`cap/auto-2.1.314-20261030-{full,model,retry,bashfail}`）；2.1.312 用的是 10-28 那份，没有同日对照。gaps 与
/// API-key 端没有重抓，沿用 2.1.309 的结论。TLS ClientHello 没有重测（Bun 换了构建）。
pub mod cc_2_1_314_missing_samples {}
