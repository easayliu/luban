//! 2.1.323 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.323 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.323 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.322（[`CC_PROFILES_2_1_322`]）完全相同**：两版的 beta 名字清单逐个相同，
/// `cap/auto-2.1.323-20261113-full` 按场景逐条对 `cap/auto-2.1.322-20261110-full`，system 各块（测试项目路径归一后）、
/// beta、body 键、工具集合都没变。这一版请求面的改动是一个新头（[`cc_first_party_request_class`]）与几个工具的描述
/// （Skill / Agent / Workflow 里 `system-reminder` 的说法按模型改写、Artifact 多电子表格、Bash 提交说明多一项）。
pub const CC_PROFILES: &[CcProfile] = &[
    at_323(CC_PROFILES_2_1_322[0]),
    at_323(CC_PROFILES_2_1_322[1]),
    at_323(CC_PROFILES_2_1_322[2]),
    at_323(CC_PROFILES_2_1_322[3]),
    at_323(CC_PROFILES_2_1_322[4]),
    at_323(CC_PROFILES_2_1_322[5]),
    at_323(CC_PROFILES_2_1_322[6]),
    at_323(CC_PROFILES_2_1_322[7]),
    at_323(CC_PROFILES_2_1_322[8]),
];

const fn at_323(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.323", ..p }
}

/// 2.1.323 起出站头 `x-claude-1p-request-class` 的取值（官方 `qya(querySource, agentContext)`）。
///
/// 与 `x-claude-code-request-class` 同一道闸（`CLAUDE_CODE_GATEWAY_HINT_HEADERS`，或直连且 `tengu_splendid_sutton`
/// 开着），`/v1/messages` 与 `count_tokens` 都带，字母序落在 `x-app` 之后。官方按 querySource 分五类：
/// - `main`：主线程（`repl_main_thread*` / `sdk`）、`compact`、`hook_prompt`、`side_question`、`web_search_tool`、
///   `web_fetch_apply`、`auto_mode`（安全分类）等主线程集合，加 `cache_touch` / `resume_cache_warm`；
/// - `blocking-subagent`：`hook_agent` 与前台子代理；`background-task`：后台子代理（323 默认后台）；
/// - `followup-generation`：猜下一句（`prompt_suggestion`）；
/// - `summary-extraction`：其余——标题生成、无工具 helper、额度探测、`/model` 预热、`count_tokens`。
///
/// `cap/auto-2.1.323-20261113-full`：主线程 97 条 `main`、Explore / Plan / general-purpose 子代理 14 条
/// `background-task`、猜下一句 38 条 `followup-generation`、标题 17 条与额度探测 16 条 `summary-extraction`；
/// `-model` 那份 `/model` 预热 7 条 `summary-extraction`。
pub const fn cc_first_party_request_class(kind: CcProfileKind) -> &'static str {
    use CcProfileKind::*;
    match kind {
        MainOpus | MainFable | MainSonnet | MainHaiku | MainHaiku55 | SecurityClassifierSonnet => {
            "main"
        }
        SdkSubagentHaiku => "background-task",
        HelperSubagentHaiku | SessionTitleHaiku | QuotaProbe | Prewarm => "summary-extraction",
    }
}

/// **2.1.323 还缺的抓包**：同 [`cc_2_1_322_missing_samples`]。这一版抓了 full / model / retry / `-bashfail`
/// （`cap/auto-2.1.323-20261113-{full,model,retry,bashfail}`，bashfail 里有同一次跑的 2.1.322 对照）。Fable 5.2 交互式、
/// 定义更新（redefine）、gaps 与 API-key 端没有重抓，TLS ClientHello 没有重测。
pub mod cc_2_1_323_missing_samples {}

/// **2.1.323 已知缺口：`-p --resume --tools <受限集>` 的 `tengu_context_size`**。
///
/// 官方按**实际启用**的工具集报（`cap/auto-2.1.323-20261113-resume/run3/00057`，`--tools Read`：1 个工具 / 268、不带
/// `non_mcp_tools_tokens_excludes_unsent`）。那条请求体的工具定义与默认恢复（`run4/00058`，官方 3336 + 遗漏）基本相同：
/// Agent、Bash 等 11 个常驻内建工具照发，已载入的延迟工具 WebFetch 也照样带着 schema 留在 `tools` 里。本轮那条中途 system
/// 消息**有**撤回通知（「The following tools are no longer available in this session.」，19 个延迟内建工具，含 WebFetch、
/// WebSearch），luban 叠上它之后不再认为有「启用却没发」的工具，所以不带遗漏标记；但常驻工具不在延迟池里、不会出现在撤回通知
/// 中，撤回信息还原不出完整启用集，`advanced-tool-use` 等 beta 的差异也不足以唯一推出 Read 限制。luban 现在报 26 个工具 /
/// 3336、不带遗漏标记。精确对齐需要实际启用工具集的可靠来源，**不按间接信号硬猜**；默认工具集的首轮与恢复（`run12/00061`、
/// `run4/00058`）已逐值对上。回归 `sdk_restricted_resume_context_size_known_gap`（`#[ignore]`）钉着官方值，缺口补上之前它是
/// 失败的——标了 ignore 只是记录缺口，不算修复。
pub mod cc_2_1_323_known_gaps {}
