//! 模拟请求（与真实 CC 来访）在会话链条上的位置：`cc_prompt_id` / `cc_prev_req` /
//! `diagnostics.previous_message_id` 三个关联字段，按 `(凭证, 会话)` 分桶维护。

use axum::http::HeaderMap;

use crate::config;
use crate::store;

use super::{
    Simulation, has_beta, incoming_session_id, is_cc_shaped, is_quota_probe_shaped,
    last_user_text_contains, last_user_text_starts_with, request_max_tokens, uuid_v4,
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
    /// 这是本会话的第几次用户输入（从 1 起），写进 2.1.285 起 billing header 的
    /// `cc_prompt_index` / `cc_turn_index`。官方的 `promptIndex` 只在 `turn_origin=human` 的新
    /// 输入上加一，`turnIndex` 每个新 turn 加一（可执行文件里的 `OQt`）；模拟路径的每一轮都是
    /// human，两者恒等。0 即不在会话链上（额度探测、连通性测试），不写。
    pub(super) prompt_index: u32,
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
    /// 见 [`CcSessionLink::prompt_index`]：换一轮 prompt id 就加一。
    prompt_index: u32,
    prev_req: Option<String>,
    prev_message_id: Option<String>,
    /// 这一轮是本地命令起的头（`!` 跑的 shell 命令、`/init` 这类提示词型斜杠命令，见
    /// [`starts_local_turn`]）：官方整轮（含工具续轮）都不写 `cc_prompt_id`，`cc_prompt_index`
    /// 照样加一（`cap/auto-2.1.285-20260930/00191`、`00266`–`00268`）。
    local_turn: bool,
    /// 模拟路径在这个会话里开过的 message thread，见 [`ThreadState`]。同一个会话 id 下可能有
    /// 好几段对话（会话槽位按账号复用，[`super::Simulation::detect`]），各占一条。
    threads: Vec<ThreadState>,
    last_seen: std::time::Instant,
}

/// 一个会话里最多记几条 thread。超了按最久未用淘汰——淘汰掉的那段对话下一轮退回 `create`，
/// 与官方切模型后的形态相同，不会出错。
const MAX_THREADS_PER_SESSION: usize = 8;

/// 模拟路径的一条 **message thread**（`message-threads-2026-08-12`）在上游那边的状态。
///
/// 官方 2.1.285 的主线程（opus / sonnet / haiku，`cap/auto-2.1.285-20260930` 的会话
/// `bcb4d47a`）：会话首条 `thread: {type: create}` 带完整上下文；此后**每一条**——工具续轮与
/// 新的用户输入都是——`thread: {type: continue, previous_message_id}`，`messages` 只放新增的
/// 那几条，`system` 只剩 billing header、不带 `tools`，`previous_message_id` 与
/// `diagnostics.previous_message_id` 同为这条线程上一条回复的 `message.id`（`00033` 起五十余条
/// 一环扣一环）。切 effort / 模型、compact 之后、中断重发、`--continue` 恢复时重新 `create`，
/// 带完整历史，`diagnostics` 指回上一条（`00094`、`00178`、`00243`、`00264`、`00314`）。
///
/// 模拟路径的来访（非 CC 客户端）每轮都发完整历史，要切出增量就得知道上游这条线程里已经有
/// 什么：`msgs` 是上一次发出去的完整上下文逐条的指纹，上游在它后面接了一条回复
/// （`last_message_id`，其中要客户端执行的 `tool_use` 按序是 `tool_use_ids`）。下一轮来访的
/// 出站历史若恰好是「`msgs` + 那条回复 + 新增的 user 消息」，就只发新增部分；对不上一律
/// `create`（[`thread_decision`]）。
#[derive(Debug, Clone)]
struct ThreadState {
    /// 首条消息的指纹：同一个会话 id 下区分不同对话的第一道筛。
    root: u64,
    /// 线程形态指纹（[`ThreadPending::shape`]）：变了即官方会重新 `create` 的那类变化。
    shape: u64,
    msgs: Vec<u64>,
    last_message_id: String,
    tool_use_ids: Vec<String>,
    /// 那条回复的内容指纹（[`ReplyFp`]）。只比 tool_use id 不够：纯文本回复两边 id 都是空的，
    /// 同一会话槽里两段开场相同的对话会互相接上对方的回复；客户端改了正文或工具入参（id 不变）
    /// 也照样接上，改动被静默丢掉。
    reply_fp: ReplyFp,
    /// 那条回复 usage 的总量（input + cache 写 + cache 读 + output）：官方 `<total_tokens>` 倒数
    /// 里的「当前上下文」（可执行文件里的 `xz(messages)`），见 [`TotalTokens`]。
    ctx_tokens: u64,
    /// 这段对话 `<total_tokens>` 倒数的锚点与已用量，见 [`TotalTokens`]。
    budget: TotalTokens,
    seen: std::time::Instant,
}

/// 官方 `<total_tokens>N tokens left</total_tokens>` 提醒（`totalTokensReminder`，缺省
/// `padded-countdown`、预算 1500 万）的倒数状态，逆向自 2.1.285 可执行文件（`zEt` 与 `wnr`）：
///
/// - 每次**普通用户输入**重新锚定：`anchor = 当前上下文`，`used = 0`，这一轮的提醒是 1500 万整；
/// - 其余请求（工具结果之后的续轮）`used = max(上次的 used, 当前上下文 − anchor)`，提醒写
///   `1500 万 − used`——只减不增，上下文变小（切到 haiku 之类）也不回涨。
///
/// 「当前上下文」是这段对话**上一条回复** usage 的总量。`cap/auto-2.1.285-20260930` 逐条核过：
/// `00036` 的 14999796 = 1500 万 −（`00033` 回复 40436 − 锚在 `00033` 时的 40232），`00041`、
/// `00148`、`00154`、`00267`、`00276` 同样对得上。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TotalTokens {
    anchor: u64,
    used: u64,
}

/// 官方 `totalTokensReminder` 的缺省预算（可执行文件里的 `GEt`）。
pub(super) const TOTAL_TOKENS_BUDGET: u64 = 15_000_000;

/// 出站 `messages` 里一条消息的线程指纹，见 [`thread_decision`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ThreadMsg {
    /// 去掉全部 `cache_control` 之后的消息指纹：断点每轮挪到最后一条上，不能让它把前缀判成变了。
    pub(super) fp: u64,
    pub(super) assistant: bool,
    /// assistant 消息里 `type: tool_use` 块的 id，按序。
    pub(super) tool_use_ids: Vec<String>,
    /// assistant 消息的回复指纹（[`ReplyFp`]），与上游那条回复的比对；user 消息是缺省值。
    pub(super) reply: ReplyFp,
}

/// 回复指纹的初值（FNV-1a 64 位偏移基）。
pub(crate) const REPLY_TEXT_FP_INIT: u64 = 0xcbf2_9ce4_8422_2325;

/// 把一段正文续进回复正文指纹（FNV-1a）：按字节流累加，分几段喂与整段一次喂结果相同——
/// 回程流式的 `text_delta` 一段段来，来访那条 assistant 是整段的，两边要算出同一个数。
pub(crate) fn reply_text_fp(h: u64, text: &str) -> u64 {
    fnv_bytes(h, text.as_bytes())
}

fn fnv_bytes(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 把一段 JSON 按规范形态续进指纹：对象键排序后逐个喂，每种值带类型标记。来访常把工具入参
/// 重新序列化一遍，键序、空白都不可信，只认内容。
fn fnv_json(mut h: u64, v: &serde_json::Value) -> u64 {
    use serde_json::Value;
    match v {
        Value::Null => fnv_bytes(h, b"n"),
        Value::Bool(b) => fnv_bytes(h, if *b { b"t" } else { b"f" }),
        Value::Number(n) => {
            h = fnv_bytes(h, b"#");
            fnv_bytes(h, n.to_string().as_bytes())
        }
        Value::String(s) => {
            h = fnv_bytes(h, b"\"");
            h = fnv_bytes(h, &(s.len() as u64).to_le_bytes());
            fnv_bytes(h, s.as_bytes())
        }
        Value::Array(a) => {
            h = fnv_bytes(h, b"[");
            h = fnv_bytes(h, &(a.len() as u64).to_le_bytes());
            a.iter().fold(h, fnv_json)
        }
        Value::Object(o) => {
            h = fnv_bytes(h, b"{");
            h = fnv_bytes(h, &(o.len() as u64).to_le_bytes());
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            for k in keys {
                h = fnv_bytes(h, &(k.len() as u64).to_le_bytes());
                h = fnv_bytes(h, k.as_bytes());
                h = fnv_json(h, &o[k]);
            }
            h
        }
    }
}

/// 一条 assistant 回复的内容指纹。模拟路径拿它核对下一轮来访带回的那条 assistant 是不是上游
/// 线程里那条回复：客户端把回复 A 改成 B（或改了工具入参、id 不变）再发下一条，`continue` 会让
/// 上游沿用存档里的 A、B 被静默丢掉，所以对不上一律 `create`（[`find_continuable`]）。
///
/// 回程嗅探器按 SSE 块攒（`UsageSniffer::reply_fp`），来访那条按 content 块算
/// （`body::thread_msg_of`），两边按块序、同一算法喂：
///
/// - `content`：`text` 块的正文与 `citations`（引用 URL、引文都算），客户端 `tool_use` 的 id、
///   名字（出站那份已是混淆名，与上游回的一致）、入参（[`fnv_json`] 规范形态），以及其余各种块
///   （`server_tool_use`、`web_search_tool_result` 之类服务端工具的调用与结果）去掉 `cache_control`
///   后的整块规范形态。空 `text` 块（也没有引用）不算——改写那一步会把它剥掉；`fallback` 块两边都不算。
/// - `unverifiable`：回程出现了认不出的增量类型，拼不出这条回复的原貌，一律不接、退回 `create`。
/// - `thinking`：`thinking` 正文与 `redacted_thinking` 的 `data`，按块序。来访一块都没带时是
///   `None`、不比——客户端丢掉 thinking 很常见，上游线程里那份本来就在；带了就得逐块对上。
///   回程那份总是 `Some`（没有 thinking 块即初值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReplyFp {
    content: u64,
    thinking: Option<u64>,
    unverifiable: bool,
}

impl Default for ReplyFp {
    fn default() -> Self {
        Self { content: REPLY_TEXT_FP_INIT, thinking: None, unverifiable: false }
    }
}

impl ReplyFp {
    /// 回程那份：thinking 一侧从初值起算，没有 thinking 块也是 `Some`。
    pub(crate) fn upstream() -> Self {
        Self { thinking: Some(REPLY_TEXT_FP_INIT), ..Self::default() }
    }

    /// 一个 `text` 块（正文非空或带引用）；`block_fp` 是它全文的 [`reply_text_fp`]（从初值起算），
    /// `citations` 是它的引用，按序。
    pub(crate) fn text(&mut self, block_fp: u64, citations: &[serde_json::Value]) {
        let mut h = fnv_bytes(fnv_bytes(self.content, b"T"), &block_fp.to_le_bytes());
        h = fnv_bytes(h, &(citations.len() as u64).to_le_bytes());
        self.content = citations.iter().fold(h, fnv_json);
    }

    /// 其余各种块（服务端工具的调用与结果等）：去掉 `cache_control` 后整块按规范形态喂。
    pub(crate) fn block(&mut self, b: &serde_json::Value) {
        let h = fnv_bytes(self.content, b"B");
        self.content = match b.as_object() {
            Some(o) if o.contains_key("cache_control") => {
                let mut o = o.clone();
                o.shift_remove("cache_control");
                fnv_json(h, &serde_json::Value::Object(o))
            }
            _ => fnv_json(h, b),
        };
    }

    /// 回程拼不出原貌（认不出的增量类型）：这条回复不给接。
    pub(crate) fn mark_unverifiable(&mut self) {
        self.unverifiable = true;
    }

    /// 一个客户端 `tool_use` 块。
    pub(crate) fn tool_use(&mut self, id: &str, name: &str, input: &serde_json::Value) {
        let mut h = fnv_bytes(self.content, b"U");
        h = fnv_bytes(h, &(id.len() as u64).to_le_bytes());
        h = fnv_bytes(h, id.as_bytes());
        h = fnv_bytes(h, &(name.len() as u64).to_le_bytes());
        h = fnv_bytes(h, name.as_bytes());
        self.content = fnv_json(h, input);
    }

    /// 一个 `thinking`（`redacted == false`，`block_fp` 是正文的 [`reply_text_fp`]）或
    /// `redacted_thinking`（`block_fp` 是 `data` 的）块。
    pub(crate) fn thinking(&mut self, redacted: bool, block_fp: u64) {
        let h = self.thinking.unwrap_or(REPLY_TEXT_FP_INIT);
        let h = fnv_bytes(h, if redacted { b"R" } else { b"K" });
        self.thinking = Some(fnv_bytes(h, &block_fp.to_le_bytes()));
    }

    /// 测试里拿来访那条的算法冒充回程那份：thinking 一侧补成回程的样子（没带即初值）。
    #[cfg(test)]
    pub(crate) fn as_upstream(self) -> Self {
        Self { thinking: self.thinking.or(Some(REPLY_TEXT_FP_INIT)), ..self }
    }

    /// 来访那条（`self`）是不是上游这条回复（`upstream`），规则见 [`ReplyFp`]。
    fn matches(&self, upstream: &ReplyFp) -> bool {
        !upstream.unverifiable
            && self.content == upstream.content
            && self.thinking.is_none_or(|t| upstream.thinking == Some(t))
    }
}

/// 这一轮线程怎么写，见 [`thread_decision`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ThreadDecision {
    /// `thread: {type: create}`，完整上下文。
    Create,
    /// `thread: {type: continue, previous_message_id}`，`messages` 只留下标 `from` 起的那几条。
    Continue { from: usize, previous_message_id: String },
}

/// 一条已经按线程改写、等上游回话的请求：回程拿到 `message.id` 才提交成 [`ThreadState`]
/// （[`CcSessionLink::record_thread`]），失败就把它接的那条线程作废
/// （[`CcSessionLink::drop_thread`]）。挂在 [`super::Simulation`] 上，由 `ReqLog` 取走。
#[derive(Debug, Clone)]
pub(crate) struct ThreadPending {
    cred_id: i64,
    session_id: String,
    root: u64,
    /// 会进上游线程、且官方一变就重新 `create` 的那几样的指纹：模型、system 正文（billing header
    /// 那块除外）、`tools`、`thinking`、`output_config`（effort）。`continue` 不发 system 与 tools，
    /// 它们一变，上游线程里的还是旧的，只能重开。
    shape: u64,
    /// 这一轮**完整**上下文的逐条指纹（发 `continue` 时也是完整的那份）。
    msgs: Vec<u64>,
    /// 这一轮接的是哪条回复（`continue` 才有）；失败时按它作废那条线程。
    continued_from: Option<String>,
    /// 这一轮算完之后的 `<total_tokens>` 倒数状态，提交时存进 [`ThreadState::budget`]。
    budget: TotalTokens,
}

impl ThreadPending {
    /// 这一轮发的是 `continue`（接着某条回复）。它失败时转发循环当场改发 `create`
    /// （[`super::upstream::retry_thread_as_create`]）。
    pub(super) fn is_continue(&self) -> bool {
        self.continued_from.is_some()
    }

    /// 本来接得上、调用方另有理由改发 `create` 时（来访指定了 `tool_choice`）：不再算接了哪条。
    pub(super) fn into_create(self) -> Self {
        Self { continued_from: None, ..self }
    }
}

/// 在 `threads` 里找能接上这一轮的那条：同一个 `root` 与 `shape`，它的 `msgs` 是这一轮出站
/// 历史的前缀，紧跟着的正好是它那条回复（assistant，`tool_use` id 逐个对上），再往后至少一条
/// 新消息、且新增部分里没有 assistant（官方续轮只发 user / system）。几条都能接时取最长的——
/// 客户端从中间某轮分叉重来的，短的那条是分叉前的旧线。
fn find_continuable<'a>(
    threads: &'a [ThreadState],
    root: u64,
    shape: u64,
    msgs: &[ThreadMsg],
) -> Option<&'a ThreadState> {
    threads
        .iter()
        .filter(|t| t.root == root && t.shape == shape)
        .filter(|t| {
            let n = t.msgs.len();
            let Some(reply) = msgs.get(n) else { return false };
            n + 1 < msgs.len()
                && msgs[..n].iter().map(|m| m.fp).eq(t.msgs.iter().copied())
                && reply.assistant
                && reply.tool_use_ids == t.tool_use_ids
                && reply.reply.matches(&t.reply_fp)
                && msgs[n + 1..].iter().all(|m| !m.assistant)
        })
        .max_by_key(|t| t.msgs.len())
}

/// 这一轮历史所在的那段对话最近一条线程状态：同一个 `root`，`msgs` 是这一轮历史的前缀、后面
/// 跟着它那条回复。不看形态与 tool_use id——只为取「上一条回复的上下文量」与倒数锚点，换了
/// 模型、改了工具照样是同一段对话（官方切模型后倒数也不重置，`cap/auto-2.1.285-20260930/00244`）。
fn find_lineage<'a>(
    threads: &'a [ThreadState],
    root: u64,
    msgs: &[ThreadMsg],
) -> Option<&'a ThreadState> {
    threads
        .iter()
        .filter(|t| t.root == root)
        .filter(|t| {
            let n = t.msgs.len();
            msgs.get(n).is_some_and(|m| m.assistant)
                && msgs[..n].iter().map(|m| m.fp).eq(t.msgs.iter().copied())
        })
        .max_by_key(|t| t.msgs.len())
}

/// 这一轮的 `<total_tokens>` 倒数，规则见 [`TotalTokens`]。`lineage` 是 [`find_lineage`] 找到的
/// 上一条；找不到（对话第一轮、状态已过期）时当前上下文按 0 算，与官方新会话首轮一致。
fn total_tokens_for(lineage: Option<&ThreadState>, regular_prompt: bool) -> (TotalTokens, u64) {
    let ctx = lineage.map_or(0, |t| t.ctx_tokens);
    let budget = if regular_prompt {
        TotalTokens { anchor: ctx, used: 0 }
    } else {
        let prev = lineage.map(|t| t.budget).unwrap_or_default();
        TotalTokens { anchor: prev.anchor, used: prev.used.max(ctx.saturating_sub(prev.anchor)) }
    };
    (budget, TOTAL_TOKENS_BUDGET.saturating_sub(budget.used))
}

/// 决定这一轮发 `create` 还是 `continue`，并把「等回程提交」的那份交出去。
///
/// 会话表里还没有这个会话（[`CcSessionLink::load`] 没跑过，只在测试里出现）时照样 `create`，
/// 回程提交时也就找不到条目、什么都不记。
///
/// 第三个返回值是这一轮 `<total_tokens>` 提醒该写的数（[`TotalTokens`]）；`regular_prompt` 即末条
/// 是一次新的用户输入（而非工具结果）。
pub(super) fn thread_decision(
    key: CcSessionKey<'_>,
    shape: u64,
    msgs: &[ThreadMsg],
    regular_prompt: bool,
) -> (ThreadDecision, ThreadPending, u64) {
    let root = msgs.first().map_or(0, |m| m.fp);
    let (found, (budget, left)) = {
        let map = CC_SESSIONS.lock();
        let threads = map.get(&key.owned()).map_or(&[][..], |e| &e.threads[..]);
        (
            find_continuable(threads, root, shape, msgs)
                .map(|t| (t.msgs.len() + 1, t.last_message_id.clone())),
            total_tokens_for(find_lineage(threads, root, msgs), regular_prompt),
        )
    };
    let pending = ThreadPending {
        cred_id: key.cred_id,
        session_id: key.session_id.to_string(),
        root,
        shape,
        msgs: msgs.iter().map(|m| m.fp).collect(),
        continued_from: found.as_ref().map(|(_, id)| id.clone()),
        budget,
    };
    let decision = match found {
        Some((from, previous_message_id)) => ThreadDecision::Continue { from, previous_message_id },
        None => ThreadDecision::Create,
    };
    (decision, pending, left)
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
        Self::load_turn(key, new_prompt, false, wants_prompt_id, None)
    }

    /// [`Self::load`] 外加 `local_turn`：`new_prompt` 为真时，这一轮是不是本地命令起的头
    /// （[`starts_local_turn`]）。是的话这一轮直到下一次新输入都不写 `cc_prompt_id`。
    ///
    /// `client_prompt_id` 是来访自己在 `x-claude-code-prompt-id` 头里报的这一轮 id：有就以它为准
    /// 并记进会话（同一轮后面没带头的续轮也沿用它）。2.1.285 起这个头与 billing header 里的
    /// `cc_prompt_id` 同值同现（[`config::CC_HEADER_ORDER`]），luban 补 billing header 时另造一个
    /// 就是一条请求里两个不同的 prompt id。
    pub(super) fn load_turn(
        key: CcSessionKey<'_>,
        new_prompt: bool,
        local_turn: bool,
        wants_prompt_id: bool,
        client_prompt_id: Option<&str>,
    ) -> Self {
        let now = std::time::Instant::now();
        let mut map = CC_SESSIONS.lock();
        map.retain(|_, e| now.duration_since(e.last_seen) < CC_SESSION_IDLE);
        let key = key.owned();
        let known = map.contains_key(&key);
        let entry = map.entry(key).or_insert_with(|| CcSessionEntry {
            prompt_id: uuid_v4(),
            prompt_index: 1,
            prev_req: None,
            prev_message_id: None,
            local_turn: false,
            threads: Vec::new(),
            last_seen: now,
        });
        // 新建的那份 id 就是这一轮的，别再换一次。
        if known && new_prompt {
            entry.prompt_id = uuid_v4();
            entry.prompt_index = entry.prompt_index.saturating_add(1);
        }
        if new_prompt || !known {
            entry.local_turn = new_prompt && local_turn;
        }
        if let Some(id) = client_prompt_id {
            entry.prompt_id = id.to_string();
        }
        entry.last_seen = now;
        Self {
            prompt_id: (wants_prompt_id && !entry.local_turn).then(|| entry.prompt_id.clone()),
            prev_req: entry.prev_req.clone(),
            prev_message_id: entry.prev_message_id.clone(),
            first_seen: !known,
            // 缺省写：模拟路径只造主线程 profile，它是要 diagnostics 的。真实 CC 那条路由
            // [`CcSessionLink::with_diagnostics`] 按 [`CcRequestKind`] 覆写。
            diagnostics: true,
            prompt_index: entry.prompt_index,
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

    /// 一条按线程改写的请求**完整成功**（有 `message.id`）后，把它记成这段对话最新的线程状态：
    /// 下一轮据此判能不能 `continue`。它接替的旧状态（同一个 `root`、`msgs` 是它的前缀）一并
    /// 删掉——那些是这条线程更早的样子，或客户端分叉前的旧线，都接不上了。
    ///
    /// `reply_fp` 是这条回复的内容指纹（[`ReplyFp`]），`ctx_tokens` 是它 usage 的
    /// 总量——下一轮 `<total_tokens>` 倒数的「当前上下文」（[`TotalTokens`]）。
    pub(super) fn record_thread(
        pending: &ThreadPending,
        message_id: &str,
        tool_use_ids: Vec<String>,
        reply_fp: ReplyFp,
        ctx_tokens: u64,
    ) {
        let now = std::time::Instant::now();
        let mut map = CC_SESSIONS.lock();
        let Some(entry) = map.get_mut(&(pending.cred_id, pending.session_id.clone())) else {
            return;
        };
        entry.threads.retain(|t| {
            !(t.root == pending.root
                && t.msgs.len() <= pending.msgs.len()
                && pending.msgs[..t.msgs.len()] == t.msgs[..])
        });
        while entry.threads.len() >= MAX_THREADS_PER_SESSION {
            let Some(oldest) =
                entry.threads.iter().enumerate().min_by_key(|(_, t)| t.seen).map(|(i, _)| i)
            else {
                break;
            };
            entry.threads.remove(oldest);
        }
        entry.threads.push(ThreadState {
            root: pending.root,
            shape: pending.shape,
            msgs: pending.msgs.clone(),
            last_message_id: message_id.to_string(),
            tool_use_ids,
            reply_fp,
            ctx_tokens,
            budget: pending.budget,
            seen: now,
        });
        entry.last_seen = now;
    }

    /// 一条 `continue` 失败（上游报错、流断在半截）：它接的那条线程作废，下一轮退回 `create`。
    /// 线程过期、`previous_message_id` 对不上这类错误，接着 `continue` 只会一直错下去。
    /// `create` 失败不用管——它本来就没接任何线程。
    pub(super) fn drop_thread(pending: &ThreadPending) {
        let Some(from) = &pending.continued_from else { return };
        let mut map = CC_SESSIONS.lock();
        if let Some(entry) = map.get_mut(&(pending.cred_id, pending.session_id.clone())) {
            entry.threads.retain(|t| &t.last_message_id != from);
        }
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
    /// 子代理那条无工具的辅助调用（billing header 带 `cc_is_subagent=true`）。
    Helper,
    /// 主线程另发的一次性辅助调用：WebSearch 子调用（`tools` 只有 `web_search`）、主线程直接调
    /// WebFetch 后的页面处理（无工具、billing header 不带子代理标记）。2.1.285 这两种的 billing
    /// header 只有 `cc_version` / `cc_entrypoint` / `cch`、也没有 `diagnostics`
    /// （`cap/auto-2.1.285-20260930/00056`、`00064`），三项会话链字段一项不写；当成主线程还会
    /// 按末条那句新的用户消息换一轮 `cc_prompt_id`，把后面整条主线程链接歪。
    Auxiliary,
    /// 主线程分叉：`/compact`（末条用户消息以「CRITICAL: Respond with TEXT ONLY」开头）、`/btw`
    /// 插问（用户消息里有「This is a side question from the user」那段提醒）与离开回来时的回顾
    /// （「The user stepped away and is coming back.」，`cap/2.1.285/00097`、`00158`）。带完整
    /// system 与工具，只写 `cc_prev_req` 与 `diagnostics`、不写 `cc_prompt_id`（`00175`、`00238`），
    /// 也不换轮。
    Fork,
    /// `/model` 选完模型后那条「Hi」预热：`max_tokens:1`、有 `system`（billing + 身份句）、没有
    /// `tools` 也没有 `stream`（`00230`、`00239`）。不进会话链，beta 只补 `oauth`。
    Prewarm,
    /// `count_tokens`（[`super::simulation::is_official_count_tokens`]）：不计费，不进会话链，beta
    /// 只补 `oauth`。
    CountTokens,
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
        if super::simulation::is_official_count_tokens(v, beta) {
            return Self::CountTokens;
        }
        let no_tools = super::simulation::field_is_empty(v.get("tools"));
        if request_max_tokens(Some(v)) == Some(1) && v.get("system").is_some() && no_tools {
            return Self::Prewarm;
        }
        // 标题生成（`structured-outputs`）与安全分类（`auto-mode-classifier`）各有独有 beta。
        if has_beta(beta, config::CC_BETA_STRUCTURED_OUTPUTS) {
            return Self::Title;
        }
        if has_beta(beta, config::CC_BETA_AUTO_MODE_CLASSIFIER) {
            return Self::Classifier;
        }
        // 子代理：billing header 里那个 `cc_is_subagent=true`。
        let subagent = v
            .get("system")
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.contains("cc_is_subagent=true"));
        // message-threads 续轮（[`super::is_official_thread_continuation`]）不带 `tools` 键，得在
        // 「无工具 = helper」之前认：`cap/2.1.285` 主线程续轮（`00115`、`00121`）照带 `cc_prev_req`
        // / `cc_prompt_id` / `diagnostics`，子代理续轮（`00127` 等五条）的 beta 是子代理那串。
        // 判成 helper 的话，前者的会话链字段补不全，后者会被 merge_beta 当主线程补项。
        if super::simulation::is_official_thread_continuation(v, beta) {
            return if subagent { Self::Subagent } else { Self::Main };
        }
        if no_tools {
            return if subagent { Self::Helper } else { Self::Auxiliary };
        }
        if subagent {
            return Self::Subagent;
        }
        if super::simulation::is_official_web_search_request(v) {
            return Self::Auxiliary;
        }
        if last_user_text_starts_with(v, "[SUGGESTION MODE:") {
            return Self::Suggestion;
        }
        if last_user_text_starts_with(v, "CRITICAL: Respond with TEXT ONLY")
            || last_user_text_starts_with(v, "The user stepped away and is coming back.")
            || last_user_text_contains(v, "This is a side question from the user")
        {
            return Self::Fork;
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
        matches!(self, Self::Main | Self::Subagent | Self::Suggestion | Self::Fork)
    }

    /// 「猜下一句」2.1.280 起也写 `diagnostics`（`cap/2.1.280`、`cap/auto-2.1.285-20260930/00037`；
    /// 2.1.277 及以前不写），按来访自报的版本给，读不出版本按老的不写。
    fn wants_diagnostics(self, version: Option<(u64, u64, u64)>) -> bool {
        match self {
            Self::Main | Self::Subagent | Self::Fork => true,
            Self::Suggestion => version.is_some_and(|v| v >= (2, 1, 280)),
            _ => false,
        }
    }

    /// 这一类要不要进会话链。三项全不写的（标题、安全分类、额度探测）整条跳过。
    pub(super) fn on_session_chain(self) -> bool {
        self.wants_prompt_id() || self.wants_prev_req() || self.wants_diagnostics(None)
    }

    /// 这一类官方**本来就是非流式**，`nonstream_as_sse` 不能把它改成 `stream:true`。
    ///
    /// 安全分类（`cap/2.1.260/00019`、`00030`）与额度探测（`cap/2.1.260-2/00004`）整条都
    /// 没有 `stream` 字段。那个开关的本意是「官方恒为流式，非流式请求一看就不是 CC」——
    /// 对这两类恰好相反：把它们改成流式才是官方不产生的形态。其余（含标题生成）官方确实
    /// 是 `stream:true`，照改。
    pub(super) fn keeps_nonstream(self) -> bool {
        matches!(self, Self::Classifier | Self::QuotaProbe | Self::Prewarm)
    }

    /// `anthropic-beta` 只补 `oauth`、别的一项不动（[`super::merge_beta_for`]）：SDK 子代理、
    /// `/model` 预热（`00230` 官方那串没有 `advanced-tool-use` 与 `extended-cache-ttl`）与
    /// `count_tokens`（`00104` 等只有五项）。其余非主线程 profile 靠 beta 串自己认得出来，见
    /// [`super::is_official_non_main_beta`]。
    pub(super) fn beta_only_oauth(self) -> bool {
        matches!(self, Self::Subagent | Self::Prewarm | Self::CountTokens)
    }

    /// 这一类能不能补 `system` 前缀（billing header + 身份句）。
    ///
    /// 额度探测**没有 `system`**，连 billing header 都没有。给它补一份，就把一条
    /// `max_tokens:1` 的探测改成了「带身份声明的请求」——官方从不产生。
    pub(super) fn allows_system_prefix(self) -> bool {
        !matches!(self, Self::QuotaProbe | Self::CountTokens)
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
    let client_prompt_id = headers
        .get("x-claude-code-prompt-id")
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|h| super::simulation::looks_like_uuid(h));
    let mut link = CcSessionLink::load_turn(
        CcSessionKey { cred_id: cred.id, session_id: &session_id },
        new_prompt,
        starts_local_turn(v),
        kind.wants_prompt_id(),
        client_prompt_id,
    );
    if !kind.wants_prev_req() {
        link.prev_req = None;
    }
    let version = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .and_then(super::body::trusted_cc_version);
    Some((session_id, link.with_diagnostics(kind.wants_diagnostics(version))))
}

/// 末条用户消息是不是本地命令起的一轮：`!` 跑的 shell 命令（`<bash-input>`）或提示词型斜杠
/// 命令（`<command-message>`，如 `/init`）。`/model` 这类纯本地命令的回显（`<local-command-caveat>`
/// 打头）官方照写 `cc_prompt_id`，不在此列。
fn starts_local_turn(v: &serde_json::Value) -> bool {
    last_user_text_starts_with(v, "<bash-input>")
        || last_user_text_starts_with(v, "<command-message>")
}

#[cfg(test)]
mod tests {
    use crate::proxy::test_support::{all_on, base_block, detect_with, parsed, test_cred};
    use crate::proxy::{Bytes, HeaderValue, config, header};

    /// `cc_prompt_index` 的计数（[`super::CcSessionLink::prompt_index`]）：会话第一条是 1，
    /// 工具续轮沿用，新输入加一；换一张凭证就是另一条链，从 1 起。
    #[test]
    fn prompt_index_counts_new_prompts_per_credential_session() {
        use super::{CcSessionKey, CcSessionLink};
        let sid = crate::proxy::uuid_v4();
        let key = CcSessionKey { cred_id: 41, session_id: &sid };
        assert_eq!(CcSessionLink::load(key, true, true).prompt_index, 1, "会话第一条");
        assert_eq!(CcSessionLink::load(key, false, true).prompt_index, 1, "工具续轮沿用");
        assert_eq!(CcSessionLink::load(key, true, true).prompt_index, 2, "新输入加一");
        let other = CcSessionKey { cred_id: 42, session_id: &sid };
        assert_eq!(CcSessionLink::load(other, true, true).prompt_index, 1, "另一张凭证另起");
        assert_eq!(CcSessionLink::default().prompt_index, 0, "不在链上的不写");
    }

    /// 来访头上的 `x-claude-code-prompt-id` 就是这一轮的 prompt id：补出来的 `cc_prompt_id` 沿用它，
    /// 同一轮后面没带头的续轮也沿用；下一次新输入没带头就另起一个。
    #[test]
    fn client_prompt_id_header_is_adopted() {
        use super::{CcSessionKey, CcSessionLink};
        let sid = crate::proxy::uuid_v4();
        let key = CcSessionKey { cred_id: 43, session_id: &sid };
        let pid = "6f5a49b5-5795-46a3-815b-05b2b4f52d2e";
        let first = CcSessionLink::load_turn(key, true, false, true, Some(pid));
        assert_eq!(first.prompt_id.as_deref(), Some(pid));
        let cont = CcSessionLink::load_turn(key, false, false, true, None);
        assert_eq!(cont.prompt_id.as_deref(), Some(pid), "续轮沿用这一轮的");
        let next = CcSessionLink::load_turn(key, true, false, true, None);
        assert!(next.prompt_id.is_some_and(|p| p != pid), "新输入换一个");
    }

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

        let out = crate::proxy::rewrite_body_out(
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
        )
        .0;
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
        let out = crate::proxy::rewrite_body_out(
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
        )
        .0;
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let billing = v["system"][0]["text"].as_str().unwrap();
        assert!(billing.contains("cc_prompt_id=mine;"), "保留客户端自己那个: {billing}");
        assert_eq!(billing.matches("cc_prompt_id=").count(), 1, "别补第二个: {billing}");

        // 来访自报 2.1.285：主线程在 `cc_prompt_id` 之后再补 `cc_turn_origin` 与会话里的第几轮
        // （`cap/2.1.285/00113`）；子代理只有 `cc_prompt_id`（`00120`）。版本读不出（上面几次）
        // 两样都不补。
        let rewrite = |kind| {
            let out = crate::proxy::rewrite_body_out(
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
                Some("2.1.285"),
                Some(&link),
                kind,
                None,
            )
            .0;
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            v["system"][0]["text"].as_str().unwrap().to_string()
        };
        let main = rewrite(crate::proxy::CcRequestKind::Main);
        let n = link.prompt_index;
        assert!(n >= 1);
        assert!(
            main.ends_with(&format!(
                "; cc_turn_origin=human; cc_prompt_index={n}; cc_turn_index={n};"
            )),
            "{main}"
        );
        let sub = rewrite(crate::proxy::CcRequestKind::Subagent);
        assert!(sub.contains("cc_prompt_id=") && !sub.contains("cc_turn_origin"), "{sub}");
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

    /// **各类请求写哪几项会话链字段，与官方抓包逐条对**（`cap/2.1.285`、
    /// `cap/auto-2.1.285-20260930` 全部带 billing header 的 `/v1/messages`）：`cc_prompt_id` 写不写、
    /// `diagnostics` 写不写两边必须一致；`cc_prev_req` 官方写了的，这一类就得是要写的（会话第一条
    /// 官方也不写，反过来不能要求）。分错一类——WebSearch 子调用当主线程、`/compact` 当主线程——
    /// 就会在这里多出或少掉一项。抓包目录不在就跳过。
    #[test]
    fn link_fields_follow_official_captures() {
        use super::CcRequestKind;
        let mut seen = 0;
        let mut bad = Vec::new();
        for sub in ["2.1.285", "auto-2.1.285-20260930"] {
            let dir = format!("{}/cap/{sub}", env!("CARGO_MANIFEST_DIR"));
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let mut files: Vec<_> = entries
                .filter_map(|e| Some(e.ok()?.path()))
                .filter(|p| p.to_str().is_some_and(|f| f.ends_with(".req.raw")))
                .collect();
            files.sort();
            for path in files {
                let raw = std::fs::read(&path).unwrap();
                let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
                let head = std::str::from_utf8(&raw[..sep]).unwrap();
                if !head.lines().next().unwrap().contains("/v1/messages") {
                    continue;
                }
                let v: serde_json::Value = serde_json::from_slice(&raw[sep + 4..]).unwrap();
                let Some(billing) = v
                    .get("system")
                    .and_then(|s| s.as_array())
                    .and_then(|s| s.first())
                    .and_then(|b| b.get("text"))
                    .and_then(|t| t.as_str())
                    .filter(|t| t.starts_with("x-anthropic-billing-header:"))
                else {
                    continue;
                };
                seen += 1;
                let beta: Vec<String> = head
                    .lines()
                    .find_map(|l| l.strip_prefix("anthropic-beta: "))
                    .unwrap_or_default()
                    .split(',')
                    .map(str::to_string)
                    .collect();
                let kind = CcRequestKind::of(&v, &beta);
                let name = format!("{sub}/{}", &path.file_name().unwrap().to_str().unwrap()[..5]);
                let has_prompt = billing.contains("cc_prompt_id=");
                let has_prev = billing.contains("cc_prev_req=");
                let has_diag = v.get("diagnostics").is_some();
                // 按抓包顺序过一遍会话链：本地命令起头的那一轮整轮不写，只看分类判不出来。
                let sid = head
                    .lines()
                    .find_map(|l| l.strip_prefix("X-Claude-Code-Session-Id: "))
                    .unwrap_or_default()
                    .to_string();
                let key = super::CcSessionKey { cred_id: -4285, session_id: &sid };
                let header_pid = head
                    .lines()
                    .find_map(|l| l.strip_prefix("x-claude-code-prompt-id: "))
                    .map(str::trim);
                let link = super::CcSessionLink::load_turn(
                    key,
                    kind.rotates_prompt() && crate::telemetry::last_is_new_prompt_body(&v),
                    super::starts_local_turn(&v),
                    kind.wants_prompt_id(),
                    header_pid,
                );
                if link.prompt_id.is_some() != has_prompt {
                    bad.push(format!("{name} {kind:?}: cc_prompt_id 官方 {has_prompt}"));
                }
                // 来访头上报了 prompt id 的，补出来的 `cc_prompt_id` 与它逐字相同——官方两处本来就同值。
                let billed = billing
                    .split(';')
                    .find_map(|f| f.trim().strip_prefix("cc_prompt_id="))
                    .map(str::trim);
                if header_pid.is_some() && billed.is_some() && link.prompt_id.as_deref() != billed {
                    bad.push(format!(
                        "{name} {kind:?}: cc_prompt_id 与 x-claude-code-prompt-id 不同"
                    ));
                }
                if has_prev && !kind.wants_prev_req() {
                    bad.push(format!("{name} {kind:?}: 官方写了 cc_prev_req"));
                }
                if kind.wants_diagnostics(Some((2, 1, 285))) != has_diag {
                    bad.push(format!("{name} {kind:?}: diagnostics 官方 {has_diag}"));
                }
            }
        }
        if seen == 0 {
            eprintln!("skipped: captures not present");
            return;
        }
        assert!(bad.is_empty(), "{} 条：\n{}", bad.len(), bad.join("\n"));
    }
}
