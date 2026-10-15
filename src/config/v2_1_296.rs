//! 2.1.296 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.296 的 profile 表：2.1.296 ~ 2.1.298 来访的 beta 参照（[`cc_profile_at`]）。2.1.299 的表
/// （[`CC_PROFILES`]）只去掉了 `mid-conversation-system`。
///
/// **各行除版本号外与 2.1.293（[`CC_PROFILES_2_1_293`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.296-20261013-full`（场景同 2.1.293-full）：
///
/// - **beta**：全部 `/v1/messages` 的 `anthropic-beta` 共 24 种，与 2.1.293 抓包的 24 种逐字相同；
///   主线程 opus `00418`、sonnet `00258`、fable `00541`（auto）、haiku-5-5 `00344`、haiku-4.5 `00382`，
///   标题 `00029`、helper `00063`、额度探测 `00012`；
/// - **system 四块**、首轮环境说明与顶层键序都与 2.1.293 逐字节相同（只差记忆目录里的会话路径与
///   billing header）；
/// - **effort**：官方默认 opus-5-5 / sonnet-5-5 / haiku-5-5 都是 medium、fable-5-1 是 high（可执行文件
///   的模型表 `default_effort`，抓包同）。模拟路径从这一版起按模型发官方默认档
///   （[`cc_outbound_effort`]），此前三族一律按 high 发；
/// - **工具**：改了两处，都不在这张表里——`Read` 多了 `allow_large` 参数；`Agent` 对服务端开关
///   `tengu_lucky_cerf` 列出的模型换成另一段说明，见 [`crate::proxy::cc_tools_core_for`]。
///
/// 2.1.294 / 2.1.295 没有抓包；可执行文件里的 beta 字面量与 2.1.296 相同，按这张表处理。
pub const CC_PROFILES_2_1_296: &[CcProfile] = &[
    at_296(CC_PROFILES_2_1_293[0]),
    at_296(CC_PROFILES_2_1_293[1]),
    at_296(CC_PROFILES_2_1_293[2]),
    at_296(CC_PROFILES_2_1_293[3]),
    at_296(CC_PROFILES_2_1_293[4]),
    at_296(CC_PROFILES_2_1_293[5]),
    at_296(CC_PROFILES_2_1_293[6]),
    at_296(CC_PROFILES_2_1_293[7]),
    at_296(CC_PROFILES_2_1_293[8]),
];

const fn at_296(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.296", ..p }
}

/// **2.1.296 还缺的抓包**：同 [`cc_2_1_293_missing_samples`]——fable 默认权限模式主线程、haiku-5-5
/// 的 1M 会话、SDK 子代理、安全分类与 API-key 端。
pub mod cc_2_1_296_missing_samples {}
