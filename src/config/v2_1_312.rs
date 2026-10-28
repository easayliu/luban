//! 2.1.312 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.312 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.312 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.311（[`CC_PROFILES_2_1_311`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.312-20261028-full`（场景同 2.1.311-full，测试项目换了干净的
/// `scratchpad/cal5`）：按场景配对的全部 `/v1/messages` 的 beta、请求体键、`thinking` /
/// `output_config` / `context_management`、system 各块（去掉项目路径与日期后）都与 2.1.311 逐字相同；
/// 预热（`cap/auto-2.1.312-20261028-model/00067` 等）除版本与 cch 外同 2.1.311。原生层（Bun 运行时，
/// 含 cch 那个函数）逐字节没变。
///
/// 这一版改在别处：AskUserQuestion 的描述多一段（工具资产 `cc_tools_core.json` / `cc_tools_core_haiku.json`）；遥测里四个事件的键
/// （`can_use_tool_allowed` / `rejected` 的 `toolUseID`、`auto_mode_decision` 的 `toolUseId` 与
/// `serverClassifierRole`、`context_announcement` 的 `assumed_utc`、`resume_model_restore` 去掉
/// `is_eap`）、`tengu_snoopy_creek` 的默认值（开改关，见 [`crate::proxy::snoopy_creek`]）与错误栈指纹，见
/// [`crate::telemetry`]。
///
/// 抓包里与版本无关的差异：每条 `/v1/messages` 多了 `x-claude-code-sandy-plover` 头——2.1.311 起就有的代码，
/// 服务端开关 `tengu_snug_mitten` 10-28 才打开（同日 2.1.311 对照照样带），见
/// [`crate::proxy::sandy_plover`]；`tengu_snoopy_creek` 服务端也改成显式下发关；本机 settings.json 多了
/// `model` 键（启动事件 `startup_manual_model_config` 多两项）；MCP 与插件环境。
pub const CC_PROFILES: &[CcProfile] = &[
    at_312(CC_PROFILES_2_1_311[0]),
    at_312(CC_PROFILES_2_1_311[1]),
    at_312(CC_PROFILES_2_1_311[2]),
    at_312(CC_PROFILES_2_1_311[3]),
    at_312(CC_PROFILES_2_1_311[4]),
    at_312(CC_PROFILES_2_1_311[5]),
    at_312(CC_PROFILES_2_1_311[6]),
    at_312(CC_PROFILES_2_1_311[7]),
    at_312(CC_PROFILES_2_1_311[8]),
];

const fn at_312(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.312", ..p }
}

/// **2.1.312 还缺的抓包**：同 [`cc_2_1_311_missing_samples`]。这一版抓了 full / model / retry 与 `-bashfail`
/// （`cap/auto-2.1.312-20261028-{full,model,retry,bashfail}`，bashfail 里另有一份同日的 2.1.311 对照，复现出已知的
/// 栈指纹）。gaps 与 API-key 端没有重抓，沿用 2.1.309 的结论。
pub mod cc_2_1_312_missing_samples {}
