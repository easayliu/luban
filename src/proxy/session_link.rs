//! 模拟请求（与真实 CC 来访）在会话链条上的位置：`cc_prompt_id` / `cc_prev_req` /
//! `diagnostics.previous_message_id` 三个关联字段，按 `(凭证, 会话)` 分桶维护。

use axum::http::HeaderMap;

use crate::config;
use crate::store;

use super::simulation::cc_profile_kind_for;
use super::{
    Simulation, has_beta, incoming_session_id, is_cc_shaped, is_quota_probe_shaped,
    last_user_text_starts_with, uuid_v4,
};

/// 一条模拟请求在会话链条上的位置：三个**指向别的请求**的字段。
///
/// 官方 2.1.260 的主线程请求全都带着这些（`cap/2.1.260-2/00059`）：
///
/// ```text
/// x-anthropic-billing-header: …; cch=f850a; cc_prev_req=req_011CeiBW8Yx9A2uzWiCBsJsU;
///                                 cc_prompt_id=16d7a19d-7939-4638-9703-b31d2fc92661;
/// diagnostics: {"previous_message_id":"msg_011CeiBW9SXwAGthrBNmSU81"}
/// ```
///
/// 一条一条独立造是造不出来的：`cc_prev_req` 是上一条请求的**上游** request-id，
/// `previous_message_id` 是上一条回复的 `message.id`，两者都得等上游回了才知道。故它们
/// 存在 [`CcSessionLink::load`] 那张按会话 id 索引的表里，回程由
/// [`CcSessionLink::record`] 写回。
#[derive(Debug, Default, Clone)]
pub(crate) struct CcSessionLink {
    /// 本轮用户输入的 id。新的用户输入换一个，工具续轮沿用同一个
    /// （`cap/2.1.260-2/00059`、`00061` 两条续轮的 prompt id 相同）。
    ///
    /// `None` 即这条请求不写 `cc_prompt_id`：官方的「猜下一句」请求就不写
    /// （`cap/2.1.260-2/00063` 有 `cc_prev_req` 没有 `cc_prompt_id`），额度探测整条
    /// billing header 都没有。
    pub(super) prompt_id: Option<String>,
    /// 同会话上一条请求的上游 `request-id`。会话第一条没有。
    pub(super) prev_req: Option<String>,
    /// 同会话上一条回复的 `message.id`，写进 `diagnostics.previous_message_id`。
    ///
    /// 会话第一条写的是 `{"previous_message_id":null}`——**字段在、值为 null**，不是不发
    /// 这个字段（`cap/2.1.260-2/00013`、`00025`、`00057` 三份首轮全是这个形态）。
    pub(super) prev_message_id: Option<String>,
    /// 这条是该会话在本进程里的**第一条**请求。启动握手（[`crate::oauth::HandshakeRunner`]）
    /// 就挂在它上面：真实客户端是「先跑完一串启动 GET，再发第一条 messages」，所以这个标记
    /// 必须在**请求发出之前**读到，不能等回程。
    pub(super) first_seen: bool,
    /// 这条请求要不要写 `diagnostics.previous_message_id`。
    ///
    /// 与 [`Self::prompt_id`] **分开**：官方的无工具 helper 带 `cc_prompt_id` 却没有
    /// `diagnostics`（`cap/2.1.260/00024`），「猜下一句」两个都没有（`2.1.260-2/00063`）。
    /// 一个开关管两件事就会给它们各补出一个官方没有的字段。判在 [`CcRequestKind`]。
    pub(super) diagnostics: bool,
}

impl CcSessionLink {
    /// 设定要不要写 `diagnostics`，见 [`Self::diagnostics`]。
    fn with_diagnostics(mut self, yes: bool) -> Self {
        self.diagnostics = yes;
        self
    }
}

/// 按会话 id 索引的模拟会话链条状态。
struct CcSessionEntry {
    prompt_id: String,
    prev_req: Option<String>,
    prev_message_id: Option<String>,
    last_seen: std::time::Instant,
}

/// 全表，键是 **`(凭证 id, 会话 id)`**。
///
/// **必须带凭证 id。** 会话 id 常常是客户端自己带的，而 429/401 换号时同一条请求会带着
/// 同一个会话 id 落到另一张凭证上（转发循环每轮都重跑一次 [`Simulation::detect`] /
/// [`client_session_link`]）。只按会话 id 分桶的话：
///
/// - A 账号的 `cc_prev_req` / `previous_message_id` 会被发到 B 账号上——那两个 id 是 A 的
///   上游请求/回复，B 那边从来没见过；
/// - 同一次用户输入在换号后会**再轮换一次** `cc_prompt_id`；
/// - B 的 `first_seen` 是 false，于是 B 这张凭证的启动握手不会触发。
///
/// 遥测那边本来就是按 `(cred_id, session_id)` 分桶的（`telemetry::State::sessions`），
/// 这里跟它对齐，两处口径才一致。
///
/// 进程内存里就够：这些值只在一个会话活着的时候有意义，重启后客户端那边多半也换了会话；
/// 落库反而会把「上次进程里那条 request-id」接到一个新会话上。
static CC_SESSIONS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<(i64, String), CcSessionEntry>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// 一个会话多久没请求就忘掉：与 [`config::TELEMETRY_SESSION_IDLE_SECS`] 同口径——同一个
/// session id 隔了几个小时再来就是另一次对话，把上次那条 request-id 接过去是假的。
const CC_SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(3 * 60 * 60);

/// 每个**前缀谱系**上一条请求的 system 正文指纹与正文（[`PrefixEntry`]），给
/// [`cache_prefix_stable`] 判「这一轮的 system 与上一轮是不是同一份」，变了的话还要拿
/// 上一轮的正文对出差异。
///
/// 键是 [`PrefixKey`]：凭证、会话之外还带**请求类别、tools 指纹、system 块数**。只按
/// （凭证，会话）分桶是错的：现网一个会话里主线程（3 块 system、一套 tools）与辅助请求
/// （2 块、另一套 tools）交替出现，两种形态互相覆盖对方的「上一轮」，稳定性永远判不成，
/// 日志里全是 tools differ / block count differs 的噪音。tools 或块数一变就是另一条谱系，
/// 对那条谱系来说是第一轮、不补断点——与「不稳定」同一个结果，但不会打掉别人的记录。
///
/// 与 [`CC_SESSIONS`] 分开存而不是并进 [`CcSessionEntry`]：那张表只在 `billing_cch` 开着、
/// 且这一类请求上会话链时才建条目（[`client_session_link`]），而补消息断点这件事与
/// billing header 无关，生产实例 `billing_cch` 关着时也要能判。过期口径同 [`CC_SESSION_IDLE`]。
///
/// 稳定性只看 system 正文的**指纹**，正文只是诊断附件：前缀一变就能在日志里看到变的是
/// 哪一块、哪一段——流水的 shape 只有长度和 sha，现网那个尾块每轮长 51 字节的会话光看
/// 流水查不出多的是什么。正文受 [`PrefixLimits`] 管，超限只留指纹、不留正文，判断照做、
/// 日志少一行差异。
///
/// **锁内只做表操作**：指纹、字节数、正文拷贝都在拿锁前算好（[`cache_prefix_stable`]），
/// 比对与日志在放锁后做。锁内多扫一遍几十 KB 正文，别的会话都得等。
static CC_PREFIX_FPS: std::sync::LazyLock<parking_lot::Mutex<PrefixTable>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(PrefixTable::default()));

/// 一条请求缓存前缀里**会进缓存键**的部分：`tools` 的指纹，加 `system` 各块正文
/// （不含 billing header 那一块）。由 [`crate::proxy::cache_prefix_of`] 算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CachePrefix {
    pub(super) tools_fp: u64,
    pub(super) system: Vec<String>,
}

impl CachePrefix {
    fn system_fp(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.system.hash(&mut h);
        h.finish()
    }

    /// 正文在表里占的字节：各块字符数之和，**加上每块 `String` 头的容器开销**。只算字符
    /// 会被「20 万个单字符块」绕过——字符 200 KB、元素区 4.8 MB。
    fn stored_bytes(&self) -> usize {
        self.system.iter().map(String::len).sum::<usize>()
            + self.system.len() * std::mem::size_of::<String>()
    }
}

/// [`PrefixTable`] 的键：一条**前缀谱系**。理由见 [`CC_PREFIX_FPS`]。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PrefixKey {
    cred_id: i64,
    session_id: String,
    kind: CcRequestKind,
    tools_fp: u64,
    blocks: usize,
}

/// [`PrefixTable`] 里一条谱系的条目。`system` 为 `None` 表示正文因预算被放弃，指纹照留。
#[derive(Debug, Clone)]
struct PrefixEntry {
    system_fp: u64,
    system: Option<Vec<String>>,
    /// `system` 占的字节数（`None` 时为 0），维护 [`PrefixTable::text_bytes`] 用。
    bytes: usize,
    seen: std::time::Instant,
}

/// 正文与条目数的上限。各管一件事：单条块数或字节太大的不存（一条 system 超过
/// `entry_bytes` 多半是把整本文档塞进了 system，诊断价值也低；块数上限挡住海量小块，
/// 前缀采集在 system 块数封顶之前，封顶保护不到这张表）；总字节到 `total_bytes` 后新来的
/// 只留指纹；条目数到 `entries` 后按最久未见淘汰——一直有请求的会话不会因
/// [`CC_SESSION_IDLE`] 过期，没有这一条表只增不减。
#[derive(Debug, Clone, Copy)]
struct PrefixLimits {
    entry_blocks: usize,
    entry_bytes: usize,
    total_bytes: usize,
    entries: usize,
}

impl PrefixLimits {
    /// 生产用的一组：单条 64 块 / 256 KiB、总共 64 MiB、一万条谱系。
    const DEFAULT: Self = Self {
        entry_blocks: 64,
        entry_bytes: 256 * 1024,
        total_bytes: 64 * 1024 * 1024,
        entries: 10_000,
    };
}

/// `谱系 → 上一轮`，外加正文总字节数。
#[derive(Debug, Default)]
struct PrefixTable {
    map: std::collections::HashMap<PrefixKey, PrefixEntry>,
    text_bytes: usize,
}

impl PrefixTable {
    /// 记下这一轮，返回上一轮的条目（没有即 `None`）。`text` 是调用方在锁外拷好的正文
    /// （单条上限已经在锁外判过），这里只再看总预算；`bytes` 是它入表要占的字节。
    /// 只做表操作，不算指纹、不比对、不打日志。
    fn record(
        &mut self,
        key: PrefixKey,
        system_fp: u64,
        text: Option<Vec<String>>,
        bytes: usize,
        now: std::time::Instant,
        limits: PrefixLimits,
    ) -> Option<PrefixEntry> {
        // 过期清理与条目数上限都要在插入前做，不然刚插的这条可能就被自己淘汰掉。
        self.map.retain(|_, e| now.duration_since(e.seen) < CC_SESSION_IDLE);
        self.text_bytes = self.map.values().map(|e| e.bytes).sum();
        while self.map.len() >= limits.entries && !self.map.contains_key(&key) {
            let Some(oldest) = self.map.iter().min_by_key(|(_, e)| e.seen).map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(e) = self.map.remove(&oldest) {
                self.text_bytes -= e.bytes;
            }
        }
        let prev = self.map.remove(&key);
        if let Some(e) = &prev {
            self.text_bytes -= e.bytes;
        }
        let keep_text = text.is_some() && self.text_bytes + bytes <= limits.total_bytes;
        let entry = PrefixEntry {
            system_fp,
            system: if keep_text { text } else { None },
            bytes: if keep_text { bytes } else { 0 },
            seen: now,
        };
        self.text_bytes += entry.bytes;
        self.map.insert(key, entry);
        prev
    }
}

/// 记下这一轮的缓存前缀，并回答同一谱系**上一轮**的 system 是不是同一份。谱系第一次出现
/// （含 tools 或块数刚变过）、或隔了 [`CC_SESSION_IDLE`] 再来，都算不稳定（`false`）。
/// 变了的话按块对出差异打一行 info（[`log_prefix_change`]）——指纹与拷贝在锁前算，
/// 比对与日志在锁后做，锁内只有表操作。
///
/// 这是 [`crate::proxy::ensure_cc_message_breakpoint`] 的闸：prompt cache 是前缀缓存，
/// `system` 里任何一块变了，它后面的 `messages` 不管标不标断点都是未命中——标了只是把
/// 「按输入价裸算」换成「按 1.25 倍写入价裸算」。v0.3.121 上线后 claude-vscode 2.1.273
/// 那个会话就是这样：system 尾块每轮长 51 字节，24 轮每轮把 25 万 token 的历史整段写进
/// 缓存，`cache_read` 始终停在 27,126（tools + system 前三块），一个字都没读到。
pub(super) fn cache_prefix_stable(
    key: CcSessionKey<'_>,
    kind: CcRequestKind,
    prefix: CachePrefix,
) -> bool {
    let limits = PrefixLimits::DEFAULT;
    // 锁外：指纹、字节数、单条上限、正文拷贝。
    let system_fp = prefix.system_fp();
    let bytes = prefix.stored_bytes();
    let text = (prefix.system.len() <= limits.entry_blocks && bytes <= limits.entry_bytes)
        .then(|| prefix.system.clone());
    let lineage = PrefixKey {
        cred_id: key.cred_id,
        session_id: key.session_id.to_string(),
        kind,
        tools_fp: prefix.tools_fp,
        blocks: prefix.system.len(),
    };
    let now = std::time::Instant::now();
    let prev = CC_PREFIX_FPS.lock().record(lineage, system_fp, text, bytes, now, limits);
    // 锁外：比对与日志。
    let Some(prev) = prev else { return false };
    if prev.system_fp == system_fp {
        return true;
    }
    log_prefix_change(key.session_id, &prev, &prefix);
    false
}

/// 差异段最多打这么多字符，两边各一段；system 块几十 KB，整块打出来没法看。
const PREFIX_DIFF_MAX_CHARS: usize = 400;

/// 变了的那块另打**当前正文的末尾**这么多字符：光看差异段不知道它接在什么后面，
/// 尾块的末尾正是客户端逐轮追加内容的地方。
const PREFIX_TAIL_CHARS: usize = 600;

/// 同一谱系相邻两轮 system 不同时，逐块把不同的那一段打进日志：块号、两轮长度、差异起点、
/// 去掉的段原文、加上的段原文、该块当前末尾原文。上一轮正文因预算没留时只打一行说明。
/// tools 变、块数变不在这里——那是另一条谱系（[`PrefixKey`]），不算同一前缀的变化。
///
/// 打的是 system 正文而不是摘要：这行日志就是给「多出来的到底是什么」这个问题的，
/// 流水的 shape 刻意不存正文，这里是唯一能看到原文的地方。只在前缀变了的轮次打。
fn log_prefix_change(session_id: &str, prev: &PrefixEntry, cur: &CachePrefix) {
    let Some(prev_system) = &prev.system else {
        tracing::info!(
            session = %session_id,
            "cache prefix changed between turns: system differs (previous text not kept, over budget)"
        );
        return;
    };
    for (i, (a, b)) in prev_system.iter().zip(&cur.system).enumerate() {
        let Some((at, removed, added)) = text_diff(a, b) else { continue };
        tracing::info!(
            session = %session_id,
            block = i,
            prev_len = a.len(),
            cur_len = b.len(),
            at,
            removed = ?truncate_chars(removed, PREFIX_DIFF_MAX_CHARS),
            added = ?truncate_chars(added, PREFIX_DIFF_MAX_CHARS),
            cur_tail = ?tail_chars(b, PREFIX_TAIL_CHARS),
            "cache prefix changed between turns: system block differs"
        );
    }
}

/// 两段文本的差异：去掉公共前缀与公共后缀后剩下的中段，返回 `(差异起点字节偏移,
/// 旧中段, 新中段)`；两段相同返回 `None`。切点落在字符边界上。
pub(super) fn text_diff<'a>(a: &'a str, b: &'a str) -> Option<(usize, &'a str, &'a str)> {
    if a == b {
        return None;
    }
    let mut p = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    while !a.is_char_boundary(p) || !b.is_char_boundary(p) {
        p -= 1;
    }
    let max_s = a.len().min(b.len()) - p;
    let mut s = a.bytes().rev().zip(b.bytes().rev()).take_while(|(x, y)| x == y).count().min(max_s);
    while !a.is_char_boundary(a.len() - s) || !b.is_char_boundary(b.len() - s) {
        s -= 1;
    }
    Some((p, &a[p..a.len() - s], &b[p..b.len() - s]))
}

/// 取末尾 `n` 个字符，截过的在开头加 `(共 N 字符)…`。
fn tail_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    let tail: String = s.chars().skip(total - n).collect();
    format!("(共 {total} 字符)…{tail}")
}

/// 截到前 `max` 个字符，截过的在末尾加 `…(共 N 字符)`。
fn truncate_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…(共 {total} 字符)")
}

/// [`CC_SESSIONS`] 的键：**凭证 + 会话**，缺一不可，理由见那张表的注释。
#[derive(Debug, Clone, Copy)]
pub(super) struct CcSessionKey<'a> {
    pub(super) cred_id: i64,
    pub(super) session_id: &'a str,
}

impl CcSessionKey<'_> {
    fn owned(self) -> (i64, String) {
        (self.cred_id, self.session_id.to_string())
    }
}

impl CcSessionLink {
    /// 取这条请求要写的三个关联字段，并按需换一轮 `cc_prompt_id`。
    ///
    /// `new_prompt` 为真（末条消息是一次新的用户输入，而非 `tool_result` 续轮）时换新的
    /// prompt id；否则沿用会话里那个。会话第一次出现时无论如何都要生成一个。
    ///
    /// `wants_prompt_id` 为假时不写 `cc_prompt_id`，但**会话状态照样推进**——官方那条
    /// 「猜下一句」请求也在同一条链上，只是自己不写这个字段。
    pub(super) fn load(key: CcSessionKey<'_>, new_prompt: bool, wants_prompt_id: bool) -> Self {
        let now = std::time::Instant::now();
        let mut map = CC_SESSIONS.lock();
        map.retain(|_, e| now.duration_since(e.last_seen) < CC_SESSION_IDLE);
        let key = key.owned();
        let known = map.contains_key(&key);
        let entry = map.entry(key).or_insert_with(|| CcSessionEntry {
            prompt_id: uuid_v4(),
            prev_req: None,
            prev_message_id: None,
            last_seen: now,
        });
        // 新建的那份 id 就是这一轮的，别再换一次。
        if known && new_prompt {
            entry.prompt_id = uuid_v4();
        }
        entry.last_seen = now;
        Self {
            prompt_id: wants_prompt_id.then(|| entry.prompt_id.clone()),
            prev_req: entry.prev_req.clone(),
            prev_message_id: entry.prev_message_id.clone(),
            first_seen: !known,
            // 缺省写：模拟路径只造主线程 profile，它是要 diagnostics 的。真实 CC 那条路由
            // [`CcSessionLink::with_diagnostics`] 按 [`CcRequestKind`] 覆写。
            diagnostics: true,
        }
    }

    /// 回程记下这一条的上游 `request-id` 与回复的 `message.id`，供同会话下一条引用。
    /// 两个都没有（上游连头都没回）时什么也不做——把 `None` 写回去会把链条清空，
    /// 而官方那边链条是不会中断的。
    pub(super) fn record(key: CcSessionKey<'_>, req_id: Option<&str>, message_id: Option<&str>) {
        if req_id.is_none() && message_id.is_none() {
            return;
        }
        let mut map = CC_SESSIONS.lock();
        let Some(entry) = map.get_mut(&key.owned()) else { return };
        if let Some(r) = req_id {
            entry.prev_req = Some(r.to_string());
        }
        if let Some(m) = message_id {
            entry.prev_message_id = Some(m.to_string());
        }
        entry.last_seen = std::time::Instant::now();
    }
}

/// **真实 CC（API-key 模式）来访**在会话链条上的位置：`(会话 id, 链)`；不该补时 `None`。
///
/// API-key 端的 CC 发的 billing header 是光秃秃的
/// `x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli;`
/// ——没有 `cch`、没有 `cc_prompt_id`、没有 `cc_prev_req`，整条请求也**没有 `diagnostics`**
/// （`cap/2.1.258-api` 六份逐一核过）。luban 拿 OAuth token 把它转出去之后，上游看到的
/// 就是「一条 OAuth 主线程请求，却一个会话关联字段都没有」——而订阅端官方那边这三个字段
/// 是每条主线程请求都有的。`cch` 早就补了，这三个一直缺着。
///
/// 一条 CC 形态的来访属于哪一类官方 profile，**只为决定那三个关联字段各写不写**。
///
/// 官方六个 profile 在这三项上各不相同（`cap/2.1.260` / `cap/2.1.260-2` 逐条核过），
/// 一刀切成主线程就会给「猜下一句」造出一个它本来没有的 `cc_prompt_id`、给无工具 helper
/// 多出一个 `diagnostics`：
///
/// | profile | 抓包 | 换新 prompt id | 写 `cc_prompt_id` | 写 `diagnostics` |
/// |---|---|:---:|:---:|:---:|
/// | 主线程 | `2.1.260-2/00025` | 新输入时换 | 是 | 是 |
/// | SDK 子代理 | `2.1.260/00020` | **不换**（跟父会话同一个） | 是 | 是 |
/// | 无工具 helper | `2.1.260/00024` | 不换 | 是 | **否** |
/// | 猜下一句 | `2.1.260-2/00063` | 不换 | **否** | **否** |
/// | 标题生成 | `2.1.260-2/00058` | 不换 | 否 | 否 |
/// | 安全分类 | `2.1.260/00019` | 不换 | 否 | 否 |
///
/// 「换新 prompt id」尤其要紧：`cc_prompt_id` 是**整个会话共享**的一个值，而
/// [`crate::telemetry::last_is_new_prompt_body`] 只看末条消息是不是新的用户输入——
/// 「猜下一句」那条的末条正是一句 `[SUGGESTION MODE: …]` 的 user 消息，照这个判就会换一轮
/// id，把主线程那条链也一起带偏。子代理同理：它的末条常常是一句全新的 user 消息。
/// 除了会话链那三项，它还管住两条「别把官方形态改坏」的规则，见
/// [`Self::keeps_nonstream`] 与 [`Self::allows_system_prefix`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CcRequestKind {
    Main,
    Subagent,
    /// 「猜下一句」：带工具，末条用户消息以 `[SUGGESTION MODE:` 开头。
    Suggestion,
    /// 无工具的辅助调用。
    Helper,
    /// 标题生成：无工具、`structured-outputs` beta、**流式**。
    Title,
    /// 安全分类：`auto-mode-classifier` beta、`max_tokens:64`、**非流式**。
    Classifier,
    /// 额度探测：`max_tokens:1`、**没有 `system`**、没有 `stream`。
    QuotaProbe,
}

impl CcRequestKind {
    /// 判据与 [`crate::telemetry`] 那边的 `Kind` 同源，只是这里还多认一个额度探测
    /// （遥测那边不需要区分它）。`beta` 是**来访自己**那串。
    pub(super) fn of(v: &serde_json::Value, beta: &[String]) -> Self {
        // 额度探测最先判：它**连 `system` 都没有**，后面每一条判据都以 `system` 或
        // `tools` 为前提，落到那里只会被当成 helper。
        if is_quota_probe_shaped(v) {
            return Self::QuotaProbe;
        }
        // 标题生成（`structured-outputs`）与安全分类（`auto-mode-classifier`）各有独有 beta。
        if has_beta(beta, config::CC_BETA_STRUCTURED_OUTPUTS) {
            return Self::Title;
        }
        if has_beta(beta, config::CC_BETA_AUTO_MODE_CLASSIFIER) {
            return Self::Classifier;
        }
        let tools = v.get("tools").and_then(|t| t.as_array()).map_or(0, |t| t.len());
        if tools == 0 {
            return Self::Helper;
        }
        // 子代理：billing header 里那个 `cc_is_subagent=true`。
        let subagent = v
            .get("system")
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.contains("cc_is_subagent=true"));
        if subagent {
            return Self::Subagent;
        }
        if last_user_text_starts_with(v, "[SUGGESTION MODE:") {
            return Self::Suggestion;
        }
        Self::Main
    }

    /// 只有主线程会换新一轮 `cc_prompt_id`；其余都挂在会话现有那一轮上。
    fn rotates_prompt(self) -> bool {
        self == Self::Main
    }

    fn wants_prompt_id(self) -> bool {
        matches!(self, Self::Main | Self::Subagent | Self::Helper)
    }

    /// `cc_prev_req` 是**独立的第三个维度**——它与 `cc_prompt_id` 的分界线不一样：
    /// 无工具 helper 带 `cc_prompt_id` 却**从不带** `cc_prev_req`
    /// （`cap/2.1.260/00024`、`00027`，后者排在一堆请求之后，仍然没有）；
    /// 「猜下一句」正相反，带 `cc_prev_req` 却不带 `cc_prompt_id`（`2.1.260-2/00063`）。
    fn wants_prev_req(self) -> bool {
        matches!(self, Self::Main | Self::Subagent | Self::Suggestion)
    }

    fn wants_diagnostics(self) -> bool {
        matches!(self, Self::Main | Self::Subagent)
    }

    /// 这一类要不要进会话链。三项全不写的（标题、安全分类、额度探测）整条跳过。
    pub(super) fn on_session_chain(self) -> bool {
        self.wants_prompt_id() || self.wants_prev_req() || self.wants_diagnostics()
    }

    /// 给这一类在 `version` 上补 billing header 时该用的**固定后缀**；这一版没有对应
    /// profile（或版本更老）时 `None`，由调用方退回 [`cc_version_suffix`] 那套派生算法。
    ///
    /// 只对 2.1.260 生效：那一版的后缀是逐 profile 定死的（见 [`config::CC_PROFILES`]），
    /// 派生算法在它上面已被证否。2.1.258 及更早仍走派生——那一版五份抓包全是 `1e2`，
    /// 而算法在 `"hi"` 上正好也算出 `1e2`。
    ///
    /// 主线程与「猜下一句」是主线程那一档，后缀跟**模型族**走（`222`/`bcd`/…），
    /// 故还要 `model`。
    pub(super) fn billing_suffix_at(self, version: &str, model: &str) -> Option<&'static str> {
        if version != config::CC_VERSION_BASE {
            return None;
        }
        let kind = match self {
            Self::Subagent => config::CcProfileKind::SdkSubagentHaiku,
            Self::Helper => config::CcProfileKind::HelperSubagentHaiku,
            Self::Title => config::CcProfileKind::SessionTitleHaiku,
            Self::Classifier => config::CcProfileKind::SecurityClassifierSonnet,
            Self::Main | Self::Suggestion => cc_profile_kind_for(model),
            // 额度探测压根没有 billing header，走不到这里。
            Self::QuotaProbe => return None,
        };
        Some(config::cc_profile(kind).billing_suffix)
    }

    /// 这一类官方**本来就是非流式**，`nonstream_as_sse` 不能把它改成 `stream:true`。
    ///
    /// 安全分类（`cap/2.1.260/00019`、`00030`）与额度探测（`cap/2.1.260-2/00004`）整条都
    /// 没有 `stream` 字段。那个开关的本意是「官方恒为流式，非流式请求一看就不是 CC」——
    /// 对这两类恰好相反：把它们改成流式才是官方不产生的形态。其余（含标题生成）官方确实
    /// 是 `stream:true`，照改。
    pub(super) fn keeps_nonstream(self) -> bool {
        matches!(self, Self::Classifier | Self::QuotaProbe)
    }

    /// 这一类能不能补 `system` 前缀（billing header + 身份句）。
    ///
    /// 额度探测**没有 `system`**，连 billing header 都没有。给它补一份，就把一条
    /// `max_tokens:1` 的探测改成了「带身份声明的请求」——官方从不产生。
    pub(super) fn allows_system_prefix(self) -> bool {
        self != Self::QuotaProbe
    }
}

/// **判据不能借用 [`is_official_non_main_beta`]**——那是「要不要补主线程那几项 beta」，
/// 和「带不带会话关联字段」是两码事，六个 profile 上的分界线根本不在同一处：
///
/// | profile | 补主线程 beta | 带 `cc_prompt_id` / `diagnostics` |
/// |---|---|---|
/// | 主线程四族 | 是 | 是 / 是 |
/// | SDK 子代理 | **否** | **是 / 是** |
/// | 无工具 helper | 否 | 是 / 否 |
/// | 标题生成 | 否 | 否 / 否 |
/// | 安全分类 | 否 | 否 / 否 |
/// | 额度探测 | 否 | 连 billing header 都没有 |
///
/// SDK 子代理正好落在两栏相反的那一格：它的 beta 集合不该被补，但它**确实**带着
/// `cc_prompt_id` 与 `diagnostics`（`cap/2.1.260/00020`、`00025`）。两个判据混用，
/// 它就被整个跳过了。
///
/// 故这里独立判：**只排除标题生成与安全分类**（这两类由各自独有的 beta 认出来），
/// 其余 CC 形态的请求都补。无工具 helper 官方带 `cc_prompt_id` 但不带 `diagnostics`，
/// 而 luban 认不出它（它没有任何独有标记），归到「补」这一侧——多一个 `diagnostics`
/// 的代价小于整类子代理都缺关联链。
pub(super) fn client_session_link(
    body: Option<&serde_json::Value>,
    headers: &HeaderMap,
    sim: Option<&Simulation>,
    flags: store::ForwardFlags,
    billable: bool,
    cred: &crate::credentials::Credential,
) -> Option<(String, CcSessionLink)> {
    // 模拟那条路自己带链；`billing_cch` 是「允不允许动 billing header」的总开关。
    if sim.is_some() || !billable || !flags.billing_cch {
        return None;
    }
    let v = body?;
    if !is_cc_shaped(v) {
        return None;
    }
    let beta: Vec<String> = headers
        .get("anthropic-beta")
        .and_then(|x| x.to_str().ok())
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    // 逐类决定那三项各写不写，见 [`CcRequestKind`]。标题生成、安全分类与额度探测三项
    // 全不写（后者连 billing header 都没有），整条就不必进链。
    let kind = CcRequestKind::of(v, &beta);
    if !kind.on_session_chain() {
        return None;
    }
    // 客户端**自己的**会话 id：认不出合法 uuid 就不补——这条链是拿会话 id 当键的，
    // 键都不可靠时接出来的「上一条」可能来自另一个客户端。
    let session_id = incoming_session_id(headers, Some(v))?;
    // 只有主线程会换新一轮 prompt id：`last_is_new_prompt_body` 只看末条消息，而
    // 「猜下一句」与子代理的末条也常常是一句全新的 user 消息，照它判就会把整个会话的
    // prompt id 带偏。
    let new_prompt = kind.rotates_prompt() && crate::telemetry::last_is_new_prompt_body(v);
    let mut link = CcSessionLink::load(
        CcSessionKey { cred_id: cred.id, session_id: &session_id },
        new_prompt,
        kind.wants_prompt_id(),
    );
    if !kind.wants_prev_req() {
        link.prev_req = None;
    }
    Some((session_id, link.with_diagnostics(kind.wants_diagnostics())))
}

#[cfg(test)]
mod tests {
    use crate::proxy::test_support::{all_on, base_block, detect_with, parsed, test_cred};
    use crate::proxy::{Bytes, HeaderValue, config, header};

    /// 前缀差异日志靠 [`super::text_diff`] 找出两轮 system 块里变的那一段：只剩中段、
    /// 切点在字符边界上；[`super::cache_prefix_stable`] 变了回 `false`、没变回 `true`。
    #[test]
    fn prefix_diff_isolates_the_changed_segment() {
        use super::{CachePrefix, cache_prefix_stable, text_diff};
        // 现网那种尾块每轮追加：差异段就是追加的那 51 字节。
        let tail = "…memory\n\n# Notes";
        let grown = format!("{tail}\n\n<total_tokens>14999746 tokens left</total_tokens>");
        let (at, removed, added) = text_diff(tail, &grown).unwrap();
        assert_eq!(at, tail.len());
        assert_eq!(removed, "");
        assert_eq!(added, "\n\n<total_tokens>14999746 tokens left</total_tokens>");
        assert_eq!(added.len(), 51, "49 字节提醒加 \\n\\n 正是流水里每轮多出的 51");
        // 中段替换、多字节字符：切点不能落在字符中间。
        let (at, removed, added) = text_diff("前缀中文后缀", "前缀英文后缀").unwrap();
        assert_eq!(at, "前缀".len());
        assert_eq!((removed, added), ("中", "英"));
        // 数字变了（同长度替换）。
        let (at, removed, added) =
            text_diff("<total_tokens>15000000 tokens left>", "<total_tokens>14999746 tokens left>")
                .unwrap();
        assert_eq!(at, "<total_tokens>1".len());
        assert_eq!((removed, added), ("5000000", "4999746"));
        assert!(text_diff("same", "same").is_none());
        assert_eq!(text_diff("", "x").unwrap(), (0, "", "x"));
        assert_eq!(super::truncate_chars("abc", 5), "abc");
        assert_eq!(super::truncate_chars("abcdefgh", 3), "abc…(共 8 字符)");
        assert_eq!(super::tail_chars("abc", 5), "abc");
        assert_eq!(super::tail_chars("abcdefgh", 3), "(共 8 字符)…fgh");

        use crate::proxy::CcRequestKind::{Main, Subagent};
        let sid = crate::proxy::uuid_v4();
        let key = crate::proxy::CcSessionKey { cred_id: 7, session_id: &sid };
        let p = |tail: &str| CachePrefix { tools_fp: 1, system: vec!["base".into(), tail.into()] };
        assert!(!cache_prefix_stable(key, Main, p(tail)), "第一轮没有上一轮可比");
        assert!(cache_prefix_stable(key, Main, p(tail)), "同一份前缀");
        assert!(!cache_prefix_stable(key, Main, p(&grown)), "尾块变了");
        assert!(cache_prefix_stable(key, Main, p(&grown)), "稳住了");
        assert!(
            !cache_prefix_stable(key, Main, CachePrefix { tools_fp: 2, ..p(&grown) }),
            "tools 变了是另一条谱系，第一轮"
        );
        assert!(
            !cache_prefix_stable(
                key,
                Main,
                CachePrefix { tools_fp: 2, system: vec!["base".into(), "x".into(), "y".into()] }
            ),
            "块数变了是另一条谱系，第一轮"
        );

        // 现网那种交替：主线程 3 块 + 一套 tools，辅助请求 2 块 + 另一套 tools，共用会话 id。
        // 两条谱系各记各的，主线程第二轮照样判稳定，不被中间那条辅助请求打掉。
        let sid = crate::proxy::uuid_v4();
        let key = crate::proxy::CcSessionKey { cred_id: 7, session_id: &sid };
        let main =
            || CachePrefix { tools_fp: 11, system: vec!["a".into(), "b".into(), "c".into()] };
        let helper = || CachePrefix { tools_fp: 22, system: vec!["h".into(), "i".into()] };
        assert!(!cache_prefix_stable(key, Main, main()), "主线程第一轮");
        assert!(!cache_prefix_stable(key, Subagent, helper()), "辅助第一轮");
        assert!(cache_prefix_stable(key, Main, main()), "主线程第二轮：中间夹的辅助请求不算变");
        assert!(cache_prefix_stable(key, Subagent, helper()), "辅助第二轮同理");
        // 同形态不同类别也分开：类别在键里。
        assert!(!cache_prefix_stable(key, Subagent, main()), "同一份前缀换个类别是新谱系");
    }

    /// 正文预算：总量超限只丢正文不丢指纹（稳定性照判）；条目数到顶按最久未见淘汰；同一
    /// 谱系更新时先扣旧条目的字节；容器开销算进字节；单条块数 / 字节上限在锁外判
    /// （[`super::cache_prefix_stable`]），这里用 `text: None` 模拟已被判掉。
    #[test]
    fn prefix_table_keeps_fingerprints_when_text_is_over_budget() {
        use super::{CachePrefix, PrefixKey, PrefixLimits, PrefixTable};
        let now = std::time::Instant::now();
        let at = |secs: u64| now + std::time::Duration::from_secs(secs);
        let sz = std::mem::size_of::<String>();
        // 总预算：两条各「10 字符 + 一块头」正好装下，第三条装不下。
        let limits = PrefixLimits {
            entry_blocks: 64,
            entry_bytes: 256 * 1024,
            total_bytes: 2 * (10 + sz) + 5,
            entries: 3,
        };
        let mut t = PrefixTable::default();
        let p = |text: &str| CachePrefix { tools_fp: 1, system: vec![text.to_string()] };
        let k = |i: i64| PrefixKey {
            cred_id: i,
            session_id: format!("s{i}"),
            kind: crate::proxy::CcRequestKind::Main,
            tools_fp: 1,
            blocks: 1,
        };
        let rec = |t: &mut PrefixTable, i: i64, text: &str, keep: bool, when| {
            let pf = p(text);
            t.record(
                k(i),
                pf.system_fp(),
                keep.then(|| pf.system.clone()),
                pf.stored_bytes(),
                when,
                limits,
            )
        };

        // 容器开销算进字节：一块 10 字符占 10 + size_of::<String>()。
        assert_eq!(p("x".repeat(10).as_str()).stored_bytes(), 10 + sz);
        // 20 万个单字符块：字符 20 万，入表字节远大于此。
        let many = CachePrefix { tools_fp: 1, system: vec!["x".to_string(); 200_000] };
        assert_eq!(many.stored_bytes(), 200_000 + 200_000 * sz);

        // 单条在锁外被判掉（text: None）：只留指纹，稳定性照判。
        assert!(rec(&mut t, 1, "x", false, at(0)).is_none());
        assert!(t.map[&k(1)].system.is_none(), "单条超限不留正文");
        assert_eq!(t.text_bytes, 0);
        let prev = rec(&mut t, 1, "x", false, at(0)).unwrap();
        assert_eq!(prev.system_fp, p("x").system_fp());
        assert_ne!(prev.system_fp, p("y").system_fp());

        // 两条各 10 字符都留；第三条总量超限只留指纹。
        rec(&mut t, 2, "a".repeat(10).as_str(), true, at(1));
        rec(&mut t, 3, "b".repeat(10).as_str(), true, at(2));
        assert_eq!(t.text_bytes, 2 * (10 + sz));
        assert_eq!(t.map.len(), 3);
        // 条目数已到 3，新谱系进来淘汰最久未见的那一个（k(1)，seen 最早）。
        rec(&mut t, 4, "c".repeat(8).as_str(), true, at(3));
        assert_eq!(t.map.len(), 3, "条目数封顶");
        assert!(!t.map.contains_key(&k(1)), "淘汰最久未见的");
        assert!(t.map[&k(4)].system.is_none(), "总量超限只留指纹");
        assert_eq!(t.text_bytes, 2 * (10 + sz));
        // 同一谱系换成短正文：先扣旧的，再加新的。
        rec(&mut t, 2, "abc", true, at(4));
        assert_eq!(t.text_bytes, (10 + sz) + (3 + sz));
        assert_eq!(t.map[&k(2)].system.as_deref(), Some(&["abc".to_string()][..]));
    }

    /// 会话链条：`cc_prompt_id` 新输入换一轮、工具续轮沿用；`cc_prev_req` 与
    /// `diagnostics.previous_message_id` 指向上一条请求/回复。
    ///
    /// 这三个字段官方是**跨请求**的（`cap/2.1.260-2/00013` → `00059` → `00061`），一条一条
    /// 独立造造不出来：造出来的会是「每条请求都是一次全新会话的第一条」。
    /// 测试用的会话键：都挂在同一张凭证上，跨凭证隔离另见
    /// [`session_link_is_isolated_per_credential`]。
    fn key(session_id: &str) -> crate::proxy::CcSessionKey<'_> {
        crate::proxy::CcSessionKey { cred_id: 1, session_id }
    }

    /// **同一个会话 id 落到不同凭证上，两条链必须各走各的。**
    ///
    /// 429/401 换号时，转发循环会带着**同一个**客户端会话 id 重跑一遍
    /// `Simulation::detect` / `client_session_link`。只按会话 id 分桶的话，A 账号的
    /// `cc_prev_req`（那是 A 的上游 request-id，B 从没见过）会被发到 B 上，同一次用户输入
    /// 还会被再轮换一次 prompt id，而 B 的 `first_seen` 是 false ——B 这张凭证的启动握手
    /// 就永远不会触发。遥测那边本来就是按 `(cred_id, session_id)` 分的，这里跟它对齐。
    #[test]
    fn session_link_is_isolated_per_credential() {
        let sid = format!("test-{}", crate::proxy::uuid_v4());
        let a = crate::proxy::CcSessionKey { cred_id: 1, session_id: &sid };
        let b = crate::proxy::CcSessionKey { cred_id: 2, session_id: &sid };

        let first_a = crate::proxy::CcSessionLink::load(a, true, true);
        assert!(first_a.first_seen, "A 的第一条");
        crate::proxy::CcSessionLink::record(a, Some("req_a"), Some("msg_a"));

        // 换到 B：同一个会话 id，但对 B 来说这是**新会话**——要触发 B 自己的启动握手，
        // 且拿不到 A 的那两个 id。
        let first_b = crate::proxy::CcSessionLink::load(b, false, true);
        assert!(first_b.first_seen, "换号后对 B 是新会话，B 的启动握手要触发");
        assert!(first_b.prev_req.is_none(), "A 的 request-id 不能发给 B");
        assert!(first_b.prev_message_id.is_none(), "A 的 message.id 同样不能");
        assert_ne!(first_b.prompt_id, first_a.prompt_id, "两条链各自的一轮");

        // B 记自己的，不影响 A。
        crate::proxy::CcSessionLink::record(b, Some("req_b"), Some("msg_b"));
        let again_a = crate::proxy::CcSessionLink::load(a, false, true);
        assert_eq!(again_a.prev_req.as_deref(), Some("req_a"), "A 还是 A 那条");
        assert_eq!(again_a.prompt_id, first_a.prompt_id, "换号没把 A 这一轮转掉");
    }

    #[test]
    fn session_link_carries_prompt_and_previous_ids() {
        let sid = format!("test-{}", crate::proxy::uuid_v4());
        let new_prompt = || crate::proxy::CcSessionLink::load(key(&sid), true, true);
        let tool_turn = || crate::proxy::CcSessionLink::load(key(&sid), false, true);

        // 首条：有 prompt_id，没有上一条可指。
        let first = new_prompt();
        let pid = first.prompt_id.clone().expect("主线程每条都带 cc_prompt_id");
        assert!(first.prev_req.is_none(), "会话第一条没有 cc_prev_req");
        assert!(first.prev_message_id.is_none(), "首轮 previous_message_id 为 null");

        // 回程记下这一条的两个 id。
        crate::proxy::CcSessionLink::record(key(&sid), Some("req_001"), Some("msg_001"));

        // 工具续轮：prompt_id 沿用，两个 prev 指向上一条。
        let second = tool_turn();
        assert_eq!(second.prompt_id.as_deref(), Some(pid.as_str()), "工具续轮沿用同一轮 id");
        assert_eq!(second.prev_req.as_deref(), Some("req_001"));
        assert_eq!(second.prev_message_id.as_deref(), Some("msg_001"));

        // 新一轮用户输入：换 prompt_id，prev 仍指最近一条。
        crate::proxy::CcSessionLink::record(key(&sid), Some("req_002"), Some("msg_002"));
        let third = new_prompt();
        assert_ne!(third.prompt_id.as_deref(), Some(pid.as_str()), "新输入要换一轮 id");
        assert_eq!(third.prev_req.as_deref(), Some("req_002"));
        assert_eq!(third.prev_message_id.as_deref(), Some("msg_002"));

        // 「猜下一句」那类请求不写 cc_prompt_id，但仍在同一条链上
        // （`cap/2.1.260-2/00063`：有 cc_prev_req、没有 cc_prompt_id）。
        let suggestion = crate::proxy::CcSessionLink::load(key(&sid), false, false);
        assert!(suggestion.prompt_id.is_none(), "不写 cc_prompt_id");
        assert_eq!(suggestion.prev_req.as_deref(), Some("req_002"), "但照样接在链上");

        // 另一个会话是另一条链，不该串味。
        let other = crate::proxy::CcSessionLink::load(
            key(&format!("test-{}", crate::proxy::uuid_v4())),
            true,
            true,
        );
        assert!(other.prev_req.is_none(), "不同会话各走各的链");
    }

    /// 上游只回了 request-id、没解析出 message.id（非流式解析失败、断流……）时，
    /// 已有的 `previous_message_id` 不能被清空——官方那条链是不会中断的。
    #[test]
    fn session_link_record_keeps_what_it_was_not_told() {
        let sid = format!("test-{}", crate::proxy::uuid_v4());
        let _ = crate::proxy::CcSessionLink::load(key(&sid), true, true);
        crate::proxy::CcSessionLink::record(key(&sid), Some("req_001"), Some("msg_001"));
        crate::proxy::CcSessionLink::record(key(&sid), Some("req_002"), None);
        let link = crate::proxy::CcSessionLink::load(key(&sid), false, true);
        assert_eq!(link.prev_req.as_deref(), Some("req_002"), "request-id 更新了");
        assert_eq!(link.prev_message_id.as_deref(), Some("msg_001"), "message.id 保留上一次的");
    }

    /// **真实 CC（API-key 模式）** 的主线程请求也要补上会话关联字段。
    ///
    /// API-key 端那条 billing header 是光秃秃的
    /// `cc_version=…; cc_entrypoint=cli;`——没有 `cch`、没有 `cc_prompt_id`、没有
    /// `cc_prev_req`，整条也没有 `diagnostics`（`cap/2.1.258-api` 六份逐一核过）。luban 拿
    /// OAuth token 转出去之后，上游看到的是「一条 OAuth 主线程请求，一个会话关联字段都
    /// 没有」，而订阅端官方每条都有。
    #[test]
    fn api_key_cc_gets_the_oauth_session_chain() {
        const SID: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
        let body = Bytes::from(format!(
            concat!(
                r#"{{"model":"claude-opus-5","max_tokens":64000,"#,
                r#""messages":[{{"role":"user","content":"hi"}}],"#,
                r#""system":[{{"type":"text","text":"x-anthropic-billing-header: "#,
                r#"cc_version=2.1.260.222; cc_entrypoint=cli;"}},"#,
                r#"{{"type":"text","text":"{}"}},{}],"#,
                r#""tools":[{{"name":"Bash"}}],"#,
                r#""metadata":{{"user_id":"{{\"device_id\":\"{}\",\"session_id\":\"{}\"}}"}}}}"#
            ),
            config::CC_SYSTEM_IDENTITY,
            base_block(),
            // 真 CC 的 device_id 恒为 64 位 hex；不合法的会被 detect 当成非官方客户端送去模拟。
            "832cb7e697190bc475b926c7994ef183a0f8a58e29818f182e11f924e1ea2870",
            SID
        ));
        let parsed_body = parsed(&body);
        let mut headers = crate::proxy::HeaderMap::new();
        headers.insert("x-claude-code-session-id", HeaderValue::from_static(SID));
        // 主线程的 beta 串（有 effort，不是那几套非主线程 profile）。
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219,effort-2025-11-24"),
        );
        // UA 自报 CC + 工具列表含官方名 → 真实 CC 客户端，不走模拟。
        headers.insert(header::USER_AGENT, HeaderValue::from_static(config::CC_USER_AGENT));

        assert!(detect_with(&body, &headers, all_on()).is_none(), "真实 CC 客户端不模拟");

        let (sid, link) = crate::proxy::client_session_link(
            parsed_body.as_ref(),
            &headers,
            None,
            all_on(),
            true,
            &test_cred(),
        )
        .expect("主线程 CC 请求该补会话链");
        assert_eq!(sid, SID, "键是客户端自己的会话 id");

        let out = crate::proxy::rewrite_body(
            &body,
            &test_cred(),
            "fp",
            all_on(),
            None,
            None,
            None,
            false,
            None,
            true,
            true,
            None,
            Some(&link),
            crate::proxy::CcRequestKind::Main,
            None,
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let billing = v["system"][0]["text"].as_str().unwrap();
        assert!(billing.contains("; cch="), "cch 照旧补: {billing}");
        assert!(billing.contains("cc_prompt_id="), "缺的这项要补上: {billing}");
        assert_eq!(
            v["diagnostics"],
            serde_json::json!({"previous_message_id": serde_json::Value::Null}),
            "首轮 diagnostics 字段在、值为 null: {v}"
        );
        // 段序：cch → cc_prev_req → cc_prompt_id（官方序）。
        assert!(billing.find("cch=").unwrap() < billing.find("cc_prompt_id=").unwrap());

        // 客户端自己写了 cc_prompt_id 的不动——它比我们更清楚自己的链。
        let own = Bytes::from(
            String::from_utf8(body.to_vec())
                .unwrap()
                .replace("cc_entrypoint=cli;", "cc_entrypoint=cli; cc_prompt_id=mine;"),
        );
        let out = crate::proxy::rewrite_body(
            &own,
            &test_cred(),
            "fp",
            all_on(),
            None,
            None,
            None,
            false,
            None,
            true,
            true,
            None,
            Some(&link),
            crate::proxy::CcRequestKind::Main,
            None,
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let billing = v["system"][0]["text"].as_str().unwrap();
        assert!(billing.contains("cc_prompt_id=mine;"), "保留客户端自己那个: {billing}");
        assert_eq!(billing.matches("cc_prompt_id=").count(), 1, "别补第二个: {billing}");
    }

    /// 三个关联字段**逐类给**，不能一刀切成主线程：官方六个 profile 在这三项上各不相同。
    ///
    /// 最要紧的是「猜下一句」：它的末条正是一句 `[SUGGESTION MODE: …]` 的 user 消息，
    /// 照 `last_is_new_prompt_body` 判就会换一轮 prompt id，把主线程那条链一起带偏；
    /// 而官方那条（`cap/2.1.260-2/00063`）**根本不写** `cc_prompt_id`。
    #[test]
    fn session_chain_fields_follow_the_request_kind() {
        const SID: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
        let body = |tools: &str, last: &str, billing_extra: &str| {
            Bytes::from(format!(
                concat!(
                    r#"{{"model":"claude-opus-5","max_tokens":64000,"#,
                    r#""messages":[{{"role":"user","content":"hi"}},"#,
                    r#"{{"role":"assistant","content":"y"}},"#,
                    r#"{{"role":"user","content":"{}"}}],"#,
                    r#""system":[{{"type":"text","text":"x-anthropic-billing-header: "#,
                    r#"cc_version=2.1.260.222; cc_entrypoint=cli;{}"}}],"#,
                    r#""tools":{},"#,
                    r#""metadata":{{"user_id":"{{\"session_id\":\"{}\"}}"}}}}"#
                ),
                last, billing_extra, tools, SID
            ))
        };
        let mut headers = crate::proxy::HeaderMap::new();
        headers.insert("x-claude-code-session-id", HeaderValue::from_static(SID));
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219,effort-2025-11-24"),
        );
        let link = |b: &Bytes| {
            let parsed_body = parsed(b);
            crate::proxy::client_session_link(
                parsed_body.as_ref(),
                &headers,
                None,
                all_on(),
                true,
                &test_cred(),
            )
            .map(|(_, l)| l)
        };

        // 主线程：三项全给，新输入换一轮 id。
        let main = link(&body(r#"[{"name":"Bash"}]"#, "next question", "")).expect("主线程要补");
        let first_pid = main.prompt_id.clone().expect("主线程写 cc_prompt_id");
        assert!(main.diagnostics, "主线程写 diagnostics");

        // 「猜下一句」：**不写 cc_prompt_id、不写 diagnostics、也不换轮**。
        let sugg = link(&body(
            r#"[{"name":"Bash"}]"#,
            "[SUGGESTION MODE: Suggest what the user might type next.]",
            "",
        ))
        .expect("它仍在链上——官方那条带 cc_prev_req");
        assert!(sugg.prompt_id.is_none(), "官方 00063 没有 cc_prompt_id");
        assert!(!sugg.diagnostics, "官方 00063 也没有 diagnostics");

        // 换轮没被它带偏：主线程再来一条新输入，拿到的仍是「上一轮之后的下一轮」，
        // 而不是被猜下一句先转过一次。
        let again = link(&body(r#"[{"name":"Bash"}]"#, "another question", "")).unwrap();
        assert_ne!(again.prompt_id.as_ref(), Some(&first_pid), "新输入该换一轮");

        // SDK 子代理：两项都写，但**不换轮**（跟父会话共用同一个 prompt id）。
        let before = link(&body(r#"[{"name":"Bash"}]"#, "tool turn", "")).unwrap();
        let sub =
            link(&body(r#"[{"name":"Bash"}]"#, "brand new user message", " cc_is_subagent=true;"))
                .expect("子代理要补");
        assert!(sub.diagnostics, "官方 00020 带 diagnostics");
        assert_eq!(sub.prompt_id, before.prompt_id, "子代理沿用父会话那一轮，不换");

        // 无工具 helper：写 cc_prompt_id，**不写** diagnostics（官方 00024 没有）。
        let helper = link(&body("[]", "anything", " cc_is_subagent=true;")).expect("helper 要补");
        assert!(helper.prompt_id.is_some(), "官方 00024 带 cc_prompt_id");
        assert!(!helper.diagnostics, "官方 00024 没有 diagnostics");
    }

    /// 官方**本来就非流式**的那两类不能被 `nonstream_as_sse` 改成流式，额度探测也不能被
    /// 补上 `system`。
    ///
    /// 抓包：安全分类（`cap/2.1.260/00019`、`00030`）与额度探测（`2.1.260-2/00004`）整条都
    /// 没有 `stream` 字段；额度探测连 `system` 都没有（四个顶层键 `model → max_tokens →
    /// messages → metadata`）。那个开关的本意是「官方恒为流式，非流式一看就不是 CC」，
    /// 对这两类恰好相反。
    #[test]
    fn official_nonstream_aux_requests_are_left_alone() {
        use crate::proxy::CcRequestKind as K;
        let beta = |s: &str| -> Vec<String> {
            s.split(',').filter(|p| !p.is_empty()).map(str::to_string).collect()
        };

        // 额度探测：没有 system、max_tokens:1。
        let probe: serde_json::Value =
            serde_json::from_slice(&crate::proxy::probe_body("claude-haiku-4-5-20251001")).unwrap();
        let kind = K::of(&probe, &beta("oauth-2025-04-20,interleaved-thinking-2025-05-14"));
        assert_eq!(kind, K::QuotaProbe, "{probe}");
        assert!(kind.keeps_nonstream(), "官方那条没有 stream");
        assert!(!kind.allows_system_prefix(), "官方那条没有 system，别补");
        assert!(!kind.on_session_chain(), "也没有 billing header，不进链");

        // 安全分类：非流式，但有 system。
        let classifier = serde_json::json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "system": [{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.3de; cc_entrypoint=cli;"}],
            "messages": [{"role":"user","content":"check"}],
            "stop_sequences": ["</severity>"],
            "thinking": {"type":"disabled"}});
        let kind =
            K::of(&classifier, &beta("claude-code-20250219,auto-mode-classifier-2026-07-16"));
        assert_eq!(kind, K::Classifier);
        assert!(kind.keeps_nonstream(), "官方那条非流式");
        assert!(kind.allows_system_prefix(), "它有 system，缺 billing 时照补");
        assert!(!kind.on_session_chain(), "三项都不写");

        // 标题生成是**流式**的，不能跟着一起豁免。
        let title = serde_json::json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 32000,
            "stream": true,
            "system": [{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.ced; cc_entrypoint=cli;"}],
            "messages": [{"role":"user","content":"name it"}],
            "tools": []});
        let kind = K::of(&title, &beta("structured-outputs-2025-12-15,advisor-tool-2026-03-01"));
        assert_eq!(kind, K::Title);
        assert!(!kind.keeps_nonstream(), "官方标题生成是 stream:true");

        // 主线程照旧要被改成流式。
        let main = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 64000,
            "system": [{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli;"}],
            "messages": [{"role":"user","content":"hi"}],
            "tools": [{"name":"Bash"}]});
        let kind = K::of(&main, &beta("claude-code-20250219,effort-2025-11-24"));
        assert_eq!(kind, K::Main);
        assert!(!kind.keeps_nonstream());
        assert!(kind.allows_system_prefix());
    }

    /// 额度探测的判据必须**窄**：只看「没有 system + max_tokens:1」的话，任何客户端的
    /// 一 token 探活都会被认成它，跟着被免掉流式化、免掉 `system` 前缀——而它需要那个前缀
    /// 才能用上订阅额度。宁可漏认（退回 helper 走通用路径），也不能错认。
    #[test]
    fn quota_probe_detection_is_narrow() {
        use crate::proxy::CcRequestKind as K;
        let no_beta: Vec<String> = Vec::new();
        // 官方那条（`cap/2.1.258/00004` 与 `cap/2.1.260-2/00004` 逐字节相同）。
        let official: serde_json::Value =
            serde_json::from_slice(&crate::proxy::probe_body("claude-haiku-4-5-20251001")).unwrap();
        assert_eq!(K::of(&official, &no_beta), K::QuotaProbe, "{official}");

        // 每一处偏离都不该再算额度探测。
        let variants: &[(&str, serde_json::Value)] = &[
            (
                "模型不是 haiku",
                serde_json::json!({
                "model": "claude-opus-5", "max_tokens": 1,
                "messages": [{"role":"user","content":"quota"}]}),
            ),
            (
                "正文不是 quota",
                serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 1,
                "messages": [{"role":"user","content":"ping"}]}),
            ),
            (
                "不止一条消息",
                serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 1,
                "messages": [{"role":"user","content":"quota"},
                             {"role":"assistant","content":"ok"}]}),
            ),
            (
                "带工具",
                serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 1, "tools": [],
                "messages": [{"role":"user","content":"quota"}]}),
            ),
            (
                "带 system",
                serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 1, "system": "s",
                "messages": [{"role":"user","content":"quota"}]}),
            ),
            (
                "max_tokens 不是 1",
                serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 2,
                "messages": [{"role":"user","content":"quota"}]}),
            ),
        ];
        for (why, v) in variants {
            assert_ne!(K::of(v, &no_beta), K::QuotaProbe, "{why}: {v}");
        }

        // 单个 text 块的写法也算（官方发的是裸字符串，但这是等价形态）。
        let block = serde_json::json!({
            "model": "claude-haiku-4-5-20251001", "max_tokens": 1,
            "messages": [{"role":"user","content":[{"type":"text","text":"quota"}]}]});
        assert_eq!(K::of(&block, &no_beta), K::QuotaProbe);
    }

    /// 会话链的判据**独立于**「要不要补主线程 beta」——两者在六个 profile 上的分界线
    /// 不在同一处，SDK 子代理正好落在两栏相反的那一格：它的 beta 不该被补，但它确实带着
    /// `cc_prompt_id` 与 `diagnostics`（`cap/2.1.260/00020`）。混用一个判据它就被整个跳过。
    ///
    /// 只有标题生成与安全分类不补——官方这两类的 billing header 里就没有 `cc_prompt_id`。
    #[test]
    fn api_key_cc_session_chain_skips_only_title_and_classifier() {
        const SID: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
        let make = |tools: &str, beta: &'static str| {
            let body = Bytes::from(format!(
                concat!(
                    r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":32000,"#,
                    r#""messages":[{{"role":"user","content":"hi"}}],"#,
                    r#""system":[{{"type":"text","text":"x-anthropic-billing-header: "#,
                    r#"cc_version=2.1.260.ced; cc_entrypoint=cli;"}}],"#,
                    r#""tools":{},"#,
                    r#""metadata":{{"user_id":"{{\"session_id\":\"{}\"}}"}}}}"#
                ),
                tools, SID
            ));
            let parsed_body = parsed(&body);
            let mut h = crate::proxy::HeaderMap::new();
            h.insert("x-claude-code-session-id", HeaderValue::from_static(SID));
            h.insert("anthropic-beta", HeaderValue::from_static(beta));
            crate::proxy::client_session_link(
                parsed_body.as_ref(),
                &h,
                None,
                all_on(),
                true,
                &test_cred(),
            )
            .is_some()
        };
        // 标题生成：有 structured-outputs → 不补。
        assert!(!make("[]", "structured-outputs-2025-12-15,advisor-tool-2026-03-01"));
        // 安全分类：有 auto-mode-classifier → 不补。
        assert!(!make("[]", "claude-code-20250219,auto-mode-classifier-2026-07-16"));
        // **SDK 子代理要补**：它的 beta 集合不该被 `merge_beta` 补主线程那几项，但它自己
        // 带着 `cc_prompt_id` 与 `diagnostics`。两个判据必须分开。
        assert!(make(
            r#"[{"name":"Bash"}]"#,
            "claude-code-20250219,thinking-display-updates-2026-08-18"
        ));
        // 主线程：补。
        assert!(make(r#"[{"name":"Bash"}]"#, "claude-code-20250219,effort-2025-11-24"));

        // 两个判据确实分家了：同一串 beta，一个说「别补主线程项」，一个说「要补会话链」。
        let sdk: Vec<String> = "claude-code-20250219,thinking-display-updates-2026-08-18"
            .split(',')
            .map(str::to_string)
            .collect();
        assert!(crate::proxy::is_official_non_main_beta(&sdk), "beta 不该被补主线程项");
    }
}
