//! 读写文件那一组事件要的会话账本：已知的文件内容、大小，以及官方「文件历史」的跟踪与快照。
//!
//! 官方每读一个文件报 `tengu_file_operation{read}`（`contentHash` 是**原始文件内容**的 sha256，超过
//! 100KB 不带），改文件报 `file_changed`（增删行数）、`file_operation{edit|write}`、`file_activity`，
//! Edit 另有 `edit_string_lengths`。`filePathHash` 是绝对路径 sha256 的前 16 位（可执行文件里的 `OE` /
//! `tqo`，`cap/auto-2.1.305-20261021-full` 测试项目的 README.md / calc.py 逐个核过）。代理看不到磁盘，
//! 原文从 Read 的工具结果里还原（去掉每行的 `行号\t`，[`read_content`]），改动按 Edit / Write 的入参推。
//!
//! 文件历史（`zwe` 那台状态机）：
//! - **跟踪**：一个文件在会话里**头一次**被改之前跟踪一次——原本存在就先备份（`backup_file_created`，
//!   版本 1、大小是改之前的）再报 `track_edit_success{isNewFile:false}`；新建的只报
//!   `track_edit_success{isNewFile:true}`。之后的快照都带着它，再改不重复跟踪。
//! - **快照**：每次用户提交，已跟踪的文件里自上次备份后又被改过的各升一版备份（大小是当前的），再报
//!   `snapshot_success{trackedFilesCount, snapshotCount}`。A0 里 v2 247、v2 611、v2 / v3 / v3 全对得上。

use std::collections::HashMap;
use std::sync::LazyLock;

use sha2::{Digest, Sha256};

/// 官方 `contentHash` 只算不超过这么长的内容（`oqo`，JS 字符数）。
const CONTENT_HASH_MAX: usize = 102_400;

/// 账本最多记多少个文件：超了先忘掉最久没碰的。
const MAX_FILES: usize = 64;

/// 一个会话账本里全文加起来最多留这么多字节：超了从最久没碰的文件起丢全文（大小、跟踪这些照留，
/// 之后它的改动按只比两段算，与内容不知道时一样）。单文件上限是 [`CONTENT_HASH_MAX`] 的四倍，64 个
/// 全满会到 25MB，按会话数一乘就太多了。
const CONTENT_BUDGET: usize = 4 * 1024 * 1024;

/// Read 读到空文件时的结果。
pub(super) const EMPTY_FILE_WARNING: &str =
    "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>";

/// 一个文件最近一次是怎么进账本的：`tengu_file_read_reread.priorOp` 报的就是它（官方 `readFileState`
/// 里的 `seededFromContext` / `offset === undefined` 两个标记）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum LastOp {
    /// Read 读到的（含压缩后恢复读回的）。
    #[default]
    Read,
    /// Edit / Write 改出来的。
    EditWrite,
    /// 记忆文件那样随上下文种进来的（重读命中去重时结果是「already in your context」那段）。
    Seeded,
}

impl LastOp {
    pub(super) fn prior_op(self) -> &'static str {
        match self {
            LastOp::Read => "read",
            LastOp::EditWrite => "edit_write",
            LastOp::Seeded => "seeded",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct FileRec {
    /// 已知的当前内容（还原不出或超过上限时为 `None`）。
    content: Option<String>,
    /// 当前大小（字节）。
    size: Option<usize>,
    /// 文件历史已经跟踪它。
    tracked: bool,
    /// 头一次被跟踪的先后：快照备份按它排（官方 `trackedFiles` 的插入顺序）。
    order: u64,
    /// 最新一份备份的版本号。
    version: u32,
    /// 上次备份之后又被改过。
    dirty: bool,
    /// 最后一次读或改的先后（越大越近）。
    touched: u64,
    /// 读到的是图片（压缩后不恢复）。
    image: bool,
    /// 哪几条线（主线程 / 各子代理）的 `readFileState` 里有它：官方那张表每个代理各一份，子代理读过的
    /// 文件主线程再读不算重读、压缩后也不恢复它们（`cap/auto-2.1.305-20261021-full` A0 的 Explore 子代理读了
    /// 主线程改过的 calc.py，官方没有 `file_read_reread`）。
    readers: Vec<Reader>,
}

/// 一条线的 `readFileState` 里这个文件的那一笔。
#[derive(Clone, Debug)]
struct Reader {
    agent: String,
    /// 最近一次是读、改还是种进来的。
    last_op: LastOp,
    /// 这条线最后一次读或改的先后。
    touched: u64,
    /// 最近一次读只读了一段（带 offset / limit）：压缩后恢复时官方整份重读，代理不知道全文。
    partial: bool,
}

/// 主线程那条线的代理键（子代理是它的 `agent-id`）。
pub(super) const MAIN_READER: &str = "main";

impl FileRec {
    /// `agent` 这条线上这个文件的那一笔（没有就新记一笔，先后记成 `seq`）。
    fn reader(&mut self, agent: &str, seq: u64) -> &mut Reader {
        if let Some(i) = self.readers.iter().position(|x| x.agent == agent) {
            self.readers[i].touched = seq;
            &mut self.readers[i]
        } else {
            self.readers.push(Reader {
                agent: agent.into(),
                last_op: LastOp::Read,
                touched: seq,
                partial: false,
            });
            self.readers.last_mut().expect("just pushed")
        }
    }
}

/// 一次跟踪的结果，见 [`Files::track`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Track {
    /// 原本存在时备份的大小（`backup_file_created.fileSize`）；新建的为 `None`。
    pub(super) backup_size: Option<usize>,
    pub(super) version: u32,
}

/// 会话的文件账本。
#[derive(Clone, Debug, Default)]
pub(crate) struct Files {
    map: HashMap<String, FileRec>,
    seq: u64,
    /// 拍过几张快照（`snapshotCount`）：只有用户真正提交的才拍，客户端自己注入的那几轮（后台任务通知）
    /// 不算，所以不能拿输入序号顶替（A0 第 9、11、14 次输入是注入的，快照号就比输入号少）。官方只留最近
    /// 100 张（`XGt`），数到 100 封顶。
    snapshots: u32,
}

impl Files {
    fn rec(&mut self, path: &str) -> &mut FileRec {
        if !self.map.contains_key(path) && self.map.len() >= MAX_FILES {
            // 跟踪中的文件留着（快照要数它们），只在没跟踪的里挑最久没碰的忘掉。
            if let Some(old) = self
                .map
                .iter()
                .filter(|(_, r)| !r.tracked)
                .min_by_key(|(_, r)| r.touched)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&old);
            }
        }
        self.seq += 1;
        let seq = self.seq;
        let r = self.map.entry(path.to_string()).or_default();
        r.touched = seq;
        r
    }

    /// 文件历史跟踪中的文件数（`trackedFilesCount`）。
    pub(super) fn tracked(&self) -> usize {
        self.map.values().filter(|r| r.tracked).count()
    }

    /// `agent` 这条线读到一个文件：记下还原出的内容（或至少大小）。`partial` 是带 offset / limit 的分段读，
    /// `image` 是读到图片。返回这条线之前读写过它的那一笔（没有即这条线头一次读）。
    pub(super) fn note_read(
        &mut self,
        agent: &str,
        path: &str,
        content: Option<String>,
        size: usize,
        partial: bool,
        image: bool,
    ) -> Option<LastOp> {
        let prior = self.prior_op(agent, path);
        let r = self.rec(path);
        if partial && r.content.is_some() {
            // 分段读不动已知的全文（账本里的还是整份），只记一笔读过。
        } else {
            r.size = Some(content.as_ref().map_or(size, String::len));
            r.content = content.filter(|c| c.len() <= CONTENT_HASH_MAX * 4);
        }
        r.image = image;
        let seq = r.touched;
        r.reader(agent, seq).last_op = LastOp::Read;
        r.reader(agent, seq).partial = partial;
        self.fit_budget(path);
        prior
    }

    /// `agent` 这条线之前读写过这个文件的那一笔（去重命中的重读不改账本，只要报 `priorOp`）。
    pub(super) fn prior_op(&self, agent: &str, path: &str) -> Option<LastOp> {
        self.map.get(path)?.readers.iter().find(|x| x.agent == agent).map(|x| x.last_op)
    }

    /// 压缩之后官方清空主线程的 `readFileState`（恢复读回的文件也不再进去）：之后再读不算重读、不去重。
    /// 文件历史（跟踪 / 备份）不受影响。
    pub(super) fn compacted(&mut self, agent: &str) {
        for r in self.map.values_mut() {
            r.readers.retain(|x| x.agent != agent);
        }
    }

    /// 改文件之前：头一次改才跟踪，返回要报的那两条；之前跟踪过返回 `None`。`exists` 是改之前文件在不在。
    pub(super) fn track(&mut self, path: &str, exists: bool) -> Option<Track> {
        let r = self.rec(path);
        if r.tracked {
            return None;
        }
        r.tracked = true;
        r.order = r.touched;
        r.version = 1;
        r.dirty = false;
        Some(Track { backup_size: exists.then(|| r.size.unwrap_or(0)), version: 1 })
    }

    /// Edit：把 `old` 换成 `new`（`replace_all` 换全部），返回（增, 删）行数。官方是改前改后**整个文件**做
    /// 差分再数 +/- 行（在一行中间插换行时与只比两段不同：`abxcd` 里 `x` → `x\ny` 是删 1 增 2）；账本里有
    /// 原文就照这个算，内容不知道时才退成只比两段字符串。
    pub(super) fn apply_edit(
        &mut self,
        agent: &str,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> (usize, usize) {
        let r = self.rec(path);
        // 删成空串、要删的那段不以换行结尾而原文里它后面跟着换行：连那个换行一起删，整行去掉
        // （官方 `B`：`a\nb\n` 里删 `a` 得 `b\n`，不是 `\nb\n`）。
        let with_nl = format!("{old}\n");
        let old = match r.content.as_deref() {
            Some(c) if new.is_empty() && !old.ends_with('\n') && c.contains(&with_nl) => {
                with_nl.as_str()
            }
            _ => old,
        };
        let hits = r
            .content
            .as_deref()
            .map_or(1, |c| if replace_all { c.matches(old).count().max(1) } else { 1 });
        r.dirty = true;
        let seq = r.touched;
        *r.reader(agent, seq) = Reader {
            agent: agent.into(),
            last_op: LastOp::EditWrite,
            touched: seq,
            partial: false,
        };
        if let Some(c) = r.content.as_mut() {
            let before = std::mem::take(c);
            *c = if replace_all { before.replace(old, new) } else { before.replacen(old, new, 1) };
            r.size = Some(c.len());
            let diff = line_diff(&before, c);
            self.fit_budget(path);
            return diff;
        }
        if let Some(s) = r.size {
            r.size = Some((s + new.len() * hits).saturating_sub(old.len() * hits));
        }
        let (a, d) = snippet_diff(old, new);
        (a * hits, d * hits)
    }

    /// Write：整份换成 `content`，返回（增, 删）行数。新建（或原先内容不知道）时官方不做差分，增加的行数
    /// 是换行符个数加一（`nWn`：末尾带换行的 23 行文件报 24，`cap/auto-2.1.305-20261021-full` A0 / B4 三条）。
    pub(super) fn apply_write(
        &mut self,
        agent: &str,
        path: &str,
        content: &str,
        created: bool,
    ) -> (usize, usize) {
        let r = self.rec(path);
        let seq = r.touched;
        *r.reader(agent, seq) = Reader {
            agent: agent.into(),
            last_op: LastOp::EditWrite,
            touched: seq,
            partial: false,
        };
        let diff = match r.content.as_deref() {
            Some(old) if !created => line_diff(old, content),
            _ if content.is_empty() => (0, 0),
            _ => (content.matches('\n').count() + 1, 0),
        };
        r.content = (content.len() <= CONTENT_HASH_MAX * 4).then(|| content.to_string());
        r.size = Some(content.len());
        r.dirty = true;
        self.fit_budget(path);
        diff
    }

    /// 全文加起来超了 [`CONTENT_BUDGET`]：除刚碰过的 `keep` 外，从最久没碰的起丢全文，直到回到预算内。
    fn fit_budget(&mut self, keep: &str) {
        let mut total: usize =
            self.map.values().filter_map(|r| r.content.as_ref()).map(String::len).sum();
        if total <= CONTENT_BUDGET {
            return;
        }
        let mut held: Vec<(u64, &mut FileRec)> = self
            .map
            .iter_mut()
            .filter(|(p, r)| p.as_str() != keep && r.content.is_some())
            .map(|(_, r)| (r.touched, r))
            .collect();
        held.sort_by_key(|(t, _)| *t);
        for (_, r) in held {
            if total <= CONTENT_BUDGET {
                break;
            }
            total -= r.content.take().map_or(0, |c| c.len());
        }
    }

    /// 快照张数，见 [`Self::snapshots`] 字段。
    pub(super) fn snapshots(&self) -> u32 {
        self.snapshots
    }

    /// 用户提交一次：已跟踪且上次备份后改过的各升一版，返回这些备份的（版本, 大小），顺序按文件被跟踪的
    /// 先后（官方遍历的是 `trackedFiles` 那张表，插入顺序；`cap/auto-2.1.305-20261021-full` A0 .487 的
    /// v2 1518 / v3 1654 / v3 765 正是 test_calc.py、calc.py 之前的跟踪次序）。
    pub(super) fn snapshot(&mut self) -> Vec<(u32, usize)> {
        self.snapshots = (self.snapshots + 1).min(100);
        let mut dirty: Vec<&mut FileRec> =
            self.map.values_mut().filter(|r| r.tracked && r.dirty).collect();
        dirty.sort_by_key(|r| r.order);
        dirty
            .into_iter()
            .map(|r| {
                r.dirty = false;
                r.version += 1;
                (r.version, r.size.unwrap_or(0))
            })
            .collect()
    }

    /// 压缩后要重新读回的文件（官方 `ldr`）：`agent` 这条线读写过的里按最近碰过的取至多 `n` 个，越近越前；
    /// 不算记忆文件（CLAUDE.md 那些）、计划文件、图片，也不算保留下来那组消息里已经读过的（`skip`）。只读
    /// 过一段的照样恢复（官方整份重读），只是代理不知道全文、不带内容哈希。
    pub(super) fn restore_candidates(
        &self,
        agent: &str,
        n: usize,
        skip: &[String],
    ) -> Vec<(String, Option<String>)> {
        let mut all: Vec<(&String, &FileRec, &Reader)> = self
            .map
            .iter()
            .filter_map(|(p, r)| {
                let reader = r.readers.iter().find(|x| x.agent == agent)?;
                (!r.image
                    && !is_memory_file(p)
                    && !p.contains("/.claude/plans/")
                    && !skip.iter().any(|s| s == p))
                .then_some((p, r, reader))
            })
            .collect();
        all.sort_by_key(|(_, _, x)| std::cmp::Reverse(x.touched));
        all.into_iter()
            .take(n)
            .map(|(p, r, x)| (p.clone(), if x.partial { None } else { r.content.clone() }))
            .collect()
    }

    /// 账本里有没有计划文件（压缩后官方另以 `plan_file_reference` 附件恢复它，算一个附件）。
    pub(super) fn has_plan_file(&self) -> bool {
        self.map.keys().any(|p| p.contains("/.claude/plans/"))
    }
}

/// 官方压缩后恢复文件时跳过的记忆文件（`ydr`：加载过的 CLAUDE.md / CLAUDE.local.md / 规则文件那张表）。
fn is_memory_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(name, "CLAUDE.md" | "CLAUDE.local.md" | "MEMORY.md")
        || path.contains("/.claude/rules/")
}

/// `filePathHash`：绝对路径 sha256 的前 16 位十六进制。
pub(super) fn path_hash(path: &str) -> String {
    crate::credentials::hex_lower(&Sha256::digest(path.as_bytes()))[..16].to_string()
}

/// `contentHash`：内容的 sha256；超过官方上限（[`CONTENT_HASH_MAX`] 个 UTF-16 码元）不带。
pub(super) fn content_hash(content: &str) -> Option<String> {
    (content.encode_utf16().count() <= CONTENT_HASH_MAX)
        .then(|| crate::credentials::hex_lower(&Sha256::digest(content.as_bytes())))
}

/// Bash 命令里提到的文件扩展名：小写后的命令按官方 `/\.(csv|docx?|html|json|md|od[pst]|pdf|pptx?|rtf|txt|xlsx?)\b/g`
/// 去匹配，去重后按字母序（官方 `hjr`）。每一种报一条 `tengu_file_activity{bash_mention}`
/// （`cap/auto-2.1.306-20261021-full/00664`：`tail … README.md` 报 `md`）。
pub(super) fn bash_mentioned_exts(command: &str) -> Vec<&'static str> {
    // 同一个位置上按正则的分支顺序试，`x?` 先吃 x。
    const EXTS: [&str; 16] = [
        "csv", "docx", "doc", "html", "json", "md", "odp", "ods", "odt", "pdf", "pptx", "ppt",
        "rtf", "txt", "xlsx", "xls",
    ];
    if !command.contains('.') {
        return Vec::new();
    }
    let lower = command.to_lowercase();
    let b = lower.as_bytes();
    let mut found: Vec<&'static str> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'.' {
            i += 1;
            continue;
        }
        let rest = &b[i + 1..];
        let hit = EXTS.iter().find(|e| {
            rest.starts_with(e.as_bytes())
                && rest.get(e.len()).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
        });
        match hit {
            Some(e) => {
                if !found.contains(e) {
                    found.push(e);
                }
                i += 1 + e.len();
            }
            None => i += 1,
        }
    }
    found.sort_unstable();
    found
}

/// 从 Read 的工具结果还原原文：每行去掉开头的 `行号\t`（老格式 `行号→`），行间一个 `\n`；末尾那个只有
/// 行号的空行就是原文结尾的换行（`1\t# calc\n…\n4\t` → `# calc\n…\n`）。认不出行号格式的（图片、目录、
/// 报错文案）返回 `None`。
pub(super) fn read_content(result: &str) -> Option<String> {
    // 空文件：官方照样算哈希（内容是空串）。
    if result.trim_start().starts_with(EMPTY_FILE_WARNING) {
        return Some(String::new());
    }
    let mut out: Vec<&str> = Vec::new();
    for line in result.split('\n') {
        if line.trim_start().starts_with("<system-reminder>") {
            break;
        }
        let t = line.trim_start();
        let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let rest = &t[digits..];
        out.push(rest.strip_prefix('\t').or_else(|| rest.strip_prefix('→'))?);
    }
    // 结果末尾若还带着一个空行（追加 system-reminder 前的分隔），不是原文的一部分。
    (!out.is_empty()).then(|| out.join("\n"))
}

/// 两段文本按行比的（增, 删）行数。官方是 jsdiff `structuredPatch` 的 `+` / `-` 行数（`nWn`），行带着
/// 行尾比：只差末尾换行（`x` → `x\n`）也是增 1 删 1。jsdiff 走 Myers，给的是最短编辑：先去掉首尾相同的
/// 行，中间段用 Myers 求编辑距离 D，增删分别是 `(D ± (新行数 − 旧行数)) / 2`。大文件只改一行时首尾一去
/// 就只剩那一行（此前按行数乘积封顶、超了就报整份替换，2100 行的文件改一行报了 2100 / 2100）。
///
/// 官方差分带 5000ms 截止，超时拿不到 hunk，增删都报 0（`sme` / `L5` 返回空）。这里按工作量封顶
/// （[`MYERS_BUDGET`]）而不按墙钟：结果可复现，这段又是在遥测全局锁里算的，不能像 JS 那样跑满 5 秒。
pub(super) fn line_diff(old: &str, new: &str) -> (usize, usize) {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let (a, b) = (&a[head..], &b[head..]);
    let tail = a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count();
    let (a, b) = (&a[..a.len() - tail], &b[..b.len() - tail]);
    // 一侧裁空了就是纯增或纯删，不必（也不能）进 Myers：两万行追加在那里要走 D² 步、撞上限报成 0 / 0。
    if a.is_empty() || b.is_empty() {
        return (b.len(), a.len());
    }
    let Some(d) = myers_distance(a, b) else { return (0, 0) };
    ((d + b.len() - a.len()) / 2, (d + a.len() - b.len()) / 2)
}

/// [`myers_distance`] 的工作量上限（对角线推进与蛇形比对的步数之和）。约合这里 0.25 秒，jsdiff 逐步建对象、
/// 慢一两个数量级，同样的量大致就是它那 5 秒：中小改动远够不着，4 万行整份重写（D ≈ 8 万）这种才会超。
const MYERS_BUDGET: u64 = 100_000_000;

/// Myers 的最短编辑距离（只要长度，按 k 对角线记最远 x，O((N+M)·D) 时间、O(N+M) 空间）。超过
/// [`MYERS_BUDGET`] 返回 `None`。
fn myers_distance(a: &[&str], b: &[&str]) -> Option<usize> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    if max == 0 {
        return Some(0);
    }
    let mut work = 0u64;
    let off = max as isize;
    let mut v = vec![0isize; 2 * max + 2];
    for d in 0..=max as isize {
        let mut k = -d;
        while k <= d {
            let i = (k + off) as usize;
            let mut x =
                if k == -d || (k != d && v[i - 1] < v[i + 1]) { v[i + 1] } else { v[i - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
                work += 1;
            }
            work += 1;
            if work > MYERS_BUDGET {
                return None;
            }
            v[i] = x;
            if x >= n && y >= m {
                return Some(d as usize);
            }
            k += 2;
        }
    }
    Some(max)
}

/// Edit 不知道整份原文时，拿 `old_string` / `new_string` 两段比。片段在文件里后面一般还接着换行，
/// 整文件差分时片段的最后一行是带着它比的；两边补上这个换行，免得 `a` → `a\nb` 被算成改了一行。
fn snippet_diff(old: &str, new: &str) -> (usize, usize) {
    line_diff(&format!("{old}\n"), &format!("{new}\n"))
}

/// 可执行文件里那张扩展名 → 语言名的表（highlight.js 的语言名加别名，`$Ge` / `ySt`，377 项）。
static LANGUAGES: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../assets/cc_language_names.json"))
        .expect("assets/cc_language_names.json must parse")
});

/// 权限弹框与 `code_edit_tool.decision` 里的 `language`（官方 `Jut`）：按扩展名查表，查不到或没有扩展名
/// 是 `unknown`。
pub(super) fn language_name(path: &str) -> String {
    let ext = super::file_ext(path).to_lowercase();
    if ext.is_empty() {
        return "unknown".into();
    }
    LANGUAGES.get(&ext).cloned().unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = "/private/tmp/claude-501/-Users-easayliu-Works-easay-luban/900f9fa1-21fc-41a3-8952-9411d4990baf/scratchpad/calc";

    /// `cap/auto-2.1.305-20261021-full` B1：读 README.md 的 `filePathHash` / `contentHash`。
    #[test]
    fn hashes_match_the_capture() {
        let path = format!("{DIR}/README.md");
        assert_eq!(path_hash(&path), "012782d3bd771b08");
        let content = read_content("1\t# calc\n2\t\n3\tA tiny calculator.\n4\t").unwrap();
        assert_eq!(content, "# calc\n\nA tiny calculator.\n");
        assert_eq!(
            content_hash(&content).unwrap(),
            "8753c1c07d3b7b45af4e72318afce5acbd4ed594d4a52c546b1e4421fac6871e"
        );
        assert_eq!(path_hash(&format!("{DIR}/calc.py")), "2c67dc29c6a94f82");
        assert!(read_content("<system-reminder>x</system-reminder>").is_none());
        assert!(read_content("File does not exist.").is_none());
    }

    #[test]
    fn line_diffs() {
        assert_eq!(snippet_diff("A tiny calculator.", "A tiny, tidy calculator."), (1, 1));
        assert_eq!(
            snippet_diff(
                "def add(a, b):\n    return a + b",
                "def add(a, b):\n    return a + b\n\n\ndef mul(a, b):\n    return a * b"
            ),
            (4, 0)
        );
        assert_eq!(line_diff("", "a\nb\nc\n"), (3, 0));
        assert_eq!(line_diff("x", "x\n"), (1, 1), "只差末尾换行也算改了一行");
        assert_eq!(line_diff("a\nb", "a\nb\nc\n"), (2, 1));
        assert_eq!(line_diff("a\nb\n", "a\nb\n"), (0, 0));
        assert_eq!(line_diff("a\nb\nc\n", "x\nb\ny\nz\n"), (3, 2));
        // 大文件只改一行（此前按行数乘积封顶，报成整份替换）。
        let big: String = (0..2100).map(|i| format!("line {i}\n")).collect();
        let edited = big.replacen("line 1000\n", "line 1000!\n", 1);
        assert_eq!(line_diff(&big, &edited), (1, 1));
        // 4 万行整份重写：超出工作量上限，像官方超时那样报 0 / 0（优化构建约 0.25 秒返回）。
        let old: String = (0..40_000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..40_000).map(|i| format!("new {i}\n")).collect();
        assert_eq!(line_diff(&old, &new), (0, 0));
        // 大段纯追加 / 纯删除：首尾一裁一侧就空了，直接数行。
        let appended = format!("{big}{}", "x\n".repeat(20_000));
        assert_eq!(line_diff(&big, &appended), (20_000, 0));
        assert_eq!(line_diff(&appended, &big), (0, 20_000));
    }

    /// Edit 把一整行删成空串：连行尾换行一起删（官方 `B`），账本里不留空行。
    #[test]
    fn deleting_a_line_takes_its_newline() {
        let mut f = Files::default();
        let m = MAIN_READER;
        f.note_read(m, "/p/a.txt", Some("a\nb\n".into()), 4, false, false);
        assert_eq!(f.apply_edit(m, "/p/a.txt", "a", "", false), (0, 1));
        assert_eq!(f.map["/p/a.txt"].content.as_deref(), Some("b\n"));
    }

    /// 全文超了会话预算：从最久没碰的起丢全文，刚读的留着，大小照记。
    #[test]
    fn content_over_budget_drops_oldest_text() {
        let mut f = Files::default();
        let m = MAIN_READER;
        let big = "x".repeat(CONTENT_HASH_MAX * 4);
        let n = CONTENT_BUDGET / big.len() + 2;
        for i in 0..n {
            f.note_read(m, &format!("/p/{i}"), Some(big.clone()), big.len(), false, false);
        }
        let total: usize = f.map.values().filter_map(|r| r.content.as_ref()).map(String::len).sum();
        assert!(total <= CONTENT_BUDGET);
        assert!(f.map["/p/0"].content.is_none(), "最久的丢了全文");
        assert_eq!(f.map["/p/0"].size, Some(big.len()), "大小还在");
        assert!(f.map[&format!("/p/{}", n - 1)].content.is_some(), "刚读的留着");
    }

    #[test]
    fn languages() {
        assert_eq!(language_name("/x/README.md"), "Markdown");
        assert_eq!(language_name("/x/calc.py"), "Python");
        assert_eq!(language_name("/x/Makefile"), "unknown");
        assert_eq!(language_name("/x/a.weird"), "unknown");
    }

    /// A0 的文件历史：Edit calc.py（存在，备份 v1）→ 下次提交 v2；Write 新建 test_calc.py（不备份）→
    /// 下次提交 v2；之后再改不重复跟踪，快照时只升改过的。
    #[test]
    fn file_history_follows_a0() {
        let mut f = Files::default();
        let m = MAIN_READER;
        f.note_read(m, "/p/calc.py", Some("x".repeat(172)), 172, false, false);
        assert_eq!(f.track("/p/calc.py", true), Some(Track { backup_size: Some(172), version: 1 }));
        f.apply_edit(m, "/p/calc.py", "x", "yy", false);
        assert_eq!(f.track("/p/calc.py", true), None, "同一个文件只跟踪一次");
        assert_eq!(f.snapshot(), [(2, 173)]);
        assert_eq!(f.snapshot(), [], "没再改就不备份");
        assert_eq!(f.track("/p/t.py", false), Some(Track { backup_size: None, version: 1 }));
        f.apply_write(m, "/p/t.py", "abc\n", true);
        assert_eq!(f.tracked(), 2);
        assert_eq!(f.snapshot(), [(2, 4)]);
        f.apply_write(m, "/p/calc.py", "z\n", false);
        f.apply_edit(m, "/p/t.py", "abc", "abcd", false);
        assert_eq!(f.snapshot(), [(3, 2), (3, 5)]);
    }

    /// 快照备份按文件被跟踪的先后排，不按最后碰过的；Edit 在账本有原文时按整个文件差分。
    #[test]
    fn snapshot_order_and_whole_file_diff() {
        let mut f = Files::default();
        let m = MAIN_READER;
        f.note_read(m, "/p/a.txt", Some("abxcd\n".into()), 6, false, false);
        f.track("/p/a.txt", true);
        f.apply_write(m, "/p/b.txt", "1\n", true);
        f.track("/p/b.txt", false);
        // 先改 b 再改 a：备份仍是 a、b 的次序。
        f.apply_edit(m, "/p/b.txt", "1", "2", false);
        assert_eq!(
            f.apply_edit(m, "/p/a.txt", "x", "x\ny", false),
            (2, 1),
            "整文件差分：删 1 增 2"
        );
        assert_eq!(f.snapshot(), [(2, 8), (2, 2)]);
    }

    /// 读状态按线分：子代理读过的文件主线程再读不算重读、压缩后也不恢复；压缩清掉主线程的读状态。
    #[test]
    fn read_state_is_per_agent_and_cleared_by_compaction() {
        let mut f = Files::default();
        let m = MAIN_READER;
        f.note_read("agent-1", "/p/a.md", Some("# a\n".into()), 4, false, false);
        assert_eq!(f.prior_op(m, "/p/a.md"), None);
        assert!(f.restore_candidates(m, 5, &[]).is_empty());
        assert_eq!(f.note_read(m, "/p/a.md", Some("# a\n".into()), 4, false, false), None);
        assert_eq!(f.prior_op(m, "/p/a.md"), Some(LastOp::Read));
        f.apply_edit(m, "/p/a.md", "# a", "# b", false);
        assert_eq!(f.prior_op(m, "/p/a.md"), Some(LastOp::EditWrite));
        f.note_read(m, "/p/big.md", None, 10, true, false);
        f.note_read(m, "/p/logo.png", None, 10, false, true);
        f.note_read(m, "/p/CLAUDE.md", Some("x".into()), 1, false, false);
        let got = f.restore_candidates(m, 5, &[]);
        assert_eq!(
            got,
            [("/p/big.md".to_string(), None), ("/p/a.md".to_string(), Some("# b\n".to_string()))]
        );
        assert_eq!(f.restore_candidates(m, 5, &["/p/a.md".to_string()]).len(), 1);
        f.compacted(m);
        assert_eq!(f.prior_op(m, "/p/a.md"), None);
        assert_eq!(f.prior_op("agent-1", "/p/a.md"), Some(LastOp::Read));
        assert!(f.restore_candidates(m, 5, &[]).is_empty());
    }
}
