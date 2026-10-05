//! 请求形态 profile：类型、顶层键序、模型代际，以及按来访版本挑 profile 表。

use super::*;

// ---------- 请求 profile（2.1.258 / 2.1.260 / 2.1.270 / 2.1.277 / 2.1.280 各一张表） ----------

/// 一条官方请求属于哪一类。
///
/// **只按模型族分不够。** 同为 haiku-4.5，SDK 子代理（`cap/2.1.260/00020`）、无工具
/// helper（`00024`）、会话标题生成（`cap/2.1.260-2/00058`）与额度探测（`00004`）四者的
/// beta 串、`thinking`、`system` 块数、顶层键序和 billing 后缀**没有一项相同**。故
/// profile 的键是「模型族 + 请求用途」，不是模型族。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcProfileKind {
    /// 主线程 opus-5（`cap/2.1.277/00357`；2.1.260 见 `cap/2.1.260-2/00013`、`00025`、`00057`）。
    MainOpus,
    /// 主线程 fable-5-1（`cap/2.1.277/00023`、`00026`；2.1.260 见 `cap/2.1.260/00018`）。
    MainFable,
    /// 主线程 sonnet-5（`cap/2.1.277/00031`；2.1.260 表里那行是由 2.1.258 外推的）。
    MainSonnet,
    /// 主线程 haiku-4.5（`cap/2.1.277/00046`；2.1.260 表里那行同样是外推的）。
    MainHaiku,
    /// agent-sdk 子代理 haiku（`cap/2.1.260/00020`、`00025`）：带工具、`cc_is_subagent`。
    SdkSubagentHaiku,
    /// 无工具的 haiku 辅助请求（`cap/2.1.260/00024`、`00027`）：`thinking:disabled`。
    HelperSubagentHaiku,
    /// 会话标题生成 haiku（`cap/2.1.260-2/00058`）：`output_config.format=json_schema`。
    SessionTitleHaiku,
    /// 安全分类 sonnet（`cap/2.1.260/00019`、`00030`）：`max_tokens:64`、非流式、键序独一份。
    SecurityClassifierSonnet,
    /// 额度探测（`cap/2.1.260-2/00004`）：`max_tokens:1`、无 `system`、无 billing header。
    QuotaProbe,
}

/// profile 的 `thinking` 形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcThinking {
    /// `{"type":"adaptive"}`——2.1.258 的 opus / sonnet 主线程（`cap/2.1.258/00012`、`00026`）。
    Adaptive,
    /// `{"type":"adaptive","display":"updates"}`——2.1.258 的 fable，2.1.260 起主线程四族都是。
    AdaptiveUpdates,
    /// `{"budget_tokens":N,"type":"enabled"}`——2.1.258 的 haiku（`cap/2.1.258/00031`）。
    Enabled,
    /// `{"budget_tokens":N,"type":"enabled","display":"updates"}`——2.1.260 的 SDK 子代理。
    EnabledUpdates,
    /// `{"type":"disabled"}`——helper / 标题 / 安全分类。**这是官方形态**，不是多余字段，
    /// 别当成第三方客户端塞的东西剥掉。
    Disabled,
    /// 整个字段都不发——额度探测。
    Absent,
}

/// `system` 的固定前缀形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcSystemShape {
    /// `[billing, 身份句, 基座, 其余]`——2.1.277 起四族主线程都是这个形态（`cap/2.1.277`）。
    /// 「其余」是官方第四块（[`CC_SYSTEM_REST`] 模板填出来的），客户端自己的 system 挪进首条
    /// 用户消息；填不出第四块（认不出的模型族）或开关关着时末块退回客户端原文。
    Identity,
    /// `[billing, 身份句, # Reporting outcomes, 基座, 其余]`——**2.1.258 / 2.1.260 的** fable
    /// 主线程。2.1.277 的 fable 已不带 reporting 块、与其余三族同为四块，2.1.277 表里没有一行用
    /// 它；留着是给 [`CC_PROFILES_2_1_260`] 那行与透传路径拆 API-key 四块形态
    /// （[`crate::proxy::align_system_shape`]）用。
    IdentityReporting,
    /// 没有 `system`——额度探测。
    None,
}

/// 一条官方请求形态的**全部**派生量，一处定义、各处引用。
///
/// 原先这些量散在 `cc_system_base(model)` / `cc_beta_seed(model)` /
/// `cc_system_reporting(model)` 三个按模型族分派的函数里，于是「同一族不同用途要有不同
/// 形态」根本表达不出来。合成一张表之后，加一个 profile 就是加一行。
#[derive(Debug, Clone, Copy)]
pub struct CcProfile {
    pub kind: CcProfileKind,
    /// 这份形态取自哪个客户端版本。写进 `cc_version` 与出站 UA 的就是它。
    ///
    /// **`cc_version` 的第四段不在表里。** 2.1.260 ~ 2.1.277 的表曾各记一个「逐 profile 定死」的
    /// 后缀（`222` / `d56` / `385` …），2.1.280 核实那是 [`crate::proxy::cc_version_suffix`] 对
    /// 会话首条非 meta 用户文本算出来的——抓包那几个会话首句相同，于是看着像常量。
    pub version: &'static str,
    /// `anthropic-beta` 的**完整**官方串，去掉 `oauth`（由落位规则补回官方位置）与动态的
    /// `afk-mode`（同模型两次请求有/无交替出现，不进固定种子）。
    pub beta: &'static str,
    /// billing header 里带 `cc_is_subagent=true`。
    pub subagent: bool,
    pub system: CcSystemShape,
    pub thinking: CcThinking,
    /// 顶层 `fallbacks` 的 JSON 字面量；`None` 即不发这个字段。
    pub fallbacks: Option<&'static str>,
    /// 顶层键序，见 [`CC_BODY_ORDER_MAIN`]。
    pub body_key_order: &'static [&'static str],
    /// 这个 profile 的官方请求里，每个**非延迟**工具带不带 `eager_input_streaming: true`。
    /// 取值只写抓包证实过的，见 [`CcEagerTools`]。
    pub eager_tools: CcEagerTools,
    /// 出站头 `x-claude-code-request-class` 的取值。2.1.277 起官方每条 `/v1/messages` 都带
    /// （`cap/2.1.277`）：主线程与「猜下一句」是 `main`，SDK 子代理是 `subagent`，标题生成与
    /// 额度探测是 `auxiliary`（无工具 helper 与安全分类没有 2.1.277 样本，按用途归入后两者）。
    /// 老版本的表里也填了同样的值，但只有模拟路径会写这个头，而模拟路径恒用 [`CC_PROFILES`]。
    pub request_class: &'static str,
    /// 顶层 `output_config.effort` 的取值：2.1.277 起 opus / fable / sonnet 主线程恒带
    /// `{"effort":"high"}`（`cap/2.1.277/00023`、`00031`、`00357`），2.1.280 的 opus 变成
    /// `medium`（`cap/2.1.280/00021`），haiku 主线程与全部辅助请求不带。`None` 即不补，见 [`crate::proxy::ensure_output_config`]。
    pub effort: Option<&'static str>,
}

/// 官方请求里工具声明带不带 `eager_input_streaming: true`——按 profile（版本 × 模型 × 用途）
/// 记，**只记抓包证实过的**。
///
/// 证据矩阵（`cap/` 全部 `/v1/messages` 抓包，逐条数过）：
///
/// | 版本 | 模型 / 用途 | 非延迟工具 | eager |
/// |---|---|---:|---|
/// | 2.1.258 OAuth | opus / fable / sonnet / haiku 主线程 | 15 | 全带 |
/// | 2.1.258 API-key | 四族主线程 | 34 / 37 | 全不带 |
/// | 2.1.260 | opus 主线程（7 条） | 15 | 全带 |
/// | 2.1.260 | fable 主线程（3 条） | 12 | 全不带 |
/// | 2.1.260 | SDK 子代理 haiku（4 条） | 4 | 全不带 |
/// | 2.1.270 | sonnet 主线程（2 条） | 15 | 全带 |
///
/// 每一条要么全带要么全不带，没有混合；`DeferredToolPlaceholder` 那条占位永远不带。fable 在
/// 2.1.258 带、2.1.260 不带，说明它是「版本 × 模型」的联合属性，不能按模型单独推，所以
/// 2.1.260 的 sonnet / haiku 主线程与 2.1.270 的其余三族一律 [`Self::Unknown`]。
///
/// **它与 `advanced-tool-use` beta 同现**：带 eager 的每一条头上都有那项 beta，API-key 端
/// 两者都没有。体侧补写因此要求出站头里真有它（[`crate::proxy::rewrite_body`]）。
///
/// **真 CC 路径按版本精确查**（[`cc_eager_tools_at`]）：只有来访自报的版本恰好是某张表抓包的
/// 那一版（2.1.258 / 2.1.260 / 2.1.270）且该 kind 有行，才拿得到 On/Off；其余版本一律
/// [`Self::Unknown`]。这与 beta 参照串的取法（[`cc_profile_at`]，落回最近一版）**刻意不同**：
/// 那是兼容兜底——新客户端来了总得给它一串 beta；这里是证据——没抓过的版本就是没证据，
/// 2.1.270 的 opus 不能因为 2.1.260 的 opus 带就跟着带。
///
/// 补写规则两条路径共用（[`crate::proxy::fill_eager_tools`]）：客户端已写的值（true 或 false）
/// 不覆盖；profile 不是 [`Self::On`] 就不补；只补有 `input_schema` 的内建形态客户端工具——
/// `defer_loading` 占位、`mcp__*`（订阅端样本里 MCP 工具全在延迟池里、正文里一个没有，带不带
/// 无从证实）、服务端工具（`type: web_search_…`，没有 `input_schema`）都不动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcEagerTools {
    /// 抓包证实该 profile 的非延迟工具全带 `eager_input_streaming: true`。
    On,
    /// 抓包证实一个都不带。
    Off,
    /// 没有这个 profile 的样本。真 CC 路径不补；模拟路径跟随注入的官方工具资产
    /// （[`crate::proxy::cc_tools_core`]）——那份资产带什么，保留下来的客户端工具就跟什么，
    /// 一条请求里不出现「注入的带、客户端的不带」这种官方从不产生的混合。
    Unknown,
}

impl CcProfile {
    /// 这个 profile 的请求带不带 `system[0]` 那条 billing header。
    pub fn has_billing_header(&self) -> bool {
        !matches!(self.system, CcSystemShape::None)
    }
}

/// 主线程与带 `system` 的辅助请求共用的顶层键序。
///
/// 主线程四族（`cap/2.1.260-2/00013`、`cap/2.1.260/00018`）、SDK 子代理（`00020`）、
/// 无工具 helper（`00024`）与标题生成（`cap/2.1.260-2/00058`）六份抓包的键序都是这一串的
/// 子序列——各自缺的键直接跳过，共有键一个都没挪位。`temperature` 只在后两者出现，
/// 排在 `thinking` 之后；主线程从不发它，故它相对 `context_management` 的位置无从观测，
/// 取「紧跟 thinking」这个唯一有证据的落点。
pub const CC_BODY_ORDER_MAIN: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "metadata",
    "max_tokens",
    "thinking",
    "temperature",
    "context_management",
    "fallbacks",
    "output_config",
    "diagnostics",
];

/// 2.1.270 主线程 sonnet 的顶层键序（`cap/2.1.270/00017`、`00025`）：比 [`CC_BODY_ORDER_MAIN`]
/// 多一个 `thread`，落在 `output_config` 与 `diagnostics` 之间；其余共有键一个没挪位。
/// `temperature` / `fallbacks` 这两条抓包都不发，位置沿用主线程那串。
pub const CC_BODY_ORDER_MAIN_2_1_270: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "metadata",
    "max_tokens",
    "thinking",
    "temperature",
    "context_management",
    "fallbacks",
    "output_config",
    "thread",
    "diagnostics",
];

/// 2.1.280 主线程的顶层键序（`cap/2.1.280/00021`、`00029`、`00033` opus / fable / sonnet，
/// `00038` haiku）：比 [`CC_BODY_ORDER_MAIN_2_1_270`] 多一个 `safeguards`，落在
/// `context_management` 与 `output_config` 之间（haiku 不发它，`thread` 仍在 `diagnostics` 前）。
/// 模拟路径自己不造 `safeguards`（见 [`cc_beta_dangerous_tool_use`]），列进来是给真 CC 来访被
/// 送进模拟时那份归位——不认识的键会被挪到 `stream` 前面，那是官方从不产生的位置。
/// `fallbacks` 与它的先后没有样本，沿用 `fallbacks` 紧跟 `context_management` 那一格。
pub const CC_BODY_ORDER_MAIN_2_1_280: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "metadata",
    "max_tokens",
    "thinking",
    "temperature",
    "context_management",
    "fallbacks",
    "safeguards",
    "output_config",
    "thread",
    "diagnostics",
];

/// 安全分类请求的顶层键序（`cap/2.1.260/00019`、`00030`）：**和主线程完全不同**，
/// `max_tokens` 排在第二、`system` 在 `messages` 前面。不能拿主线程那串硬套。
pub const CC_BODY_ORDER_CLASSIFIER: &[&str] =
    &["model", "max_tokens", "system", "messages", "stop_sequences", "thinking", "metadata"];

/// 额度探测的顶层键序（`cap/2.1.260-2/00004`、`00021`、`00047`）。
pub const CC_BODY_ORDER_QUOTA: &[&str] = &["model", "max_tokens", "messages", "metadata"];

/// 主线程模型的**代际**：2.1.285 同一族内不同模型的 beta 串只差这几档（`cap/2.1.285`，
/// 11 个模型逐条核过，见 [`CC_PROFILES`]）。profile 表里记的是每族最新一代的全集，
/// [`cc_model_beta`] 按代际从全集里去掉 [`Self::dropped_betas`]，其余项与顺序一个不动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcModelTier {
    /// opus-5-5、fable-5-1、sonnet-5-5，以及认不出版本的模型：全集照发。
    Latest,
    /// opus-5、fable-5、opus-4-8、sonnet-5：少 `per-turn-control`。
    Gen5,
    /// opus-4-7、opus-4-6、sonnet-4-6 及更老：`mid-conversation-system` 一系（连同
    /// `-clear-at`）、`per-turn-control`、`mid-conversation-tool-changes` 全不发。这几个模型的
    /// 官方请求里也确实没有 `role: "system"` 的消息，环境说明整段落在首条 user 消息的
    /// `<system-reminder>` 里——模拟路径本来就是这么放的（[`crate::proxy::stash_client_system`]）。
    Legacy,
}

impl CcModelTier {
    /// 这一代比全集少发的 beta。
    pub fn dropped_betas(self) -> &'static [&'static str] {
        match self {
            Self::Latest => &[],
            Self::Gen5 => &[CC_BETA_PER_TURN_CONTROL],
            Self::Legacy => &[
                "mid-conversation-system-2026-04-07",
                CC_BETA_PER_TURN_CONTROL,
                "mid-conversation-tool-changes-2026-07-01",
                "mid-conversation-system-clear-at-2026-08-21",
            ],
        }
    }
}

/// 模型名 → 代际，见 [`CcModelTier`]。版本号取族名后面的数字段（`opus-4-8` → 4.8、`opus-5` →
/// 5.0，八位日期段不算），族名在后的老写法（`claude-3-7-sonnet-…`）取族名前面的。
///
/// 分界按族定：opus 5.5 起 `Latest`、4.8 起 `Gen5`；fable 5.1 起 `Latest`、其余 `Gen5`（fable
/// 没有更老的一代）；sonnet 5.5 起 `Latest`、5 起 `Gen5`。haiku 的串里本来就没有这几项，恒
/// `Latest`。读不出版本的一律 `Latest`——与此前「按族一份串」的行为相同，比猜成老一代少一项
/// 更接近新模型的实情。
pub fn cc_model_tier(model: &str) -> CcModelTier {
    let m = model.to_ascii_lowercase();
    let tokens: Vec<&str> = m.split(['-', '_', '.', '@', '[', '/']).collect();
    let Some(at) = tokens.iter().position(|t| matches!(*t, "opus" | "fable" | "sonnet" | "haiku"))
    else {
        return CcModelTier::Latest;
    };
    // 一段版本号：1~2 位纯数字（日期段八位，不算）。
    let num = |t: &str| (!t.is_empty() && t.len() <= 2).then(|| t.parse::<u32>().ok()).flatten();
    let after: Vec<u32> = tokens[at + 1..].iter().map_while(|t| num(t)).take(2).collect();
    let before: Vec<u32> = tokens[..at].iter().rev().map_while(|t| num(t)).collect();
    let version = if !after.is_empty() {
        (after[0], after.get(1).copied().unwrap_or(0))
    } else if !before.is_empty() {
        let mut b = before;
        b.reverse();
        (b[0], b.get(1).copied().unwrap_or(0))
    } else {
        return CcModelTier::Latest;
    };
    let (latest, gen5) = match tokens[at] {
        "opus" => ((5, 5), (4, 8)),
        "fable" => ((5, 1), (0, 0)),
        "sonnet" => ((5, 5), (5, 0)),
        _ => return CcModelTier::Latest,
    };
    if version >= latest {
        CcModelTier::Latest
    } else if version >= gen5 {
        CcModelTier::Gen5
    } else {
        CcModelTier::Legacy
    }
}

/// 模拟路径给这个模型发的 beta 串：`profile.beta` 按 [`cc_model_tier`] 去掉那一代不发的项。
/// 只动主线程三族（opus / fable / sonnet）；haiku、额度探测等其余 profile 原样返回——代际的
/// 证据只有这三族的主线程。
pub fn cc_model_beta(profile: &CcProfile, model: &str) -> std::borrow::Cow<'static, str> {
    let main3 = matches!(
        profile.kind,
        CcProfileKind::MainOpus | CcProfileKind::MainFable | CcProfileKind::MainSonnet
    );
    let dropped = if main3 { cc_model_tier(model).dropped_betas() } else { &[] };
    if dropped.is_empty() {
        return std::borrow::Cow::Borrowed(profile.beta);
    }
    std::borrow::Cow::Owned(
        profile
            .beta
            .split(',')
            .map(str::trim)
            .filter(|b| !b.is_empty() && !dropped.contains(b))
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// 按 kind 取**当前模拟版本**（2.1.285，[`CC_PROFILES`]）的 profile；那张表没编的 kind 依次
/// 落回 [`CC_PROFILES_2_1_280`]、[`CC_PROFILES_2_1_277`]（SDK 子代理）与 [`CC_PROFILES_2_1_260`]
/// （无工具 helper、安全分类）。表是常量，三张都查不到即编译期就漏写了一行，故直接兜底到 `MainOpus`
/// 而不是返回 `Option`——调用点没有「没有 profile」这种状态可处理。
///
/// 模拟路径只发主线程四族，这四行在 2.1.285 表里都有；落回旧表的 kind 只用作认来访形态的参照。
pub fn cc_profile(kind: CcProfileKind) -> &'static CcProfile {
    CC_PROFILES
        .iter()
        .chain(CC_PROFILES_2_1_280)
        .chain(CC_PROFILES_2_1_277)
        .chain(CC_PROFILES_2_1_260)
        .find(|p| p.kind == kind)
        .unwrap_or(&CC_PROFILES[0])
}

/// 某 kind 在**所有**版本表里的行（从旧到新）。给「带齐任一版官方形态」这类判据用——
/// 探针判定那条路上拿不到来访版本（[`crate::proxy::is_official_helper_request`]）。
pub fn cc_profile_rows(kind: CcProfileKind) -> impl Iterator<Item = &'static CcProfile> {
    cc_profile_tables().into_iter().flat_map(|t| t.iter()).filter(move |p| p.kind == kind)
}

/// 某 kind 在**恰好**这一版（形如 `2.1.277`）上的 profile 行；没有就 `None`。
#[cfg(test)]
pub fn cc_profile_exact(kind: CcProfileKind, version: &str) -> Option<&'static CcProfile> {
    cc_profile_tables()
        .iter()
        .flat_map(|t| t.iter())
        .find(|p| p.kind == kind && p.version == version)
}

/// 六张 profile 表，按版本从旧到新。
pub(super) fn cc_profile_tables() -> [&'static [CcProfile]; 6] {
    [
        CC_PROFILES_2_1_258,
        CC_PROFILES_2_1_260,
        CC_PROFILES_2_1_270,
        CC_PROFILES_2_1_277,
        CC_PROFILES_2_1_280,
        CC_PROFILES,
    ]
}

/// 按 kind **与来访自报的版本**取 profile。
///
/// `version` 是 `(major, minor, patch)`，来自客户端 UA（`claude-cli/x.y.z`）。分档：
/// - 低于 2.1.260 取 [`CC_PROFILES_2_1_258`]；**读不出版本时也取旧那份**——绝大多数在跑的
///   客户端还不是 2.1.260，猜新的一版等于给它们集体换一套形态；
/// - 2.1.285 及以上查 [`CC_PROFILES`]，没行的 kind 依次再查 2.1.280、2.1.277 表；
/// - 2.1.280 ~ 2.1.284 查 [`CC_PROFILES_2_1_280`]，没行的 kind（子代理、标题）再查 2.1.277 表；
/// - 2.1.277 ~ 2.1.279 查 [`CC_PROFILES_2_1_277`]；
/// - 2.1.270 ~ 2.1.276 先查 [`CC_PROFILES_2_1_270`]，那张表里只有抓到样本的 kind；
/// - 其余（2.1.260 ~ 2.1.269，以及各版本表里没有样本的 kind）取 [`CC_PROFILES_2_1_260`]。
///
/// 只有主线程四族有 2.1.258 行、只有 sonnet 有 2.1.270 行，其余 kind 一律落回 2.1.260 那张表。
///
/// 「2.1.270 及以上」而不是「恰为 2.1.270」：抓不到每一个小版本，新客户端来了先按最近一份
/// 已证的形态处理，比退回两版之前的表离真相更近。此前所有 ≥2.1.260 都套 2.1.260 表也是这个
/// 思路，只是那张表的 sonnet 行把 2.1.270 已经不发的 `server-side-fallback` / `fallback-credit`
/// 又补回了一条**完整的**订阅端请求——见 [`crate::proxy::merge_beta_for`] 的测试
/// `merged_beta_is_idempotent_on_2_1_270_sonnet`。
pub fn cc_profile_at(kind: CcProfileKind, version: Option<(u64, u64, u64)>) -> &'static CcProfile {
    let find = |table: &'static [CcProfile]| table.iter().find(|p| p.kind == kind);
    // 2.1.260 表是所有版本的最后兜底：它是唯一编全了九个 kind 的一张。
    let at_260 = || find(CC_PROFILES_2_1_260).unwrap_or(&CC_PROFILES_2_1_260[0]);
    match version {
        Some(v) if v >= (2, 1, 285) => find(CC_PROFILES)
            .or_else(|| find(CC_PROFILES_2_1_280))
            .or_else(|| find(CC_PROFILES_2_1_277))
            .unwrap_or_else(at_260),
        Some(v) if v >= (2, 1, 280) => {
            find(CC_PROFILES_2_1_280).or_else(|| find(CC_PROFILES_2_1_277)).unwrap_or_else(at_260)
        }
        Some(v) if v >= (2, 1, 277) => find(CC_PROFILES_2_1_277).unwrap_or_else(at_260),
        Some(v) if v >= (2, 1, 270) => find(CC_PROFILES_2_1_270).unwrap_or_else(at_260),
        Some(v) if v >= (2, 1, 260) => at_260(),
        _ => find(CC_PROFILES_2_1_258).unwrap_or_else(at_260),
    }
}

/// 某 kind 在**恰好**这一版上的 eager 证据（[`CcEagerTools`]）：三张表里找 `kind` 相同、
/// `version` 与来访自报版本逐段相等的那一行；没有就是 [`CcEagerTools::Unknown`]。
///
/// 不走 [`cc_profile_at`]：那条会把 2.1.270 的 opus 落到 2.1.260 的行上，beta 参照需要这种
/// 兜底，eager 的证据不需要——见 [`CcEagerTools`] 的说明。读不出版本同样 Unknown。
/// 2.1.277 四族主线程与子代理都是 On（`cap/2.1.277` 每条内建工具都带）。
pub fn cc_eager_tools_at(kind: CcProfileKind, version: Option<(u64, u64, u64)>) -> CcEagerTools {
    let Some(version) = version else { return CcEagerTools::Unknown };
    cc_profile_tables()
        .iter()
        .flat_map(|t| t.iter())
        .find(|p| p.kind == kind && crate::proxy::parse_version(p.version) == Some(version))
        .map_or(CcEagerTools::Unknown, |p| p.eager_tools)
}
