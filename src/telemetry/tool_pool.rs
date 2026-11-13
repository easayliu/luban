//! 延迟工具池的账本：`tengu_deferred_tools_pool_change` 那条汇总里「这一轮之前」的那几项。
//!
//! 官方（2.1.321 `ePi` / `Gxe`）每轮组装附件前把历史里每一条 `deferred_tools_delta` 依次叠一遍：
//! - 宣告过的工具名（`addedNames`，去掉 `KWe` 那几个内置名），之后的 `removedNames` / `wireHiddenNames` 从中拿掉，
//!   剩下的个数报 `priorAnnouncedCount`；
//! - 叠了几条，报 `dtdCount`；
//! - 最后一条带的「没连上的 MCP 服务器」名单（`failedMcpServers`）报 `lastFailedCount`，与这一轮的（`failedCount`）按
//!   服务器名、错误码、错误文本、重试状态逐项比，有变报 `failedChanged`。
//!
//! luban 看不到附件本身，只看得到它渲染进请求体的那几段提醒。一条附件渲染成同一条消息（mid-conversation system）或
//! 同一段 `<system-reminder>` 里的几段，客户端每次发请求前至多组装一条：故两条 assistant 之间那几条消息里出现的工具池
//! 段落合起来算一条（[`PoolDelta`]）。整段历史在体里（完整请求）就从体里叠；线程续轮的增量体只有末条回复之后那几条，
//! 历史从线程底本接（[`super::session::ThreadBase`]）。
//!
//! 只叠认得出的：摘要写法（超过 30 个按服务器归并成 `mcp__<服务器>__*`）丢了工具名，能从账本里配回的配回，配不回的
//! 不瞎填——宣告名单不准就不报 `priorAnnouncedCount`，名单截断就不比 `failedChanged`。

use std::collections::{BTreeSet, HashSet};

use serde_json::{Value, json};

/// 官方 `Zm`：逐项列出的上限，超过就改写摘要 / 截断。
const ZM: usize = 30;

/// 摘要里单项与合计的上限：比真实的 MCP 工具数大几个数量级，挡住构造出来的超大数字。
const MAX_COUNT: usize = 100_000;

/// 官方 `KWe`：这几个内置名从不算进宣告名单。
const NEVER_ANNOUNCED: [&str; 11] = [
    "Frame",
    "FrameRead",
    "TeamCreate",
    "TeamDelete",
    "SuggestBackgroundPR",
    "AutofixPr",
    "TaskOutput",
    "AgentOutputTool",
    "BashOutputTool",
    "AgentOutput",
    "BashOutput",
];

/// 宣告工具的那几段的开头一行（官方 `pQs` / `mQs` / `uQs` / `fQs` / `yQs` / `hQs`），下一行起每行一个工具名、到空行为止。
/// 刚在体里补了完整定义的（`tool_addition`，`pQs`）与只给名字的（`uQs` 等）同属一条附件的 `addedNames`：
/// `cap/auto-2.1.321-20261110-full/00756` 是 3 + 412，批次 `00760` 报 `priorAnnouncedCount: 415`。
const ANNOUNCE_HEADS: [&str; 6] = [
    "The following tools just became available and are ready to use:",
    "The following tools have updated definitions, which replace the earlier ones from here on:",
    "The following deferred tools are now available via ",
    "The following deferred tools are available via ",
    "The following tools are now available:",
    "The following tools are available again. Their definitions earlier in this conversation still apply, so call them directly:",
];

/// 「服务器断开」那段逐项列出时的开头（官方 `H` 在没有 ToolSearch 的会话里是 `tool`，否则 `deferred tool`）。
const DISCONNECTED_HEADS: [&str; 2] = [
    "The following deferred tools are no longer available (their MCP server disconnected).",
    "The following tools are no longer available (their MCP server disconnected).",
];

/// 因为别的原因走掉的那段（官方 `Dt.other`：不是 MCP 工具、或 MCP 工具被撤回 / 另有原因）；超过 30 个的服务器在清单里
/// 归并成 `mcp__<服务器>__* (<个数>)`（官方 `fgn`）。原因看不出。
const OTHER_CAUSE_HEADS: [&str; 2] = [
    "The following deferred tools are no longer available in this session.",
    "The following tools are no longer available in this session.",
];

/// 定义还在上文、被拿掉的那段（官方 `gQs`，`removedByBlock` / `leftDefinedNames`）。原因看不出。
const DEFINED_GONE_HEAD: &str = "The following tools are no longer available. Do not call them:";

/// 走掉的超过 30 个时那句摘要里数字之后、清单之前的那截（官方 `Lre`）：`<N> deferred tools are no longer available (MCP
/// server disconnected): mcp__a__* (31), mcp__b__*. Do not search for them — …`。
const DISCONNECTED_SUMMARY: &str = " are no longer available (MCP server disconnected): ";

/// 断开又连回来的 MCP 工具（`readdedNames`），同样按服务器归并：`<N> deferred tools are available again (MCP server
/// reconnected — names announced earlier in this conversation): mcp__a__* (3).`
const READDED_MCP: &str = " available again (MCP server reconnected \u{2014} names announced earlier in this conversation): ";

/// 别的重新可用的工具，逐个列名：`<N> deferred tools are available again in this session (announced earlier in this
/// conversation): A, B.`
const READDED_OTHER: &str =
    " available again in this session (announced earlier in this conversation): ";

/// 「配置了但没连上」那段（官方按重试状态分三组，各一段），开头之后是 `for this session` / `for now; the session
/// retries …` / `for now; a ToolSearch query …`，下一行起每行 `<名字> (<错误码>): "<错误>"`（官方 `Wmo`）。
const FAILED_HEAD: &str = "The following MCP servers are configured but failed to connect \u{2014} their tools (typically named mcp__<server>__*) are unavailable ";

/// 被组织策略拦下的（官方 `Men`），只列名字。
const POLICY_HEAD: &str =
    "The following MCP servers are configured but blocked by the organization's managed policy";

/// 地址被 Claude Code 自己保留的（官方 `Den`），只列名字。
const RESERVED_HEAD: &str =
    "The following MCP servers are configured, but Claude Code itself refuses them";

/// 还在连接的 MCP 服务器（官方 `pendingMcpServers`）那段的开头（有没有 ToolSearch 两种说法，开头这截相同），下一行起
/// 每行一个名字；超过 30 个时并成一行 `a, b, …, …and N more`。
const PENDING_HEAD: &str = "The following MCP servers are still connecting \u{2014} their tools (typically named mcp__<server>__*) are not yet available";

/// 要登录的 MCP 服务器（官方 `needsAuthMcpServers`）那段的开头，名单写法同上。
const NEEDS_AUTH_HEAD: &str =
    "The following MCP servers require authentication before their tools can be used:";

/// 应用自带的回复服务器要登录（官方 `tSe("needs_auth")`）：不写名字，名单里却算一个。
const HIDDEN_NEEDS_AUTH: &str = "The app's own tool server could not sign in";

/// 应用自带的回复服务器没连上（官方 `tSe("failed" / "failed_for_session" / "failed_retrying")`）：不写名字，名单里却算一个。
const HIDDEN_FAILED: &str = "The app's own tool server failed to connect, so its tools";

/// 官方认作 Chrome 扩展的那几个工具名前缀（2.1.321 `_te`，含 `uPr` 那三个 Claude_Preview / Claude_Browser）：
/// `tengu_deferred_tools_pool_change_by_owner` 按它分「chrome」与「other」（工具名以 `mcp__remote-devices__` 开头的官方另记
/// `remote_devices_browser` / `remote_devices_other`，见 [`McpDeparted::owners`]）。
const CHROME_TOOL_PREFIXES: [&str; 8] = [
    "mcp__claude-in-chrome__",
    "mcp__Claude_in_Chrome__",
    "mcp__remote-devices__claude-in-chrome__",
    "mcp__remote-devices__Claude_in_Chrome__",
    "mcp__claude-device__chrome_",
    "mcp__Claude_Preview__",
    "mcp__Claude_Browser__",
    "mcp__remote-devices__Claude_Browser__",
];

/// 远程设备转出来的 MCP 工具的名字前缀（官方 `Fr` 是 `remote-devices`）。
const REMOTE_DEVICES_PREFIX: &str = "mcp__remote-devices__";

/// 官方 `uEn` 分的四种归属，按 [`McpDeparted::by_owner`] 的下标。
const OWNERS: [&str; 4] = ["other", "chrome", "remote_devices_other", "remote_devices_browser"];

/// 一个工具名的归属下标。
fn owner_of(tool: &str) -> usize {
    let chrome = CHROME_TOOL_PREFIXES.iter().any(|p| tool.starts_with(p));
    match (tool.starts_with(REMOTE_DEVICES_PREFIX), chrome) {
        (false, false) => 0,
        (false, true) => 1,
        (true, false) => 2,
        (true, true) => 3,
    }
}

/// 按服务器归并后的 `mcp__<服务器>__` 整组的归属：这台服务器下面还分得出 Chrome 子前缀的（`remote-devices`、
/// `claude-device`）一组里两种都可能有，看不出，`None`。
fn owner_of_server(prefix: &str) -> Option<usize> {
    let split =
        CHROME_TOOL_PREFIXES.iter().any(|p| p.len() > prefix.len() && p.starts_with(prefix));
    (!split).then(|| owner_of(prefix))
}

/// 按服务器归并的一组可能落进哪几种归属：定得下的就那一种；分不出 Chrome 子前缀的，`remote-devices` 那组是两种远程设备
/// 归属之一，别的（`claude-device`）是 `other` / `chrome` 之一。名字配不回时只有这几种的计数说不准，别的照报。
fn possible_owners(prefix: &str) -> [bool; 4] {
    let mut out = [false; 4];
    match owner_of_server(prefix) {
        Some(o) => out[o] = true,
        None if prefix.starts_with(REMOTE_DEVICES_PREFIX) => {
            out[2] = true;
            out[3] = true;
        }
        None => {
            out[0] = true;
            out[1] = true;
        }
    }
    out
}

/// MCP 服务器名在工具名里的写法（官方 `Sn`：`[A-Za-z0-9_-]` 之外的字符换成 `_`）。
fn mcp_server_slug(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// 工具名里的服务器段（`mcp__<服务器>__…`）。
fn server_of(tool: &str) -> Option<&str> {
    tool.strip_prefix("mcp__").and_then(|r| r.split_once("__")).map(|(s, _)| s)
}

/// 一个没连上的 MCP 服务器（官方 `failedMcpServers` 的一项）。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct FailedServer {
    name: String,
    code: Option<String>,
    error: Option<String>,
    /// 重试状态（官方 `willRetry`），按所在那一段分：`false` / `true` / `on_search`；策略拦下的、地址保留的另记
    /// `policy` / `reserved`（那两段不写错误文本，同一分组前后两轮一致就算没变）。
    retry: &'static str,
    /// 错误文本截断过（[`MAX_ERROR_LEN`]）：截下来的前缀相同，原文也可能不同。
    truncated: bool,
}

/// 一份「没连上」名单。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FailedList {
    /// 认出来的那几项，按名字排。
    servers: Vec<FailedServer>,
    /// 名单里一共几个（截断时比认出来的多）。
    count: usize,
    /// 每一项都认出来了：没截断（`…and N more`）、没有不写名字的那个。只有两份都完整才比 `failedChanged`。
    complete: bool,
}

impl Default for FailedList {
    /// 空名单：附件里给了、一个都没有（渲染时这段就不写）。
    fn default() -> Self {
        Self { servers: Vec::new(), count: 0, complete: true }
    }
}

impl FailedList {
    fn merge(&mut self, o: FailedList) {
        self.servers.extend(o.servers);
        self.count = self.count.saturating_add(o.count);
        self.complete &= o.complete;
    }

    /// 名单里各服务器换成工具名写法后的集合：一轮里先算一次，逐个走掉的工具再查（[`PoolDelta::departed`]）。
    fn slugs(&self) -> HashSet<String> {
        self.servers.iter().map(|s| mcp_server_slug(&s.name)).collect()
    }
}

/// 一份只列名字的 MCP 服务器名单（还在连接的、要登录的）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NameList {
    /// 认出来的名字，排好序。
    names: Vec<String>,
    count: usize,
    /// 没截断、没有不写名字的那个。
    complete: bool,
}

impl Default for NameList {
    fn default() -> Self {
        Self { names: Vec::new(), count: 0, complete: true }
    }
}

impl NameList {
    fn merge(&mut self, o: NameList) {
        self.names.extend(o.names);
        self.count = self.count.saturating_add(o.count);
        self.complete &= o.complete;
    }

    /// 名单那几行：每行一个名字；超过 30 个时官方并成一行 `a, b, …, …and N more`。
    fn of<'a>(lines: impl Iterator<Item = &'a str>, budget: &mut usize) -> Self {
        let mut out = NameList::default();
        for l in lines {
            match l.rsplit_once(", \u{2026}and ") {
                Some((shown, more)) => {
                    // 官方并成一行时只列前 30 个；边数边收，过了上限的不再复制。
                    let more = more.strip_suffix(" more").and_then(|m| m.parse::<usize>().ok());
                    let mut shown_n = 0usize;
                    for name in shown.split(", ") {
                        shown_n += 1;
                        if name.trim().is_empty() {
                            out.complete = false;
                            continue;
                        }
                        if out.names.len() < MAX_LIST_ITEMS
                            && name.len() <= MAX_NAME_LEN
                            && charge(budget, name.len())
                        {
                            out.names.push(name.to_string());
                        }
                    }
                    out.count = out
                        .count
                        .saturating_add(shown_n)
                        .saturating_add(more.unwrap_or(0).min(MAX_COUNT));
                    out.complete = false;
                }
                None => {
                    if out.names.len() < MAX_LIST_ITEMS
                        && l.len() <= MAX_NAME_LEN
                        && charge(budget, l.len())
                    {
                        out.names.push(l.to_string());
                    } else {
                        out.complete = false;
                    }
                    out.count = out.count.saturating_add(1);
                }
            }
        }
        out
    }
}

/// 从一份变更的字节预算（[`MAX_DELTA_BYTES`]）里扣复制 `n` 字节正文的代价：正文本身加每项固定的 [`ENTRY_COST`]（一个
/// `String` 的头、集合里的槽位），空条目也不白记。够就扣、返回 true，不够不扣、返回 false（调用方不再复制，名单记为不
/// 完整 / 宣告名单记为不准）。
fn charge(budget: &mut usize, n: usize) -> bool {
    let cost = n.saturating_add(ENTRY_COST);
    if *budget < cost {
        return false;
    }
    *budget -= cost;
    true
}

/// 记一项的固定开销（字节）。
const ENTRY_COST: usize = 32;

/// 两份名单的（这一轮个数, 上一份个数, 变没变）：个数各自认得出就报；两份都完整才逐项比，不完整但个数不同也算变了。
fn compare(
    now: Option<(usize, bool)>,
    last: Option<(usize, bool)>,
    same: impl Fn() -> bool,
) -> [Option<Value>; 3] {
    let changed = match (now, last) {
        (Some((_, true)), Some((_, true))) => Some(!same()),
        (Some((a, _)), Some((b, _))) if a != b => Some(true),
        _ => None,
    };
    [now.map(|(n, _)| json!(n)), last.map(|(n, _)| json!(n)), changed.map(|c| json!(c))]
}

/// 走掉的一项：逐项列出的工具名，或摘要里按服务器归并的一组（`mcp__<服务器>__` 前缀与个数）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Gone {
    Name(String),
    Server { prefix: String, count: usize },
}

/// 走掉的原因能不能分：「服务器断开」那段按这台服务器在不在「没连上」名单里分 `disconnected` / `not_offered`
/// （`cap/auto-2.1.319-20261106-confirm/2.1.319/00162` 在名单里、批次 `00165` 报 `departed_disconnected`；
/// `cap/auto-2.1.321-20261110-full/00756` 不在、批次 `00760` 报 `departed_not_offered`）；别的段看不出。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Cause {
    ByFailedList,
    /// 「no longer available in this session」那段：官方附件里的 MCP 工具另记 `removedForOtherCause`（2.1.321
    /// `tellsByCause`，不是断连、要认证、重连中的那几种原因）。
    Other,
    Unknown,
}

/// 一条 `deferred_tools_delta` 附件渲染出来、认得出的那几样。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PoolDelta {
    /// 定义被撤销的工具（官方 `retractedTools`）与原因键（说明认不出的为 `None`）。
    retracted: Vec<(String, Option<&'static str>)>,
    /// 撤销那段没记全（归并成组、过长、预算）：估值里 `retractedTools` 说不准。
    retracted_incomplete: bool,
    /// 权限拒绝那段另起开头（官方 `deniedLead`，[`DENIED_HEAD`]）：这一支官方附件带 `restoresDeferred`，拒掉的工具也还在
    /// `removedForOtherCause` 里（原因 `denied` 不在官方 `l0r` 排除的那几种里）。
    denied_lead: bool,
    added: Vec<String>,
    /// 其中刚在体里补了完整定义的（`tool_addition`，[`ANNOUNCE_HEADS`] 头两种），官方另记 `surfacedNames`；它们照样在
    /// `addedLines` 里（`cap/auto-2.1.322-20261110-bashfail/2.1.322` 请求 `00059`）。
    surfaced: Vec<String>,
    /// 这一轮体里 `tool_addition` 块带的完整定义：（工具名, 定义紧凑 JSON 的 UTF-16 长度）。估值里 `surfacedDefinitions`
    /// 那段按它算（[`Self::token_estimate`]），由 [`scan`] 从这一轮的消息里补。
    surfaced_defs: Vec<(String, usize)>,
    /// 「定义更新」那段（[`ANNOUNCE_HEADS`] 第二种 `…have updated definitions…`）的工具：已宣告过、按值定义过的工具定义变了
    /// （官方 2.1.321 起 `R3r` 的 `xe`）。它们不在 [`Self::added`] / [`Self::surfaced`] 里——官方附件不把它们算进
    /// `addedNames` / `addedLines` / `surfacedNames`，只记 `replacedNames` 并把新定义放进 `surfacedDefinitions`；只有这一段的
    /// 一轮官方不报工具池汇总（`cap/auto-2.1.322-20261111-redefine` 请求 `00078` → 批次 `00085`：只有 `tengu_attachments`）。
    replaced: Vec<String>,
    /// 连回来的 MCP 工具，按服务器归并（前缀, 个数）。
    readded: Vec<(String, usize)>,
    /// 正文逐个点名「又能用了」的工具（`N deferred tools are available again in this session (announced earlier …): A, B.`，
    /// 官方 `Ke.other`，就是 `readdedNames` 里不是 MCP 的那些）：它们也在 [`Self::added`] 里，但恢复身份是正文明说的，不靠账本推。
    readded_listed: Vec<String>,
    /// `The following tools are available again. Their definitions earlier in this conversation still apply …` 那段的工具
    /// （官方 `stillDefinedNames`，是 `readdedNames` 里定义还留在上文的那些）：同样是正文明说连回来的。
    still_defined: Vec<String>,
    removed: Vec<(Gone, Cause)>,
    failed: FailedList,
    pending: NameList,
    needs_auth: NameList,
    /// 这份变更还能复制多少字节的名字与错误文本（[`MAX_DELTA_BYTES`] 起扣，见 [`charge`]）。
    budget: usize,
    /// 宣告 / 走掉的名字有因为预算或上限没记下的：账本叠上它就不准了。
    overflow: bool,
    /// 走掉的那几段有没记全的（预算、项数上限、几组合计越界整份不收）：这一轮走掉了几个说不准，`removedCount` 与按原因、
    /// 按归属的计数都不报，不拿零或部分计数充数。
    removed_incomplete: bool,
}

impl PoolDelta {
    /// 这份附件只有「定义更新」那段（官方 `nPi` 返回 `null`、`R3r` 单独组装）：附件照有，工具池汇总不报，估值按
    /// 五个空名单 + `surfacedDefinitions` + `replacedNames` 算（`cap/auto-2.1.322-20261111-redefine` 批次 `00085`：118）。
    pub(super) fn replace_only(&self) -> bool {
        !self.replaced.is_empty()
            && self.added.is_empty()
            && self.surfaced.is_empty()
            && self.readded.is_empty()
            && self.still_defined.is_empty()
            && self.removed.is_empty()
            && self.retracted.is_empty()
            && !self.retracted_incomplete
            && !self.removed_incomplete
            && !self.overflow
            && self.failed.servers.is_empty()
            && self.failed.count == 0
            && self.failed.complete
            && self.pending.names.is_empty()
            && self.pending.count == 0
            && self.pending.complete
            && self.needs_auth.names.is_empty()
            && self.needs_auth.count == 0
            && self.needs_auth.complete
    }

    /// 按值补定义的那两种的个数（新增的 `surfacedNames`, 定义更新的 `replacedNames`），两样都没有是 `None`：官方 2.1.321 起
    /// `R3r` 组装附件时报一条 `tengu_inline_tools_announced{definitions, replacements, pointers, withdrawals}`
    /// （`cap/auto-2.1.322-20261111-redefine` 批次 `00076`：2 / 0，`00085`：0 / 1；`00070` 首轮 3 / 0）。`pointers`（连回来、
    /// 定义没变的）与 `withdrawals`（按块撤掉的）从正文认不出，按 0。
    pub(super) fn inline_counts(&self) -> Option<(usize, usize)> {
        (!self.surfaced.is_empty() || !self.replaced.is_empty())
            .then_some((self.surfaced.len(), self.replaced.len()))
    }

    /// 一组提醒正文（同一段的几条消息）里的工具池段落；一段都没有是 `None`。
    pub(super) fn parse<'a>(texts: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let mut out = PoolDelta { budget: MAX_DELTA_BYTES, ..Default::default() };
        let mut found = false;
        for t in texts {
            found |= parse_text(t, &mut out);
        }
        // 几段合起来的名单最后统一排一次（合并时不排）。
        out.failed.servers.sort();
        out.pending.names.sort();
        out.needs_auth.names.sort();
        // 同一项按同一原因走掉（同名、或同前缀同个数的一组）重复出现只算一项：在合计上限之前去重，合计、归属、
        // `removedCount`、账本看的是同一份（恢复组同理，见下）。
        // 归并的组原因不同是两回事：同一台服务器断连走了 31 个、又因别的原因走了 31 个，是 62 个。逐个点名的就是那个工具，
        // 两类段落里各列一次也只走了一个（留头一次的原因）。
        let mut gone_seen: HashSet<(Gone, Option<Cause>)> = HashSet::new();
        out.removed.retain(|(g, c)| {
            let cause = matches!(g, Gone::Server { .. }).then_some(*c);
            gone_seen.insert((g.clone(), cause))
        });
        // 每一组都过了上限，几组、几段加起来还得过：合计越界的（构造出来的）移除与连回来的整份不收。
        let removed = out.removed.iter().map(|(g, _)| match g {
            Gone::Name(_) => 1,
            Gone::Server { count, .. } => *count,
        });
        if bounded_sum(removed).is_none() {
            out.removed.clear();
            out.removed_incomplete = true;
        }
        // 一模一样的恢复组（同前缀同个数）重复出现只算一组：在合计上限之前去重，合计、归属、账本看的是同一份。
        let mut seen: HashSet<(String, usize)> = HashSet::new();
        out.readded.retain(|g| seen.insert(g.clone()));
        if bounded_sum(out.readded.iter().map(|(_, n)| *n)).is_none() {
            // 连回来了几个说不准：新增分不清、账本叠上它也不准。
            out.readded.clear();
            out.overflow = true;
        }
        found.then_some(out)
    }

    /// 这一轮新宣告（官方 `addedCount`，`$t`：池里有、账本里没宣告过的）与其中连回来的（`readdedCount`，`mn`：先前列过、
    /// 走掉后又回来的）各几个。这一轮没有宣告段落是 `(0, 0)`；有宣告却要账本才分得清、账本又不准（或配不回连回来的名字）
    /// 时 `None`。只给定义更新（`mQs`）或本就宣告过的名字不算新增。
    pub(super) fn additions(&self, history: Option<&PoolHistory>) -> Option<(usize, usize)> {
        self.fresh_names(history).map(|(fresh, back)| (fresh.len(), back.len()))
    }

    /// 正文明说连回来的工具：逐个点名恢复的（[`Self::readded_listed`]）与定义还在上文的（[`Self::still_defined`]）。
    fn explicit_readds(&self) -> HashSet<&str> {
        self.readded_listed
            .iter()
            .chain(&self.still_defined)
            .map(String::as_str)
            .filter(|n| !NEVER_ANNOUNCED.contains(n))
            .collect()
    }

    /// 同 [`Self::additions`]，给出名字：新宣告的（按名字排）与其中连回来的（按名字排）。
    fn fresh_names<'a>(
        &'a self,
        history: Option<&'a PoolHistory>,
    ) -> Option<(Vec<&'a str>, Vec<&'a str>)> {
        if self.added.is_empty() && self.readded.is_empty() && !self.overflow {
            return Some((Vec::new(), Vec::new()));
        }
        if self.overflow {
            return None;
        }
        // 正文明说连回来的（逐个点名恢复的、定义还在上文的那段）：恢复身份是明说的，算新增也算连回来，不看账本（账本没记过
        // 它、或记着它还在，都是账本漏了）。
        let explicit = self.explicit_readds();
        let mut fresh = explicit.clone();
        let mut back = explicit.clone();
        // 别的（逐行宣告的、刚补了完整定义的、按服务器归并连回来的）要拿准的账本才分得清是不是头一回宣告。逐行宣告的从来不是
        // 连回来的（官方 `addedLines` 只有此前没逐行列过的），刚补了完整定义的看账本里它走掉过没有。
        let others = self
            .added
            .iter()
            .map(String::as_str)
            .filter(|n| !NEVER_ANNOUNCED.contains(n) && !explicit.contains(n))
            .collect::<Vec<_>>();
        if !others.is_empty() || !self.readded.is_empty() {
            let h = history.filter(|h| h.exact)?;
            for n in others {
                if h.announced.contains(n) {
                    continue;
                }
                fresh.insert(n);
                if self.surfaced.iter().any(|s| s == n) && h.retracted.contains(n) {
                    back.insert(n);
                }
            }
            for (prefix, count) in &self.readded {
                let found: Vec<&str> = h
                    .retracted
                    .iter()
                    .map(String::as_str)
                    .filter(|n| n.starts_with(prefix.as_str()) && !h.announced.contains(*n))
                    .collect();
                if found.len() != *count {
                    return None;
                }
                fresh.extend(found.iter().copied());
                back.extend(found);
            }
        }
        let mut back: Vec<&str> = back.into_iter().collect();
        back.sort_unstable();
        let mut fresh: Vec<&str> = fresh.into_iter().collect();
        fresh.sort_unstable();
        Some((fresh, back))
    }

    /// 连回来的工具按归属的计数（官方 `uEn("returned", mn)`），报得出的那几种：正文逐个点名恢复的按名字定；按服务器归并的
    /// 一组，归属由前缀定得下的直接计数，定不下的要准的账本配回名字，配不回就只让它可能落进的那几种说不准；逐行宣告的新增
    /// 要准的账本才知道是不是连回来的，没有时它所属的那种说不准。名字没记全（`overflow`）就一种都报不出。各种归属互不连累：
    /// 混进一组分不清的 Chrome 工具，不影响 `other` 那条。
    fn returned_owners(&self, history: Option<&PoolHistory>) -> Vec<(String, usize)> {
        if self.overflow {
            return Vec::new();
        }
        let h = history.filter(|h| h.exact);
        let mut uncertain = [false; 4];
        // 连回来的按名字记成一个集合再按归属数（同 [`Self::fresh_names`]）：同一个工具在几段里重复出现（两段完整定义、
        // 两条一样的归并组、完整定义又落在归并组里）只算一个，`readdedCount` 与归属才对得上。
        let explicit = self.explicit_readds();
        let mut names: BTreeSet<&str> = explicit.iter().copied().collect();
        // 逐行宣告的从来不是连回来的，不看；刚补了完整定义的要准的账本才知道是不是连回来的，没有时它那种归属说不准。
        for n in self
            .surfaced
            .iter()
            .map(String::as_str)
            .filter(|n| !NEVER_ANNOUNCED.contains(n) && !explicit.contains(n))
        {
            match h {
                Some(h) if !h.announced.contains(n) && h.retracted.contains(n) => {
                    names.insert(n);
                }
                Some(_) => {}
                None => uncertain[owner_of(n)] = true,
            }
        }
        // 按服务器归并的一组：准的账本配得回名字就并进集合；配不回时归属由前缀定得下的按个数计（集合里已有同前缀的名字，
        // 分不清是不是同一个，那种归属说不准），定不下的只让它可能落进的那几种说不准。
        let mut extra = [0usize; 4];
        let mut seen: Vec<&(String, usize)> = Vec::new();
        for g in &self.readded {
            if seen.contains(&g) {
                continue;
            }
            seen.push(g);
            let (prefix, count) = g;
            let resolved: Option<Vec<&str>> = h.map(|h| {
                h.retracted
                    .iter()
                    .map(String::as_str)
                    .filter(|n| n.starts_with(prefix.as_str()) && !h.announced.contains(*n))
                    .collect()
            });
            match (resolved, owner_of_server(prefix)) {
                (Some(found), _) if found.len() == *count => names.extend(found),
                (_, Some(o)) => {
                    if names.iter().any(|n| n.starts_with(prefix.as_str())) {
                        uncertain[o] = true;
                    } else {
                        extra[o] += count;
                    }
                }
                (_, None) => {
                    for (u, p) in uncertain.iter_mut().zip(possible_owners(prefix)) {
                        *u |= p;
                    }
                }
            }
        }
        let mut counts = extra;
        for n in names {
            counts[owner_of(n)] += 1;
        }
        owner_counts("returned", counts, uncertain)
    }

    /// `tengu_attachments` 里这条的估值（官方 `Co(Hen(附件))`）：把附件数据本身照官方的形状（`ePi` 的返回值加 `type`）转成
    /// 紧凑 JSON，UTF-16 长度 ÷ 4 四舍五入（[`token_round`]）。`bashfail/00063` → 批次 `00065` 是 1085、`-fable/00060` → `00062` 是 1158，
    /// 子代理 `-full/00107` → `00111` 是 180（交互式会话没有要登录的名单，那个键不在），三组逐个相同。
    ///
    /// 认不全的照认得的算：按服务器归并的走掉的一组只记 `mcp__<服务器>__*`，策略拦下的、地址保留的没有错误文本。
    pub(super) fn token_estimate(
        &self,
        history: Option<&PoolHistory>,
        interactive: bool,
    ) -> Option<usize> {
        // 只有定义更新：官方附件是 `R3r` 在空底子上组装的，只有五个空名单、`surfacedDefinitions` 与 `replacedNames`。
        if self.replace_only() {
            let att = json!({
                "type": "deferred_tools_delta",
                "addedNames": [],
                "addedLines": [],
                "removedNames": [],
                "wireHiddenNames": [],
                "readdedNames": [],
            });
            return Some(token_round(utf16_len(&att.to_string()) + self.by_value_len()?));
        }
        // 附件数据没记全（预算、上限、名单截断、错误文本截断过、连回来的配不回名字）：照残缺的数据估出来的数不可靠，不报。
        if self.overflow
            || self.removed_incomplete
            || !self.failed.complete
            || !self.pending.complete
            || !self.needs_auth.complete
            || self.failed.servers.iter().any(|f| f.truncated)
            || ((!self.added.is_empty() || !self.readded.is_empty())
                && self.fresh_names(history).is_none())
        {
            return None;
        }
        let names = |v: &[String]| json!(v);
        // 按服务器归并的一组：账本里这台服务器正好有同样个数的宣告名就逐个配回（官方附件里是逐个工具名），配不回估不准。
        let mut removed: Vec<String> = Vec::new();
        let mut done: HashSet<&str> = HashSet::new();
        for (g, _) in &self.removed {
            match g {
                Gone::Name(n) => removed.push(n.clone()),
                Gone::Server { prefix, .. } => {
                    // 只拿准的账本配（[`PoolHistory::exact`]）；同一台服务器的几组合起来配一次。
                    let names = self.group_names(history, prefix)?;
                    if done.insert(prefix.as_str()) {
                        removed.extend(names.into_iter().cloned());
                    }
                }
            }
        }
        let readded: Vec<String> = self
            .fresh_names(history)
            .map(|(_, back)| back.into_iter().map(str::to_string).collect())
            .unwrap_or_default();
        // `addedLines`（官方 `Dr`，未列过的加重新露面的）不含连回来的（此前列过）；刚在体里补了完整定义的（`surfacedNames`）
        // 照样在里面（`cap/auto-2.1.322-20261110-bashfail/2.1.322` 请求 `00059` → 批次 `00062`：少了这三个名字差 27 个 token）。
        let listed: Vec<String> =
            self.added.iter().filter(|n| !readded.contains(n)).cloned().collect();
        let failed: Vec<Value> = self
            .failed
            .servers
            .iter()
            .map(|f| {
                let mut o = serde_json::Map::new();
                o.insert("name".into(), json!(f.name));
                if let Some(c) = &f.code {
                    o.insert("errorCode".into(), json!(c));
                }
                if let Some(e) = &f.error {
                    o.insert("error".into(), json!(e));
                }
                match f.retry {
                    "true" => {
                        o.insert("willRetry".into(), json!(true));
                    }
                    "on_search" => {
                        o.insert("willRetry".into(), json!("on_search"));
                    }
                    _ => {}
                }
                Value::Object(o)
            })
            .collect();
        // 官方 `addedNames` 含连回来的那几个（`Hr` 把 `$t` 整个算进去，`mn` 是其中一部分）：按服务器归并、配回名字的也要
        // 补进来。
        let mut added_names = self.added.clone();
        for n in &readded {
            if !added_names.contains(n) {
                added_names.push(n.clone());
            }
        }
        let other_cause = self.other_cause_names(history)?;
        let mut att = json!({
            "type": "deferred_tools_delta",
            "addedNames": names(&added_names),
            "addedLines": names(&listed),
            "removedNames": removed,
            "wireHiddenNames": [],
            "readdedNames": readded,
            "pendingMcpServers": names(&self.pending.names),
        });
        // 2.1.321：其他原因走掉的 MCP 工具另列一份（官方 `removedForOtherCause`，空的不写；估的是长度，键的先后不影响）。
        if !other_cause.is_empty() {
            att["removedForOtherCause"] = json!(other_cause);
        }
        // 定义被撤销的：官方附件另带 `retractedTools: [{name, cause}]`，只要有撤销、客户端开着 `tellsDenies` 就还带
        // `restoresDeferred: true`（2.1.321 `Gr.length>0&&Y?.tellsDenies`），正文渲染一样、看不出带没带：估不准。
        if !self.retracted.is_empty() || self.retracted_incomplete {
            return None;
        }

        // 要登录的服务器名单只有非交互（`-p` / SDK）才带（官方 `ye()` 即 `!isInteractive()` 时才算 `A3r`，否则 `undefined`、
        // 键不在）：交互式的 `cap/auto-2.1.322-20261111-redefine` 两条附件、`-full/00107` 子代理那条都没有这个键，`bashfail`
        // 几组有。
        if !interactive {
            att["needsAuthMcpServers"] = names(&self.needs_auth.names);
        }
        att["failedMcpServers"] = Value::Array(failed);
        if !self.surfaced.is_empty() {
            att["surfacedNames"] = names(&self.surfaced);
        }
        let len = utf16_len(&att.to_string()) + self.by_value_len()?;
        Some(token_round(len))
    }

    /// 按值定义那两项的长度（拼在附件 JSON 末尾的 `,"surfacedDefinitions":[…]` 与 `,"replacedNames":[…]`）：官方 2.1.321 起
    /// `R3r` 给刚补了完整定义的（`surfacedNames`）与定义更新的（`replacedNames`）各放一项 `{name, listing, definition}`——
    /// `listing` 是 16 位摘要，`definition` 是工具定义 `qW` 的产物，键与体里 `tool_addition` 那份相同，按体里的定义算长度。
    /// 有哪个找不到定义的，估不准（`None`）；两样都没有是 0。
    /// `cap/auto-2.1.322-20261110-bashfail/2.1.322` 请求 `00059`（三个 claude.ai 连接器）照此拼出 7349 字符、
    /// `cap/auto-2.1.322-20261111-redefine` 请求 `00071`（新增两个）1712、`00078`（定义更新一个）472，都与会话记录里那条附件
    /// 原文逐字同长。
    fn by_value_len(&self) -> Option<usize> {
        let defined: Vec<&String> = self.surfaced.iter().chain(&self.replaced).collect();
        if defined.is_empty() {
            return Some(0);
        }
        let mut len = r#","surfacedDefinitions":[]"#.len() + defined.len() - 1;
        for n in &defined {
            let (_, def) = self.surfaced_defs.iter().find(|(d, _)| d == *n)?;
            // `{"name":<名字>,"listing":"<16 位>","definition":<定义>}`
            len += utf16_len(&json!(n).to_string())
                + r#"{"name":,"listing":"0123456789abcdef","definition":}"#.len()
                + def;
        }
        if !self.replaced.is_empty() {
            len += r#","replacedNames":"#.len() + utf16_len(&json!(self.replaced).to_string());
        }
        Some(len)
    }

    #[cfg(test)]
    pub(super) fn failed(&self) -> &FailedList {
        &self.failed
    }

    /// 官方 `removedForOtherCause`：其他原因（[`Cause::Other`]）走掉的 MCP 工具名，排好序。归并的组按账本配回名字；
    /// 同一台服务器还有别的原因的组时，哪几个名字是这一组的分不出——候选名字一样长时随便取那么多个，估出的长度一样，
    /// 否则估不准（`None`）。
    fn other_cause_names(&self, history: Option<&PoolHistory>) -> Option<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for (g, c) in &self.removed {
            if *c != Cause::Other {
                continue;
            }
            match g {
                // 定义被撤销的也列在同一段里，但不是其他原因走掉的（官方 `removedForOtherCause` 不含它们）。
                // 权限拒绝的（原因 `denied`）例外，照样在里面。
                Gone::Name(n)
                    if self.retracted.iter().any(|(r, c)| r == n && *c != Some("denied")) => {}
                Gone::Name(n) if n.starts_with("mcp__") => out.push(n.clone()),
                Gone::Name(_) => {}
                Gone::Server { prefix, count } => {
                    let names = self.group_names(history, prefix)?;
                    let mixed = self.removed.iter().any(|(x, xc)| {
                        matches!(x, Gone::Server { prefix: p, .. } if p == prefix) && *xc != *c
                    });
                    if mixed {
                        let len = names.first().map(|n| utf16_len(n));
                        if names.iter().any(|n| Some(utf16_len(n)) != len) {
                            return None;
                        }
                    }
                    out.extend(names.into_iter().take(*count).cloned());
                }
            }
        }
        out.sort();
        out.dedup();
        Some(out)
    }

    /// 这一轮按服务器归并走掉的、前缀是 `prefix` 的那几组（同一台服务器可能按不同原因各走一组）合起来的个数。
    fn group_total(&self, prefix: &str) -> usize {
        self.removed
            .iter()
            .filter_map(|(g, _)| match g {
                Gone::Server { prefix: p, count } if p == prefix => Some(*count),
                _ => None,
            })
            .fold(0usize, usize::saturating_add)
    }

    /// 按服务器归并走掉的、前缀是 `prefix` 的那几组合起来在准的账本里对应哪些名字：这台服务器宣告过的，去掉这一轮已经
    /// 逐个点名走掉的（同一台服务器一组加单列一项时，单列那项不在组里——同 [`PoolHistory::apply`] 先叠逐名的），个数要正好
    /// 等于那几组的合计（[`Self::group_total`]）。同一台服务器按不同原因走了几组时，哪个名字属于哪组分不出，但合起来是
    /// 哪些名字是定的（归属、估值、账本都只要合起来那份）。账本不准、没有，或个数对不上时 `None`。
    fn group_names<'a>(
        &'a self,
        history: Option<&'a PoolHistory>,
        prefix: &str,
    ) -> Option<Vec<&'a String>> {
        let h = history.filter(|h| h.exact)?;
        let named: HashSet<&str> = self
            .removed
            .iter()
            .filter_map(|(g, _)| match g {
                Gone::Name(x) if x.starts_with(prefix) => Some(x.as_str()),
                _ => None,
            })
            .collect();
        let names: Vec<&String> = h
            .announced
            .iter()
            .filter(|n| n.starts_with(prefix) && !named.contains(n.as_str()))
            .collect();
        (names.len() == self.group_total(prefix)).then_some(names)
    }

    /// 这一轮走掉的工具，按原因、归属分好。摘要里按服务器归并、看不出归属的一组，账本（`history`）里正好有这台服务器
    /// 同样个数的宣告名就逐个配回，否则只计入总数、不报归属。
    pub(super) fn departed(&self, history: Option<&PoolHistory>) -> McpDeparted {
        let mut d = McpDeparted { incomplete: self.removed_incomplete, ..Default::default() };
        // 「没连上」名单换成工具名写法先算一次，逐个走掉的工具只查集合。
        let failed = FailedSlugs { slugs: self.failed.slugs(), complete: self.failed.complete };
        let failed = &failed;
        // 同一台服务器的几组合起来配名字，归属只按合起来那份记一次（原因按各组自己的个数记）。
        let mut owned: HashSet<&str> = HashSet::new();
        for (gone, cause) in &self.removed {
            match gone {
                Gone::Name(n) => d.note(Some(owner_of(n)), server_of(n), 1, *cause, failed),
                Gone::Server { prefix, count } => {
                    let server = server_of(prefix);
                    if let Some(o) = owner_of_server(prefix) {
                        d.note(Some(o), server, *count, *cause, failed);
                        continue;
                    }
                    // 只拿准的账本配：失准的账本里「正好也有这么多个」不足以说明就是这些。
                    if let Some(known) = self.group_names(history, prefix) {
                        d.note(None, server, *count, *cause, failed);
                        if owned.insert(prefix.as_str()) {
                            for n in known {
                                let o = owner_of(n);
                                d.by_owner[o] = d.by_owner[o].saturating_add(1);
                            }
                        }
                    } else {
                        d.note(None, server, *count, *cause, failed);
                        for (u, p) in d.uncertain.iter_mut().zip(possible_owners(prefix)) {
                            *u |= p;
                        }
                    }
                }
            }
        }
        d
    }
}

/// 一段正文里的工具池段落记进 `out`，有一段就返回 true。段落都要顶格（正文开头或一行的开头）。
///
/// 只用得上少数几行（段落开头与紧跟的清单），所以不给整段建行索引：先看正文里有没有任何一种段落的标记，没有直接
/// 返回；有就逐行流式地走，清单边走边记、每份最多 [`MAX_LIST_ITEMS`] 项。提醒正文能有几十 MB（贴进来的大段日志被
/// 包成提醒），按行建索引会成倍放大内存。
fn parse_text(t: &str, out: &mut PoolDelta) -> bool {
    if !POOL_MARKERS.iter().any(|m| t.contains(m)) {
        return false;
    }
    let mut it = t.lines().peekable();
    // 下一行起、到空行为止的那份清单（边走边交给 `f`，不攒）。`<system-reminder>` 包着的提醒，闭标签可能紧跟在最后
    // 一项下一行，标签行（`<` 开头，工具名不会这样开头）也算清单结束。
    fn each_item<'a>(
        it: &mut std::iter::Peekable<std::str::Lines<'a>>,
        max_len: usize,
        mut f: impl FnMut(&'a str),
    ) -> bool {
        let mut n = 0usize;
        let mut dropped = false;
        while let Some(l) = it.peek().map(|l| l.trim()) {
            if l.is_empty() || l.starts_with('<') {
                break;
            }
            it.next();
            if n < MAX_LIST_ITEMS && l.len() <= max_len {
                f(l);
                n += 1;
            } else {
                dropped = true;
            }
        }
        // 有没记下的（过了项数上限、名字过长）：调用方据此把名单记为不全。
        dropped
    }
    let mut found = false;
    while let Some(l) = it.next() {
        if ANNOUNCE_HEADS.iter().any(|h| l.starts_with(h)) {
            let surfaced = ANNOUNCE_HEADS[..2].iter().any(|h| l.starts_with(h));
            let still_defined = l.starts_with(ANNOUNCE_HEADS[5]);
            let replaced = l.starts_with(ANNOUNCE_HEADS[1]);
            let dropped = each_item(&mut it, MAX_NAME_LEN, |n| {
                let cost = if surfaced || still_defined { 2 * n.len() } else { n.len() };
                if !charge(&mut out.budget, cost) {
                    out.overflow = true;
                    return;
                }
                if replaced {
                    out.replaced.push(n.to_string());
                    return;
                }
                if surfaced {
                    out.surfaced.push(n.to_string());
                }
                if still_defined {
                    out.still_defined.push(n.to_string());
                }
                out.added.push(n.to_string());
            });
            out.overflow |= dropped;
            found = true;
        } else if DISCONNECTED_HEADS.iter().any(|h| l.starts_with(h)) {
            // 走掉的没记全只记 `removed_incomplete`（账本照样记为不准），不碰连回来那头的 `overflow`：同轮独立的恢复归属照报。
            let dropped = each_item(&mut it, MAX_NAME_LEN, |n| {
                if !charge(&mut out.budget, n.len()) {
                    out.removed_incomplete = true;
                    return;
                }
                out.removed.push((Gone::Name(n.to_string()), Cause::ByFailedList));
            });
            out.removed_incomplete |= dropped;
            found = true;
        } else if l.trim_end() == RETRACTED_HEAD || l.trim_end() == DENIED_HEAD {
            // 定义被撤销的（官方 `retractedTools`）：名字记下、原因换回键；归并成组的（超过 30 个）配不回名字，记为不全。
            let denied = l.trim_end() == DENIED_HEAD;
            out.denied_lead |= denied;
            let dropped = each_item(&mut it, MAX_RETRACT_LINE, |line| {
                let (cause, items) = if denied {
                    (Some("denied"), line)
                } else {
                    let Some((label, items)) =
                        line.strip_prefix("- ").and_then(|r| r.split_once(": "))
                    else {
                        out.retracted_incomplete = true;
                        return;
                    };
                    (retract_cause(label), items)
                };
                for item in items.split(", ").map(str::trim).filter(|x| !x.is_empty()) {
                    if server_group(item).is_some() || item.len() > MAX_NAME_LEN {
                        out.retracted_incomplete = true;
                        continue;
                    }
                    if !charge(&mut out.budget, item.len()) {
                        out.retracted_incomplete = true;
                        return;
                    }
                    out.retracted.push((item.to_string(), cause));
                }
            });
            out.retracted_incomplete |= dropped;
            found = true;
        } else if OTHER_CAUSE_HEADS.iter().any(|h| l.starts_with(h)) || l == DEFINED_GONE_HEAD {
            let cause = if l == DEFINED_GONE_HEAD { Cause::Unknown } else { Cause::Other };
            let dropped = each_item(&mut it, MAX_NAME_LEN, |item| {
                let gone = match server_group(item) {
                    None => Gone::Name(item.to_string()),
                    // 官方 `fgn` 只把超过 30 个的服务器归并成一组，别的（含越界、写坏的）不收——可这一项确实是走掉的一组，
                    // 数不清，走掉的总数与之后的账本都记为不全。
                    Some(Some(Gone::Server { prefix, count })) if count > ZM => {
                        Gone::Server { prefix, count }
                    }
                    Some(_) => {
                        out.removed_incomplete = true;
                        return;
                    }
                };
                if !charge(&mut out.budget, item.len()) {
                    out.removed_incomplete = true;
                    return;
                }
                out.removed.push((gone, cause));
            });
            out.removed_incomplete |= dropped;
            found = true;
        } else if let Some(suffix) = l.strip_prefix(FAILED_HEAD) {
            let retry = if suffix.starts_with("for this session") {
                "false"
            } else if suffix.starts_with("for now; the session retries") {
                "true"
            } else {
                "on_search"
            };
            let mut items = Vec::new();
            let dropped = each_item(&mut it, usize::MAX, |x| items.push(x));
            let mut list = failed_entries(items.into_iter(), retry, true, &mut out.budget);
            list.complete &= !dropped;
            out.failed.merge(list);
            found = true;
        } else if l.starts_with(POLICY_HEAD) || l.starts_with(RESERVED_HEAD) {
            let retry = if l.starts_with(POLICY_HEAD) { "policy" } else { "reserved" };
            let mut items = Vec::new();
            let dropped = each_item(&mut it, usize::MAX, |x| items.push(x));
            let mut list = failed_entries(items.into_iter(), retry, false, &mut out.budget);
            list.complete &= !dropped;
            out.failed.merge(list);
            found = true;
        } else if l.starts_with(PENDING_HEAD) || l.starts_with(NEEDS_AUTH_HEAD) {
            let mut items = Vec::new();
            let dropped = each_item(&mut it, usize::MAX, |x| items.push(x));
            let mut names = NameList::of(items.into_iter(), &mut out.budget);
            names.complete &= !dropped;
            if l.starts_with(PENDING_HEAD) {
                out.pending.merge(names);
            } else {
                out.needs_auth.merge(names);
            }
            found = true;
        } else if l.starts_with(HIDDEN_NEEDS_AUTH) {
            out.needs_auth.merge(NameList { names: Vec::new(), count: 1, complete: false });
            found = true;
        } else if l.starts_with(HIDDEN_FAILED) {
            out.failed.merge(FailedList { servers: Vec::new(), count: 1, complete: false });
            found = true;
        } else if l.starts_with(|c: char| c.is_ascii_digit()) {
            found |= parse_count_line(l, out);
        }
    }
    found
}

/// 一份清单最多记几项：官方逐项列的清单不超过 30 项，宣告名单是工具数（几百），这个上限只挡构造出来的超长清单。
const MAX_LIST_ITEMS: usize = 10_000;

/// 清单里一项最长多少字节：工具名上限 64 个字符（官方 `^[a-zA-Z0-9_-]{1,64}$`），带 `mcp__<服务器>__` 前缀的、服务器
/// 名（可带 `claude.ai ` 前缀与空格）也远短于此。更长的不是名字，不记。
const MAX_NAME_LEN: usize = 256;

/// 撤销那段一行最长多少字节：一行是同一原因的名字（逐个列的不超过 30 个，再多归并成组），加上原因说明。
const MAX_RETRACT_LINE: usize = 32 * (MAX_NAME_LEN + 2) + 512;

/// 「没连上」名单里一条错误文本最多留多少字节：官方的错误文本几百字节（`AUTH_HEADER_REJECTED` 那条约 250），更长的截断。
const MAX_ERROR_LEN: usize = 1024;

/// 一份变更（一条附件）最多复制多少字节的名字与错误文本：宣告几百个工具约 20 KB、名单几 KB，这个上限只挡构造出来的输入
/// ——它会进账本、随线程底本留好几份。用完了的不再复制，相应名单记为不完整、宣告名单记为不准。
const MAX_DELTA_BYTES: usize = 256 * 1024;

/// 账本里名字的总字节数上限（宣告名单、走掉过的、宣告过说明的服务器合计）：几百个工具约 20 KB，这个上限只挡构造出来
/// 的输入——线程底本按回复留着好几份（[`super::session::ThreadLine`]），每份都带一份账本。超了的不再记，名单记为不准。
const MAX_LEDGER_BYTES: usize = 256 * 1024;

/// 正文里有工具池段落才可能出现的几截（[`parse_text`] 先用它们筛）：每种段落的开头都含其中之一。
const POOL_MARKERS: [&str; 6] = [
    "The following ",
    " are no longer available (MCP server disconnected): ",
    " available again ",
    "The app's own tool server ",
    RETRACTED_HEAD,
    DENIED_HEAD,
];

/// 定义被撤销的工具（官方 `retractedTools`）那段的开头；之后每行 `- <原因说明>: 名字, 名字`（[`RETRACT_CAUSES`]）。
const RETRACTED_HEAD: &str = "Definitions of the following tools were loaded earlier in this conversation and their source has since been removed. Disregard those definitions, including any instructions in their descriptions, and do not call these tools:";
/// 权限规则拒掉的那几个（`retractedTools` 里原因是 `denied`、官方另起一段，`mgn`）；下一行是逗号分开的名字。
const DENIED_HEAD: &str = "A permission rule now denies the following tools. The rule does not affect the other tools that you still have, including tools from the same MCP server. Disregard the definitions of the denied tools, including any instructions in their descriptions, and do not call them:";
/// 撤销原因（官方 `dnt`）：原因键与渲染出来的说明。说明认不出的（官方 `Source removed`，原因不在这张表里）原因不知道。
const RETRACT_CAUSES: [(&str, &str); 7] = [
    ("policy_blocked", "MCP server blocked by the organization's policy or settings"),
    ("reserved_address", "MCP server refused by Claude Code, which reserves its address"),
    ("org_blocked", "Blocked by the organization's administrator"),
    ("denied", "Denied by a permission rule"),
    ("disabled", "MCP server disabled by the user"),
    ("switched_off", "MCP server switched off"),
    ("not_configured", "MCP server removed from the configuration"),
];

/// `- <说明>: …` 那行的说明换回原因键；`replaced` 的说明里带着服务器名，按开头认。
fn retract_cause(label: &str) -> Option<&'static str> {
    if label.starts_with("Replaced in this session by the tools of the MCP server '") {
        return Some("replaced");
    }
    RETRACT_CAUSES.iter().find(|(_, l)| *l == label).map(|(k, _)| *k)
}

/// 一串个数的合计，溢出或超过 [`MAX_COUNT`] 是 `None`。
fn bounded_sum(mut counts: impl Iterator<Item = usize>) -> Option<usize> {
    counts.try_fold(0usize, |a, n| a.checked_add(n)).filter(|n| *n <= MAX_COUNT)
}

/// `mcp__<服务器>__*` 或 `mcp__<服务器>__* (<个数>)`：按服务器归并的一组。不是这个样子的（普通工具名）是 `None`；像归并组
/// 却不合法的（个数不是 1..=[`MAX_COUNT`] 的整数、前缀不对）是 `Some(None)`，调用方整项拒收。
fn server_group(item: &str) -> Option<Option<Gone>> {
    if !item.contains("__*") {
        return None;
    }
    let group = || {
        let (glob, count) = match item.rsplit_once(" (") {
            Some((g, k)) => (g, k.strip_suffix(')')?.parse::<usize>().ok()?),
            None => (item, 1),
        };
        if count == 0 || count > MAX_COUNT {
            return None;
        }
        let prefix = glob.strip_suffix('*')?;
        (prefix.starts_with("mcp__") && prefix.ends_with("__") && server_of(prefix).is_some())
            .then(|| Gone::Server { prefix: prefix.to_string(), count })
    };
    Some(group())
}

/// 以数字开头的那几种一句话段落（走掉的摘要、连回来的两种）。清单各项的个数加起来要与句首的数字一致才记，个数用
/// checked 运算、不超过 [`MAX_COUNT`]——对不上的（引用、构造出来的）整句不算。
fn parse_count_line(l: &str, out: &mut PoolDelta) -> bool {
    let Some((n, rest)) = l.split_once(' ') else { return false };
    // 句首的数字先只认是不是数字：过了上限的（构造的超大声明）也要先认出是哪种句子，认出来的就是「这一轮确有这种变化、
    // 只是数不清」，得把未知状态记下（走掉的记 `removed_incomplete`，连回来的记 `overflow`），不能当成没有这句。
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let n = n.parse::<usize>().unwrap_or(usize::MAX);
    if n == 0 {
        return false;
    }
    let what_ok = |what: &str| {
        matches!(what, "deferred tools are" | "tools are" | "deferred tool is" | "tool is")
    };
    for (marker, kind) in [(DISCONNECTED_SUMMARY, 0), (READDED_MCP, 1), (READDED_OTHER, 2)] {
        let Some((what, list)) = rest.split_once(marker) else { continue };
        // 「走掉」那句的 marker 以 ` are` 开头，前面只剩 `deferred tools` / `tools`。
        let what_ok =
            if kind == 0 { matches!(what, "deferred tools" | "tools") } else { what_ok(what) };
        if !what_ok {
            return false;
        }
        // 认出来了、却数不清（声明超上限、清单超项数、名字过长）：记未知，这句算认出。
        // 走掉的数不清只让走掉那几项与账本不准（`removed_incomplete`），不连累同轮连回来的归属——同无总数的分组清单。
        let unknown = |out: &mut PoolDelta| {
            if kind == 0 {
                out.removed_incomplete = true;
            } else {
                out.overflow = true;
            }
            true
        };
        if n > MAX_COUNT {
            return unknown(out);
        }
        let list = list.split(". ").next().unwrap_or(list).trim_end_matches('.');
        // 逐项边数边校验，不先把整串拆进一个大索引里。
        let mut items: Vec<&str> = Vec::new();
        for item in list.split(", ").map(str::trim).filter(|s| !s.is_empty()) {
            if items.len() >= n {
                return false;
            }
            if items.len() >= MAX_LIST_ITEMS || item.len() > MAX_NAME_LEN {
                return unknown(out);
            }
            items.push(item);
        }
        if kind == 2 {
            if items.len() != n {
                return false;
            }
            for item in items {
                if !charge(&mut out.budget, 2 * item.len()) {
                    out.overflow = true;
                    break;
                }
                out.added.push(item.to_string());
                out.readded_listed.push(item.to_string());
            }
            return true;
        }
        let mut groups = Vec::with_capacity(items.len());
        let mut sum = 0usize;
        for item in items {
            let Some(Some(Gone::Server { prefix, count })) = server_group(item) else {
                return false;
            };
            let Some(s) = sum.checked_add(count) else { return false };
            sum = s;
            groups.push((prefix, count));
        }
        if sum != n {
            return false;
        }
        // 归并组也按项扣预算（前缀照复制）。
        let mut kept = Vec::with_capacity(groups.len());
        for (prefix, count) in groups {
            if !charge(&mut out.budget, prefix.len()) {
                if kind == 0 {
                    out.removed_incomplete = true;
                } else {
                    out.overflow = true;
                }
                break;
            }
            kept.push((prefix, count));
        }
        let groups = kept;
        if kind == 0 {
            // 官方只在超过 30 个时才写摘要。
            if n <= ZM {
                return false;
            }
            out.removed.extend(
                groups
                    .into_iter()
                    .map(|(prefix, count)| (Gone::Server { prefix, count }, Cause::ByFailedList)),
            );
        } else {
            out.readded.extend(groups);
        }
        return true;
    }
    false
}

/// 「没连上」那几行：`<名字> (<错误码>): "<错误>"`，码与错误都可以没有；`…and N more` 是截断。错误文本超过
/// [`MAX_ERROR_LEN`] 的截到这个长度（同一份文本每轮截得一样，比对 `failedChanged` 照样有效），名字过长、或这份变更的字节
/// 预算用完的不复制，只计数、名单记为不完整——服务器没列进名单不等于它连上了。
fn failed_entries<'a>(
    lines: impl Iterator<Item = &'a str>,
    retry: &'static str,
    detailed: bool,
    budget: &mut usize,
) -> FailedList {
    let mut out = FailedList::default();
    for l in lines {
        if let Some(more) = l.strip_prefix("\u{2026}and ").and_then(|r| r.strip_suffix(" more")) {
            let more = more.parse::<usize>().unwrap_or(0).min(MAX_COUNT);
            out.count = out.count.saturating_add(more);
            out.complete = false;
            continue;
        }
        out.count = out.count.saturating_add(1);
        let (head, error) = match l.split_once(": \"") {
            Some((h, e)) if detailed && e.ends_with('"') => (h, Some(&e[..e.len() - 1])),
            _ => (l, None),
        };
        let (name, code) = match head.strip_suffix(')').and_then(|h| h.rsplit_once(" (")) {
            Some((n, c)) if detailed => (n, Some(c)),
            _ => (head, None),
        };
        let truncated = error.is_some_and(|e| e.len() > MAX_ERROR_LEN);
        let error = error.map(|e| {
            let mut end = e.len().min(MAX_ERROR_LEN);
            while !e.is_char_boundary(end) {
                end -= 1;
            }
            &e[..end]
        });
        let cost = name.len() + code.map_or(0, str::len) + error.map_or(0, str::len);
        if name.len() > MAX_NAME_LEN
            || code.is_some_and(|c| c.len() > MAX_NAME_LEN)
            || !charge(budget, cost)
        {
            out.complete = false;
            continue;
        }
        out.servers.push(FailedServer {
            name: name.to_string(),
            code: code.map(str::to_string),
            error: error.map(str::to_string),
            retry,
            truncated,
        });
    }
    out
}

/// 这一轮之前的账本（官方 `Gxe` 的产物里汇总要用的那几样）。
#[derive(Clone, Debug)]
pub(super) struct PoolHistory {
    announced: HashSet<String>,
    /// 宣告名单逐项准：摘要写法里配不回名字的（连回来的个数对不上）就不准了。
    exact: bool,
    /// 走掉过的工具名（官方 `retractedAsDisconnected`），连回来时按服务器配回。
    retracted: HashSet<String>,
    dtd: usize,
    last_failed: FailedList,
    last_pending: NameList,
    last_needs_auth: NameList,
    /// 这一轮之前对话里有过几段（两条 assistant 之间算一段）：0 是会话头一轮。
    pub(super) turns: usize,
    /// MCP 服务器说明（另一条附件 `mcp_instructions_delta`，[`McpInstr`]）宣告过的服务器，与叠过几条（`midCount`）。
    mcp_servers: HashSet<String>,
    mid: usize,
    /// 宣告过说明的服务器名单逐项准：叠过一条服务器数不确定的说明附件（[`McpInstr::certain`]）就不准了。
    mcp_exact: bool,
    /// 账本里名字的总字节数（[`MAX_LEDGER_BYTES`]）。
    bytes: usize,
}

impl Default for PoolHistory {
    /// 还没有任何一条附件：名单是空的、准的，上一份「没连上」名单是空的（官方 `Ct` 初值 `[]`）。
    fn default() -> Self {
        Self {
            announced: HashSet::new(),
            exact: true,
            retracted: HashSet::new(),
            dtd: 0,
            last_failed: FailedList::default(),
            last_pending: NameList::default(),
            last_needs_auth: NameList::default(),
            turns: 0,
            mcp_servers: HashSet::new(),
            mid: 0,
            mcp_exact: true,
            bytes: 0,
        }
    }
}

impl PoolHistory {
    /// 账本里仍宣告着的内建工具（不是 MCP 的）：叠上这一轮的变更后就是当前启用、可能还没发 schema 的那些，2.1.323 `-p` 的
    /// `context_size` 认「有工具没算进去」看它（[`super::shape::RequestShape::unsent_builtin`]）。
    pub(super) fn builtin_announced(&self) -> impl Iterator<Item = &str> {
        self.announced.iter().map(String::as_str).filter(|n| !n.starts_with("mcp__"))
    }

    /// 测试用：宣告过这几个名字、准的账本。
    #[cfg(test)]
    pub(super) fn of_announced(names: &[&str]) -> Self {
        let mut h = Self::default();
        for n in names {
            h.announced.insert((*n).to_string());
        }
        h
    }

    /// 测试用：上一份「还在连」名单。
    #[cfg(test)]
    pub(super) fn set_pending_for_test(&mut self, names: &[&str]) {
        self.last_pending = NameList {
            names: names.iter().map(|n| (*n).to_string()).collect(),
            count: names.len(),
            complete: true,
        };
    }

    /// 客户端工具池里的 MCP 工具有几个：账本里宣告着的 `mcp__*`，加上正文里直接带着定义、没宣告过的（`body`）。账本不准时
    /// 不知道。
    pub(super) fn mcp_tool_count(&self, body: &[String]) -> Option<usize> {
        if !self.exact {
            return None;
        }
        let announced = self.announced.iter().filter(|n| n.starts_with("mcp__")).count();
        let extra = body.iter().filter(|n| !self.announced.contains(*n)).count();
        Some(announced + extra)
    }

    /// 最近一条附件说没有还在连的 MCP 服务器（`pendingMcpServers` 是空的、名单完整）。
    pub(super) fn nothing_pending(&self) -> bool {
        self.last_pending.complete && self.last_pending.count == 0
    }

    /// 叠上一条附件。
    pub(super) fn apply(&mut self, d: &PoolDelta) {
        self.dtd = self.dtd.saturating_add(1);
        // 宣告或走掉的没记全：叠完的名单少扣了或少加了，之后的 `priorAnnouncedCount` 不报。
        if d.overflow || d.removed_incomplete {
            self.exact = false;
        }
        // 先叠按服务器归并连回来的组，再叠逐个宣告的：同一个工具既在组里、又补了完整定义时，组按叠之前的账本配名字，
        // 个数对得上；一模一样的组重复出现只叠一次（第二次已无可配，不能当成对不上）。
        let mut seen: Vec<&(String, usize)> = Vec::new();
        for g in &d.readded {
            if seen.contains(&g) {
                continue;
            }
            seen.push(g);
            let (prefix, count) = g;
            let back: Vec<String> =
                self.retracted.iter().filter(|n| n.starts_with(prefix.as_str())).cloned().collect();
            if back.len() != *count {
                self.exact = false;
            }
            for n in back {
                self.retracted.remove(&n);
                self.announced.insert(n);
            }
        }
        for n in &d.added {
            if NEVER_ANNOUNCED.contains(&n.as_str()) || !self.admit(n) {
                continue;
            }
            self.retracted.remove(n);
            self.announced.insert(n.clone());
        }
        // 先叠逐个列出的，再叠按服务器归并的组：同一台服务器既有一组、又单独列了一项时，单列的那项已走，组按剩下的配名字，
        // 个数对得上——与两段的先后无关。
        for (gone, _) in &d.removed {
            if let Gone::Name(n) = gone {
                if !self.admit(n) {
                    continue;
                }
                self.announced.remove(n);
                self.retracted.insert(n.clone());
            }
        }
        // 同一台服务器按不同原因走了几组：合起来配一次（个数比合计）。
        let mut done: HashSet<&str> = HashSet::new();
        for (gone, _) in &d.removed {
            let Gone::Server { prefix, .. } = gone else { continue };
            if !done.insert(prefix.as_str()) {
                continue;
            }
            let names: Vec<String> =
                self.announced.iter().filter(|n| n.starts_with(prefix.as_str())).cloned().collect();
            if names.len() != d.group_total(prefix) {
                self.exact = false;
            }
            for n in names {
                self.announced.remove(&n);
                self.retracted.insert(n);
            }
        }
        // 只有定义更新的那份附件没有这三个名单（官方 `R3r` 在空底子上组装），账本里的上一份照旧（官方 `Hxe` 只在附件带了
        // `failedMcpServers` 等字段时才换）。
        if d.replace_only() {
            return;
        }
        self.last_failed = d.failed.clone();
        self.last_pending = d.pending.clone();
        self.last_needs_auth = d.needs_auth.clone();
    }

    /// 一个名字要不要记进账本：已经记着的（宣告名单或走掉过的）照记；新的要在总字节上限（[`MAX_LEDGER_BYTES`]）以内，
    /// 超了不记、宣告名单记为不准。
    fn admit(&mut self, n: &str) -> bool {
        if self.announced.contains(n) || self.retracted.contains(n) {
            return true;
        }
        if self.bytes.saturating_add(n.len()) > MAX_LEDGER_BYTES {
            self.exact = false;
            return false;
        }
        self.bytes += n.len();
        true
    }

    /// 叠上一条 MCP 服务器说明附件。
    pub(super) fn apply_mcp(&mut self, m: &McpInstr) {
        self.mid = self.mid.saturating_add(1);
        self.mcp_exact &= m.certain && !m.removed_incomplete;
        for n in &m.added {
            if self.mcp_servers.contains(n) {
                continue;
            }
            if self.bytes.saturating_add(n.len()) > MAX_LEDGER_BYTES {
                // 记不下了：宣告过说明的服务器名单从此不全，之后不报 `priorAnnouncedCount`。
                self.mcp_exact = false;
                break;
            }
            self.bytes += n.len();
            self.mcp_servers.insert(n.clone());
        }
        for n in &m.removed {
            self.mcp_servers.remove(n);
        }
    }

    /// 宣告名单里的 MCP 工具数（不准时 `None`）。
    fn announced_mcp(&self) -> Option<usize> {
        self.exact.then(|| self.announced.iter().filter(|n| n.starts_with("mcp__")).count())
    }

    /// 宣告名单的个数（不准时 `None`）。
    pub(super) fn announced_count(&self) -> Option<usize> {
        self.exact.then_some(self.announced.len())
    }

    pub(super) fn dtd_count(&self) -> usize {
        self.dtd
    }

    #[cfg(test)]
    pub(super) fn last_failed(&self) -> &FailedList {
        &self.last_failed
    }
}

/// 一条消息里客户端自己附的提醒正文：`system` 角色的消息整段，或用户消息里以 `<system-reminder>` 开头的文本块（不走
/// mid-conversation system 的客户端把提醒包成这样）。用户自己贴进来的同样文字（日志、文档）不算，工具结果也不看。
fn reminder_texts(m: &Value) -> Vec<&str> {
    let system = m.get("role").and_then(|r| r.as_str()) == Some("system");
    let texts: Vec<&str> = match m.get("content") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(b)) => b
            .iter()
            .filter(|x| x.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
            .collect(),
        _ => Vec::new(),
    };
    if system {
        return texts;
    }
    texts.into_iter().flat_map(wrapped_reminders).collect()
}

/// 用户文本块里打头那几段 `<system-reminder>…</system-reminder>` 的正文：只取标签里面的，闭标签之后不再是提醒的接着
/// 的普通正文（用户自己写的、贴的）不看；不以提醒打头的块一段都不算。没闭合的最后一段算到块尾。
fn wrapped_reminders(t: &str) -> Vec<&str> {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = Vec::new();
    let mut rest = t;
    while let Some(body) = rest.trim_start().strip_prefix(OPEN) {
        match body.find(CLOSE) {
            Some(end) => {
                out.push(&body[..end]);
                rest = &body[end + CLOSE.len()..];
            }
            None => {
                out.push(body);
                break;
            }
        }
    }
    out
}

/// [`scan`] 的结果。
#[derive(Debug, Default)]
pub(super) struct Scan {
    /// 末条 assistant 之前的历史叠成的账本；线程续轮的增量体（历史不在体里）是 `None`，由线程底本补。
    pub(super) history: Option<PoolHistory>,
    /// 末条 assistant 之后这一轮的工具池附件。
    pub(super) delta: Option<PoolDelta>,
    /// 这一轮的 MCP 服务器说明附件。
    pub(super) mcp: Option<McpInstr>,
    /// 这一轮的提醒里有 `<total_tokens>` 那条（`total_tokens_reminder` 附件）。只看客户端附的提醒：工具结果、用户正文里
    /// 碰巧有这几个字不算。
    pub(super) total_tokens: bool,
}

/// 整段消息拆成历史与这一轮（`has_history` 为 false 是线程续轮的增量体）。
pub(super) fn scan(msgs: &[Value], has_history: bool, tool_names: &[String]) -> Scan {
    let is_assistant = |m: &Value| m.get("role").and_then(|r| r.as_str()) == Some("assistant");
    let last = msgs.iter().rposition(is_assistant);
    let (before, now) = match last {
        Some(i) => (&msgs[..i], &msgs[i + 1..]),
        None => (&msgs[..0], msgs),
    };
    let history = has_history.then(|| {
        // 这一轮之前有过几段对话：末条 assistant 之前的 assistant 数加一（体里没有 assistant 是会话头一轮，0）。
        let turns = last.map_or(0, |_| before.iter().filter(|m| is_assistant(m)).count() + 1);
        let mut h = PoolHistory { turns, ..Default::default() };
        let segs: Vec<&[Value]> = before.split(|m| is_assistant(m)).collect();
        let deltas: Vec<Option<PoolDelta>> =
            segs.iter().map(|seg| PoolDelta::parse(seg.iter().flat_map(reminder_texts))).collect();
        // 说明里的 `## <名字>` 拿整段对话里出现过的服务器确认（[`McpInstr::resolve`]）。
        let mut known = KnownServers::default();
        deltas.iter().flatten().for_each(|d| known.delta(d));
        tool_names.iter().for_each(|n| known.tool(n));
        for (seg, d) in segs.iter().zip(&deltas) {
            if let Some(d) = d {
                h.apply(d);
            }
            if let Some(mut m) = McpInstr::parse(seg.iter().flat_map(reminder_texts)) {
                m.resolve(|n| known.knows(n));
                h.apply_mcp(&m);
            }
        }
        h
    });
    let mut delta = PoolDelta::parse(now.iter().flat_map(reminder_texts));
    if let Some(d) = delta.as_mut().filter(|d| !d.surfaced.is_empty() || !d.replaced.is_empty()) {
        d.surfaced_defs = tool_addition_defs(now);
    }
    Scan {
        history,
        delta,
        mcp: McpInstr::parse(now.iter().flat_map(reminder_texts)),
        total_tokens: now
            .iter()
            .flat_map(reminder_texts)
            .any(|t| t.contains("<total_tokens>") && t.contains("</total_tokens>")),
    }
}

/// 这一轮消息里 `tool_addition` 块带的完整定义（`{"type":"tool_addition","tool":{"type":"tool_definition","definition":{…}}}`）：
/// （工具名, 定义紧凑 JSON 的 UTF-16 长度）。
fn tool_addition_defs(msgs: &[Value]) -> Vec<(String, usize)> {
    msgs.iter()
        .filter_map(|m| m.get("content").and_then(|c| c.as_array()))
        .flatten()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_addition"))
        .filter_map(|b| {
            let def = b.get("tool")?.get("definition")?;
            let name = def.get("name")?.as_str()?;
            Some((name.to_string(), utf16_len(&def.to_string())))
        })
        .collect()
}

/// 官方附件估值的取整（2.1.322 `R(e, n = 4) = Math.round(e / n)`，`Hen` → `mc` → `R`）：长度 ÷ 4 四舍五入（.5 进位）。
/// 原先按向上取整，余 1 时多一个（`cap/auto-2.1.322-20261110-bashfail/2.1.322` 批次 `00062`：首轮 20521 字符报 5130、
/// 续轮 7349 字符报 1837，长度取自会话记录里那两条附件原文，与这里拼出的逐字相同）。
fn token_round(len: usize) -> usize {
    (len + 2) / 4
}

/// JS 字符串长度（UTF-16 码元数），官方估值按它算。
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// MCP 服务器说明的增减（官方另一条附件 `mcp_instructions_delta`，`$Tn`）渲染出的两种段落：新增的一节（`# MCP Server
/// Instructions` 下每台服务器一个 `## <名字>`），与断开的名单。
const MCP_INSTRUCTIONS_HEAD: &str = "# MCP Server Instructions";
const MCP_INSTRUCTIONS_GONE: &str =
    "The following MCP servers have disconnected. Their instructions above no longer apply";

/// 一条 MCP 服务器说明附件：新宣告的、断开的服务器（官方 `addedNames` / `removedNames`）。
///
/// 新增那一节里每台服务器是一块 `## <名字>` 起头的说明（官方 `Git`：说明正文原样拼在后面），可服务器自己的说明正文里
/// 也可能写 `## Setup` 这种二级标题，光看格式分不出。所以先把每个 `## ` 都记成候选（[`McpBlock`]），等账本、本轮宣告、
/// 出站工具表都齐了（[`Self::resolve`]），按「是已知服务器、或排序对得上」确认分界，别的并进上一块。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct McpInstr {
    candidates: Vec<McpBlock>,
    removed: Vec<String>,
    /// [`Self::resolve`] 之后：新宣告的服务器，与各块 JSON 字符串的长度之和（含引号与分隔的逗号），估值用。
    added: Vec<String>,
    blocks_units: usize,
    /// 新增那一节被一个一级标题截住了（后面可能还有服务器，也可能是别的东西，分不出）。
    ended_early: bool,
    /// 解析时有没记下的：新增那一节的标题（名字过长、过了项数上限）、断开名单的名字。没记全的那一样不报确数。
    added_incomplete: bool,
    removed_incomplete: bool,
    /// [`Self::resolve`] 之后：每一处分界都确认了（起新块的都是已知服务器，并掉的都是排序对不上的）、也没被截住，
    /// `added` 的个数可信；否则服务器数留为未知（[`fill_mcp_summary`] 不报）。
    certain: bool,
}

/// 新增那一节里一个 `## ` 起头的候选块。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct McpBlock {
    name: String,
    /// 这一块去掉尾部换行后、转成 JSON 字符串（不含引号）的 UTF-16 长度。
    units: usize,
    /// 去掉的尾部换行转义后的长度（并块时接回去）。
    trailing: usize,
}

/// 一段正文转成 JSON 字符串内容（不含两头引号）后的 UTF-16 长度，与 `JSON.stringify` 一致：`"` `\\` 与几个控制字符写成
/// 两字符转义，其余控制字符写成 `\\uXXXX`。
fn json_units(t: &str) -> usize {
    t.chars()
        .map(|c| match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf16(),
        })
        .sum()
}

impl McpInstr {
    /// 一组提醒正文里的 MCP 服务器说明段落；一段都没有是 `None`。新增那一节到正文结尾、下一个一级标题或断开名单那句
    /// 为止。逐行流式地走，不建行索引；偏移按每行带着自己的换行符的真实字节长度累加（`\r\n`、末行没有换行都对）。
    pub(super) fn parse<'a>(texts: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let mut out = McpInstr::default();
        let mut found = false;
        for t in texts {
            if !t.contains(MCP_INSTRUCTIONS_HEAD) && !t.contains(MCP_INSTRUCTIONS_GONE) {
                continue;
            }
            // 正在新增那一节里：当前候选块的（名字, 起点字节偏移）。
            let mut in_add = false;
            let mut block: Option<(String, usize)> = None;
            let mut in_gone = false;
            let mut at = 0usize;
            let close = |block: &mut Option<(String, usize)>, end: usize, out: &mut McpInstr| {
                if let Some((name, start)) = block.take() {
                    let raw = &t[start..end];
                    let body = raw.trim_end_matches(['\n', '\r']);
                    out.candidates.push(McpBlock {
                        name,
                        units: json_units(body),
                        trailing: json_units(&raw[body.len()..]),
                    });
                }
            };
            for raw in t.split_inclusive('\n') {
                let start = at;
                at += raw.len();
                let l = raw.trim_end_matches(['\n', '\r']);
                if in_gone {
                    if l.trim().is_empty() {
                        in_gone = false;
                    } else {
                        if l.trim().len() <= MAX_NAME_LEN && out.removed.len() < MAX_LIST_ITEMS {
                            out.removed.push(l.trim().to_string());
                        } else {
                            out.removed_incomplete = true;
                        }
                        continue;
                    }
                }
                if in_add {
                    let top = l.starts_with("# ") && !l.starts_with("## ");
                    if top || l.starts_with(MCP_INSTRUCTIONS_GONE) {
                        close(&mut block, start, &mut out);
                        in_add = false;
                        // 说明正文里自己也可能写一级标题：截在这里，后面还有没有服务器看不出。
                        out.ended_early |= top;
                    } else {
                        if let Some(name) = l.strip_prefix("## ") {
                            // 连同正开着的那一块一起数，过了上限的标题不再起块（并进当前块），记为没记全。
                            let open = out.candidates.len() + usize::from(block.is_some());
                            if open < MAX_LIST_ITEMS && name.trim().len() <= MAX_NAME_LEN {
                                close(&mut block, start, &mut out);
                                block = Some((name.trim().to_string(), start));
                            } else {
                                out.added_incomplete = true;
                            }
                        }
                        continue;
                    }
                }
                if l.starts_with(MCP_INSTRUCTIONS_HEAD) {
                    found = true;
                    in_add = true;
                } else if l.starts_with(MCP_INSTRUCTIONS_GONE) {
                    found = true;
                    in_gone = true;
                }
            }
            close(&mut block, t.len(), &mut out);
        }
        found.then(|| {
            // 先只按排序划一遍（[`Self::resolve`]，已知服务器等齐了再划）。
            out.resolve(|_| false);
            out
        })
    }

    /// 按能确认的服务器名重新划块：第一块总是起；之后的 `## <名字>` 是已知服务器（`known`），或按官方的排序（`$Tn` 把
    /// 新增的服务器按名字排好再拼，比较是 `B4e`，见 [`sorts_after`]）正好排在上一台之后，才起新块，否则并进上一块。
    ///
    /// 块的正文原样拼接（官方 `Git`），光看文字分不出服务器自己说明里的 `## setup` 与一台叫 `setup` 的服务器。排序对不上的
    /// 一定不是服务器（并掉）；对得上又不认识的先按服务器划块（估值用），但把这一份记为不确定（[`Self::certain`]），服务器数
    /// 与之后的账本都不报——真实抓包里就有这种一台工具都没宣告、只给了说明的服务器（`bashfail-fable/00056` 的 `luban-47-…`）。
    pub(super) fn resolve(&mut self, known: impl Fn(&str) -> bool) {
        let mut merged: Vec<McpBlock> = Vec::new();
        let mut certain = !self.ended_early && !self.added_incomplete;
        for b in &self.candidates {
            match merged.last_mut() {
                Some(last) if !known(&b.name) && !sorts_after(&b.name, &last.name) => {
                    last.units += last.trailing + b.units;
                    last.trailing = b.trailing;
                }
                Some(_) => {
                    // 起了新块却不认识：只是排序对得上，也可能是说明正文里的标题。
                    certain &= known(&b.name);
                    merged.push(b.clone());
                }
                None => merged.push(b.clone()),
            }
        }
        self.certain = certain;
        self.added = merged.iter().map(|b| b.name.clone()).collect();
        // 每块两个引号，块与块之间一个逗号。
        self.blocks_units = merged.iter().map(|b| b.units + 2 + 1).sum();
    }

    /// `tengu_attachments` 里这条的估值：照附件的形状 `{type, addedNames, addedBlocks, removedNames}` 转紧凑 JSON，UTF-16 长度
    /// ÷ 4 向上取整。`bashfail` 两组算出 1781（官方 1780）、子代理 `-full/00107` 算出 4814（官方 4814）：服务器那几块的确切
    /// 形状 luban 看不到，按渲染正文里的那一块算，差一两个 token。
    pub(super) fn token_estimate(&self) -> Option<usize> {
        // 解析时没记全（名字过长、过了上限、新增那一节被一级标题截住）：照残缺的内容估出来的数不可靠，不报。
        if self.added_incomplete || self.removed_incomplete || self.ended_early {
            return None;
        }
        let shell = json!({
            "type": "mcp_instructions_delta",
            "addedNames": self.added,
            "addedBlocks": [],
            "removedNames": self.removed,
        });
        // 空的 `addedBlocks` 占 `[]`，换成各块（逗号比块数少一个）。
        let blocks = self.blocks_units.saturating_sub(usize::from(!self.added.is_empty()));
        Some(token_round(utf16_len(&shell.to_string()) + blocks))
    }
}

/// 按官方给新增服务器排序的规则（2.1.321 `B4e`），`b` 排在 `a` 之后：两个都是 ASCII 时按 `localeCompare`
/// （[`locale_cmp`]），一个 ASCII 一个不是时 ASCII 在前，都不是时按码位比。
fn sorts_after(b: &str, a: &str) -> bool {
    use std::cmp::Ordering;
    let ord = match (a.is_ascii(), b.is_ascii()) {
        (true, true) => locale_cmp(a, b),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.cmp(b),
    };
    ord == Ordering::Less
}

/// ASCII 字符串的 `localeCompare`（ICU 根排序、默认强度）近似：先比主权重——空白、标点符号在数字前，数字在字母前，
/// 字母不分大小写，标点之间照 ICU 根排序的次序；主权重全同再比大小写，小写在前。
fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    const PUNCT: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    let primary = |c: u8| -> u32 {
        if let Some(i) = PUNCT.bytes().position(|p| p == c) {
            i as u32
        } else if c.is_ascii_digit() {
            100 + u32::from(c - b'0')
        } else if c.is_ascii_alphabetic() {
            200 + u32::from(c.to_ascii_lowercase() - b'a')
        } else {
            // 控制字符，排最前。
            0
        }
    };
    let key = |s: &str| s.bytes().map(primary).collect::<Vec<_>>();
    key(a).cmp(&key(b)).then_with(|| {
        // 大小写：同位置上小写排在大写前。
        let case =
            |s: &str| s.bytes().map(|c| u8::from(c.is_ascii_uppercase())).collect::<Vec<_>>();
        case(a).cmp(&case(b))
    })
}

/// 已知的 MCP 服务器（工具名里的写法）：宣告过的 MCP 工具的服务器段、各名单（没连上 / 要登录 / 还在连）里的服务器、
/// 宣告过说明的服务器，与出站工具表里的 MCP 工具。[`McpInstr::resolve`] 拿它确认说明里的 `## <名字>` 是不是一台服务器。
#[derive(Debug, Default)]
pub(super) struct KnownServers(HashSet<String>);

impl KnownServers {
    pub(super) fn tool(&mut self, name: &str) {
        if let Some(s) = server_of(name) {
            self.0.insert(s.to_string());
        }
    }

    fn server(&mut self, name: &str) {
        self.0.insert(mcp_server_slug(name));
    }

    pub(super) fn history(&mut self, h: &PoolHistory) {
        h.announced.iter().chain(&h.retracted).for_each(|n| self.tool(n));
        // 宣告过说明的服务器只在那份名单可信时才算已知：认不准的一份里猜进去的（比如说明正文里的 `## setup`）不能下一轮
        // 就摇身一变成了「已知服务器」。
        if h.mcp_exact {
            h.mcp_servers.iter().for_each(|n| self.server(n));
        }
        h.last_failed.servers.iter().for_each(|f| self.server(&f.name));
        h.last_pending.names.iter().chain(&h.last_needs_auth.names).for_each(|n| self.server(n));
    }

    pub(super) fn delta(&mut self, d: &PoolDelta) {
        d.added.iter().for_each(|n| self.tool(n));
        d.failed.servers.iter().for_each(|f| self.server(&f.name));
        d.pending.names.iter().chain(&d.needs_auth.names).for_each(|n| self.server(n));
    }

    pub(super) fn knows(&self, name: &str) -> bool {
        self.0.contains(&mcp_server_slug(name))
    }
}

/// 这一轮从延迟工具池里走掉的工具（[`PoolDelta::departed`]）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct McpDeparted {
    /// 提醒里列的工具总数。
    pub(crate) total: usize,
    /// 服务器这一轮没连上（在「没连上」名单里），官方记 `departed_disconnected`。
    pub(crate) disconnected: usize,
    /// 服务器连着、只是这里不再给这个工具，官方记 `departed_not_offered`。原因看不出的两边都不记。
    pub(crate) not_offered: usize,
    /// 按归属分的个数，下标见 [`OWNERS`]；归属看不出的不记。
    pub(crate) by_owner: [usize; 4],
    /// 走掉的没记全（[`PoolDelta::removed_incomplete`]）：上面几样都只是部分，不报。
    pub(crate) incomplete: bool,
    /// 哪几种归属的计数说不准（按服务器归并、分不出子前缀、账本又配不回名字的那组可能落进的那几种）：那几种不报，别的照报。
    pub(crate) uncertain: [bool; 4],
}

/// 「没连上」名单换成工具名写法的服务器集合，与名单完不完整（[`McpDeparted::note`] 分原因用）。
struct FailedSlugs {
    slugs: HashSet<String>,
    complete: bool,
}

impl McpDeparted {
    fn note(
        &mut self,
        owner: Option<usize>,
        server: Option<&str>,
        n: usize,
        cause: Cause,
        failed: &FailedSlugs,
    ) {
        self.total = self.total.saturating_add(n);
        if let Some(o) = owner {
            self.by_owner[o] = self.by_owner[o].saturating_add(n);
        }
        // 服务器在名单里就是断了；不在名单里只有名单完整时才能说是「连着、不再给」——截断（`…and N more`）或有不写
        // 名字的那个时，没列出来的也可能没连上，原因不填。
        if cause == Cause::ByFailedList {
            if server.is_some_and(|s| failed.slugs.contains(s)) {
                self.disconnected = self.disconnected.saturating_add(n);
            } else if failed.complete {
                self.not_offered = self.not_offered.saturating_add(n);
            }
        }
    }

    /// 按原因分的 `departed_<原因>` 计数，照先 `disconnected` 后 `not_offered` 排（官方按工具出现的先后，两种原因混在
    /// 同一轮的还没见过）。
    pub(crate) fn causes(self) -> Vec<(&'static str, usize)> {
        [("departed_disconnected", self.disconnected), ("departed_not_offered", self.not_offered)]
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .collect()
    }

    /// 2.1.321 `tengu_deferred_tools_pool_change_by_owner` 的 `(reason, count)`：走掉的工具按归属分组，各报一条。
    pub(crate) fn owners(self) -> Vec<(String, usize)> {
        if self.incomplete {
            return Vec::new();
        }
        owner_counts("removed", self.by_owner, self.uncertain)
    }
}

/// 各归属的计数里报得出的那几条 `(<前缀>_<归属>, 个数)`：有数、又不是说不准的，按 [`OWNERS`] 的顺序。
fn owner_counts(prefix: &str, counts: [usize; 4], uncertain: [bool; 4]) -> Vec<(String, usize)> {
    OWNERS
        .iter()
        .zip(counts)
        .zip(uncertain)
        .filter(|((_, n), u)| *n > 0 && !*u)
        .map(|((o, n), _)| (format!("{prefix}_{o}"), n))
        .collect()
}

/// 汇总那条要的「这一轮」与「这一轮之前」：同一份移除事实（[`super::shape::RequestShape::pool_delta`]）同时给按归属
/// 那几条与汇总那条用。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PoolTurn<'a> {
    pub(super) departed: McpDeparted,
    pub(super) history: Option<&'a PoolHistory>,
    pub(super) delta: Option<&'a PoolDelta>,
    pub(super) mcp: Option<&'a McpInstr>,
}

impl<'a> PoolTurn<'a> {
    /// 这一轮工具池变了、汇总要按这一轮报：有工具走掉，或接着已有对话的这一轮来了一条附件（新增、连回来、名单变化；官方
    /// 只在有变化时才组装附件），哪怕之前一条附件都没有过。会话头一轮的那条照模板，不在这里。
    /// `cap/auto-2.1.321-20261110-bashfail/2.1.321` 请求 `00063`（`-p` 工具结果续轮，新宣告 25 个）→ 批次 `00065`。
    pub(super) fn changed(&self) -> bool {
        self.departed.total > 0
            || self.departed.incomplete
            || (self.delta.is_some_and(|d| !d.replace_only()) && self.continuation())
    }

    /// 这一轮要列进 `tengu_attachments` 的工具池附件：有变化的（[`Self::changed`]），或接着已有对话、只有定义更新的那份
    /// （[`PoolDelta::replace_only`]：附件照有，汇总不报）。
    pub(super) fn attachment_delta(&self) -> Option<&'a PoolDelta> {
        let d = self.delta?;
        (self.changed() || (d.replace_only() && self.continuation())).then_some(d)
    }

    /// 接着已有对话（体里带着前面几段的历史，`--continue` 接回的会话头一条就是这样）而工具池没有变化：官方按历史里宣告过的
    /// 算增量，没有变化就不组装附件，汇总那条也不报——首轮模板里那条不能照发（`cap/auto-2.1.322-20261110-full` 请求
    /// `00763`：MCP 服务器这次没断开，批次 `00767` 与这个会话之后的批次里都没有 `tengu_deferred_tools_pool_change`；2.1.321
    /// 那次 `00756` 碰巧有两个工具走掉，按 [`Self::changed`] 报）。历史里一条工具池附件都没有的不算（官方那时是整池头一回
    /// 宣告，照报）；账本不在的（线程增量没接上底本）看不出，照旧。
    pub(super) fn resumed_unchanged(&self) -> bool {
        self.history.is_some_and(|h| h.turns > 0 && h.dtd_count() > 0) && !self.changed()
    }

    /// 这一轮接着已有对话：账本不在（线程增量没接上底本）的必是；在的看前面有没有过对话。
    fn continuation(&self) -> bool {
        self.history.is_none_or(|h| h.turns > 0)
    }

    /// 会话（或子代理）头一轮、带着工具池附件：汇总那条按这一轮的事实填，首轮的消息数、附件数本来就是 0（`bashfail`
    /// 两组首轮 `addedCount` 308 / 304，子代理 `-full/00103` 首轮 399）。
    pub(super) fn first_turn(&self) -> bool {
        self.delta.is_some() && self.history.is_some_and(|h| h.turns == 0)
    }

    /// 续轮带着 MCP 服务器说明附件：同样报 `tengu_mcp_instructions_pool_change`（`00065` / `00062` 续轮各一条，`-full/00111`
    /// 子代理续轮一条）。
    pub(super) fn mcp_changed(&self) -> bool {
        self.mcp.is_some() && self.continuation()
    }

    /// 2.1.321 `-p` 续轮里工具表刷新后 MCP 工具数变了（官方 `tengu_mcp_tools_refreshed_mid_turn`）：（刷新前, 刷新后）。刷新前
    /// 是账本里宣告过的 MCP 工具加上正文里非延迟的 MCP 工具，刷新后再加这一轮新宣告的、减走掉的（`00065`：292 → 314，
    /// `00062`：288 → 310）。账本不准、新宣告分不清时 `None`；没变也 `None`。
    pub(super) fn mcp_refresh(&self, non_deferred_mcp: usize) -> Option<(usize, usize)> {
        // 走掉的没记全：刷新后剩几个说不准，不报。
        if !self.changed() || self.departed.incomplete {
            return None;
        }
        let h = self.history?;
        let old = h.announced_mcp()? + non_deferred_mcp;
        let d = self.delta?;
        let (fresh, _) = d.fresh_names(Some(h))?;
        let added = fresh.iter().filter(|n| n.starts_with("mcp__")).count();
        let removed: usize = d
            .removed
            .iter()
            .map(|(g, _)| match g {
                Gone::Name(n) => usize::from(n.starts_with("mcp__")),
                Gone::Server { count, .. } => *count,
            })
            .sum();
        let new = (old + added).saturating_sub(removed);
        (new != old).then_some((old, new))
    }

    /// 2.1.321 汇总前按归属报的 `tengu_deferred_tools_pool_change_by_owner`：先 `removed_*`（走掉的，归属看不出的不报），
    /// 再 `returned_*`（连回来的，官方 `uEn("returned", mn)`；要账本配得出名字才报）。两种各自独立：只连回来、没有走掉
    /// 的一轮照报 `returned_*`。
    pub(super) fn owner_events(&self) -> Vec<(String, usize)> {
        if !self.changed() {
            return Vec::new();
        }
        let mut out = self.departed.owners();
        if let Some(d) = self.delta {
            out.extend(d.returned_owners(self.history));
        }
        out
    }
}

/// `tengu_inline_tools_announced` 的 meta（[`PoolDelta::inline_counts`]）。
pub(super) fn inline_announced(d: &PoolDelta) -> Option<Value> {
    let (definitions, replacements) = d.inline_counts()?;
    Some(json!({
        "definitions": definitions,
        "replacements": replacements,
        "pointers": 0,
        "withdrawals": 0,
    }))
}

/// 汇总那条的模板事件：交互式在首次输入那段，`-p` 在启动那段（`cc_telemetry_template_*_sdk.json`）。续轮按这一轮的
/// 工具池变化补发时借它的位置与键序。
pub(super) fn summary_template(
    tpl: &super::template::Template,
) -> Option<&super::template::TplEvent> {
    tpl.prompt
        .iter()
        .chain(tpl.startup.iter())
        .find(|e| e.name == "tengu_deferred_tools_pool_change")
}

/// MCP 服务器说明汇总那条的模板事件（交互式在首次输入段、`-p` 在启动段）。
pub(super) fn mcp_template(tpl: &super::template::Template) -> Option<&super::template::TplEvent> {
    tpl.prompt
        .iter()
        .chain(tpl.startup.iter())
        .find(|e| e.name == "tengu_mcp_instructions_pool_change")
}

/// 2.1.319 起 `tengu_mcp_instructions_pool_change` 按这一轮的 MCP 服务器说明附件与账本填（官方 `$Tn`）：`addedCount` 是这一节
/// 列出的服务器数（渲染出来的就是这一轮新宣告的那几台），`removedCount` 是断开名单，`priorAnnouncedCount` / `midCount` 是
/// 账本里宣告过的服务器数与叠过几条。头一轮的 `messagesLength` / `attachmentCount` 是 0 照报，续轮复算不出不报；账本
/// 不在的那两项也不报。对照 `bashfail/00063` → `00065`（首轮 6、续轮 3 / 6 / 1）、`-fable/00060` → `00062`（5、3 / 5 / 1）、
/// `-full/00107` → `00111` 子代理（10 / 0 / 0）。
pub(super) fn fill_mcp_summary(meta: &mut Value, version: &str, pool: PoolTurn<'_>) {
    let Some(m) = pool.mcp else { return };
    let first = pool.history.is_some_and(|h| h.turns == 0);
    if !(first || pool.mcp_changed()) || !super::kind::version_at_least(version, "2.1.319") {
        return;
    }
    let h = pool.history;
    let facts = [
        ("addedCount", m.certain.then(|| json!(m.added.len()))),
        ("removedCount", (!m.removed_incomplete).then(|| json!(m.removed.len()))),
        ("priorAnnouncedCount", h.filter(|h| h.mcp_exact).map(|h| json!(h.mcp_servers.len()))),
        ("messagesLength", first.then(|| json!(0))),
        ("attachmentCount", first.then(|| json!(0))),
        ("midCount", h.map(|h| json!(h.mid))),
    ];
    let Some(obj) = meta.as_object_mut() else { return };
    for (k, v) in facts {
        match v {
            Some(v) => {
                if let Some(slot) = obj.get_mut(k) {
                    *slot = v;
                }
            }
            None => {
                obj.shift_remove(k);
            }
        }
    }
}

/// 2.1.319：这一轮延迟工具池里有工具走掉时，汇总那条按这一轮的事实与账本改：`removedCount` 与紧跟其后按原因分的
/// `departed_*`；`addedCount` / `readdedCount` / `unlistedCount` 按这一轮的宣告段落（没有宣告就都是 0，两份样本都是）；
/// `failedCount` / `lastFailedCount` / `failedChanged`、`priorAnnouncedCount`、`dtdCount` 按账本。
///
/// 对照 `cap/auto-2.1.321-20261110-full` 请求 `00756` → 批次 `00760`（415、`dtdCount: 1`、失败 2 → 2 没变）与
/// `cap/auto-2.1.319-20261106-confirm/2.1.319` 请求 `00162` → 批次 `00165`（409、1、2 → 3 变了）。
///
/// 这只是部分对齐：`messagesLength` / `attachmentCount` / `attachmentTypesSeen` 数的是客户端内部的消息列表（每条附件
/// 各占一条，两份样本出站体都是 7 条、官方报 23 / 25），luban 还复算不出，这一轮就不报这三项；账本看不出的
/// （线程续轮没有底本、名单截断、摘要配不回名字）同样不报，不拿模板里首轮的零值充数。
pub(super) fn fill_summary(meta: &mut Value, version: &str, pool: PoolTurn<'_>) {
    let departed = pool.departed;
    let first = pool.first_turn();
    if !(pool.changed() || first) || !super::kind::version_at_least(version, "2.1.319") {
        return;
    }
    let Some(obj) = meta.as_object_mut() else { return };
    if departed.incomplete {
        // 走掉了几个说不准：总数与按原因分的都不报。
        obj.shift_remove("removedCount");
    } else {
        obj.insert("removedCount".into(), json!(departed.total));
        let causes = departed.causes().into_iter().map(|(k, n)| (k, json!(n))).collect();
        super::kind::insert_after(meta, "removedCount", causes);
    }

    let (d, h) = (pool.delta, pool.history);
    let [failed_now, failed_last, mut failed_changed] = compare(
        d.map(|d| (d.failed.count, d.failed.complete)),
        h.map(|h| (h.last_failed.count, h.last_failed.complete)),
        || d.zip(h).is_some_and(|(d, h)| d.failed.servers == h.last_failed.servers),
    );
    // 逐项看着一样，可里头有截断过的错误文本（截下来的前缀相同、原文可能不同）：变没变看不出，不报。
    let truncated = |l: &FailedList| l.servers.iter().any(|f| f.truncated);
    if failed_changed == Some(json!(false))
        && (d.is_some_and(|d| truncated(&d.failed)) || h.is_some_and(|h| truncated(&h.last_failed)))
    {
        failed_changed = None;
    }
    let [pending_now, pending_last, pending_changed] = compare(
        d.map(|d| (d.pending.count, d.pending.complete)),
        h.map(|h| (h.last_pending.count, h.last_pending.complete)),
        || d.zip(h).is_some_and(|(d, h)| d.pending.names == h.last_pending.names),
    );
    let [auth_now, auth_last, auth_changed] = compare(
        d.map(|d| (d.needs_auth.count, d.needs_auth.complete)),
        h.map(|h| (h.last_needs_auth.count, h.last_needs_auth.complete)),
        || d.zip(h).is_some_and(|(d, h)| d.needs_auth.names == h.last_needs_auth.names),
    );
    // 新宣告的按这一轮的宣告段落与账本分新增 / 连回来。`unlistedCount`（官方 `Zt`：池里有、还没逐行列过的）：新宣告里
    // 除了连回来的（先前列过）都是头一回列，`bashfail/00063` → `00065` 是 25 / 25、首轮 308 / 308。
    let additions = d.and_then(|d| d.additions(h));
    let facts = [
        ("addedCount", additions.map(|(a, _)| json!(a))),
        ("readdedCount", additions.map(|(_, r)| json!(r))),
        ("unlistedCount", additions.map(|(a, r)| json!(a - r))),
        ("pendingChanged", pending_changed),
        ("pendingCount", pending_now),
        ("lastPendingCount", pending_last),
        ("needsAuthChanged", auth_changed),
        ("needsAuthCount", auth_now),
        ("lastNeedsAuthCount", auth_last),
        ("failedChanged", failed_changed),
        ("failedCount", failed_now),
        ("lastFailedCount", failed_last),
        (
            "priorAnnouncedCount",
            pool.history.and_then(PoolHistory::announced_count).map(|n| json!(n)),
        ),
        ("dtdCount", pool.history.map(|h| json!(h.dtd_count()))),
        // 头一轮的内部消息列表是空的：0 / 0 / 空串（模板里本就是这些值），照报；续轮复算不出，不报。
        ("messagesLength", first.then(|| json!(0))),
        ("attachmentCount", first.then(|| json!(0))),
        ("attachmentTypesSeen", first.then(|| json!(""))),
    ];
    let Some(obj) = meta.as_object_mut() else { return };
    for (k, v) in facts {
        match v {
            Some(v) => {
                if let Some(slot) = obj.get_mut(k) {
                    *slot = v;
                }
            }
            None => {
                obj.shift_remove(k);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "The following deferred tools are no longer available (their MCP server disconnected). Do not search for them — ToolSearch will return no match:";

    fn delta(text: &str) -> PoolDelta {
        PoolDelta::parse([text]).unwrap_or_default()
    }

    /// 官方 `_te`：Claude_Browser / Claude_Preview 也算 Chrome；`mcp__remote-devices__` 开头的另分两类。
    #[test]
    fn owners_follow_the_official_prefix_table() {
        let text = format!(
            "{HEAD}\nmcp__Claude_Browser__open\nmcp__Claude_Preview__shot\nmcp__claude-in-chrome__nav\n\
             mcp__remote-devices__Claude_Browser__open\nmcp__remote-devices__files__read\nmcp__a__b\nSomeBuiltin\n\nrest"
        );
        let d = delta(&text).departed(None);
        assert_eq!(d.total, 7);
        assert_eq!(
            d.owners(),
            vec![
                ("removed_other".to_string(), 2),
                ("removed_chrome".to_string(), 3),
                ("removed_remote_devices_other".to_string(), 1),
                ("removed_remote_devices_browser".to_string(), 1),
            ]
        );
        // 没有 ToolSearch 的会话里那句写 `tools`。
        let plain = "The following tools are no longer available (their MCP server disconnected). Do not call them:\nmcp__a__b";
        assert_eq!(delta(plain).departed(None).total, 1);
    }

    /// 超过 30 个时官方改发带总数的摘要（`Lre`：按服务器归并，多于一个的带个数）。
    #[test]
    fn the_summary_form_counts_per_server() {
        let text = "31 deferred tools are no longer available (MCP server disconnected): mcp__a__* (28), mcp__claude-in-chrome__* (2), mcp__solo__*. Do not search for them — ToolSearch will return no match.\n\n\
            The following MCP servers are configured but failed to connect — their tools (typically named mcp__<server>__*) are unavailable for this session:\na (CONNECTION_FAILED): \"x\"";
        let d = delta(text).departed(None);
        assert_eq!(
            (d.total, d.disconnected, d.not_offered),
            (31, 28, 3),
            "a 在没连上的名单里，其余两台不在"
        );
        assert_eq!(
            d.owners(),
            vec![("removed_other".to_string(), 29), ("removed_chrome".to_string(), 2)]
        );
        assert_eq!(d.causes(), vec![("departed_disconnected", 28), ("departed_not_offered", 3)]);
        // 句子中间出现的同样文字（前面不是 `<N> deferred tools` 顶格）不算。
        let prose = "He said 31 deferred tools are no longer available (MCP server disconnected): mcp__a__* (31). Do not search.";
        assert_eq!(delta(prose).departed(None).total, 0);
    }

    /// 摘要里的个数是构造出来的超大值、或加起来与句首对不上：整句不算，也不会溢出。
    #[test]
    fn summary_counts_are_bounded_and_must_add_up() {
        let huge = format!(
            "31 deferred tools are no longer available (MCP server disconnected): mcp__a__* ({}), mcp__b__* (1). Do not search.",
            usize::MAX
        );
        assert!(PoolDelta::parse([huge.as_str()]).is_none());
        let wrap = format!(
            "31 deferred tools are no longer available (MCP server disconnected): mcp__a__* ({}), mcp__b__* ({}). Do not search.",
            MAX_COUNT,
            usize::MAX - MAX_COUNT + 32
        );
        assert!(PoolDelta::parse([wrap.as_str()]).is_none());
        let mismatch = "31 deferred tools are no longer available (MCP server disconnected): mcp__a__* (30). Do not search.";
        assert!(PoolDelta::parse([mismatch]).is_none());
        let small = "2 deferred tools are no longer available (MCP server disconnected): mcp__a__* (2). Do not search.";
        assert!(PoolDelta::parse([small]).is_none(), "不到 30 个官方逐项列，不写摘要");
    }

    /// 另一种移除提醒（「in this session」、定义还在上文那段）也算走掉，原因看不出就不分；`remote-devices` 整组的摘要
    /// 分不出浏览器子前缀，账本里有同样个数的名字就逐个配回，没有就只计总数。
    #[test]
    fn other_removal_forms_and_unknown_owners() {
        let text = "The following deferred tools are no longer available in this session. Do not search for them — ToolSearch will return no match:\nmcp__a__x\nmcp__remote-devices__* (31)";
        let d = delta(text);
        let dep = d.departed(None);
        assert_eq!(dep.total, 32);
        assert_eq!((dep.disconnected, dep.not_offered), (0, 0), "原因看不出");
        assert_eq!(dep.owners(), vec![("removed_other".to_string(), 1)], "整组归属未知");

        let mut h = PoolHistory::default();
        let mut names: Vec<String> =
            (0..30).map(|i| format!("mcp__remote-devices__files__t{i}")).collect();
        names.push("mcp__remote-devices__claude-in-chrome__nav".into());
        h.apply(&PoolDelta { added: names, ..Default::default() });
        let dep = d.departed(Some(&h));
        assert_eq!(
            dep.owners(),
            vec![
                ("removed_other".to_string(), 1),
                ("removed_remote_devices_other".to_string(), 30),
                ("removed_remote_devices_browser".to_string(), 1),
            ]
        );

        let gone = "The following tools are no longer available. Do not call them:\nWebFetch";
        assert_eq!(delta(gone).departed(None).total, 1);
    }

    /// 「in this session」清单里的归并组同样受上限约束：越界、不到 30 个（官方 `fgn` 只归并超过 30 个的）、写坏的都拒收。
    #[test]
    fn other_cause_groups_are_validated() {
        let head = OTHER_CAUSE_HEADS[0];
        for bad in [
            format!("mcp__a__* ({})", usize::MAX),
            format!("mcp__a__* ({})", MAX_COUNT + 1),
            "mcp__a__* (0)".to_string(),
            "mcp__a__* (5)".to_string(),
            "mcp__a__* (x)".to_string(),
            "mcp__* (40)".to_string(),
        ] {
            let d = delta(&format!("{head} x:\n{bad}\nmcp__b__y"));
            let dep = d.departed(None);
            assert_eq!((dep.total, dep.by_owner[0]), (1, 1), "{bad}");
        }
        let ok = delta(&format!("{head} x:\nmcp__a__* (31)"));
        assert_eq!(ok.departed(None).total, 31);
    }

    /// 每组都在上限内、合计越界的整份不收；`<system-reminder>` 的闭标签紧跟在最后一项下一行不算工具名。
    #[test]
    fn group_totals_are_bounded_and_closing_tags_end_lists() {
        let head = OTHER_CAUSE_HEADS[0];
        let two = format!("{head} x:\nmcp__a__* ({MAX_COUNT})\nmcp__b__* ({MAX_COUNT})");
        assert_eq!(delta(&two).departed(None).total, 0);
        let split = format!("{head} x:\nmcp__a__* ({MAX_COUNT})\n\n{head} x:\nmcp__b__* (31)");
        assert_eq!(delta(&split).departed(None).total, 0, "分在两段也一样");

        let wrapped = format!("<system-reminder>\n{HEAD}\nmcp__a__b\n</system-reminder>");
        let m = json!({"role": "user", "content": [{"type": "text", "text": wrapped}]});
        let d = PoolDelta::parse(reminder_texts(&m)).unwrap();
        assert_eq!(d.departed(None).total, 1);
    }

    /// 只解析 `<system-reminder>` 标签里的正文：闭标签之后接着的普通用户正文里出现同样的标题不算。
    #[test]
    fn text_after_the_closing_tag_is_not_a_reminder() {
        let text = format!("<system-reminder>\nhello\n</system-reminder>\n{HEAD}\nmcp__a__b");
        let m = json!({"role": "user", "content": [{"type": "text", "text": text}]});
        assert!(PoolDelta::parse(reminder_texts(&m)).is_none());
        let two = format!(
            "<system-reminder>\nx\n</system-reminder>\n<system-reminder>\n{HEAD}\nmcp__a__b\n</system-reminder>\n{HEAD}\nmcp__c__d"
        );
        let m = json!({"role": "user", "content": two});
        assert_eq!(PoolDelta::parse(reminder_texts(&m)).unwrap().departed(None).total, 1);
    }

    /// MCP 服务器说明里的 `\r\n` 与多字节字符：切片按真实字节边界，不 panic；新增那一节到断开名单那句为止，同一段里的
    /// 新增与断开各记各的，账本里断开的服务器拿掉。
    #[test]
    fn mcp_instructions_parse_crlf_and_add_then_remove() {
        let crlf = "# MCP Server Instructions\r\n## a\r\né";
        let m = McpInstr::parse([crlf]).unwrap();
        assert_eq!(m.added, ["a"]);
        assert!(m.token_estimate().is_some_and(|n| n > 0));
        let both = "# MCP Server Instructions\n\nThe following MCP servers have provided instructions:\n\n## new\nuse it\n\n\
            The following MCP servers have disconnected. Their instructions above no longer apply:\nold\n";
        let m = McpInstr::parse([both]).unwrap();
        assert_eq!(
            (m.added.clone(), m.removed.clone()),
            (vec!["new".to_string()], vec!["old".to_string()])
        );
        let mut h = PoolHistory::default();
        h.apply_mcp(&McpInstr { added: vec!["old".into()], ..Default::default() });
        h.apply_mcp(&m);
        assert!(h.mcp_servers.contains("new") && !h.mcp_servers.contains("old"));
    }

    /// 服务器说明正文里自己写的 `## Setup` 不算一台服务器：已知服务器、或按官方排序排在上一台之后的才起新块。估值随块
    /// 合并。
    #[test]
    fn markdown_headings_inside_instructions_are_not_servers() {
        let text = "# MCP Server Instructions\n\nThe following MCP servers have provided instructions:\n\n\
            ## srv.one\nUse it.\n\n## Setup\nRun this first.\n\n## two\nOther.";
        let mut m = McpInstr::parse([text]).unwrap();
        // `Setup` 排在 `srv.one` 前面（localeCompare 不分大小写，`se` < `sr`），排序对不上，不是一台服务器；`two` 排在后面，是。
        assert_eq!(m.added, ["srv.one", "two"]);
        // 已知服务器照认，哪怕排序对不上。
        let mut known = KnownServers::default();
        known.server("Setup");
        m.resolve(|n| known.knows(n));
        assert_eq!(m.added, ["srv.one", "Setup", "two"]);
        let known = KnownServers::default();
        m.resolve(|n| known.knows(n));
        // 并块后的估值等于把 `## Setup` 那段算进第一块。
        let merged = "# MCP Server Instructions\n\nThe following MCP servers have provided instructions:\n\n\
            ## srv.one\nUse it.\n\n## Setup\nRun this first.\n\n## two\nOther.";
        assert_eq!(merged, text);
        let first = "## srv.one\nUse it.\n\n## Setup\nRun this first.";
        let second = "## two\nOther.";
        let expect = json!({
            "type": "mcp_instructions_delta",
            "addedNames": ["srv.one", "two"],
            "addedBlocks": [first, second],
            "removedNames": [],
        });
        assert_eq!(m.token_estimate(), Some(token_round(utf16_len(&expect.to_string()))));
    }

    /// 官方排序是 `localeCompare`（ASCII）：`alpha`、`Beta` 是两台；大写的 `Setup` 排在 `srv.one` 前面，对不上，并掉。
    #[test]
    fn server_order_follows_locale_compare() {
        let two = "# MCP Server Instructions\n\n## alpha\na\n\n## Beta\nb";
        assert_eq!(McpInstr::parse([two]).unwrap().added, ["alpha", "Beta"]);
        assert!(sorts_after("Beta", "alpha") && !sorts_after("Setup", "srv.one"));
        assert!(sorts_after("luban-beehears-01-p01cto", "luban-47-251-186-173-4600-6uai3q"));
        assert!(sorts_after("opdash-prod", "luban-beehears-01-p01cto"));
        assert!(sorts_after(
            "plugin:claude-for-nonprofits:granola",
            "plugin:claude-for-nonprofits:gmail"
        ));
        assert!(sorts_after("rea", "plugin:claude-for-nonprofits:kindora"));
    }

    /// 单行逗号清单、超长名字：边数边校验，项数超过声明或名字过长的整句不收；账本总字节有上限。
    #[test]
    fn comma_lists_and_long_names_are_bounded() {
        let many = (0..100_000).map(|i| format!("x{i}")).collect::<Vec<_>>().join(", ");
        let line = format!(
            "1 deferred tool is available again in this session (announced earlier in this conversation): {many}."
        );
        assert!(PoolDelta::parse([line.as_str()]).is_none());
        let long = "a".repeat(MAX_NAME_LEN + 1);
        let d = PoolDelta::parse([
            format!("The following tools are now available:\n{long}\nok").as_str()
        ])
        .unwrap();
        assert_eq!(d.added, ["ok"]);
        let mut h = PoolHistory::default();
        let names: Vec<String> = (0..20_000).map(|i| format!("mcp__s__{i:0>200}")).collect();
        h.apply(&PoolDelta { added: names, ..Default::default() });
        assert!(h.bytes <= MAX_LEDGER_BYTES && h.announced_count().is_none());
    }

    /// 认不准的分界留为未知：小写 `## setup` 排序对得上又不认识、说明段被一级标题截住，服务器数都不报；全认得的照报。
    #[test]
    fn unconfirmed_instruction_boundaries_stay_unknown() {
        let lower = "# MCP Server Instructions\n\n## a\nuse it\n\n## setup\nrun this";
        let mut m = McpInstr::parse([lower]).unwrap();
        let mut known = KnownServers::default();
        known.tool("mcp__a__x");
        m.resolve(|n| known.knows(n));
        assert!(!m.certain, "`setup` 不认识");
        known.tool("mcp__setup__y");
        m.resolve(|n| known.knows(n));
        assert!(m.certain && m.added == ["a", "setup"]);
        let cut = "# MCP Server Instructions\n\n## a\nuse it\n# Setup\nmore\n## b\nx";
        let mut m = McpInstr::parse([cut]).unwrap();
        m.resolve(|_| true);
        assert!(!m.certain, "被一级标题截住");
        let mut h = PoolHistory::default();
        h.apply_mcp(&m);
        let turn = PoolTurn { history: Some(&h), mcp: Some(&m), ..Default::default() };
        let mut meta =
            json!({"addedCount": 9, "removedCount": 9, "priorAnnouncedCount": 9, "midCount": 9});
        fill_mcp_summary(&mut meta, "2.1.321", turn);
        assert!(
            meta.get("addedCount").is_none() && meta.get("priorAnnouncedCount").is_none(),
            "{meta}"
        );
    }

    /// 名单的错误文本过长就截断、服务器照列（原因照分 `disconnected`）；整份变更复制的字节有预算，用完的不再复制、名单记为
    /// 不完整（这时没列出的服务器不算连着）。
    #[test]
    fn failed_lists_are_budgeted_and_long_errors_truncated() {
        let long = "e".repeat(5_000);
        let text =
            format!("{HEAD}\nmcp__a__x\n\n{FAILED_HEAD}for this session:\na (X): \"{long}\"");
        let d = delta(&text);
        assert_eq!(d.failed.servers[0].error.as_ref().unwrap().len(), MAX_ERROR_LEN);
        assert!(d.failed.complete);
        assert_eq!(d.departed(None).disconnected, 1);

        let many = (0..300).map(|i| format!("s{i} (X): \"{long}\"")).collect::<Vec<_>>().join("\n");
        let text = format!("{HEAD}\nmcp__zz__x\n\n{FAILED_HEAD}for this session:\n{many}");
        let d = delta(&text);
        let kept: usize = d
            .failed
            .servers
            .iter()
            .map(|f| {
                f.name.len()
                    + f.code.as_ref().map_or(0, String::len)
                    + f.error.as_ref().map_or(0, String::len)
            })
            .sum();
        assert!(kept <= MAX_DELTA_BYTES, "{kept}");
        assert!(!d.failed.complete && d.failed.count == 300);
        let dep = d.departed(None);
        assert_eq!((dep.disconnected, dep.not_offered), (0, 0), "名单不完整，原因不填");
    }

    /// 预算按「正文 + 每项固定开销」扣：空条目、计数格式的恢复名单也不白记。
    #[test]
    fn every_kept_entry_is_charged() {
        let empties = format!("{NEEDS_AUTH_HEAD}\n{}, \u{2026}and 5 more", ", ".repeat(80_000));
        let d = delta(&empties);
        assert!(d.needs_auth.names.is_empty() && !d.needs_auth.complete);
        let mut kept = 0usize;
        let mut text = String::new();
        for k in 0..9 {
            let names = (0..10_000).map(|i| format!("n{k}_{i}")).collect::<Vec<_>>().join(", ");
            text.push_str(&format!(
                "10000 deferred tools are available again in this session (announced earlier in this conversation): {names}.\n"
            ));
        }
        let d = delta(&text);
        kept += d.added.iter().map(|n| n.len() + ENTRY_COST).sum::<usize>();
        assert!(kept <= MAX_DELTA_BYTES && d.overflow, "{kept}");
    }

    /// 认不准的一份说明里猜进账本的名字（`## setup`）下一轮不算已知服务器。
    #[test]
    fn uncertain_instruction_names_are_not_known_next_turn() {
        let text = "# MCP Server Instructions\n\n## a\nuse it\n\n## setup\nrun this";
        let mut m = McpInstr::parse([text]).unwrap();
        let mut known = KnownServers::default();
        known.tool("mcp__a__x");
        m.resolve(|n| known.knows(n));
        let mut h = PoolHistory::default();
        h.apply_mcp(&m);
        let mut next = KnownServers::default();
        next.history(&h);
        assert!(!next.knows("setup"));
    }

    /// 错误文本截断过、截下来的一样：失败名单变没变看不出，`failedChanged` 不报。
    #[test]
    fn truncated_errors_leave_failed_changed_unknown() {
        let err = |tail: char| format!("{}{tail}", "e".repeat(MAX_ERROR_LEN));
        let list = |e: &str| {
            format!("{HEAD}\nmcp__a__x\n\n{FAILED_HEAD}for this session:\na (X): \"{e}\"")
        };
        let mut h = PoolHistory { turns: 1, ..Default::default() };
        h.apply(&delta(&list(&err('1'))));
        let d = delta(&list(&err('2')));
        let turn = PoolTurn {
            departed: d.departed(Some(&h)),
            history: Some(&h),
            delta: Some(&d),
            mcp: None,
        };
        let mut meta = json!({"removedCount": 0, "failedChanged": false, "failedCount": 0, "lastFailedCount": 0});
        fill_summary(&mut meta, "2.1.321", turn);
        assert!(meta.get("failedChanged").is_none(), "{meta}");
        assert_eq!((&meta["failedCount"], &meta["lastFailedCount"]), (&json!(1), &json!(1)));
    }

    /// 走掉的没记全（几组合计越界、预算用完）：`removedCount` 与按原因、按归属的都不报，不拿零或部分计数充数。
    #[test]
    fn incomplete_removals_leave_the_count_unknown() {
        let head = OTHER_CAUSE_HEADS[0];
        let over = format!("{head} x:\nmcp__a__* ({MAX_COUNT})\nmcp__b__* ({MAX_COUNT})");
        let many =
            (0..9_000).map(|i| format!("mcp__s__tool_{i:04}")).collect::<Vec<_>>().join("\n");
        let big = format!("{HEAD}\n{many}");
        for text in [over, big] {
            let d = delta(&text);
            let h = PoolHistory { turns: 1, ..Default::default() };
            let dep = d.departed(Some(&h));
            assert!(dep.incomplete && dep.owners().is_empty());
            let turn = PoolTurn { departed: dep, history: Some(&h), delta: Some(&d), mcp: None };
            assert!(turn.changed());
            let mut meta = json!({"addedCount": 0, "removedCount": 0, "dtdCount": 0});
            fill_summary(&mut meta, "2.1.321", turn);
            assert!(meta.get("removedCount").is_none(), "{meta}");
            assert!(
                meta.get("departed_disconnected").is_none()
                    && meta.get("departed_not_offered").is_none()
            );
        }
    }

    /// 分原因时「没连上」名单只换算一次：两千个走掉的工具对两千台不相干的服务器也很快。
    #[test]
    fn departure_causes_do_not_rescan_the_failed_list() {
        let gone = (0..2_000).map(|i| format!("mcp__g{i}__t")).collect::<Vec<_>>().join("\n");
        let failed = (0..2_000).map(|i| format!("f{i} (X)")).collect::<Vec<_>>().join("\n");
        let text = format!("{HEAD}\n{gone}\n\n{FAILED_HEAD}for this session:\n{failed}");
        let d = delta(&text);
        let at = std::time::Instant::now();
        let dep = d.departed(None);
        assert_eq!(dep.not_offered, 2_000);
        assert!(at.elapsed() < std::time::Duration::from_millis(300), "{:?}", at.elapsed());
    }

    /// 宣告过说明的服务器记满账本后再来一台：记不下，之后不报 `priorAnnouncedCount`。
    #[test]
    fn a_full_instruction_ledger_is_not_exact() {
        let mut h = PoolHistory { bytes: MAX_LEDGER_BYTES - 3, ..Default::default() };
        let m = McpInstr { added: vec!["abcd".into()], certain: true, ..Default::default() };
        h.apply_mcp(&m);
        assert!(!h.mcp_exact && !h.mcp_servers.contains("abcd"));
    }

    /// 走掉的没记全时，不确定贯穿到刷新计数与之后的账本；连回来的合计越界同样当作新增分不清。
    #[test]
    fn incomplete_removals_reach_the_refresh_and_the_ledger() {
        let head = OTHER_CAUSE_HEADS[0];
        let text = format!(
            "The following tools are now available:\nmcp__n__a\n\n{head} x:\nmcp__a__* ({MAX_COUNT})\nmcp__b__* ({MAX_COUNT})"
        );
        let d = delta(&text);
        let mut h = PoolHistory { turns: 1, ..Default::default() };
        h.apply(&delta("The following tools are now available:\nmcp__old__x"));
        let turn = PoolTurn {
            departed: d.departed(Some(&h)),
            history: Some(&h),
            delta: Some(&d),
            mcp: None,
        };
        assert_eq!(turn.mcp_refresh(0), None);
        h.apply(&d);
        assert_eq!(h.announced_count(), None);

        // 两句各自合法，合起来超了上限。
        let line = |s: &str| {
            format!(
                "{MAX_COUNT} deferred tools are available again (MCP server reconnected — names announced earlier in this conversation): mcp__{s}__* ({MAX_COUNT})."
            )
        };
        let readd = format!("{}\n{}", line("a"), line("b"));
        let d = delta(&readd);
        assert!(d.overflow && d.additions(Some(&h)).is_none());
    }

    /// MCP 说明解析时没记全：断开名单里过长的名字让 `removedCount` 不报；标题过了项数上限让服务器数不报。
    #[test]
    fn instruction_parse_truncation_is_not_exact() {
        let long = "x".repeat(MAX_NAME_LEN + 1);
        let gone = format!("{MCP_INSTRUCTIONS_GONE}:\nold\n{long}\n");
        let m = McpInstr::parse([gone.as_str()]).unwrap();
        let h = PoolHistory { turns: 1, ..Default::default() };
        let turn = PoolTurn { history: Some(&h), mcp: Some(&m), ..Default::default() };
        let mut meta = json!({"addedCount": 9, "removedCount": 9});
        fill_mcp_summary(&mut meta, "2.1.321", turn);
        assert!(meta.get("removedCount").is_none(), "{meta}");

        let heads = (0..MAX_LIST_ITEMS + 2)
            .map(|i| format!("## s{i:05}\nx"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut m =
            McpInstr::parse([format!("{MCP_INSTRUCTIONS_HEAD}\n{heads}").as_str()]).unwrap();
        m.resolve(|_| true);
        assert!(m.added.len() <= MAX_LIST_ITEMS && !m.certain);
    }

    /// 单条摘要声明超上限：认出是哪种句子，记为数不清（走掉的总数、连回来的新增都不报），不当成没有这句。
    #[test]
    fn an_oversized_single_summary_is_unknown_not_absent() {
        let gone = format!(
            "{} deferred tools are no longer available (MCP server disconnected): mcp__a__* ({}). Do not search.",
            MAX_COUNT + 1,
            MAX_COUNT + 1
        );
        let d = PoolDelta::parse([gone.as_str()]).unwrap();
        let dep = d.departed(None);
        assert!(dep.incomplete && dep.owners().is_empty());
        let h = PoolHistory { turns: 1, ..Default::default() };
        let turn = PoolTurn { departed: dep, history: Some(&h), delta: Some(&d), mcp: None };
        assert_eq!(turn.mcp_refresh(0), None);
        let back = format!(
            "{} deferred tools are available again (MCP server reconnected — names announced earlier in this conversation): mcp__a__* ({}).",
            MAX_COUNT + 1,
            MAX_COUNT + 1
        );
        let d = PoolDelta::parse([back.as_str()]).unwrap();
        assert!(d.overflow && d.additions(Some(&h)).is_none());
    }

    /// 附件数据没记全时估值不报（工具池、MCP 说明两种）。
    #[test]
    fn incomplete_attachments_have_no_estimate() {
        let long = "x".repeat(MAX_NAME_LEN + 1);
        let m = McpInstr::parse([format!("{MCP_INSTRUCTIONS_GONE}:\n{long}\n").as_str()]).unwrap();
        assert_eq!(m.token_estimate(), None);
        let d = delta(&format!(
            "{HEAD}\n{}",
            (0..MAX_LIST_ITEMS + 5).map(|i| format!("mcp__a__{i}")).collect::<Vec<_>>().join("\n")
        ));
        assert_eq!(d.token_estimate(None, false), None);
        assert!(delta(&format!("{HEAD}\nmcp__a__x")).token_estimate(None, false).is_some());
    }

    /// 不带句首总数的移除清单里有越界的一组：数不清，走掉的总数与账本都记为不全。
    #[test]
    fn an_oversized_group_in_a_plain_removal_list_is_incomplete() {
        let head = OTHER_CAUSE_HEADS[0];
        let d = delta(&format!("{head} x:\nmcp__a__* ({})\nmcp__b__y", MAX_COUNT + 1));
        assert!(d.departed(None).incomplete);
        let mut h = PoolHistory::default();
        h.apply(&d);
        assert_eq!(h.announced_count(), None);
    }

    /// 估值算不准的几种也不报：说明段被一级标题截住、错误文本截断过、连回来的配不回名字。
    #[test]
    fn more_unreliable_estimates_are_omitted() {
        let cut = format!("{MCP_INSTRUCTIONS_HEAD}\n\n## a\nx\n# Setup\n{}", "y".repeat(2000));
        assert_eq!(McpInstr::parse([cut.as_str()]).unwrap().token_estimate(), None);
        let long = "e".repeat(MAX_ERROR_LEN + 1);
        let d = delta(&format!(
            "{HEAD}\nmcp__a__x\n\n{FAILED_HEAD}for this session:\na (X): \"{long}\""
        ));
        assert_eq!(d.token_estimate(None, false), None);
        let h = PoolHistory { turns: 1, ..Default::default() };
        let back = delta(
            "31 deferred tools are available again (MCP server reconnected — names announced earlier in this conversation): mcp__q__* (31).",
        );
        assert_eq!(back.token_estimate(Some(&h), false), None);
    }

    /// 按服务器归并的移除：账本能配回全部名字时，估值与逐个列名的一样；逐行宣告的新增缺账本时估值不报。
    #[test]
    fn grouped_removals_estimate_like_listed_ones() {
        let names: Vec<String> = (0..31).map(|i| format!("mcp__a__t{i:02}")).collect();
        let mut h = PoolHistory { turns: 1, ..Default::default() };
        h.apply(&PoolDelta { added: names.clone(), ..Default::default() });
        let grouped = delta(
            "31 deferred tools are no longer available (MCP server disconnected): mcp__a__* (31). Do not search.",
        );
        let listed = delta(&format!("{HEAD}\n{}", names.join("\n")));
        assert_eq!(grouped.token_estimate(Some(&h), false), listed.token_estimate(Some(&h), false));
        assert!(grouped.token_estimate(Some(&h), false).is_some());
        assert_eq!(grouped.token_estimate(None, false), None, "账本不在配不回");
        // 逐行宣告的新增要账本才分得清是不是连回来的：账本不在不报。
        let plain = delta("The following tools are now available:\nWebSearch");
        assert_eq!(plain.token_estimate(None, false), None);
    }

    /// 账本失准时，按服务器归并的移除不拿它配名字：归属与估值都不报（总数照报）。
    #[test]
    fn an_inexact_ledger_does_not_resolve_groups() {
        let names: Vec<String> =
            (0..31).map(|i| format!("mcp__remote-devices__claude-in-chrome__t{i:02}")).collect();
        let mut h = PoolHistory { turns: 1, exact: false, ..Default::default() };
        h.apply(&PoolDelta { added: names, ..Default::default() });
        let d = delta(
            "31 deferred tools are no longer available (MCP server disconnected): mcp__remote-devices__* (31). Do not search.",
        );
        let dep = d.departed(Some(&h));
        assert_eq!(dep.total, 31);
        assert!(dep.owners().is_empty(), "{:?}", dep.owners());
        assert_eq!(d.token_estimate(Some(&h), false), None);
    }

    /// 按服务器归并、配回名字的恢复工具同时算进 `addedNames` 与 `readdedNames`（官方 `ePi`）。
    #[test]
    fn readded_names_are_also_added_names() {
        let names: Vec<String> = (0..31).map(|i| format!("mcp__a__t{i:02}")).collect();
        let mut h = PoolHistory { turns: 1, ..Default::default() };
        h.apply(&PoolDelta { added: names.clone(), ..Default::default() });
        h.apply(&delta("31 deferred tools are no longer available (MCP server disconnected): mcp__a__* (31). Do not search."));
        let d = delta(
            "31 deferred tools are available again (MCP server reconnected — names announced earlier in this conversation): mcp__a__* (31).",
        );
        assert_eq!(d.additions(Some(&h)), Some((31, 31)));
        let expect = json!({
            "type": "deferred_tools_delta",
            "addedNames": names,
            "addedLines": [],
            "removedNames": [],
            "wireHiddenNames": [],
            "readdedNames": names,
            "pendingMcpServers": [],
            "needsAuthMcpServers": [],
            "failedMcpServers": [],
        });
        assert_eq!(
            d.token_estimate(Some(&h), false),
            Some(token_round(utf16_len(&expect.to_string())))
        );
    }

    /// 正文逐个点名「又能用了」的工具：账本没记过它也照算连回来（新增、恢复各 1，`returned_*` 照报），估值里 `addedNames`
    /// 与 `readdedNames` 都有它；没有账本也分得清。
    #[test]
    fn listed_readds_are_readded_without_the_ledger() {
        let d = delta(
            "1 deferred tool is available again in this session (announced earlier in this conversation): WebSearch.",
        );
        let h = PoolHistory { turns: 1, ..Default::default() };
        assert_eq!(d.additions(Some(&h)), Some((1, 1)));
        assert_eq!(d.additions(None), Some((1, 1)));
        let turn = PoolTurn {
            departed: d.departed(Some(&h)),
            history: Some(&h),
            delta: Some(&d),
            mcp: None,
        };
        assert_eq!(turn.owner_events(), vec![("returned_other".to_string(), 1)]);
        let expect = json!({
            "type": "deferred_tools_delta",
            "addedNames": ["WebSearch"],
            "addedLines": [],
            "removedNames": [],
            "wireHiddenNames": [],
            "readdedNames": ["WebSearch"],
            "pendingMcpServers": [],
            "needsAuthMcpServers": [],
            "failedMcpServers": [],
        });
        assert_eq!(
            d.token_estimate(None, false),
            Some(token_round(utf16_len(&expect.to_string())))
        );
    }

    /// 走掉的一组分不出子前缀、账本又配不回名字：只有它可能落进的那两种归属不报，别的照报。
    #[test]
    fn an_ambiguous_removed_group_only_hides_its_own_owners() {
        let d = delta(&format!(
            "{HEAD}\nWebSearch\n\n31 deferred tools are no longer available (MCP server disconnected): mcp__remote-devices__* (31). Do not search."
        ));
        let dep = d.departed(None);
        assert_eq!(dep.total, 32);
        assert_eq!(dep.owners(), vec![("removed_other".to_string(), 1)]);
        let both = delta(&format!(
            "{HEAD}\nmcp__remote-devices__files__a\n\n31 deferred tools are no longer available (MCP server disconnected): mcp__remote-devices__* (31). Do not search."
        ));
        assert!(both.departed(None).owners().is_empty(), "远程设备那两种都说不准");
    }

    /// 恢复的身份看段落：逐行宣告的（`now available via ToolSearch` 那类，官方 `addedLines`）从来不是连回来的，不会连累别的
    /// 归属；`available again. Their definitions … still apply` 那段（官方 `stillDefinedNames`）明说是连回来的，不看账本。
    /// 「定义更新」那段与新增分开记：不进 `added` / `surfaced`，只有它的一轮不算工具池变化；估值按 `replacedNames` 那种
    /// 形状算，体里没有它的定义时估不准。
    #[test]
    fn updated_definitions_are_kept_apart_from_additions() {
        let added = delta(&format!("{}\nmcp__a__x", ANNOUNCE_HEADS[0]));
        let replaced = delta(&format!("{}\nmcp__a__x", ANNOUNCE_HEADS[1]));
        assert_eq!(replaced.replaced, ["mcp__a__x"]);
        assert!(
            replaced.added.is_empty() && replaced.surfaced.is_empty() && replaced.replace_only()
        );
        assert!(added.replaced.is_empty() && !added.replace_only());
        let h = PoolHistory::default();
        assert_eq!(replaced.token_estimate(Some(&h), false), None, "没有定义");
        assert_eq!(added.token_estimate(Some(&h), false), None, "没有定义");
        let (mut replaced, mut added) = (replaced, added);
        replaced.surfaced_defs = vec![("mcp__a__x".into(), 40)];
        added.surfaced_defs = vec![("mcp__a__x".into(), 40)];
        // 只有定义更新：五个空名单 + 一项定义 + replacedNames。
        let base = r#"{"type":"deferred_tools_delta","addedNames":[],"addedLines":[],"removedNames":[],"wireHiddenNames":[],"readdedNames":[]}"#.len();
        let defs = r#","surfacedDefinitions":[{"name":"mcp__a__x","listing":"0123456789abcdef","definition":}]"#.len() + 40;
        let names = r#","replacedNames":["mcp__a__x"]"#.len();
        assert_eq!(
            replaced.token_estimate(Some(&h), false),
            Some(token_round(base + defs + names))
        );
        assert!(added.token_estimate(Some(&h), false).is_some());
        // 只有定义更新的一轮：附件照列，汇总不报。
        let turn = PoolTurn {
            departed: McpDeparted::default(),
            history: Some(&PoolHistory { turns: 1, ..Default::default() }),
            delta: Some(&replaced),
            mcp: None,
        };
        assert!(!turn.changed() && turn.attachment_delta().is_some());
    }

    /// 同一份里还有不写名字的故障 / 要登录（名单空、个数却是 1、记为不全）的，不是纯定义更新：照报汇总，估值另按完整性判。
    #[test]
    fn hidden_server_states_are_not_a_pure_redefinition() {
        let r = format!("{}\nmcp__a__x", ANNOUNCE_HEADS[1]);
        for extra in [
            format!("{HIDDEN_FAILED} are unavailable."),
            format!("{HIDDEN_NEEDS_AUTH}, so its tools are unavailable."),
        ] {
            let d = delta(&format!("{r}\n\n{extra}"));
            assert_eq!(d.replaced, ["mcp__a__x"], "{extra}");
            assert!(!d.replace_only(), "{extra}");
        }
    }

    /// 只有定义更新的那份附件不带服务器名单：账本里上一份失败 / 待连 / 要登录的名单照旧，不被清空。
    #[test]
    fn a_pure_redefinition_keeps_the_last_server_lists() {
        let mut h = PoolHistory::default();
        h.apply(&delta(&format!(
            "{}\nmcp__a__x\n\n{FAILED_HEAD}for this session:\na (X): \"e\"\nb (X): \"e\"\nc (X): \"e\"",
            ANNOUNCE_HEADS[0]
        )));
        assert_eq!(h.last_failed().count, 3);
        let r = delta(&format!("{}\nmcp__a__x", ANNOUNCE_HEADS[1]));
        assert!(r.replace_only());
        h.apply(&r);
        assert_eq!(h.last_failed().count, 3, "纯定义更新不动失败名单");
        assert_eq!(h.dtd_count(), 2, "附件照样算一份");
    }

    #[test]
    fn readd_identity_follows_the_paragraph_kind() {
        let listed_plus_back = delta(&format!(
            "The following deferred tools are now available via ToolSearch. x:\nmcp__plugin_a__x\nmcp__plugin_a__y\n\n\
             1 deferred tool is available again in this session (announced earlier in this conversation): WebSearch."
        ));
        assert_eq!(listed_plus_back.returned_owners(None), vec![("returned_other".to_string(), 1)]);
        let still = delta(
            "The following tools are available again. Their definitions earlier in this conversation still apply, so call them directly:\nWebFetch",
        );
        assert_eq!(still.additions(None), Some((1, 1)));
        assert_eq!(still.returned_owners(None), vec![("returned_other".to_string(), 1)]);
    }

    /// 解析不给整段建行索引：没有任何标记的大段正文直接跳过；清单再长也只记 [`MAX_LIST_ITEMS`] 项。
    #[test]
    fn huge_reminders_are_scanned_without_indexing_every_line() {
        let noise = "\n".repeat(200_000);
        assert!(PoolDelta::parse([noise.as_str()]).is_none());
        assert!(McpInstr::parse([noise.as_str()]).is_none());
        let long = format!(
            "The following tools are now available:\n{}",
            (0..MAX_LIST_ITEMS + 50).map(|i| format!("t{i}")).collect::<Vec<_>>().join("\n")
        );
        let d = PoolDelta::parse([long.as_str()]).unwrap();
        // 项数上限与字节预算哪个先到算哪个：记下的不超过上限，没记全的记为溢出。
        assert!(d.added.len() <= MAX_LIST_ITEMS && d.overflow);
    }

    /// `<total_tokens>` 那条只认客户端附的提醒，工具结果里同样的字不算。
    #[test]
    fn total_tokens_only_counts_client_reminders() {
        let msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "Bash", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "<total_tokens>5 tokens left</total_tokens>"}]}),
        ];
        assert!(!scan(&msgs, true, &[]).total_tokens);
        let mut with = msgs.clone();
        with.push(
            json!({"role": "system", "content": "<total_tokens>5 tokens left</total_tokens>"}),
        );
        assert!(scan(&with, true, &[]).total_tokens);
    }

    /// 同一轮连回来又走掉：2.1.321 在 `removed_*` 后面按归属报 `returned_*`（官方 `uEn("returned", mn)`）；只连回来的一轮照报。
    /// 各种归属互不连累：一组分不清的工具只让它可能落进的那几种说不准。
    #[test]
    fn returned_owner_events_follow_removed_ones() {
        let mut h = PoolHistory::default();
        h.apply(&delta(
            "The following tools are now available:\nmcp__claude-in-chrome__nav\nmcp__a__x",
        ));
        h.apply(&delta(&format!("{HEAD}\nmcp__claude-in-chrome__nav")));
        h.turns = 2;
        let d = delta(&format!(
            "1 deferred tool is available again (MCP server reconnected — names announced earlier in this conversation): mcp__claude-in-chrome__*.\n\n{HEAD}\nmcp__a__x"
        ));
        let turn = PoolTurn {
            departed: d.departed(Some(&h)),
            history: Some(&h),
            delta: Some(&d),
            mcp: None,
        };
        assert_eq!(
            turn.owner_events(),
            vec![("removed_other".to_string(), 1), ("returned_chrome".to_string(), 1)]
        );
        assert_eq!(d.additions(Some(&h)), Some((1, 1)));
        let only_back = delta(
            "1 deferred tool is available again (MCP server reconnected — names announced earlier in this conversation): mcp__claude-in-chrome__*.",
        );
        let turn = PoolTurn {
            departed: only_back.departed(Some(&h)),
            history: Some(&h),
            delta: Some(&only_back),
            mcp: None,
        };
        assert_eq!(
            turn.owner_events(),
            vec![("returned_chrome".to_string(), 1)],
            "只连回来、没有走掉的一轮照报"
        );
        let first_turn = PoolTurn {
            departed: only_back.departed(None),
            history: Some(&PoolHistory::default()),
            delta: Some(&only_back),
            mcp: None,
        };
        assert!(first_turn.owner_events().is_empty(), "会话头一轮照模板，不报");
        // 账本不在：Chrome 那组单看前缀就定得下归属，照报；`remote-devices` 那组分不出子前缀、又配不回名字，两种远程设备
        // 归属都说不准、不报，但不连累别的归属。
        let turn =
            PoolTurn { departed: d.departed(None), history: None, delta: Some(&d), mcp: None };
        assert_eq!(
            turn.owner_events(),
            vec![("removed_other".to_string(), 1), ("returned_chrome".to_string(), 1)]
        );
        let mixed = delta(&format!(
            "1 deferred tool is available again in this session (announced earlier in this conversation): WebSearch.\n\n\
             31 deferred tools are available again (MCP server reconnected — names announced earlier in this conversation): mcp__remote-devices__* (31).\n\n\
             {HEAD}\nmcp__a__x"
        ));
        let turn = PoolTurn {
            departed: mixed.departed(Some(&h)),
            history: Some(&h),
            delta: Some(&mixed),
            mcp: None,
        };
        assert_eq!(
            turn.owner_events(),
            vec![("removed_other".to_string(), 1), ("returned_other".to_string(), 1)]
        );
        assert_eq!(mixed.additions(Some(&h)), None, "那组配不回名字，新增数说不准");
    }

    /// 同一个工具在同一轮里重复宣告（两段完整定义、两条一样的归并组）：归属与 `readdedCount` 一样只算一个。
    #[test]
    fn duplicate_readds_count_their_owner_once() {
        let mut h = PoolHistory::default();
        h.apply(&delta("The following tools are now available:\nmcp__claude-in-chrome__nav"));
        h.apply(&delta(&format!("{HEAD}\nmcp__claude-in-chrome__nav")));
        h.turns = 2;
        let twice = delta(
            "The following tools just became available and are ready to use:\nmcp__claude-in-chrome__nav\n\n\
             The following tools have updated definitions, which replace the earlier ones from here on:\nmcp__claude-in-chrome__nav",
        );
        assert_eq!(twice.additions(Some(&h)), Some((1, 1)));
        assert_eq!(twice.returned_owners(Some(&h)), vec![("returned_chrome".to_string(), 1)]);
        let group = "1 deferred tool is available again (MCP server reconnected — names announced earlier in this conversation): mcp__claude-in-chrome__*.";
        let groups = delta(&format!("{group}\n\n{group}"));
        assert_eq!(groups.returned_owners(Some(&h)), vec![("returned_chrome".to_string(), 1)]);
    }

    /// 同一组恢复重复出现、或同一个工具既补了完整定义又落在恢复组里：账本照样准（之后的 `priorAnnouncedCount` 报得出），
    /// 归属只算一个。
    #[test]
    fn overlapping_readds_keep_the_ledger_exact() {
        let base = || {
            let mut h = PoolHistory::default();
            h.apply(&delta("The following tools are now available:\nmcp__claude-in-chrome__nav"));
            h.apply(&delta(&format!("{HEAD}\nmcp__claude-in-chrome__nav")));
            h.turns = 2;
            h
        };
        let group = "1 deferred tool is available again (MCP server reconnected — names announced earlier in this conversation): mcp__claude-in-chrome__*.";
        let full = "The following tools just became available and are ready to use:\nmcp__claude-in-chrome__nav";
        for text in [format!("{group}\n\n{group}"), format!("{full}\n\n{group}")] {
            let h = base();
            let d = delta(&text);
            assert_eq!(d.additions(Some(&h)), Some((1, 1)), "{text}");
            assert_eq!(
                d.returned_owners(Some(&h)),
                vec![("returned_chrome".to_string(), 1)],
                "{text}"
            );
            let mut after = h.clone();
            after.apply(&d);
            assert!(after.exact, "{text}");
            assert!(after.announced.contains("mcp__claude-in-chrome__nav"));
        }
    }

    /// 同一恢复组重复出现：合计上限按去重后的算，不因重复判超限清空（单组 60000 个照报）。
    #[test]
    fn a_repeated_readd_group_is_bounded_after_dedup() {
        let group = "60000 deferred tools are available again (MCP server reconnected — names announced earlier in this conversation): mcp__claude-in-chrome__* (60000).";
        let once = delta(group);
        let twice = delta(&format!("{group}\n\n{group}"));
        assert_eq!(once.returned_owners(None), vec![("returned_chrome".to_string(), 60000)]);
        assert_eq!(twice.returned_owners(None), once.returned_owners(None));
    }

    /// 同一撤回组重复出现：只算一组，`removedCount`、归属与之后的账本都不翻倍。
    #[test]
    fn a_repeated_removal_group_counts_once() {
        let names: Vec<String> = (0..31).map(|i| format!("mcp__claude-in-chrome__t{i}")).collect();
        let mut h = PoolHistory::default();
        h.apply(&delta(&format!("The following tools are now available:\n{}", names.join("\n"))));
        h.turns = 1;
        let group = "31 deferred tools are no longer available (MCP server disconnected): mcp__claude-in-chrome__* (31). Do not search.";
        let once = delta(group);
        let twice = delta(&format!("{group}\n\n{group}"));
        assert_eq!(twice.departed(Some(&h)).total, 31);
        assert_eq!(twice.departed(Some(&h)).owners(), once.departed(Some(&h)).owners());
        let mut after = h.clone();
        after.apply(&twice);
        assert!(after.exact);
        assert!(after.announced.is_empty());
    }

    /// 同一台服务器的一组撤回与单列的一项混在一起：单列那项先走，组按剩下的配名字，账本准与否与两段先后无关。
    #[test]
    fn a_mixed_group_and_name_removal_is_order_independent() {
        let names: Vec<String> =
            (0..32).map(|i| format!("mcp__claude-in-chrome__t{i:02}")).collect();
        let mut h = PoolHistory::default();
        h.apply(&delta(&format!("The following tools are now available:\n{}", names.join("\n"))));
        h.turns = 1;
        let group = "31 deferred tools are no longer available (MCP server disconnected): mcp__claude-in-chrome__* (31). Do not search.";
        let single = format!("{HEAD}\nmcp__claude-in-chrome__t31");
        for text in [format!("{group}\n\n{single}"), format!("{single}\n\n{group}")] {
            let mut after = h.clone();
            after.apply(&delta(&text));
            assert!(after.exact, "{text}");
            assert!(after.announced.is_empty(), "{text}");
        }
    }

    /// 远程设备服务器一组 31 项加单列 1 项：组按账本配名字时不算单列那项，归属与估值都报得出（同全部逐个列出）。
    #[test]
    fn a_group_beside_a_named_removal_resolves_its_names() {
        let mut names: Vec<String> =
            (0..31).map(|i| format!("mcp__remote-devices__browser_t{i:02}")).collect();
        names.push("mcp__remote-devices__files__x".to_string());
        let mut h = PoolHistory::default();
        h.apply(&delta(&format!("The following tools are now available:\n{}", names.join("\n"))));
        h.turns = 1;
        let group = "31 deferred tools are no longer available (MCP server disconnected): mcp__remote-devices__* (31). Do not search.";
        let single = format!("{HEAD}\nmcp__remote-devices__files__x");
        let listed = format!("{HEAD}\n{}", names.join("\n"));
        let want = delta(&listed);
        for text in [format!("{group}\n\n{single}"), format!("{single}\n\n{group}")] {
            let d = delta(&text);
            assert_eq!(d.departed(Some(&h)).owners(), want.departed(Some(&h)).owners(), "{text}");
            assert!(!d.departed(Some(&h)).owners().is_empty(), "{text}");
            assert!(d.token_estimate(Some(&h), false).is_some(), "{text}");
        }
    }

    /// 同一台服务器断连走了一组 31 个、又因别的原因走了一组 31 个：是两回事，合计 62，不按重复去掉。
    #[test]
    fn same_group_for_different_causes_is_not_a_duplicate() {
        let summary = "31 deferred tools are no longer available (MCP server disconnected): mcp__big__* (31). Do not search.";
        let other = "The following deferred tools are no longer available in this session.\nmcp__big__* (31)";
        let d = delta(&format!("{summary}\n\n{other}"));
        assert_eq!(d.departed(None).total, 62);
        let twice = delta(&format!("{summary}\n\n{summary}"));
        assert_eq!(twice.departed(None).total, 31, "同一原因重复照旧只算一次");
    }

    /// 同一台服务器按两种原因各走一组：合起来按账本配名字，归属、估值报得出，下一轮账本照样准；同一个工具名在两类逐名段落里
    /// 各列一次只走了一个。
    #[test]
    fn groups_of_one_server_resolve_together() {
        let names: Vec<String> = (0..62).map(|i| format!("mcp__big__t{i:02}")).collect();
        let mut h = PoolHistory::default();
        h.apply(&delta(&format!("The following tools are now available:\n{}", names.join("\n"))));
        h.turns = 1;
        let summary = "31 deferred tools are no longer available (MCP server disconnected): mcp__big__* (31). Do not search.";
        let other = "The following deferred tools are no longer available in this session.\nmcp__big__* (31)";
        let d = delta(&format!("{summary}\n\n{other}"));
        let dep = d.departed(Some(&h));
        assert_eq!(dep.total, 62);
        assert_eq!(dep.owners(), vec![("removed_other".to_string(), 62)]);
        assert!(d.token_estimate(Some(&h), false).is_some());
        let mut after = h.clone();
        after.apply(&d);
        assert!(after.exact && after.announced.is_empty());

        let twice = delta(&format!(
            "{HEAD}\nmcp__big__t00\n\nThe following deferred tools are no longer available in this session.\nmcp__big__t00"
        ));
        assert_eq!(twice.departed(Some(&h)).total, 1);
    }

    /// 「no longer available in this session」那段里的 MCP 工具：官方附件另列 `removedForOtherCause`，估值算上它；非 MCP 的、
    /// 断连段落里的不进那份。
    #[test]
    fn other_cause_removals_are_listed_in_the_estimate() {
        let mut h = PoolHistory::default();
        h.apply(&delta("The following tools are now available:\nmcp__a__x\nmcp__b__y\nWebSearch"));
        h.turns = 1;
        let d = delta(&format!(
            "The following deferred tools are no longer available in this session.\nmcp__a__x\nWebSearch\n\n{HEAD}\nmcp__b__y"
        ));
        let expect = json!({
            "type": "deferred_tools_delta",
            "addedNames": [],
            "addedLines": [],
            "removedNames": ["mcp__a__x", "WebSearch", "mcp__b__y"],
            "removedForOtherCause": ["mcp__a__x"],
            "wireHiddenNames": [],
            "readdedNames": [],
            "pendingMcpServers": [],
            "needsAuthMcpServers": [],
            "failedMcpServers": [],
        });
        assert_eq!(
            d.token_estimate(Some(&h), false),
            Some(token_round(utf16_len(&expect.to_string())))
        );
    }

    /// 定义被撤销的工具也列在「in this session」那段里：它不进 `removedForOtherCause`，估值另按 `retractedTools` 结构算；原因说明
    /// 认不出（`Source removed`）时估不准。
    #[test]
    fn retracted_tools_are_not_other_cause_removals() {
        let mut h = PoolHistory::default();
        h.apply(&delta("The following tools are now available:\nmcp__a__x\nmcp__b__y"));
        h.turns = 1;
        let text = format!(
            "The following deferred tools are no longer available in this session.\nmcp__a__x\nmcp__b__y\n\n{RETRACTED_HEAD}\n- MCP server disabled by the user: mcp__a__x"
        );
        let d = delta(&text);
        assert_eq!(d.retracted, vec![("mcp__a__x".into(), Some("disabled"))]);
        assert_eq!(d.other_cause_names(Some(&h)), Some(vec!["mcp__b__y".to_string()]));
        // 有撤销时附件带不带 `restoresDeferred` 看不出（客户端开关），估值不报。
        assert_eq!(d.token_estimate(Some(&h), false), None);
        let denied = delta(&format!("{DENIED_HEAD}\nmcp__a__x, mcp__b__y"));
        assert_eq!(
            denied.retracted,
            vec![("mcp__a__x".into(), Some("denied")), ("mcp__b__y".into(), Some("denied"))]
        );
    }

    /// 权限拒绝那一支（官方 `deniedLead`）：拒掉的工具既在 `retractedTools`、也还在 `removedForOtherCause`；附件带不带
    /// `restoresDeferred` 从正文看不出（两种开关组合渲染一样），估值不报。
    #[test]
    fn denied_tools_stay_in_other_cause_removals() {
        let mut h = PoolHistory::default();
        h.apply(&delta("The following tools are now available:\nmcp__a__x"));
        h.turns = 1;
        let d = delta(&format!(
            "The following deferred tools are no longer available in this session.\nmcp__a__x\n\n{DENIED_HEAD}\nmcp__a__x"
        ));
        assert_eq!(d.other_cause_names(Some(&h)), Some(vec!["mcp__a__x".to_string()]));
        assert_eq!(d.token_estimate(Some(&h), false), None);
    }

    /// 普通撤回清单里有名字过长没记下：走掉的数不清，同轮逐名恢复的归属照报。
    #[test]
    fn a_truncated_removal_list_keeps_the_returned_owners() {
        let back = "1 deferred tool is available again in this session (announced earlier in this conversation): WebSearch.";
        let long = format!("mcp__x__{}", "a".repeat(257));
        let d = delta(&format!("{back}\n\n{HEAD}\n{long}"));
        assert!(d.removed_incomplete);
        assert_eq!(d.returned_owners(None), vec![("returned_other".to_string(), 1)]);
    }

    /// 走掉那句写了句首总数、却超了上限：走掉的数不清（`removedCount` 不报），同轮逐名恢复的归属照报——同无总数的分组清单。
    #[test]
    fn an_oversized_removal_total_keeps_the_returned_owners() {
        let back = "1 deferred tool is available again in this session (announced earlier in this conversation): WebSearch.";
        for removal in [
            "100001 deferred tools are no longer available (MCP server disconnected): mcp__big__* (100001). Do not search.",
            "The following deferred tools are no longer available in this session.\nmcp__big__* (100001)",
        ] {
            let d = delta(&format!("{back}\n\n{removal}"));
            assert!(d.removed_incomplete, "{removal}");
            assert_eq!(
                d.returned_owners(None),
                vec![("returned_other".to_string(), 1)],
                "{removal}"
            );
        }
    }

    /// 「没连上」名单截断了：没列出来的服务器也可能没连上，不在已列出那几个里的不报原因，只计总数与归属。
    #[test]
    fn a_truncated_failed_list_leaves_the_cause_unknown() {
        let failed = (0..30).map(|i| format!("s{i} (X)")).collect::<Vec<_>>().join("\n");
        let text = format!(
            "{HEAD}\nmcp__s0__a\nmcp__zz__b\n\n{FAILED_HEAD}for this session:\n{failed}\n\u{2026}and 3 more"
        );
        let dep = delta(&text).departed(None);
        assert_eq!((dep.total, dep.disconnected, dep.not_offered), (2, 1, 0));
        assert_eq!(dep.owners(), vec![("removed_other".to_string(), 2)]);
    }

    /// 同一轮有新宣告：按账本分新增与连回来的；只有移除是 0；账本不在 / 不准时看不出。
    #[test]
    fn additions_in_the_same_turn() {
        let mut h = PoolHistory::default();
        h.apply(&delta("The following tools are now available:\nmcp__a__x\nmcp__a__y\nOld"));
        h.apply(&delta(&format!("{HEAD}\nmcp__a__x\nmcp__a__y")));
        let quiet = delta(&format!("{HEAD}\nOld"));
        assert_eq!(quiet.additions(Some(&h)), Some((0, 0)));
        assert_eq!(quiet.additions(None), Some((0, 0)));
        let both = delta(&format!(
            "The following tools are now available:\nNew\nOld\n\n\
             2 deferred tools are available again (MCP server reconnected — names announced earlier in this conversation): mcp__a__* (2).\n\n\
             {HEAD}\nmcp__c__z"
        ));
        assert_eq!(both.additions(Some(&h)), Some((3, 2)), "New + 连回来的两个；Old 早宣告过");
        assert_eq!(both.additions(None), None);
        let unknown = delta(
            "3 deferred tools are available again (MCP server reconnected — names announced earlier in this conversation): mcp__q__* (3).",
        );
        assert_eq!(unknown.additions(Some(&h)), None, "配不回名字");
    }

    /// 账本：宣告名去重、拿掉走掉的，连回来的按服务器配回；dtdCount 按条数；失败名单按段落分组记重试状态。
    #[test]
    fn history_tracks_announcements_and_failed_servers() {
        let first = "The following tools just became available and are ready to use:\nmcp__d__a\nmcp__d__b\n\n\
            The following deferred tools are now available via ToolSearch. Their schemas are NOT loaded — calling them directly will fail with InputValidationError. Use ToolSearch with query \"select:<name>[,<name>...]\" to load tool schemas before calling them:\nCronCreate\nmcp__s__x\nmcp__s__y\nTaskOutput\n\n\
            The following MCP servers are configured but failed to connect — their tools (typically named mcp__<server>__*) are unavailable for this session:\nluban (AUTH_HEADER_REJECTED): \"Server rejected: \"quoted\"\"\nbeta\n\n\
            The following MCP servers are configured but failed to connect — their tools (typically named mcp__<server>__*) are unavailable for now; the session retries them in the background when a user message arrives, so their tools may appear later in this turn or in a later one:\ngamma (TIMEOUT): \"slow\"";
        let mut h = PoolHistory::default();
        h.apply(&delta(first));
        assert_eq!(h.announced_count(), Some(5), "TaskOutput 不算");
        assert_eq!(h.dtd_count(), 1);
        let f = h.last_failed();
        assert_eq!((f.count, f.complete), (3, true));
        let luban = f.servers.iter().find(|s| s.name == "luban").unwrap();
        assert_eq!(luban.code.as_deref(), Some("AUTH_HEADER_REJECTED"));
        assert_eq!(luban.error.as_deref(), Some("Server rejected: \"quoted\""));
        assert_eq!(f.servers.iter().find(|s| s.name == "gamma").unwrap().retry, "true");

        h.apply(&delta(&format!("{HEAD}\nmcp__s__x\nmcp__s__y")));
        assert_eq!((h.announced_count(), h.dtd_count()), (Some(3), 2));
        assert_eq!(h.last_failed().count, 0, "这一条没有失败段落：名单空了");

        h.apply(&delta("2 deferred tools are available again (MCP server reconnected — names announced earlier in this conversation): mcp__s__* (2). Load via ToolSearch as before."));
        assert_eq!(h.announced_count(), Some(5));
        h.apply(&delta("3 deferred tools are available again (MCP server reconnected — names announced earlier in this conversation): mcp__zz__* (3)."));
        assert_eq!(h.announced_count(), None, "配不回名字，名单不准");

        let trunc = format!(
            "The following MCP servers are configured but failed to connect — their tools (typically named mcp__<server>__*) are unavailable for this session:\n{}\n\u{2026}and 2 more",
            (0..30).map(|i| format!("s{i}")).collect::<Vec<_>>().join("\n")
        );
        let f = delta(&trunc).failed;
        assert_eq!((f.count, f.complete), (32, false));
    }

    /// 只认客户端自己附的提醒：`system` 角色的消息，或用户消息里包在 `<system-reminder>` 里的文本块。用户贴进来的同样文字、
    /// 工具结果里的都不算。
    #[test]
    fn only_client_reminders_count() {
        let text = format!("{HEAD}\nmcp__a__b\nmcp__a__c\n\nThis is ambient context.");
        let n =
            |m: Value| PoolDelta::parse(reminder_texts(&m)).map_or(0, |d| d.departed(None).total);
        assert_eq!(n(json!({"role": "system", "content": [{"type": "text", "text": text}]})), 2);
        assert_eq!(n(json!({"role": "system", "content": text})), 2);
        let wrapped = format!("<system-reminder>\n{text}\n</system-reminder>");
        assert_eq!(n(json!({"role": "user", "content": [{"type": "text", "text": wrapped}]})), 2);
        assert_eq!(n(json!({"role": "user", "content": wrapped})), 2);
        // 用户自己贴的日志。
        assert_eq!(n(json!({"role": "user", "content": [{"type": "text", "text": text}]})), 0);
        assert_eq!(n(json!({"role": "user", "content": format!("look at this:\n{text}")})), 0);
        // 工具结果里读到的文件。
        assert_eq!(
            n(
                json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": text, "text": text}]})
            ),
            0
        );
        // 提醒里引用的那句不在行首也不算。
        let quoted = format!("<system-reminder>\nnote: \"{HEAD}\"\nmcp__a__b\n</system-reminder>");
        assert_eq!(n(json!({"role": "user", "content": quoted})), 0);
    }

    /// 两条 assistant 之间的几条消息算一条附件；末条 assistant 之后的是这一轮。
    #[test]
    fn scan_splits_history_and_this_turn() {
        let announce = "The following deferred tools are now available via ToolSearch. x:\nmcp__a__x\nmcp__a__y\nmcp__b__z";
        let msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": announce}),
            json!({"role": "system", "content": "The following MCP servers are configured but failed to connect — their tools (typically named mcp__<server>__*) are unavailable for this session:\nb (X)"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": "again"}),
            json!({"role": "system", "content": format!("{HEAD}\nmcp__a__x\nmcp__a__y")}),
        ];
        let Scan { history: h, delta: d, .. } = scan(&msgs, true, &[]);
        let h = h.unwrap();
        assert_eq!((h.announced_count(), h.dtd_count(), h.last_failed().count), (Some(3), 1, 1));
        let d = d.unwrap();
        assert_eq!(d.departed(Some(&h)).not_offered, 2);
        assert_eq!(d.failed().count, 0);
        let Scan { history: h, delta: d, .. } = scan(&msgs[4..], false, &[]);
        assert!(h.is_none() && d.is_some());
    }
}
