//! 2.1.310 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.310 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.310 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.309（[`CC_PROFILES_2_1_309`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.310-20261024-full`（场景同 2.1.309-full，测试项目换了干净的
/// `scratchpad/cal3`，同从那份 clone）：工具定义按名逐字相同，system 各块归一项目路径后逐字不变，
/// beta、`metadata.user_id` 与 billing 字段及次序都没变。
///
/// 两版内嵌 JS 逐字符串比过（模块 2804→2819）：messages 请求上只有 `CronCreate`（延迟工具，不在注入资产里）
/// 的描述模板重排，Monitor 可用时渲染结果与 2.1.309 相同（抓包两版逐字一致）。可执行文件换了一版 Bun 构建
/// （版本串仍 1.4.4），cch 那个原生函数重新编译过（寄存器与栈布局都变了），cch 全量回归覆盖这一版、全中；
/// 变的是遥测里报错栈（[`crate::telemetry::shape::error_trace`] 与失败 Bash 那条）与
/// `tengu_org_config_gate_shadow` 多出的五个键（[`crate::telemetry`] 模板）。
///
/// 抓包里两处与版本无关的差异：主线程不再带 `cache-keepalive-2026-09-03`——服务端把
/// `tengu_sequential_cloud` 关了（309 时 force 开，这次 defaultValue=false，判断代码两版逐字同，
/// [`crate::proxy::cache_keepalive`] 本就按开关走）；A 会话一次上游连接重置后官方整个进程关 keep-alive，
/// 之后的请求不带 `Connection`（「Stale connection — disabling keep-alive for retry」，两版同逻辑）。
pub const CC_PROFILES: &[CcProfile] = &[
    at_310(CC_PROFILES_2_1_309[0]),
    at_310(CC_PROFILES_2_1_309[1]),
    at_310(CC_PROFILES_2_1_309[2]),
    at_310(CC_PROFILES_2_1_309[3]),
    at_310(CC_PROFILES_2_1_309[4]),
    at_310(CC_PROFILES_2_1_309[5]),
    at_310(CC_PROFILES_2_1_309[6]),
    at_310(CC_PROFILES_2_1_309[7]),
    at_310(CC_PROFILES_2_1_309[8]),
];

const fn at_310(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.310", ..p }
}

/// **2.1.310 还缺的抓包**：同 [`cc_2_1_309_missing_samples`]。这一版只抓了 full / model / retry
/// （`cap/auto-2.1.310-20261024-{full,model,retry}`），外加 `-bashfail`（全量包里那次没有失败的 Bash，
/// 另用 `-p` 补抓、并用 2.1.309 同法复现出已知值做对照）。gaps 与 API-key 端没有重抓，沿用 2.1.309 的结论。
pub mod cc_2_1_310_missing_samples {}
