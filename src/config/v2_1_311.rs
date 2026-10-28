//! 2.1.311 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.311 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.311 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// 相对 2.1.310（[`CC_PROFILES_2_1_310`]）**只改了 beta**：`thinking-token-count-2026-05-13` 移出了
/// 官方的公共 beta 规则表，改在请求发出前、`advisor-tool` 之后单独补（条件是这一档模型支持 thinking），
/// 于是带它的各行都把它从 `interleaved-thinking` 后面挪到 `advisor-tool` 后面；额度探测那条
/// （haiku-4.5、不开 thinking）不再带它。依据 `cap/auto-2.1.311-20261027-full`（场景同 2.1.310-full，
/// 测试项目换了干净的 `scratchpad/cal4`）：opus 默认权限模式 `00217`、`00303`，sonnet `00377`，
/// fable `00920`，haiku-5-5 `00463`、`00536`，haiku-4.5 `00609`，helper `00093`、`00102`，标题 `00069`
/// 等 16 条，额度探测 `00019` 等 16 条；预热（`cap/auto-2.1.311-20261027-model/00067` 等）与 2.1.310
/// 逐字相同。别的项与顺序都没变。
///
/// 同一处改动还让 `context-1m` 只发给**名字本身不是原生 1M** 的模型：5 系四族与 opus-4.7 / 4.8 的上下文
/// 窗口本来就是 1M，`[1m]` 会话（`00303`、`model/00079`）不再带它，见
/// [`crate::proxy::context_1m_beta_wanted`]。遥测顶层那份会话级 beta 也跟着少了 `thinking-token-count`
/// （官方从公共规则表算），见 [`crate::telemetry::session_betas`]。
///
/// 抓包里与版本无关的差异：服务端 bootstrap 的 `client_data` 新下发了 `tengu_lapis_anchor:"off"`，
/// `<total_tokens>` 提醒整个关了（两版判断代码逐字相同），luban 按号跟着走，见
/// [`crate::proxy::lapis_anchor`]；账号额度状态变了，310 那次全被 `rate_limit` 压掉的「猜下一句」这次照常发出；
/// 本机多装了插件与 MCP，请求数 1237→1879 多出来的都是 MCP 连接。
pub const CC_PROFILES: &[CcProfile] = &[
    at_311(
        CC_PROFILES_2_1_310[0],
        "claude-code-20250219,interleaved-thinking-2025-05-14,\
         context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
         per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
         inline-tools-2026-09-15,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,\
         advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
         effort-2025-11-24,thinking-binding-controls-2026-08-01,\
         thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
         cache-diagnosis-2026-04-07,message-threads-2026-08-12",
    ),
    // fable 与 sonnet 与 opus 同一串。
    at_311(
        CC_PROFILES_2_1_310[1],
        "claude-code-20250219,interleaved-thinking-2025-05-14,\
         context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
         per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
         inline-tools-2026-09-15,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,\
         advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
         effort-2025-11-24,thinking-binding-controls-2026-08-01,\
         thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
         cache-diagnosis-2026-04-07,message-threads-2026-08-12",
    ),
    at_311(
        CC_PROFILES_2_1_310[2],
        "claude-code-20250219,interleaved-thinking-2025-05-14,\
         context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
         per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
         inline-tools-2026-09-15,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,\
         advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
         effort-2025-11-24,thinking-binding-controls-2026-08-01,\
         thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
         cache-diagnosis-2026-04-07,message-threads-2026-08-12",
    ),
    at_311(
        CC_PROFILES_2_1_310[3],
        "interleaved-thinking-2025-05-14,context-management-2025-06-27,\
         prompt-caching-scope-2026-01-05,claude-code-20250219,per-turn-control-2026-07-01,\
         mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,\
         advisor-tool-2026-03-01,thinking-token-count-2026-05-13,\
         advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
         effort-2025-11-24,thinking-binding-controls-2026-08-01,\
         thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
         cache-diagnosis-2026-04-07,message-threads-2026-08-12",
    ),
    at_311(
        CC_PROFILES_2_1_310[4],
        "interleaved-thinking-2025-05-14,context-management-2025-06-27,\
         prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
         thinking-token-count-2026-05-13,advanced-tool-use-2025-11-20,\
         thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
         extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
         message-threads-2026-08-12",
    ),
    at_311(
        CC_PROFILES_2_1_310[5],
        "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
         context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
         per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
         inline-tools-2026-09-15,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,\
         mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
         dangerous-tool-use-2026-09-03,cache-diagnosis-2026-04-07,\
         message-threads-2026-08-12",
    ),
    at_311(
        CC_PROFILES_2_1_310[6],
        "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
         context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
         per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
         inline-tools-2026-09-15,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,\
         mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
         structured-outputs-2025-12-15,cache-diagnosis-2026-04-07",
    ),
    at_311(
        CC_PROFILES_2_1_310[7],
        "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
         context-management-2025-06-27,prompt-caching-scope-2026-01-05",
    ),
    at_311(CC_PROFILES_2_1_310[8], CC_PROFILES_2_1_310[8].beta),
];

const fn at_311(p: CcProfile, beta: &'static str) -> CcProfile {
    CcProfile { version: "2.1.311", beta, ..p }
}

/// **2.1.311 还缺的抓包**：同 [`cc_2_1_310_missing_samples`]。这一版抓了 full / model / retry 与 `-bashfail`
/// （`cap/auto-2.1.311-20261027-{full,model,retry,bashfail}`，bashfail 里另有一份 2.1.310 对照，复现出已知的
/// 栈指纹）。gaps 与 API-key 端没有重抓，沿用 2.1.309 的结论。
pub mod cc_2_1_311_missing_samples {}
