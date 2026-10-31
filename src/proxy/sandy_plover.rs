//! 2.1.311 起 `/v1/messages` 的请求头 `x-claude-code-sandy-plover`：这个会话里用过的工具名。
//!
//! 官方客户端（2.1.311 / 2.1.312 逐字相同）在发请求那一层补它，条件是服务端开关 `tengu_snug_mitten` 开着、
//! `tengu_silly_shore` 没关（默认开）、路径以 `/v1/messages` 结尾（`count_tokens` 不带）、走官方地址（设了第三方
//! `ANTHROPIC_BASE_URL` 的不带）。`tengu_snug_mitten` 10-27 的 eval 里还没有，10-28 起 `force: true`
//! （`cap/auto-2.1.311-20261027-full` 与 `cap/auto-2.1.312-20261028-full` 的 eval 回包），同日 2.1.311 对照也带
//! （`cap/auto-2.1.312-20261028-bashfail/2.1.311`）——是服务端开关，不是版本改动。两个开关按号从 eval 里记，并落库
//! （`tools_used_flags` 表，[`save_eval`] / [`restore`]）：官方把 eval 缓存在本机、一启动就读得到，握手里额度探测不等
//! eval 回包就发，只记在内存的话 luban 重启后每个号第一个会话的那条探测会漏带这个头。
//!
//! 值是会话级锁存的「用过的工具」（官方 `conversationToolsUsed`）：主线程、子代理、辅助请求与预热、额度探测带的是同一份，
//! 只增不减，压缩之后也还在。拼法照官方 `ny` / `rf`：名字先按首次使用排，非核心工具挪到前头、[`CORE_TOOLS`] 那 13 个
//! 排后头（各自保持先后）；每个写成 `{"name":"…"}`（`\u007f` 起的字符逐个 UTF-16 码元写 `\uXXXX`），逗号连起来套
//! 方括号；最多 64 个、整串不超过 7168 字节，装不下的丢掉、队首换成哨兵 [`TRUNCATED_SENTINEL`]。会话刚开头是 `[]`。
//!
//! 记的是哪些名字（官方工具执行入口的 `yKs`、助手消息到达时的 `_Ks`、按转录重建的 `a5`）：`tool_use` 记名字（先过旧名
//! 别名表 [`TOOL_ALIASES`]），`server_tool_use` 同样，`mcp_tool_use` 记 `server_name:name`（任一为空记哨兵）；每个名字再过
//! 一道字符集校验（[`readable`]，不合格的记成哨兵）。权限判定之前就记，被拒的工具也算用过。客户端还把调用块里看不到的
//! 那些写成清单给模型看（压缩摘要开头、子代理与后台任务送达时那段提醒），`a5` 同样从清单里认（[`listed_tools`]）。
//!
//! luban 这边：模拟路径（含 billing-only）与 luban 自己发的预热、连通性测试、额度探测补这个头；锁存按（号，出站会话
//! id）记，每条模拟请求把来访对话里的这三类调用与那几份清单（名字一律按出站改名之后的，[`ToolNameMap`]）并进去，空闲
//! [`LATCH_IDLE`] 就忘掉，口径同会话链。头在发出前那一刻定（[`apply_session_header`]）：新会话首条的启动握手刚把这个号的
//! eval 拉回来，开关以它为准。真实 CC 经 `ANTHROPIC_BASE_URL` 透传的请求客户端自己不带，这里也不补。

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::http::HeaderValue;

use super::ToolNameMap;

/// 头名（线上小写，位置见 [`crate::config::CC_HEADER_ORDER`]）。
pub(crate) const SANDY_PLOVER_HEADER: &str = "x-claude-code-sandy-plover";

/// 总开关：服务端下发 `true` 才开，读不到按关。
const SNUG_MITTEN_FEATURE: &str = "tengu_snug_mitten";

/// 另一道开关：读不到按开（官方 `An("tengu_silly_shore", true)`）。
const SILLY_SHORE_FEATURE: &str = "tengu_silly_shore";

/// 官方排在后头的那 13 个工具（`nf`）：其余的——MCP 工具、`WebSearch` / `WebFetch`、`AskUserQuestion`、
/// `SubagentHandback`……——都排在它们前面。
const CORE_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "NotebookEdit",
    "TodoWrite",
    "Agent",
    "Task",
    "ToolSearch",
    "EnterPlanMode",
    "ExitPlanMode",
];

/// 最多写几个名字（官方 `Jb`）。
const MAX_NAMES: usize = 64;

/// 整串（含方括号与逗号）的字节上限（官方 `Qb`）。
const MAX_BYTES: usize = 7168;

/// 单个名字超过这么多字节就不写（官方 `Zb`）。
const MAX_NAME_BYTES: usize = 256;

/// 有名字没写进去（超了上限，或名字是空串、超长）时排在最前面的哨兵名（官方 `jae`）；锁存里认不出的名字也记成它
/// （[`readable`]）。
const TRUNCATED_SENTINEL: &str = "unreadable-tools-used-before-summary";

/// 旧工具名 → 现名（官方 `cd` 查的那张表，2.1.312 逐字）：`tool_use` 与 `server_tool_use` 的名字先过它再记，于是来访
/// 自己声明的 `Task`（官方旧名，白名单放行、不进改名表）记成 `Agent`。
const TOOL_ALIASES: &[(&str, &str)] = &[
    ("Task", "Agent"),
    ("KillShell", "TaskStop"),
    ("KillBash", "TaskStop"),
    ("ListPeers", "ListAgents"),
    ("Brief", "SendUserMessage"),
    ("ListMcpResources", "ListMcpResourcesTool"),
    ("ReadMcpResource", "ReadMcpResourceTool"),
    ("ReadMcpResourceDir", "ReadMcpResourceDirTool"),
];

/// 压缩摘要那条用户消息的开头（官方 `AKe`）：后面紧跟用过的工具清单（[`listed_tools`]）。
pub(crate) const SUMMARY_PREAMBLE: &str = "This session is being continued from a previous conversation that ran out of context. \
     The summary below covers the earlier portion of the conversation.";

/// 摘要开头那段的形态（官方 `x`）：可选一行他人编写内容的标记 → 固定开头 → 空行 → 可选一行 → `<system-reminder>` 包着的
/// 或裸的 `<tools-used-before-summary>…</tools-used-before-summary>`，锚在那段文本开头。
static SUMMARY_TOOLS: LazyLock<regex::Regex> = LazyLock::new(|| {
    let marked = "(?:<artifact-content-authored-by-others/>\n.*\n)?";
    let tag = "<tools-used-before-summary>([^<]*)</tools-used-before-summary>";
    regex::Regex::new(&format!(
        "^{marked}{}\n\n{marked}(?:<system-reminder>\n{tag}\n</system-reminder>|{tag})",
        regex::escape(SUMMARY_PREAMBLE)
    ))
    .expect("summary tools pattern")
});

/// 单独一段提醒的形态（官方 `w`，`MVt` 用它）：整段文本恰好是 `<system-reminder>` 包着一行
/// `<tools-used-before-summary>…</tools-used-before-summary>`，末尾可多一个换行。子代理或后台任务用过、对话里还没出现的
/// 工具，客户端这样告诉模型，跟送达的那条（子代理交回、任务通知）并进同一条用户消息
/// （`cap/auto-2.1.312-20261028-full/00125` 的 `SubagentHandback`、`00925` 的 `Read`）。
static REMINDER_TOOLS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        "^<system-reminder>\n<tools-used-before-summary>([^<\n]*)</tools-used-before-summary>\n</system-reminder>\n?$",
    )
    .expect("reminder tools pattern")
});

/// 锁存的会话多久没请求就忘掉：同会话链（`session_link` 的 `CC_SESSION_IDLE`）。
const LATCH_IDLE: Duration = Duration::from_secs(3 * 60 * 60);

/// 整表清过期条目的最小间隔，同会话链。
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// 一个会话最多锁存多少个名字。官方不设上限，但头里最多写 64 个；留足余量只为不让一个会话把表撑大。
const LATCH_MAX: usize = 1024;

/// 号 → （`tengu_snug_mitten`，`tengu_silly_shore`）。
static FLAGS: LazyLock<parking_lot::RwLock<HashMap<i64, (bool, bool)>>> =
    LazyLock::new(Default::default);

/// 号 → 上次落库的那份（同 [`FLAGS`] 的两个值），与内存里的相同就不写。
static PERSISTED: LazyLock<parking_lot::Mutex<HashMap<i64, (bool, bool)>>> =
    LazyLock::new(Default::default);

/// 落库整段串行：总是拿内存里最新的那份去比去写，慢的旧写入排在前面，后到的会把最新值写回去。eval 一个号几小时
/// 才拉一次（外加新会话握手），一把全局锁够用。
static SAVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Latch {
    names: Vec<String>,
    /// 头装不下、有名字没写进去的那次诊断已经报过（官方 `reportTruncationOnce`，跟锁存同生同灭）。
    truncation_reported: bool,
    last_seen: Instant,
}

/// 头装不下时那条诊断的两个数（官方 `{sent, dropped}`）：写进去几个名字（不算哨兵）、丢了几个。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Truncation {
    pub(crate) sent: usize,
    pub(crate) dropped: usize,
}

#[derive(Default)]
struct Latches {
    map: HashMap<(i64, String), Latch>,
    swept: Option<Instant>,
}

static LATCHES: LazyLock<parking_lot::Mutex<Latches>> = LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的两个开关：一份 eval 是整份特性缓存，没有的按默认。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let served = |k: &str| features.get(k).and_then(|f| f.get("value")).and_then(|v| v.as_bool());
    let snug = served(SNUG_MITTEN_FEATURE) == Some(true);
    let silly = served(SILLY_SHORE_FEATURE).unwrap_or(true);
    tracing::debug!(
        cred_id,
        snug_mitten = snug,
        silly_shore = silly,
        "tools-used header flags from eval"
    );
    FLAGS.write().insert(cred_id, (snug, silly));
}

/// 这个号内存里的两个开关与上次落库的不同（或还没落过）就写进 `tools_used_flags` 表。eval 回包记完（[`note_eval`]）
/// 之后调；内存里没有这个号（回包解析不出 `features`）什么都不做。写失败只告警，下一份 eval 再试。
pub(crate) async fn save_eval(store: &crate::store::CredentialStore, cred_id: i64) {
    let _guard = SAVE_LOCK.lock().await;
    let Some(flags) = FLAGS.read().get(&cred_id).copied() else { return };
    if PERSISTED.lock().get(&cred_id) == Some(&flags) {
        return;
    }
    match store.save_tools_used_flags(cred_id, flags.0, flags.1).await {
        Ok(()) => {
            PERSISTED.lock().insert(cred_id, flags);
        }
        Err(e) => tracing::warn!(cred_id, error = %e, "failed to persist tools-used header flags"),
    }
}

/// 启动时（开始转发之前）把 `tools_used_flags` 表读回内存，返回读回的条数。
pub(crate) async fn restore(store: &crate::store::CredentialStore) -> anyhow::Result<usize> {
    Ok(restore_rows(store.all_tools_used_flags().await?))
}

/// [`restore`] 的本体：逐条记进内存并登记成「已落库」。启动之后才到的 eval 已经记过的号以内存为准，不拿库里的盖。
fn restore_rows(rows: Vec<(i64, bool, bool)>) -> usize {
    let mut flags = FLAGS.write();
    let mut persisted = PERSISTED.lock();
    let mut n = 0;
    for (cred_id, snug, silly) in rows {
        persisted.insert(cred_id, (snug, silly));
        if flags.contains_key(&cred_id) {
            continue;
        }
        flags.insert(cred_id, (snug, silly));
        tracing::debug!(
            cred_id,
            snug_mitten = snug,
            silly_shore = silly,
            "tools-used header flags restored"
        );
        n += 1;
    }
    n
}

/// 这个号上、`version` 这一版的官方客户端记不记用过的工具：只看 `tengu_silly_shore`（读不到按开），与发不发头无关——
/// 官方 `yKs` / `_Ks` / `UU` 都只判它，`tengu_snug_mitten` 关着的那段照样记，开关打开时历史还在。
fn records_for(cred_id: i64, version: &str) -> bool {
    let at_311 = super::parse_version(version).is_some_and(|v| v >= (2, 1, 311));
    at_311 && FLAGS.read().get(&cred_id).is_none_or(|&(_, silly)| silly)
}

/// 这个号上、`version` 这一版的官方客户端发 `/v1/messages` 时带不带这个头：记着、且 `tengu_snug_mitten` 开着。
pub(crate) fn sent_for(cred_id: i64, version: &str) -> bool {
    records_for(cred_id, version) && FLAGS.read().get(&cred_id).is_some_and(|&(snug, _)| snug)
}

/// luban 自己发的那几条（预热、连通性测试、额度探测）：不在任何对话里，锁存是空的，带 `[]`。
pub(crate) fn fresh_header(cred_id: i64, version: &str) -> Option<HeaderValue> {
    sent_for(cred_id, version).then(|| HeaderValue::from_static("[]"))
}

/// 发出前那一刻定模拟路径这条 `/v1/messages` 的头：先摘掉（重新整形、换号重来时可能带着上一次的），开关开着再按锁存补上。
/// 要放在新会话的启动握手之后——握手刚拉回的 eval 可能把这个号的开关打开或关掉，首条请求得跟着它。头装不下、这个锁存
/// 头一回丢名字时返回那条诊断的两个数（遥测报 `tengu_feature_sad{api_tools_used_header, incomplete_list}`）。
pub(crate) fn apply_session_header(
    headers: &mut axum::http::HeaderMap,
    cred_id: i64,
    session_id: &str,
    version: &str,
    body: Option<&serde_json::Value>,
    tool_names: Option<&ToolNameMap>,
) -> Option<Truncation> {
    headers.remove(SANDY_PLOVER_HEADER);
    let Some((value, truncation)) = session_header(cred_id, session_id, version, body, tool_names)
    else {
        tracing::debug!(cred_id, session = %session_id.chars().take(8).collect::<String>(), "tools-used header not sent");
        return None;
    };
    tracing::debug!(
        cred_id,
        session = %session_id.chars().take(8).collect::<String>(),
        value = %String::from_utf8_lossy(value.as_bytes()),
        "tools-used header sent"
    );
    headers.insert(SANDY_PLOVER_HEADER, value);
    truncation
}

/// 模拟路径的一条 `/v1/messages`：来访对话里用过的工具并进这个会话的锁存（`tengu_silly_shore` 开着就记，不管发不发头），
/// 头要发时返回头值与头一回丢名字时的诊断。
pub(crate) fn session_header(
    cred_id: i64,
    session_id: &str,
    version: &str,
    body: Option<&serde_json::Value>,
    tool_names: Option<&ToolNameMap>,
) -> Option<(HeaderValue, Option<Truncation>)> {
    if !records_for(cred_id, version) {
        return None;
    }
    let used = body.map(|b| tools_used_in(b, tool_names)).unwrap_or_default();
    let (value, truncation) = latch(cred_id, session_id, used, sent_for(cred_id, version))?;
    Some((HeaderValue::from_str(&value).ok()?, truncation))
}

/// 对话里调过的工具，按出现先后、去重：用户与助手消息都扫三类调用块（官方 `a5`）——`tool_use` 先换成出站那个名字
/// （[`outbound`]），再与 `server_tool_use` 一样过别名表；`mcp_tool_use` 记 `server_name:name`。名字去掉首尾空白后
/// 为空的不记，认不出的记成哨兵（[`readable`]）。
///
/// 调用块里看不到的，在客户端写给模型的清单里（[`listed_tools`]，用户消息的每段文本都看）：压缩过的对话调用块已经没了，
/// 用过的工具在摘要开头那份里（`cap/auto-2.1.312-20261028-full/00206` 一个调用块都没有，官方头照样是 11 个）；子代理
/// 最后那次调用之后不再有请求，哪条请求体里都没有它的调用块，只在送达时那段提醒里（`00125` 的 `SubagentHandback`）。
/// 官方锁存跨压缩一直在、子代理执行那一刻就记，luban 只能从清单里认回来，按清单的先后（官方写的是排过序的）并进去。
fn tools_used_in(body: &serde_json::Value, tool_names: Option<&ToolNameMap>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let add = |used: String, out: &mut Vec<String>| {
        if !out.contains(&used) {
            out.push(used);
        }
    };
    let messages = body.get("messages").and_then(|m| m.as_array()).map(Vec::as_slice);
    for m in messages.unwrap_or_default() {
        let user = m.get("role").and_then(|r| r.as_str()) == Some("user");
        let content = m.get("content");
        if user && let Some(text) = content.and_then(|c| c.as_str()) {
            for used in listed_tools(text, tool_names) {
                add(used, &mut out);
            }
        }
        let Some(blocks) = content.and_then(|c| c.as_array()) else { continue };
        for b in blocks {
            let kind = b.get("type").and_then(|t| t.as_str());
            if user && kind == Some("text") {
                let text = b.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                for used in listed_tools(text, tool_names) {
                    add(used, &mut out);
                }
                continue;
            }
            let Some(name) = b.get("name").and_then(|n| n.as_str()) else { continue };
            let used = match kind {
                Some("tool_use") => canonical(outbound(name, tool_names).trim()).to_string(),
                Some("server_tool_use") => canonical(name.trim()).to_string(),
                Some("mcp_tool_use") => {
                    let server = b.get("server_name").and_then(|v| v.as_str()).unwrap_or("").trim();
                    let tool = name.trim();
                    if server.is_empty() || tool.is_empty() {
                        TRUNCATED_SENTINEL.to_string()
                    } else {
                        format!("{server}:{tool}")
                    }
                }
                _ => continue,
            };
            if used.is_empty() {
                continue;
            }
            add(readable(used), &mut out);
        }
    }
    out
}

/// 一段用户文本若是客户端写给模型的用过的工具清单——压缩摘要开头那份（[`SUMMARY_TOOLS`]，官方 `X`）或单独一段提醒
/// （[`REMINDER_TOOLS`]，官方 `MVt`）——取出那些名字：按逗号拆，去掉两头空格，空的不记（官方 `g`）。清单上是客户端那头的
/// 名字，与调用块同一口径先换成出站那个（[`outbound`]，不然改过名的工具会在头里露出原名、与假名各占一个），再过 [`readable`]。
fn listed_tools(text: &str, tool_names: Option<&ToolNameMap>) -> Vec<String> {
    let caps = SUMMARY_TOOLS.captures(text).or_else(|| REMINDER_TOOLS.captures(text));
    let Some(list) = caps.as_ref().and_then(|c| c.get(1).or_else(|| c.get(2))) else {
        return Vec::new();
    };
    list.as_str()
        .split(',')
        .map(|n| n.trim_matches(' '))
        .filter(|n| !n.is_empty())
        .map(|n| readable(outbound(n, tool_names).to_string()))
        .collect()
}

/// 客户端那头的工具名换成出站那个：改名表里有就是假名（[`ToolNameMap`]），没有就是原名——与出站体里调用块的改法一致。
fn outbound<'a>(name: &'a str, tool_names: Option<&'a ToolNameMap>) -> &'a str {
    tool_names.and_then(|t| t.forward.get(name)).map_or(name, String::as_str)
}

/// 官方 `cd`：旧名换现名，见 [`TOOL_ALIASES`]。
fn canonical(name: &str) -> &str {
    TOOL_ALIASES.iter().find(|(old, _)| *old == name).map_or(name, |&(_, new)| new)
}

/// 官方 `Qte`：名字只许 `A-Z a-z 0-9 _ . : -`、1 ~ 256 个字符，否则记成哨兵 [`TRUNCATED_SENTINEL`]。
fn readable(name: String) -> String {
    let ok = (1..=MAX_NAME_BYTES).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b':' | b'-'));
    if ok { name } else { TRUNCATED_SENTINEL.to_string() }
}

/// 把这条请求里用过的并进（号，会话）的锁存；`render` 时按并完的整份拼出头值，头一回丢名字时带上那条诊断（之后同一个
/// 锁存不再报）。
fn latch(
    cred_id: i64,
    session_id: &str,
    used: Vec<String>,
    render_header: bool,
) -> Option<(String, Option<Truncation>)> {
    let now = Instant::now();
    let mut guard = LATCHES.lock();
    let latches = &mut *guard;
    if latches.swept.is_none_or(|t| now.duration_since(t) >= SWEEP_EVERY) {
        latches.swept = Some(now);
        latches.map.retain(|_, l| now.duration_since(l.last_seen) < LATCH_IDLE);
    }
    let key = (cred_id, session_id.to_string());
    let entry = latches.map.entry(key).or_insert_with(|| Latch {
        names: Vec::new(),
        truncation_reported: false,
        last_seen: now,
    });
    if now.duration_since(entry.last_seen) >= LATCH_IDLE {
        entry.names.clear();
        entry.truncation_reported = false;
    }
    entry.last_seen = now;
    for name in used {
        if entry.names.len() >= LATCH_MAX {
            break;
        }
        if !entry.names.contains(&name) {
            entry.names.push(name);
        }
    }
    if !render_header {
        return None;
    }
    let (value, truncation) = render(&entry.names);
    let report = truncation.filter(|_| !std::mem::replace(&mut entry.truncation_reported, true));
    Some((value, report))
}

/// 照官方 `ny`：空名与超长名不写；非核心在前、核心在后；装不下（或有名字没写）时队首换哨兵重排，并给出写进去几个
/// （不算哨兵）、丢了几个。
fn render(latched: &[String]) -> (String, Option<Truncation>) {
    let kept: Vec<&str> = latched
        .iter()
        .map(String::as_str)
        .filter(|n| !n.is_empty() && n.len() <= MAX_NAME_BYTES)
        .collect();
    let ordered: Vec<&str> = kept
        .iter()
        .copied()
        .filter(|n| !CORE_TOOLS.contains(n))
        .chain(kept.iter().copied().filter(|n| CORE_TOOLS.contains(n)))
        .collect();
    let items = fit(&ordered);
    if items.len() >= latched.len() {
        return (format!("[{}]", items.join(",")), None);
    }
    let with_sentinel: Vec<&str> =
        std::iter::once(TRUNCATED_SENTINEL).chain(ordered.iter().copied()).collect();
    let items = fit(&with_sentinel);
    let sent = items.len() - 1;
    (format!("[{}]", items.join(",")), Some(Truncation { sent, dropped: latched.len() - sent }))
}

/// 照官方 `rf`：逐个写成 `{"name":"…"}`，到 64 个或整串将超 7168 字节为止。
fn fit(names: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut used = 2;
    for name in names {
        if out.len() == MAX_NAMES {
            break;
        }
        let item = name_item(name);
        let cost = item.len() + usize::from(!out.is_empty());
        if used + cost > MAX_BYTES {
            break;
        }
        used += cost;
        out.push(item);
    }
    out
}

/// `{"name":"…"}`：JSON 转义之后，`\u007f` 起的字符按 UTF-16 码元逐个写成小写 `\uXXXX`（官方
/// `replace(/[\u007f-\uffff]/g, …)`，码点在基本平面之外的拆成一对代理），于是整串只剩 ASCII。
fn name_item(name: &str) -> String {
    let json = serde_json::json!({ "name": name }).to_string();
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if (c as u32) < 0x7f {
            out.push(c);
            continue;
        }
        let mut units = [0u16; 2];
        for u in c.encode_utf16(&mut units) {
            out.push_str(&format!("\\u{u:04x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v["features"].as_object().unwrap().clone()
    }

    #[test]
    fn follows_both_flags_and_starts_at_2_1_311() {
        let id = -93_312;
        assert!(!sent_for(id, "2.1.312"), "没见过 eval 按关");
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snug_mitten": {"value": true}}})),
        );
        assert!(sent_for(id, "2.1.312"));
        assert!(sent_for(id, "2.1.311"), "2.1.311 起就有这段代码");
        assert!(!sent_for(id, "2.1.310"));
        assert!(!sent_for(id, "garbage"));
        note_eval(
            id,
            &features(serde_json::json!({"features": {
                "tengu_snug_mitten": {"value": true}, "tengu_silly_shore": {"value": false}
            }})),
        );
        assert!(!sent_for(id, "2.1.312"), "silly_shore 下发关就不带");
        note_eval(id, &features(serde_json::json!({"features": {}})));
        assert!(!sent_for(id, "2.1.312"), "整份 eval 里没有 snug_mitten 按关");
        assert_eq!(fresh_header(id, "2.1.312"), None);
    }

    /// 取值与排法对上 `cap/auto-2.1.312-20261028-full`：`00090` 全是核心工具按首次使用；`00200` 非核心的
    /// `WebSearch` / `WebFetch` / `SubagentHandback` / `AskUserQuestion` 挪到最前，其余保持先后。
    #[test]
    fn renders_like_the_captures() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(render(&[]), ("[]".to_string(), None));
        assert_eq!(
            render(&s(&["Bash", "Read", "Edit", "Write", "ToolSearch"])).0,
            r#"[{"name":"Bash"},{"name":"Read"},{"name":"Edit"},{"name":"Write"},{"name":"ToolSearch"}]"#
        );
        assert_eq!(
            render(&s(&[
                "Bash",
                "Read",
                "Edit",
                "Write",
                "ToolSearch",
                "WebSearch",
                "WebFetch",
                "Agent",
                "SubagentHandback",
                "AskUserQuestion",
                "ExitPlanMode"
            ]))
            .0,
            concat!(
                r#"[{"name":"WebSearch"},{"name":"WebFetch"},{"name":"SubagentHandback"},"#,
                r#"{"name":"AskUserQuestion"},{"name":"Bash"},{"name":"Read"},{"name":"Edit"},"#,
                r#"{"name":"Write"},{"name":"ToolSearch"},{"name":"Agent"},{"name":"ExitPlanMode"}]"#
            )
        );
    }

    #[test]
    fn escapes_non_ascii_and_truncates_with_the_sentinel() {
        assert_eq!(name_item("t\u{7f}é😀"), r#"{"name":"t\u007f\u00e9\ud83d\ude00"}"#);
        // 64 个以上：只写前 64 个里排在最前的那些，队首换成哨兵。
        let many: Vec<String> = (0..70).map(|i| format!("mcp__s__t{i}")).collect();
        let (v, truncation) = render(&many);
        assert_eq!(truncation, Some(Truncation { sent: 63, dropped: 7 }));
        let items: Vec<serde_json::Value> = serde_json::from_str(&v).unwrap();
        assert_eq!(items.len(), 64);
        assert_eq!(items[0]["name"], TRUNCATED_SENTINEL);
        assert_eq!(items[1]["name"], "mcp__s__t0");
        assert!(v.len() <= MAX_BYTES);
        // 空名与超长名不写，也算没写进去：队首照样是哨兵。
        let odd = vec![String::new(), "Bash".to_string()];
        assert_eq!(
            render(&odd),
            (
                format!(r#"[{{"name":"{TRUNCATED_SENTINEL}"}},{{"name":"Bash"}}]"#),
                Some(Truncation { sent: 1, dropped: 1 })
            )
        );
    }

    /// 锁存只增不减：后一条请求的对话里少了先前用过的工具（被压缩掉了），头里照样还在；换个会话从头记。
    #[test]
    fn latch_only_grows_within_a_session() {
        let id = -93_313;
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snug_mitten": {"value": true}}})),
        );
        let body = |names: &[&str]| {
            let blocks: Vec<_> = names
                .iter()
                .enumerate()
                .map(|(i, n)| serde_json::json!({"type":"tool_use","id":format!("t{i}"),"name":n,"input":{}}))
                .collect();
            serde_json::json!({"messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","content":blocks},
                {"role":"user","content":[{"type":"text","text":"ok"}]}
            ]})
        };
        let h = |b: serde_json::Value, sid: &str| {
            session_header(id, sid, "2.1.312", Some(&b), None)
                .unwrap()
                .0
                .to_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(h(serde_json::json!({"messages":[{"role":"user","content":"hi"}]}), "s1"), "[]");
        assert_eq!(
            h(body(&["Bash", "WebSearch", "Bash"]), "s1"),
            r#"[{"name":"WebSearch"},{"name":"Bash"}]"#
        );
        assert_eq!(
            h(body(&["Read"]), "s1"),
            r#"[{"name":"WebSearch"},{"name":"Bash"},{"name":"Read"}]"#,
            "先前用过的留着"
        );
        assert_eq!(h(body(&["Read"]), "s2"), r#"[{"name":"Read"}]"#, "别的会话从头记");
        assert_eq!(session_header(id, "s1", "2.1.310", Some(&body(&["Bash"])), None), None);
    }

    /// 记哪些名字照官方 `a5`：`tool_use` 先换出站名、再过旧名别名表（`Task` 记成 `Agent`、`KillShell` 记成
    /// `TaskStop`）；`server_tool_use` 同样过别名表；`mcp_tool_use` 记 `server_name:name`，任一为空记哨兵；用户消息里的也算；
    /// 字符集外的名字记成哨兵；同名只记一次。
    #[test]
    fn collects_like_the_official_transcript_scan() {
        let body = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Task", "input": {}},
                {"type": "server_tool_use", "id": "s1", "name": "web_search", "input": {}},
                {"type": "mcp_tool_use", "id": "m1", "server_name": " linear ", "name": "list_issues", "input": {}},
                {"type": "tool_use", "id": "t2", "name": "lookup_docs", "input": {}},
                {"type": "tool_use", "id": "t3", "name": "KillShell", "input": {}},
                {"type": "mcp_tool_use", "id": "m2", "server_name": "", "name": "x", "input": {}},
                {"type": "tool_use", "id": "t4", "name": "bad name!", "input": {}},
                {"type": "tool_use", "id": "t5", "name": "Agent", "input": {}},
                {"type": "text", "text": "done"}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
                {"type": "server_tool_use", "id": "s2", "name": "  ", "input": {}}
            ]}
        ]});
        let map = ToolNameMap {
            forward: [("lookup_docs".to_string(), "mcp__luban__query_docs".to_string())].into(),
            reverse: Vec::new(),
        };
        assert_eq!(
            tools_used_in(&body, Some(&map)),
            [
                "Agent",
                "web_search",
                "linear:list_issues",
                "mcp__luban__query_docs",
                "TaskStop",
                TRUNCATED_SENTINEL
            ]
        );
        assert_eq!(readable("a".repeat(256)), "a".repeat(256));
        assert_eq!(readable("a".repeat(257)), TRUNCATED_SENTINEL);
        assert_eq!(readable("mcp__s__t-1.v2:x".into()), "mcp__s__t-1.v2:x");
        assert_eq!(canonical("ReadMcpResourceDir"), "ReadMcpResourceDirTool");
        assert_eq!(canonical("Bash"), "Bash");
    }

    /// 新会话首条：建头那时这个号还没见过 eval，按关；握手把开关拉回来之后、发出之前再定一次，以它为准——
    /// 开了就补上，关了就摘掉先前那份。
    #[test]
    fn the_send_time_decision_follows_the_handshakes_eval() {
        let id = -93_314;
        let body = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        let mut headers = axum::http::HeaderMap::new();
        apply_session_header(&mut headers, id, "s1", "2.1.312", Some(&body), None);
        assert!(headers.get(SANDY_PLOVER_HEADER).is_none(), "还没见过 eval");
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snug_mitten": {"value": true}}})),
        );
        apply_session_header(&mut headers, id, "s1", "2.1.312", Some(&body), None);
        assert_eq!(headers.get(SANDY_PLOVER_HEADER).unwrap(), "[]");
        note_eval(id, &features(serde_json::json!({"features": {}})));
        apply_session_header(&mut headers, id, "s1", "2.1.312", Some(&body), None);
        assert!(headers.get(SANDY_PLOVER_HEADER).is_none(), "开关被关掉就不再带");
    }

    /// 压缩过的对话里没有调用块，用过的工具在摘要开头那份清单里（`cap/auto-2.1.312-20261028-full/00206`）：头一回见到这个
    /// 会话也认得回来；裸标签那种同样认，不在消息开头的、不是摘要那段开头的不认。
    #[test]
    fn a_compacted_conversation_brings_its_tools_back_from_the_summary() {
        let summary = "This session is being continued from a previous conversation that ran out of context. \
            The summary below covers the earlier portion of the conversation.\n\n<system-reminder>\n\
            <tools-used-before-summary>Agent, AskUserQuestion, Bash, Edit, ExitPlanMode, Read, SubagentHandback, \
            ToolSearch, WebFetch, WebSearch, Write</tools-used-before-summary>\n</system-reminder>\nSummary:\n1. ...";
        let body = serde_json::json!({"messages": [
            {"role": "user", "content": summary},
            {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
            {"role": "user", "content": [{"type": "text", "text": "next"}]}
        ]});
        let names = tools_used_in(&body, None);
        assert_eq!(names.len(), 11, "{names:?}");
        assert_eq!(
            render(&names).0,
            concat!(
                r#"[{"name":"AskUserQuestion"},{"name":"SubagentHandback"},{"name":"WebFetch"},"#,
                r#"{"name":"WebSearch"},{"name":"Agent"},{"name":"Bash"},{"name":"Edit"},"#,
                r#"{"name":"ExitPlanMode"},{"name":"Read"},{"name":"ToolSearch"},{"name":"Write"}]"#
            )
        );
        let bare = "This session is being continued from a previous conversation that ran out of context. \
            The summary below covers the earlier portion of the conversation.\n\n\
            <tools-used-before-summary>Bash,  ,mcp__s__x, bad name</tools-used-before-summary>\nSummary: ...";
        let body = serde_json::json!({"messages": [{"role": "user", "content": [{"type": "text", "text": bare}]}]});
        assert_eq!(tools_used_in(&body, None), ["Bash", "mcp__s__x", TRUNCATED_SENTINEL]);
        let quoted = format!("look at this:\n{summary}");
        let body = serde_json::json!({"messages": [{"role": "user", "content": quoted}]});
        assert!(tools_used_in(&body, None).is_empty(), "不在消息开头的不认");
    }

    /// 子代理交回、任务通知送达时客户端单独补的那段提醒（`cap/auto-2.1.312-20261028-full/00125`：`SubagentHandback` 只在这里，
    /// 哪条请求体里都没有它的调用块）：整段恰好是 `<system-reminder>` 包着一行清单才认，并进用户消息的哪一段都行，正文是字符串
    /// 也认；前后多了字、清单里换了行、裸标签、在助手消息里的都不认。
    #[test]
    fn a_standalone_reminder_brings_back_what_a_subagent_used() {
        let reminder = "<system-reminder>\n<tools-used-before-summary>SubagentHandback</tools-used-before-summary>\n\
            </system-reminder>\n";
        let body = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Agent", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "launched"},
                {"type": "text", "text": reminder},
                {"type": "text", "text": "<system-reminder>\n[SYSTEM NOTIFICATION - NOT USER INPUT]\n</system-reminder>\n"},
                {"type": "text", "text": "next"}
            ]}
        ]});
        assert_eq!(tools_used_in(&body, None), ["Agent", "SubagentHandback"]);
        let alone = |role: &str, content: serde_json::Value| {
            tools_used_in(
                &serde_json::json!({"messages": [{"role": role, "content": content}]}),
                None,
            )
        };
        let two = "<system-reminder>\n<tools-used-before-summary>Read,  Grep</tools-used-before-summary>\n\
            </system-reminder>";
        assert_eq!(alone("user", serde_json::json!(two)), ["Read", "Grep"]);
        for text in [
            format!("{reminder}more"),
            format!("x{reminder}"),
            "<tools-used-before-summary>Read</tools-used-before-summary>\n".to_string(),
            "<system-reminder>\n<tools-used-before-summary>Read\nGrep</tools-used-before-summary>\n\
                </system-reminder>\n"
                .to_string(),
        ] {
            let block = serde_json::json!([{"type": "text", "text": text}]);
            assert!(alone("user", block).is_empty(), "{text:?}");
        }
        assert!(
            alone("assistant", serde_json::json!([{"type": "text", "text": reminder}])).is_empty()
        );
    }

    /// 清单上是客户端那头的名字，与调用块一样换成出站那个：先前那条调用记的是假名，压缩之后摘要与提醒里列的原名不再
    /// 另记一个；头一回见到的、压缩过的会话也只报假名。
    #[test]
    fn listed_names_go_through_the_tool_name_map() {
        let id = -93_317;
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snug_mitten": {"value": true}}})),
        );
        let map = ToolNameMap {
            forward: [("lookup_docs".to_string(), "mcp__luban__query_docs".to_string())].into(),
            reverse: Vec::new(),
        };
        let before = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "lookup_docs", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}
        ]});
        let summary = "This session is being continued from a previous conversation that ran out of context. \
            The summary below covers the earlier portion of the conversation.\n\n<system-reminder>\n\
            <tools-used-before-summary>Bash, lookup_docs</tools-used-before-summary>\n</system-reminder>\nSummary: ...";
        let after = serde_json::json!({"messages": [
            {"role": "user", "content": summary},
            {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
            {"role": "user", "content": [
                {"type": "text", "text": "<system-reminder>\n<tools-used-before-summary>lookup_docs, Read\
                    </tools-used-before-summary>\n</system-reminder>\n"},
                {"type": "text", "text": "next"}
            ]}
        ]});
        assert_eq!(tools_used_in(&after, Some(&map)), ["Bash", "mcp__luban__query_docs", "Read"]);
        assert_eq!(
            tools_used_in(&after, None),
            ["Bash", "lookup_docs", "Read"],
            "没有改名表就是原名"
        );
        let h = |b: &serde_json::Value, sid: &str| {
            let (v, _) = session_header(id, sid, "2.1.312", Some(b), Some(&map)).unwrap();
            v.to_str().unwrap().to_string()
        };
        let want = r#"[{"name":"mcp__luban__query_docs"},{"name":"Bash"},{"name":"Read"}]"#;
        assert_eq!(h(&before, "s1"), r#"[{"name":"mcp__luban__query_docs"}]"#);
        assert_eq!(h(&after, "s1"), want, "压缩前后同一个工具只有假名");
        assert_eq!(h(&after, "s2"), want, "头一回见到的会话也一样");
    }

    /// 对着 `cap/auto-2.1.312-20261028-full` 每条带这个头的官方请求：只看这一条请求体认出来的名字都在官方头里；`00125`、
    /// `00175` 的 `SubagentHandback` 只在单独那段提醒里，也认得出来。抓包目录不在仓库里（`.gitignore`），没有就跳过。
    #[test]
    fn collects_a_subset_of_the_2_1_312_headers() {
        let dir = format!("{}/cap/auto-2.1.312-20261028-full", env!("CARGO_MANIFEST_DIR"));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("skipped: cap/auto-2.1.312-20261028-full not present");
            return;
        };
        let mut seen = 0;
        for path in entries.filter_map(|e| Some(e.ok()?.path())) {
            let Some(file) = path.file_name().and_then(|f| f.to_str()) else { continue };
            if !file.ends_with(".req.raw") {
                continue;
            }
            let raw = std::fs::read(&path).unwrap();
            let Some(sep) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
            let head = String::from_utf8_lossy(&raw[..sep]);
            let Some(official) = head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(SANDY_PLOVER_HEADER).then(|| v.trim().to_string())
            }) else {
                continue;
            };
            let Ok(body) = serde_json::from_slice::<serde_json::Value>(&raw[sep + 4..]) else {
                continue;
            };
            let official: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(&official)
                .unwrap()
                .iter()
                .map(|n| n["name"].as_str().unwrap().to_string())
                .collect();
            let got = tools_used_in(&body, None);
            assert!(got.iter().all(|n| official.contains(n)), "{file}: {got:?} ⊄ {official:?}");
            if file.starts_with("00125_") || file.starts_with("00175_") {
                assert!(got.iter().any(|n| n == "SubagentHandback"), "{file}: {got:?}");
            }
            seen += 1;
        }
        assert_eq!(seen, 182, "带头的官方请求条数");
    }

    /// `tengu_snug_mitten` 关着时不发头、照样记（只看 `tengu_silly_shore`）：那段用过的 Bash 随后被压缩掉，开关打开时头里还有；
    /// `tengu_silly_shore` 关了就既不记也不发。
    #[test]
    fn records_while_the_header_is_off() {
        let id = -93_315;
        let with = |names: &[&str]| {
            let blocks: Vec<_> = names
                .iter()
                .map(|n| serde_json::json!({"type":"tool_use","id":"t","name":n,"input":{}}))
                .collect();
            serde_json::json!({"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":blocks}]})
        };
        note_eval(id, &features(serde_json::json!({"features": {}})));
        assert_eq!(session_header(id, "s1", "2.1.312", Some(&with(&["Bash"])), None), None, "不发");
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snug_mitten": {"value": true}}})),
        );
        let (v, _) = session_header(id, "s1", "2.1.312", Some(&with(&["Read"])), None).unwrap();
        assert_eq!(v, r#"[{"name":"Bash"},{"name":"Read"}]"#, "关着那段记下的还在");
        note_eval(
            id,
            &features(serde_json::json!({"features": {
                "tengu_snug_mitten": {"value": false}, "tengu_silly_shore": {"value": false}
            }})),
        );
        assert_eq!(session_header(id, "s2", "2.1.312", Some(&with(&["Grep"])), None), None);
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snug_mitten": {"value": true}}})),
        );
        let (v, _) = session_header(id, "s2", "2.1.312", Some(&with(&["Edit"])), None).unwrap();
        assert_eq!(v, r#"[{"name":"Edit"}]"#, "silly_shore 关着那段没记");
    }

    /// 头装不下：同一个锁存只报一次诊断（官方 `reportTruncationOnce`），65 个名字写进去 63 个、丢 2 个。
    #[test]
    fn truncation_is_reported_once_per_latch() {
        let id = -93_316;
        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snug_mitten": {"value": true}}})),
        );
        let blocks: Vec<_> = (0..65)
            .map(|i| serde_json::json!({"type":"tool_use","id":"t","name":format!("t{i}"),"input":{}}))
            .collect();
        let body = serde_json::json!({"messages":[{"role":"assistant","content":blocks}]});
        let mut headers = axum::http::HeaderMap::new();
        let first = apply_session_header(&mut headers, id, "s1", "2.1.312", Some(&body), None);
        assert_eq!(first, Some(Truncation { sent: 63, dropped: 2 }));
        assert!(headers.get(SANDY_PLOVER_HEADER).is_some());
        let again = apply_session_header(&mut headers, id, "s1", "2.1.312", Some(&body), None);
        assert_eq!(again, None, "同一个锁存只报一次");
        let other = apply_session_header(&mut headers, id, "s2", "2.1.312", Some(&body), None);
        assert_eq!(other, Some(Truncation { sent: 63, dropped: 2 }), "别的会话另算");
    }

    /// 启动时读回的那份与刚拉到的 eval 一样算数：重启前开着的号，重启后的额度探测照样带 `[]`；启动之后才到的 eval
    /// 已经记过的号不被库里的旧值盖掉。
    #[test]
    fn restored_flags_count_like_a_fresh_eval() {
        let (on, off, fresh) = (-93_331, -93_332, -93_333);
        note_eval(fresh, &features(serde_json::json!({"features": {}})));
        let n = restore_rows(vec![(on, true, true), (off, true, false), (fresh, true, true)]);
        assert_eq!(n, 2);
        assert_eq!(fresh_header(on, "2.1.312").as_ref().map(|v| v.as_bytes()), Some(&b"[]"[..]));
        assert!(fresh_header(off, "2.1.312").is_none(), "silly_shore 关着不带");
        assert!(fresh_header(fresh, "2.1.312").is_none(), "内存里那份新的为准");
        assert_eq!(
            PERSISTED.lock().get(&fresh),
            Some(&(true, true)),
            "库里那份记成已落库，下一份 eval 会写回"
        );
    }

    /// 内存里的开关与上次落库的不同才写：同一份再来不写，服务端改了就写成新的；内存里没有的号不写。
    #[sqlx::test]
    async fn only_changed_flags_are_written(pool: sqlx::PgPool) {
        let store = crate::store::CredentialStore::for_test(pool.clone()).await;
        // 号 id 显式给一个不与别的用例撞的：开关的内存表是进程级的。
        let id = 93_334;
        sqlx::query(
            "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
             VALUES ($1, 'a', 'r', 0)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        save_eval(&store, id).await;
        assert!(store.all_tools_used_flags().await.unwrap().is_empty(), "没见过 eval 不写");

        note_eval(
            id,
            &features(serde_json::json!({"features": {"tengu_snug_mitten": {"value": true}}})),
        );
        save_eval(&store, id).await;
        assert_eq!(store.all_tools_used_flags().await.unwrap(), vec![(id, true, true)]);

        // 同一份再来不写：库里那条删掉之后再来一遍，也不会补回来。
        sqlx::query("DELETE FROM tools_used_flags").execute(&pool).await.unwrap();
        save_eval(&store, id).await;
        assert!(store.all_tools_used_flags().await.unwrap().is_empty());

        note_eval(id, &features(serde_json::json!({"features": {}})));
        save_eval(&store, id).await;
        assert_eq!(store.all_tools_used_flags().await.unwrap(), vec![(id, false, true)]);
    }
}
