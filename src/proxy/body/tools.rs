//! 工具声明：注入官方工具、schema 拍平、去重与 eager 工具流式。

use super::*;

/// 这条请求的客户端工具该不该补 `eager_input_streaming: true`。证据与规则见
/// [`config::CcEagerTools`]，两条路径的**条件来源不同**：
///
/// - **真 CC 路径**（`sim` 为 `None`、来访是 CC 形态）：按来访**自报的版本 × 模型**查证据
///   （[`config::cc_eager_tools_at`]，版本要**精确命中**抓包那一版，不沿 beta 参照那套
///   「落回最近一版」的兜底——2.1.270 的 opus 没样本就不补），且用途得是主线程或猜下一句（[`CcRequestKind::Main`] /
///   [`CcRequestKind::Suggestion`]——2.1.258/00025 那条猜下一句同样全带；子代理 / helper /
///   标题 / 分类没有主线程的证据，不猜），且出站头里真有 `advanced-tool-use`（带 eager 的官方
///   请求头上都有它，API-key 端两样都没有，只补体不补头就是另一个官方不产生的组合）。
///   读不出版本一律不补——不知道是哪一版就没法查表。
/// - **模拟路径**：按**出站的模拟 profile** 判，与来访客户端自报什么版本无关（出站 UA 是
///   profile 那一版）。profile 记了 On/Off 就照记的；[`config::CcEagerTools::Unknown`]
///   （2.1.260 的 sonnet / haiku 外推行）跟随注入的那份官方工具资产（[`cc_tools_core`]）——
///   资产带则客户端保留的工具也带，一条请求里不出现「注入的带、客户端的不带」。只给主线程
///   profile 判（`has_billing_header`，与注入同一道闸）。
pub(super) fn eager_tools_wanted(
    v: &serde_json::Value,
    sim: Option<&Simulation>,
    cc_inbound: bool,
    cc_kind: CcRequestKind,
    client_version: Option<&str>,
    adv_beta: bool,
) -> bool {
    use config::CcEagerTools::{Off, On, Unknown};
    match sim {
        Some(sim) => {
            let profile = sim.profile;
            if !profile.has_billing_header()
                || !profile.beta.split(',').any(|b| b.trim() == config::CC_BETA_ADVANCED_TOOL_USE)
            {
                return false;
            }
            match profile.eager_tools {
                On => true,
                Off => false,
                Unknown => {
                    let asset = cc_tools_core(profile, false);
                    !asset.is_empty()
                        && asset.iter().all(|t| {
                            t.get("eager_input_streaming").and_then(|e| e.as_bool()) == Some(true)
                        })
                }
            }
        }
        None => {
            if !cc_inbound
                || !adv_beta
                || !matches!(cc_kind, CcRequestKind::Main | CcRequestKind::Suggestion)
            {
                return false;
            }
            let Some(version) = client_version.and_then(parse_version) else { return false };
            let Some(model) = v.get("model").and_then(|m| m.as_str()) else { return false };
            config::cc_eager_tools_at(cc_profile_kind_for(model), Some(version)) == On
        }
    }
}

/// 给客户端声明的**内建形态**工具补 `eager_input_streaming: true`。该不该补由
/// [`eager_tools_wanted`] 定，这里只管「补到哪些工具上」，两条路径共用：
///
/// - 客户端已写了这个键（`true` 或 `false`）的不覆盖——那是它的显式设置；
/// - 只补**有 `input_schema`、名字不以 `mcp__` 开头、没有 `defer_loading: true`** 的工具：
///   订阅端样本里 MCP 工具全在延迟池里、正文里一个没有，带不带无从证实；`DeferredToolPlaceholder`
///   占位在每条样本里都不带；服务端工具（`type: web_search_…`）没有 `input_schema`，也没有
///   带着 eager 的样本。没有证据的工具类型不猜。
///
/// 键追加在对象末尾：官方声明序是 `name, description, input_schema, eager_input_streaming`
/// （`cap/2.1.258/00012` 每一条）。
pub(in crate::proxy) fn fill_eager_tools(v: &mut serde_json::Value) -> bool {
    let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) else { return false };
    let mut changed = false;
    for tool in tools.iter_mut() {
        let Some(obj) = tool.as_object_mut() else { continue };
        let builtin_name =
            obj.get("name").and_then(|n| n.as_str()).is_some_and(|n| !n.starts_with("mcp__"));
        let deferred = obj.get("defer_loading").and_then(|d| d.as_bool()) == Some(true);
        if !builtin_name
            || deferred
            || !obj.contains_key("input_schema")
            || obj.contains_key("eager_input_streaming")
        {
            continue;
        }
        obj.insert("eager_input_streaming".into(), serde_json::Value::Bool(true));
        changed = true;
    }
    changed
}

/// 模拟路径下注入的官方主线程工具声明（opus / sonnet / fable）：14 条，与
/// `cap/auto-2.1.291-20261006-full` 默认权限模式的主线程（`00340` / `00216` opus-5-5 ↔ `00253`
/// sonnet-5-5）逐字节相同。haiku 那份见 [`CC_TOOLS_CORE_HAIKU`]。
///
/// **为什么要注入**：上游判第三方的信号之一是「自称 CC 但没有 CC 工具」。光把客户端自有
/// 工具名加 `mcp__` 前缀不够——那只是消去负面信号（被 blocklist 的名字），而正面信号
/// （存在 CC 官方工具声明）仍然缺失。注入之后请求的工具组合是「CC 内建 + MCP 扩展」，
/// 与真实 CC 接 MCP server 的形态一致（`cap/2.1.258-api/00006`：内建在前，`mcp__*` 在尾）。
///
/// **为什么是 14 个而不是 4 个**：2.1.285 / 2.1.291 四族主线程抓包（`cap/2.1.285/00030` / `00039` /
/// `00045` / `00051`、`cap/auto-2.1.291-20261006-full/00340` / `00303`）每条都带 16 ~ 20 个工具，
/// 其中四族共有的内建工具 15 个：下面这 14 个
/// 真工具，加上 `ToolSearch`；其余是延迟池里的 `DeferredToolPlaceholder` 占位、用户自己的
/// MCP 工具（`mcp__claude_ai_Claude_Docs__*`）与 opus 独有的服务端工具 `advisor`
/// （`type: advisor_20260301`，没有 `input_schema`）。`TaskStop` 等其余内建工具在延迟清单里，
/// 不在正文声明中。只带 Bash/Edit/Read/Write 是一个
/// 官方不产生的组合，与「零个工具」一样是自证。
/// 那一对**故意不注**：`ToolSearch` 被模型调起来时客户端拿到一个自己没声明的
/// tool_use 且没法执行，而 `DeferredToolPlaceholder` 只是它的占位；两者都不是「工具」。第四块
/// 模板里依赖它的那段指令也一并去掉了（[`config::CC_SYSTEM_REST`]）。`advisor` 是服务端在
/// 模型之外再跑一个模型，要另计费、也改变行为，不替客户端拨这个开关。
///
/// **顺序也是抓包的一部分**：按官方声明序 `Agent → Artifact → AskUserQuestion → Bash → Edit →
/// ListAgents → Read → ReportFindings → ScheduleWakeup → SendFeedback → ShareOnboardingGuide →
/// Skill → Workflow → Write`（官方在 `Skill` 与 `Workflow` 之间还有 `ToolSearch`、`Workflow`
/// 与 `Write` 之间还有 MCP 工具与占位），不是字母序。
///
/// **Bash 取默认权限模式那版**（3018 字节）：同一版里 auto 模式会话的 opus / fable 发的是少一句
/// 「别用 Bash 跑 cat/head/tail……」的 2707 字节那版（`00032`、`00464`），默认模式（`00340`、
/// `00216`）与 sonnet 两种模式下都是 3018 那版。模拟请求不带 `safeguards` / `afk-mode`、遥测报
/// `default`，工具描述跟着默认模式走（2.1.285 的资产取的是 auto 模式那版，这一版改正）。
///
/// **`eager_input_streaming`**：2.1.277 起四族的 OAuth 主线程每个内建工具都带
/// `eager_input_streaming: true`——fable 也带（2.1.260 时一个都不带），2.1.291 仍如此。资产原样保留，
/// 不另加也不剥。客户端改名后的 `mcp__luban__*` 带不带这个键**没有 OAuth 样本**
/// （`2.1.258-api` 的 `mcp__ide__*` 不带，但那是 API-key 模式；2.1.277 订阅端的 MCP 工具在
/// 延迟池里带 `eager_input_streaming: true` 加 `defer_loading: true`，与正文声明不是一回事），
/// 这里不猜。
///
/// **模型会不会调这些工具**：概率低。客户端的 system prompt 会指名自己的工具
/// （被混淆成 `mcp__luban__*`），模型优先响应 system 的指令。万一调了，客户端收到一个
/// 自己没声明的 tool_use，按协议返回错误 tool_result 即可，不影响会话继续。
///
/// **代价**：资产约 71KB（Artifact 一条就 34KB），每条模拟主线程请求都带，首轮进 prompt cache
/// 之前按输入 token 计费；同一会话后续轮次命中缓存。开关 `sim_trim_tools` 去掉其中三条
/// （[`CC_TRIMMED_TOOLS`]），约剩 29KB。
static CC_TOOLS_CORE: std::sync::LazyLock<Vec<serde_json::Value>> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../../assets/cc_tools_core.json"))
            .expect("cc_tools_core.json must be a valid JSON array of tool objects")
    });

/// haiku 主线程的 14 条（2.1.291 起与另外三族分家）：`cap/auto-2.1.291-20261006-full/00303`、`00553`
/// 逐字节相同，配 [`config::CC_SYSTEM_BASE_HAIKU`] 那套长提示词——`Agent` / `Bash` / `Edit` /
/// `Read` / `Write` / `AskUserQuestion` 六条是长描述（`Bash` 11913 字节），其余八条与
/// [`CC_TOOLS_CORE`] 逐字节相同。名字与先后同那份（官方 haiku 的 `DeferredToolPlaceholder` 夹在
/// `Workflow` 与 `Write` 之间，注入时本来就不带它）。
static CC_TOOLS_CORE_HAIKU: std::sync::LazyLock<Vec<serde_json::Value>> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../../assets/cc_tools_core_haiku.json"))
            .expect("cc_tools_core_haiku.json must be a valid JSON array of tool objects")
    });

/// 开关 `sim_trim_tools` 打开时不注的三条：官方客户端里它们都能由**用户自己**关掉，关掉之后
/// 主线程正文少的正是这三条、其余 11 条与 system 逐字节不变（`cap/auto-2.1.291-20261006` 四族
/// 关前 / 关后各一条：`00031` ↔ `00066`、`00100` ↔ `00136`、`00172` ↔ `00205`、`00242` ↔ `00276`）：
///
/// | 工具 | 字节 | 官方怎么关 |
/// |---|---:|---|
/// | `Artifact` | 34399（2.1.293 改了一段措辞，34605） | 环境变量 `CLAUDE_CODE_DISABLE_ARTIFACT=1`，或设置 `enableArtifact: false` |
/// | `ListAgents` | 1180 | 环境变量 `CLAUDE_CODE_HARBOR_KITE=0` |
/// | `SendFeedback` | 5537 | 环境变量 `CLAUDE_CODE_SEND_FEEDBACK=0`，或设置 `feedbackDrafts: "off"` |
///
/// 只收用户侧能关的：`ReportFindings` 无条件进工具表，`ShareOnboardingGuide` 只能由组织策略与
/// 服务端开关关，`TaskStop` 进不进正文也由服务端开关定——这几样少了是「服务端没给这个号下发」，
/// 而服务端知道它给每个号下发了什么，模拟不了。遥测那侧照官方关掉后的形态改，见
/// `crate::telemetry` 的 `trimmed_tools`。
pub(in crate::proxy) const CC_TRIMMED_TOOLS: &[&str] = &["Artifact", "ListAgents", "SendFeedback"];

static CC_TOOLS_CORE_TRIM: std::sync::LazyLock<Vec<serde_json::Value>> =
    std::sync::LazyLock::new(|| trimmed(&CC_TOOLS_CORE));
static CC_TOOLS_CORE_HAIKU_TRIM: std::sync::LazyLock<Vec<serde_json::Value>> =
    std::sync::LazyLock::new(|| trimmed(&CC_TOOLS_CORE_HAIKU));

fn trimmed(all: &[serde_json::Value]) -> Vec<serde_json::Value> {
    all.iter()
        .filter(|t| !t["name"].as_str().is_some_and(|n| CC_TRIMMED_TOOLS.contains(&n)))
        .cloned()
        .collect()
}

/// 注入用的工具声明（2.1.293，主线程 17 / 16 条里去掉 `ToolSearch` / `DeferredToolPlaceholder`
/// 那一对与服务端工具 `advisor` 之后的 14 条；MCP 工具 2.1.293 起已不在 `tools` 里）：haiku-4.5 一份
/// （[`CC_TOOLS_CORE_HAIKU`]），其余——含 haiku-5-5——同一份（[`CC_TOOLS_CORE`]，
/// `cap/auto-2.1.293-20261008-full/00344` 与 `00419` 的工具逐字节相同）。2.1.293 的 `Agent` 多了
/// `effort` 参数，`Artifact` 改了一段措辞，其余逐字未变。`trim` 是开关
/// `sim_trim_tools`，开着去掉 [`CC_TRIMMED_TOOLS`] 那三条、剩 11 条，先后不变。
pub(in crate::proxy) fn cc_tools_core(
    profile: &config::CcProfile,
    trim: bool,
) -> &'static [serde_json::Value] {
    match (profile.kind == config::CcProfileKind::MainHaiku, trim) {
        (true, false) => &CC_TOOLS_CORE_HAIKU,
        (true, true) => &CC_TOOLS_CORE_HAIKU_TRIM,
        (false, false) => &CC_TOOLS_CORE,
        (false, true) => &CC_TOOLS_CORE_TRIM,
    }
}

/// [`inject_cc_tools`] 会往这条请求里**补**哪几个工具名（客户端没声明的那些）；**不改体**。
///
/// 只给注入那一步用（[`inject_cc_tools`]，此时 `tool_choice` 已归一）。流水**不**拿它预判：
/// 那边读的是来访原文，`tool_choice: "required"` 这类方言还没归一，会算错——流水改按实际出站
/// 体对来访算（[`injected_tools_of`]）。
///
/// **不带工具的来访**（[`declares_no_tools`]：没有 `tools` 键、`tools: null`、`tools: []` 三种
/// 一视同仁）只在 `fill_absent`（开关 `fill_absent_tools`，默认开）开着时按全缺算，且来访的
/// `tool_choice` 是 `any` / 指定工具时不补（[`forces_tool_use`]）。理由：模拟路径只发主线程
/// profile，而官方主线程一条不带工具的样本都没有（`cap/2.1.280` 主线程恒为 19 / 20 个），「主线程
/// 的 beta 与 system、零个工具」是官方不产生的组合。已知代价：这类来访多半是没有工具循环的纯
/// 聊天客户端，模型调了注入的工具时它拿到的是一个处理不了的 `tool_use`（流水 `rewrites` 列打
/// `injected_tool_called`；补了工具的请求本身打 `tools_filled`；算调用率时分子要数**两个标签都有**
/// 的——单数 `injected_tool_called` 还混着自带工具、只被补缺的客户端）；每个新
/// 会话首轮还要按写入价多付约两万 token 的工具声明。
///
/// 返回空的情形：不带工具且开关关着或来访强制调用工具、`tools` 是数组以外的怪值、该 profile
/// 的每个工具名客户端都已声明。
///
/// **客户端已带部分官方名时照样补缺的**。原先「有任何一个官方名就一个都不注」，理由是
/// 「真 CC 或抄了 CC 声明的中转，别动」——但真 CC 不走模拟路径，会走到这里的是抄了一部分的：
/// 现网一条 Go-http-client 声明了 15 个官方名，其中 TaskCreate / TaskGet / TaskUpdate /
/// TaskList 是 2.1.258 API-key 端才有的拼法，2.1.260 恒带的 11 个里又缺 ListAgents /
/// ReportFindings / ScheduleWakeup / Workflow 四个，出站 UA 却自报 2.1.260。「自称 2.1.260、
/// 工具集是上一版的拼法、还缺四个恒带的」这个组合官方同样不产生。只加不删：老版本多出来
/// 的那几个是客户端的能力，删了是改它的行为。
///
/// **不能借 [`has_cc_tool_profile`] 判**：那个函数回答的是「这看起来像不像真的 CC 客户端」，
/// 对「没带 tools」和「`tools: []`」都答**是**（判不出来就不冤枉人）。而这里问的是「这条
/// 请求缺哪些官方工具」——空数组的答案显然是**全缺**。借用之后，一条 `tools: []` 的
/// 主线程请求就永远注不进工具，正是「零个 CC 工具等于自证不是 CC」那个要消灭的形态。
pub(in crate::proxy) fn cc_tools_to_inject(
    v: &serde_json::Value,
    profile: &config::CcProfile,
    fill_absent: bool,
    trim: bool,
) -> Vec<&'static str> {
    if declares_no_tools(v) && (!fill_absent || forces_tool_use(v)) {
        return Vec::new();
    }
    let declared: Vec<&str> = match v.get("tools") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(tools)) => {
            tools.iter().filter_map(|t| t.get("name")?.as_str()).collect()
        }
        Some(_) => return Vec::new(),
    };
    cc_tools_core(profile, trim)
        .iter()
        .filter_map(|stub| stub.get("name")?.as_str())
        .filter(|name| !declared.contains(name))
        .collect()
}

/// 模拟路径**实际**注进出站体、来访自己没声明的官方工具名：出站 `tools` 里属于该 profile
/// 官方资产（[`cc_tools_core`]）的名字，减去来访 `tools` 里已有的名字。
///
/// 流水按它记 `injected_tool_called` / `tools_filled`，不再拿来访体预判
/// （[`cc_tools_to_inject`]）：来访的 `tool_choice: "required"` / `"any"` / OpenAI 的
/// `{"type":"function"}` 要先被 [`normalize_tool_choice`] 归一成 `any` / `tool`，注入那一步才
/// 据此不补；读来访原文会把这类请求误记成「补了」。官方工具名不参与假名混淆，出站里认得出。
pub(in crate::proxy) fn injected_tools_of(
    inbound: &serde_json::Value,
    outbound: &serde_json::Value,
    profile: &config::CcProfile,
) -> Vec<&'static str> {
    let names = |v: &serde_json::Value| -> Vec<String> {
        v.get("tools")
            .and_then(|t| t.as_array())
            .map(|a| a.iter().filter_map(|t| t.get("name")?.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    let (declared, sent) = (names(inbound), names(outbound));
    // 取完整那份：开关 `sim_trim_tools` 开着时出站里没有那三条，按完整的数也一样。
    cc_tools_core(profile, false)
        .iter()
        .filter_map(|stub| stub.get("name")?.as_str())
        .filter(|n| sent.iter().any(|s| s == n) && !declared.iter().any(|d| d == n))
        .collect()
}

/// 来访一个工具都没声明：没有 `tools` 键、`tools: null` 或 `tools: []`。三种在上游眼里都是
/// 「没有工具」，开关 `fill_absent_tools` 对它们一视同仁（[`cc_tools_to_inject`]）。
pub(in crate::proxy) fn declares_no_tools(v: &serde_json::Value) -> bool {
    match v.get("tools") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    }
}

/// 来访的 `tool_choice` 要求模型**必须**调工具（`any` 或指定某个工具）。配上「一个工具都没
/// 声明」这条请求本来就不成立（上游 400）；替它补官方工具等于逼模型去调 Bash 之类，把客户端
/// 自己的错误换成一次它接不住的工具调用。`auto` / `none` 不算。
fn forces_tool_use(v: &serde_json::Value) -> bool {
    matches!(
        v.get("tool_choice").and_then(|c| c.get("type")).and_then(|t| t.as_str()),
        Some("any" | "tool")
    )
}

/// 这条请求是谁发的——**只用于日志**，让「某个客户端的 Bash 被换成官方声明了」对得回是谁。
///
/// 模拟路径上能拿到的身份就这两项：服务它的凭证，与 luban 给它派生的会话 id（流水里
/// `cc_session` 记的是同一个串，据此能把那一行日志翻回具体哪条请求）。来访 UA 在这儿拿不到
/// ——出站头里那个已经是伪装成官方 CC 的那份了。
#[derive(Clone, Copy)]
pub(in crate::proxy) struct ToolAlignWho<'a> {
    pub(in crate::proxy) cred_id: i64,
    pub(in crate::proxy) cred: &'a str,
    pub(in crate::proxy) session: &'a str,
}

/// 客户端自带的同名工具与官方声明的**参数表面**是否一致：`input_schema.properties` 的键集相同、
/// `required` 的集合相同。
///
/// 只用于日志，不再决定换不换。换是一律换的（见 [`inject_cc_tools`]）；这个判据标出的是换了
/// 之后**可能**执行不了的那几条——客户端若要一个官方 schema 里没有的必填参数，模型按官方
/// schema 永远不会给。那种客户端的工具在换之前本来也和官方不是一回事，出了问题至少日志里
/// 点得出名字。
pub(super) fn same_schema_surface(
    client: &serde_json::Value,
    official: &serde_json::Value,
) -> bool {
    fn surface(t: &serde_json::Value) -> Option<(Vec<&str>, Vec<&str>)> {
        let schema = t.get("input_schema")?.as_object()?;
        let mut props: Vec<&str> = match schema.get("properties") {
            Some(p) => p.as_object()?.keys().map(String::as_str).collect(),
            None => Vec::new(),
        };
        let mut required: Vec<&str> = match schema.get("required") {
            Some(r) => r.as_array()?.iter().filter_map(|x| x.as_str()).collect(),
            None => Vec::new(),
        };
        props.sort_unstable();
        required.sort_unstable();
        Some((props, required))
    }
    matches!((surface(client), surface(official)), (Some(a), Some(b)) if a == b)
}

/// [`inject_cc_tools`] 会不会给这条补官方工具：声明了工具就补；一个都没声明（[`declares_no_tools`]）
/// 时看开关 `fill_absent_tools`，且来访没有强制调某个工具。首轮环境说明（[`insert_env_note`]）按同一个判据
/// 决定补不补——它列的 Agent 类型与技能离不开那两个工具。
pub(super) fn injects_cc_tools(v: &serde_json::Value, fill_absent: bool) -> bool {
    !(declares_no_tools(v) && (!fill_absent || forces_tool_use(v)))
}

/// 把该 profile 的 14 个官方主线程工具对齐进 `tools`：出站列表**以这 14 条按官方声明序开头**，
/// 每一条都是资产里那个对象（客户端没声明的是补的，声明了同名的是换的），客户端其余工具
/// 跟在后面、相对次序不变。补哪几个由 [`cc_tools_to_inject`] 定，流水那侧对的也是这一份。
///
/// **同名为什么一律换**：会走到这里的客户端本来就在模拟路径上，它用了官方名却自己写描述、
/// 自己拼 schema，这条声明与官方的差别正是上游最容易盯的形态之一（官方客户端连
/// `toolSchemaCharLengths` 都逐条上报）。既然整条请求已在按官方形态重建，同名工具留一份
/// 自己写的版本只是留一处破绽。代价：参数表面与官方不一致的客户端（多要一个必填参数之类），
/// 模型按官方 schema 拼的入参它可能不认——换之前 [`same_schema_surface`] 把这些名字打进
/// 日志，出了事对得上是哪个客户端的哪条工具。
///
/// **为什么不是原位换、缺的插头部**：那样客户端只缺 ListAgents 等四个时，补的四个全排在
/// Agent 前面，14 条的相对次序就不是官方的了。官方的内建工具是一段固定次序，MCP 工具跟在
/// 最后（`cap/2.1.258-api/00006`：`Write` 之后才是 `mcp__ide__*`），这里照这个形态排。
///
/// **同名声明里显式写的 `eager_input_streaming` 保留**：那是客户端的设置（true 或 false 都是），
/// 换成官方对象时不能顺手抹掉（fable 资产没有这个键）或改成资产的值（opus 资产是 true）。
/// 与 [`fill_eager_tools`]「已有值不覆盖」是同一条约定，只是这里发生在替换那一步。其余字段
/// 一律取资产的。
///
/// **换不换不看 JSON 值相等**：`Value` 的相等忽略对象键序，客户端一条内容全同、键序不同的
/// 声明会被当成「已经是官方的」跳过，出站就不是逐字节的官方声明了。故 14 条一律以资产对象
/// 落位，「有没有变」按紧凑序列化的字节比——这只影响日志计数与 [`rewrite_body`] 那条
/// 「什么都没改就原样透传」的快路。
pub(super) fn inject_cc_tools(
    v: &mut serde_json::Value,
    profile: &config::CcProfile,
    fill_absent: bool,
    trim: bool,
    who: ToolAlignWho<'_>,
) -> bool {
    // 不带工具、且开关关着或来访强制调工具：整条不动。闸必须落在这里而不只在
    // [`cc_tools_to_inject`] 里——下面的对齐不看缺哪几个，只要有 `tools` 数组就把 14 条排到开头，
    // `tools: []` 会绕过那道判据被补满。
    if !injects_cc_tools(v, fill_absent) {
        return false;
    }
    let missing = cc_tools_to_inject(v, profile, fill_absent, trim);
    // 没带 `tools` 键或是 `null`、又要补：先按官方键序放一个空数组（`system` 之后，没有
    // `system` 就跟 `messages`；`null` 原位换掉），下面照「全缺」对齐。补不出东西时不动。
    if !missing.is_empty() && v.get("tools").is_none_or(|t| t.is_null()) {
        insert_top_level(v, "tools", serde_json::json!([]), &["model", "messages", "system"]);
    }
    let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return false;
    };
    let stubs = cc_tools_core(profile, trim);
    let official_names: Vec<&str> =
        stubs.iter().filter_map(|s| s.get("name").and_then(|n| n.as_str())).collect();
    let name_of = |t: &serde_json::Value| t.get("name").and_then(|n| n.as_str()).map(str::to_owned);

    // 客户端的同名声明与官方那条差在哪：只为日志与计数，落位一律用官方对象。
    let mut replaced = 0usize;
    let mut surface_differs: Vec<&str> = Vec::new();
    for stub in stubs {
        let Some(name) = stub.get("name").and_then(|n| n.as_str()) else { continue };
        for t in tools.iter().filter(|t| t.get("name").and_then(|n| n.as_str()) == Some(name)) {
            if serde_json::to_string(t).ok() != serde_json::to_string(stub).ok() {
                replaced += 1;
                if !same_schema_surface(t, stub) {
                    surface_differs.push(name);
                }
            }
        }
    }

    let before: Vec<Option<String>> = tools.iter().map(name_of).collect();
    let mut aligned: Vec<serde_json::Value> = stubs
        .iter()
        .map(|stub| {
            let mut out = stub.clone();
            let name = stub.get("name").and_then(|n| n.as_str());
            let explicit = tools
                .iter()
                .find(|t| t.get("name").and_then(|n| n.as_str()) == name)
                .and_then(|t| t.get("eager_input_streaming"))
                .cloned();
            if let Some(explicit) = explicit
                && let Some(obj) = out.as_object_mut()
            {
                obj.insert("eager_input_streaming".into(), explicit);
            }
            out
        })
        .collect();
    aligned.extend(
        tools
            .iter()
            .filter(|t| {
                !t.get("name").and_then(|n| n.as_str()).is_some_and(|n| official_names.contains(&n))
            })
            .cloned(),
    );
    let reordered = aligned.iter().map(name_of).collect::<Vec<_>>() != before;
    if missing.is_empty() && replaced == 0 && !reordered {
        return false;
    }
    *tools = aligned;

    // **warn 而不是 info**：这一行标的是「这条请求换完之后可能执行不了」——模型按官方 schema
    // 拼的入参，客户端那条声明不一定认。混在下面那条计数 info 里会被当成例行输出刷过去。
    if !surface_differs.is_empty() {
        // 模型名到这儿才读：`tools` 的可变借用刚随上一行结束，而这条日志本来就是少数派，
        // 不必为它在每条模拟请求上都拷一个串。
        let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("-");
        tracing::warn!(
            cred_id = who.cred_id, cred = %who.cred, session = %who.session, %model,
            tools = %surface_differs.join(","),
            "replaced same-named client tools whose parameter surface differs from the official one; \
             the model will fill the official schema, which this client may not accept"
        );
    }
    tracing::info!(
        injected = missing.len(),
        replaced,
        reordered,
        "aligned CC main-thread tool stubs for simulation"
    );
    true
}

/// `tools` 数组按 `name` 去重：保留每个名字的首次出现，丢弃后续重复声明。
/// 上游对重复名直接 400（`Tool names must be unique`），而客户端侧不一定能改。
/// 上游不支持 `input_schema` 顶层的 `allOf` / `oneOf` / `anyOf`（直接 400），
/// 这里把它们展平为一个普通 `object` schema。
///
/// - **`allOf`**：按序合并——`properties` 取并集（后覆前），`required` 取并集，其余键后覆前。
///   顶层如果还有 `type`/`properties` 等，先当第 0 块参与合并。
/// - **`oneOf` / `anyOf`**：单元素直接解包；多元素按 `allOf` 策略合并（properties 取并集，
///   required 取并集——比「丢掉所有分支」保留了更多信息）。
/// - 嵌套不管：只修顶层，深层的 `allOf` 等留给上游——它只对顶层报错。
pub(in crate::proxy) fn flatten_tool_schemas(v: &mut serde_json::Value) -> bool {
    let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for tool in tools.iter_mut() {
        let Some(schema) = tool.get_mut("input_schema").and_then(|s| s.as_object_mut()) else {
            continue;
        };
        // 取出 compound 关键字（只看顶层）。
        let compound = ["allOf", "oneOf", "anyOf"].iter().find_map(|k| schema.remove(*k));
        let Some(serde_json::Value::Array(parts)) = compound else {
            continue;
        };
        // 把当前顶层属性也算进去作为「第 0 块」。
        let mut merged = serde_json::Value::Object(std::mem::take(schema));
        for part in &parts {
            merge_schema_into(&mut merged, part);
        }
        if let Some(obj) = merged.as_object_mut() {
            obj.entry("type").or_insert_with(|| serde_json::Value::String("object".into()));
        }
        let serde_json::Value::Object(m) = merged else { continue };
        *schema = m;
        changed = true;
    }
    if changed {
        tracing::info!("flattened top-level allOf/oneOf/anyOf in tool input_schema");
    }
    changed
}

/// 把 `src` 的字段合并进 `dst`：`properties` 取并集，`required` 取并集，其余后覆前。
fn merge_schema_into(dst: &mut serde_json::Value, src: &serde_json::Value) {
    let (Some(dst_obj), Some(src_obj)) = (dst.as_object_mut(), src.as_object()) else {
        return;
    };
    for (k, v) in src_obj {
        match k.as_str() {
            "properties" => {
                let props = dst_obj
                    .entry("properties")
                    .or_insert_with(|| serde_json::Value::Object(Default::default()));
                if let (Some(existing), Some(new)) = (props.as_object_mut(), v.as_object()) {
                    for (pk, pv) in new {
                        existing.insert(pk.clone(), pv.clone());
                    }
                }
            }
            "required" => {
                let req =
                    dst_obj.entry("required").or_insert_with(|| serde_json::Value::Array(vec![]));
                if let (Some(existing), Some(new)) = (req.as_array_mut(), v.as_array()) {
                    for item in new {
                        if !existing.contains(item) {
                            existing.push(item.clone());
                        }
                    }
                }
            }
            _ => {
                dst_obj.insert(k.clone(), v.clone());
            }
        }
    }
}

pub(in crate::proxy) fn dedup_tools(v: &mut serde_json::Value) -> bool {
    let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return false;
    };
    let before = tools.len();
    let mut seen = std::collections::HashSet::new();
    tools.retain(|t| {
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or_default();
        seen.insert(name.to_string())
    });
    let removed = before - tools.len();
    if removed > 0 {
        tracing::info!(removed, "deduped tools array (duplicate tool names)");
    }
    removed > 0
}
