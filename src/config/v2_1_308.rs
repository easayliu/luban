//! 2.1.308 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.308 的 profile 表：2.1.308 来访的 beta 参照（[`cc_profile_at`]）。2.1.309 的表
/// （[`CC_PROFILES`]）逐行同这张，只换版本号。
///
/// **各行除版本号外与 2.1.307（[`CC_PROFILES_2_1_307`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.308-20261022-full`（场景同 2.1.307-full）：请求头除 UA 外逐字相同
/// （原始报文里的头名大小写与次序也比过），beta、`metadata.user_id` 与 billing 字段及次序都没变，system
/// 各块逐字不变。
///
/// 两版内嵌 JS 逐模块比过，除版本号 / 构建时间 / 提交号外只有内置 claude-test 插件里两个 agent 文件换了
/// 次序（默认不启用，请求里从不出现）。可执行文件换了一版 Bun 运行时（版本串仍是 1.4.4），出口层的 cch
/// 计算、HTTP/1.1 请求头写出与 TLS ClientHello 都没变（cch 全量回归覆盖这一版 178 条）；变的只有遥测里
/// 报错栈的打包块名（`telemetry::shape::error_trace` 与失败 Bash 那条）。
pub const CC_PROFILES_2_1_308: &[CcProfile] = &[
    at_308(CC_PROFILES_2_1_307[0]),
    at_308(CC_PROFILES_2_1_307[1]),
    at_308(CC_PROFILES_2_1_307[2]),
    at_308(CC_PROFILES_2_1_307[3]),
    at_308(CC_PROFILES_2_1_307[4]),
    at_308(CC_PROFILES_2_1_307[5]),
    at_308(CC_PROFILES_2_1_307[6]),
    at_308(CC_PROFILES_2_1_307[7]),
    at_308(CC_PROFILES_2_1_307[8]),
];

const fn at_308(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.308", ..p }
}

/// **2.1.308 的抓包缺口**。[`cc_2_1_307_missing_samples`] 那五项里前三项由
/// `cap/auto-2.1.308-20261022-gaps`（OAuth 直连）补上了：
///
/// 1. **fable 默认权限模式主线程**（`00037` 首轮、`00039` 续轮）：beta 与 opus 默认模式那串逐字相同，
///    与 auto 模式只差 `afk-mode` / `dangerous-tool-use`、体里的 `safeguards` 与 `Bash` 说明——表里 fable
///    行按 opus 那串记的做法成立；
/// 2. **haiku-5-5 的 `[1m]` 会话**（`00079`）：模型名不带后缀，只比普通会话多一项 `context-1m`，紧跟
///    `oauth`，体逐字相同（[`crate::proxy::simulated_beta`] 给 `context-1m` 排的位置与之一致）；
/// 3. **claude-code-guide 子代理**（`00119` 首轮、`00123` / `00124` 续轮）：跑在 haiku-5-5 上，beta 已带
///    `effort` / `per-turn-control` 这类主线程标记（2.1.285 那行早不是这个样子），靠 billing header 的
///    `cc_is_subagent=true` 认作子代理、原样透传。
///
/// **API-key 端**（luban 真实来访的模式，不模拟、只对比透传）由 `cap/auto-2.1.308-20261022-api` 补上（本机 luban
/// 4600 + luban Key，场景照 293 那份，另有 2.1.307 对照与 auto 模式）：52 条 messages 全部透传、不进模拟。来访串
/// 308 与 307 逐字相同；相对 293 少 `prompt-caching-scope`（2.1.307 起 BASE_URL 下不带，merge_beta 照直连补回）与
/// `mid-conversation-system`（2.1.299 转 GA），opus / sonnet 主线程多 `advanced-tool-use`；请求头集合与顺序同 293，
/// body 只多 auto 模式的 `safeguards`。
///
/// 仍缺一项：
///
/// - **安全分类**：auto 模式直连与经 `ANTHROPIC_BASE_URL`（`cap/auto-2.1.307-20261022-baseurl`）都改成主
///   请求带 `safeguards` 由服务端分类，抓包里不再有单独的分类请求；客户端自己发分类请求只在服务端分类
///   用不了时回退（`serverClassifierFallbackChoice`），没有样本（API-key 端 auto 模式同样走 `safeguards`，没回退）。
pub mod cc_2_1_308_missing_samples {}
