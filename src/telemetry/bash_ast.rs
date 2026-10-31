//! 官方 2.1.299 自带的 bash 解析器（`$_().parse`），逐函数移植。
//!
//! `bash_word_split_use` 的判定（[`super::tools::bash_word_split_use`]）在这棵语法树上遍历；手写的近似扫描器
//! 总在续行、注释、嵌套 here-doc 这类边角上对不齐，所以照原文搬过来，连它的错误恢复与放弃规则一起。
//!
//! 与原文保持一致的几处约定：
//! - 源码按 UTF-16 码元处理（`i`），位置按 UTF-8 字节记（`b`），节点文本按同一张字节表二分取回（原文 `z`）；
//! - 读到文本末尾记成 `-1`（原文的空串 `""`）：它不等于任何字符，`>= 0x80` 为假、`< 0x80` 为真；
//! - 原文 `throw` 的地方（定界符不合规、节点超过 5 万、超过 50ms）返回 [`Abort`]，整个解析作废；
//! - 另加一道栈用量上限：原文在极深的嵌套上会爆调用栈（`RangeError`，被同一个 `catch` 接住），这里在几个
//!   递归枢纽量一下已用的线程栈，超过 [`STACK_BUDGET`] 就提前作废，免得把线程栈压穿（帧大小随构建方式变，
//!   按层数设上限不可靠）。
//!
//! 函数名后的注释是原文里的函数名，便于对照。

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub(super) type Id = usize;

thread_local! {
    /// 这一段（一条调用的事件，[`ParseBudget`]）里解析还能用到什么时候。
    static BUDGET_END: Cell<Option<Instant>> = const { Cell::new(None) };
    /// 最近一次解析的源码与结果：同一条命令 `bash_word_split_use` 与 `bash_command_kinds` 各要一棵树，第二次直接取。
    static LAST: RefCell<Option<(String, Option<Rc<Tree>>)>> = const { RefCell::new(None) };
}

/// 一条调用的事件里解析 shell 命令的总额度（[`crate::config::TELEMETRY_BASH_PARSE_BUDGET_MS`]）。
///
/// 遥测只有一个消费者、建事件时拿着状态锁（转发路径上新会话的握手也要那把锁），而命令是租户给的：一轮塞几十上百条
/// 上万字的命令，每条都能吃满单次解析的 50ms。正常命令解析几十微秒，这个额度够几千条；用完之后的命令按「解析器
/// 放弃」处理（`bash_word_split_use` 为假、`bash_command_kinds` 报 `unparsed`，都是官方解析失败时的取值），单次
/// 解析的时限也压到剩余额度以内。
pub(super) struct ParseBudget {
    outer: bool,
}

impl ParseBudget {
    /// 开一段额度；已经在一段里（处理一条调用时连带补处理扣住的那几条）时不另开，外层那段说了算。
    pub(super) fn start(limit: Duration) -> Self {
        BUDGET_END.with(|b| {
            if b.get().is_some() {
                return Self { outer: false };
            }
            b.set(Some(Instant::now() + limit));
            Self { outer: true }
        })
    }
}

impl Drop for ParseBudget {
    fn drop(&mut self) {
        if self.outer {
            BUDGET_END.with(|b| b.set(None));
            LAST.with(|l| l.borrow_mut().take());
        }
    }
}

/// 语法树节点：类型、字节范围、子节点。
pub(super) struct Node {
    pub(super) kind: Cow<'static, str>,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) children: Vec<Id>,
}

/// 解析结果：节点表与根。
pub(super) struct Tree {
    src: Vec<u16>,
    table: Vec<u32>,
    pub(super) nodes: Vec<Node>,
    pub(super) root: Id,
}

impl Tree {
    /// 节点的源码（UTF-16 码元）。
    pub(super) fn text(&self, id: Id) -> &[u16] {
        let n = &self.nodes[id];
        slice_bytes(&self.src, &self.table, n.start, n.end)
    }

    pub(super) fn kind(&self, id: Id) -> &str {
        &self.nodes[id].kind
    }

    pub(super) fn children(&self, id: Id) -> &[Id] {
        &self.nodes[id].children
    }
}

/// 解析；原文返回 `null` 的情形（放弃、超时、超限）返回 `None`。同一条源码连着要两次时第二次直接取上一棵
/// （[`LAST`]）；这一段的额度（[`ParseBudget`]）用完了也返回 `None`。
pub(super) fn parse(src: &str) -> Option<Rc<Tree>> {
    let hit = LAST.with(|l| l.borrow().as_ref().filter(|(s, _)| s == src).map(|(_, t)| t.clone()));
    if let Some(tree) = hit {
        return tree;
    }
    let end = BUDGET_END.with(Cell::get);
    if end.is_some_and(|end| Instant::now() >= end) {
        return None;
    }
    let tree = parse_fresh(src, end).map(Rc::new);
    LAST.with(|l| *l.borrow_mut() = Some((src.to_string(), tree.clone())));
    tree
}

/// [`parse`] 的本体：`budget_end` 是这一段额度的尽头，单次解析的时限不越过它。
fn parse_fresh(src: &str, budget_end: Option<Instant>) -> Option<Tree> {
    let bytes = src.len();
    if bytes >= 67_108_864 {
        return None;
    }
    let u: Vec<u16> = src.encode_utf16().collect();
    let mut p = P::new(u, bytes, budget_end);
    let mut root = p.program().ok()?;
    if p.zsh_brace_diff || p.zsh_cond_paren_diff {
        let (s, e) = (p.nodes[root].start, p.nodes[root].end);
        root = p.mk("ERROR", s, e, vec![root]).ok()?;
    }
    Some(Tree { src: p.src, table: p.table, nodes: p.nodes, root })
}

/// 原文 `throw`：整个解析作废。
struct Abort;
type R<T> = Result<T, Abort>;

/// 节点上限（原文 `We`）。
const NODE_BUDGET: usize = 50_000;
/// 解析时限（原文 `deadline`）。
const DEADLINE: Duration = Duration::from_millis(50);
/// 解析最多用多少线程栈（tokio 阻塞线程默认 2MiB，留出调用方那一截的余量）。官方大约在上千层嵌套处
/// 爆栈，这里按 release 构建能撑几百层，真实命令远到不了。
const STACK_BUDGET: usize = 1 << 20;

/// 当前栈位置（取一个局部变量的地址）。
#[inline(never)]
fn stack_addr() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

const EOF: i32 = -1;

const fn u(b: u8) -> i32 {
    b as i32
}

/// 一个字节在 ASCII 集合里。
fn in_set(c: i32, set: &[u8]) -> bool {
    (0..0x80).contains(&c) && set.contains(&(c as u8))
}

fn is_letter(c: i32) -> bool {
    (u(b'a')..=u(b'z')).contains(&c) || (u(b'A')..=u(b'Z')).contains(&c) || c == u(b'_')
}
fn is_digit(c: i32) -> bool {
    (u(b'0')..=u(b'9')).contains(&c)
}
fn is_alnum(c: i32) -> bool {
    is_letter(c) || is_digit(c)
}
fn is_hex(c: i32) -> bool {
    is_digit(c) || (u(b'a')..=u(b'f')).contains(&c) || (u(b'A')..=u(b'F')).contains(&c)
}
fn is_base_digit(c: i32) -> bool {
    is_alnum(c) || c == u(b'@')
}
/// `we`：普通词字符。
fn is_word_char(c: i32) -> bool {
    is_alnum(c) || in_set(c, b"/.-+:@%,~^?*!=[]") || c >= 0x80
}
/// `Te`：词边界。
fn is_boundary(c: i32) -> bool {
    c == EOF || in_set(c, b" \t\n\r;&|()<>")
}
/// `$e`：here-doc 定界符里能出现的字符。
fn is_delim_char(c: i32) -> bool {
    c != EOF && !in_set(c, b" \t\n<>|&;()'\"`\\")
}
/// `j`：特殊参数名。
fn is_special(c: i32) -> bool {
    in_set(c, b"?$@*#-!_")
}
/// `ee`：`${` 之后可以打头的运算符。
fn is_param_op(c: i32) -> bool {
    in_set(c, b"#^=~+")
}

/// `/^-?\d+$/`。
fn is_int(s: &[u16]) -> bool {
    let d = s.strip_prefix(&[u16::from(b'-')]).unwrap_or(s);
    !d.is_empty() && d.iter().all(|c| (u16::from(b'0')..=u16::from(b'9')).contains(c))
}
/// `/^\d+$/`。
fn is_uint(s: &[u16]) -> bool {
    !s.is_empty() && s.iter().all(|c| (u16::from(b'0')..=u16::from(b'9')).contains(c))
}

fn eq(s: &[u16], lit: &str) -> bool {
    s.len() == lit.len() && s.iter().zip(lit.bytes()).all(|(a, b)| *a == u16::from(b))
}

fn has_unit(s: &[u16], set: &[u8]) -> bool {
    s.iter().any(|c| *c < 0x80 && set.contains(&(*c as u8)))
}

fn owned(s: &[u16]) -> Cow<'static, str> {
    Cow::Owned(String::from_utf16_lossy(s))
}

/// 原文 `z`：按字节范围取回源码。字节表 `t[i]` 是第 `i` 个码元起点的字节偏移（代理对的后半记成 `+2`）。
fn slice_bytes<'a>(src: &'a [u16], t: &[u32], o: usize, e: usize) -> &'a [u16] {
    let c = lower_bound(t, 0, src.len(), o);
    let r = lower_bound(t, c, src.len(), e);
    &src[c..r]
}

fn lower_bound(t: &[u32], mut lo: usize, mut hi: usize, x: usize) -> usize {
    while lo < hi {
        let mid = (lo + hi) / 2;
        if (t[mid] as usize) < x {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// 原文 `ge`。
fn byte_table(src: &[u16]) -> Vec<u32> {
    let mut t = vec![0u32; src.len() + 1];
    let (mut s, mut i) = (0u32, 0usize);
    while i < src.len() {
        t[i] = s;
        let r = src[i];
        if r < 128 {
            s += 1;
            i += 1;
        } else if r < 2048 {
            s += 2;
            i += 1;
        } else if (0xD800..=0xDBFF).contains(&r) {
            if i + 1 < t.len() {
                t[i + 1] = s + 2;
            }
            s += 4;
            i += 2;
        } else {
            s += 3;
            i += 1;
        }
    }
    t[src.len()] = s;
    t
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tk {
    Eof,
    Newline,
    Comment,
    Op,
    LtParen,
    GtParen,
    Dquote,
    Squote,
    DollarDparen,
    DollarParen,
    DollarBrace,
    AnsiC,
    Dollar,
    Backtick,
    Word,
    Number,
}

/// 词法单元：类型、值（码元范围）、字节范围。
#[derive(Clone, Copy)]
struct Tok {
    ty: Tk,
    vs: usize,
    ve: usize,
    start: usize,
    end: usize,
}

struct Heredoc {
    delim: Vec<u16>,
    strip_tabs: bool,
    quoted: bool,
    body_start: usize,
    body_end: usize,
    end_start: usize,
    end_end: usize,
}

/// 算术表达式的模式（原文 `"var"` / `"assign"` / `"word"`）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ar {
    Var,
    Assign,
    Word,
}

struct P {
    src: Vec<u16>,
    len: usize,
    i: usize,
    b: usize,
    heredocs: Vec<Heredoc>,
    table: Vec<u32>,
    zsh_cond_paren_diff: bool,
    src_bytes: usize,
    nodes: Vec<Node>,
    node_count: usize,
    deadline: Instant,
    in_backtick: i32,
    in_dquote: i32,
    stop_bracket: bool,
    zsh_brace_diff: bool,
    stack_base: usize,
}

const CLOSE_WORDS: [&str; 7] = ["then", "elif", "else", "fi", "do", "done", "esac"];
/// `OEr`：命令位置上的保留字。
const RESERVED: [&str; 15] = [
    "if", "then", "elif", "else", "fi", "while", "until", "for", "in", "do", "done", "case",
    "esac", "function", "select",
];
/// `Ee`：声明命令。
const DECL: [&str; 5] = ["export", "declare", "typeset", "readonly", "local"];

impl P {
    fn new(src: Vec<u16>, src_bytes: usize, budget_end: Option<Instant>) -> Self {
        let table = byte_table(&src);
        let deadline = Instant::now() + DEADLINE;
        P {
            len: src.len(),
            src,
            i: 0,
            b: 0,
            heredocs: Vec::new(),
            table,
            zsh_cond_paren_diff: false,
            src_bytes,
            nodes: Vec::new(),
            node_count: 0,
            deadline: budget_end.map_or(deadline, |end| end.min(deadline)),
            in_backtick: 0,
            in_dquote: 0,
            stop_bracket: false,
            zsh_brace_diff: false,
            stack_base: stack_addr(),
        }
    }

    // ---- 基础设施 ----

    /// 原文 `e.src[idx]`（越界为 `-1`）。
    fn c(&self, idx: usize) -> i32 {
        self.src.get(idx).map_or(EOF, |c| i32::from(*c))
    }

    /// 原文 `L(e, o)`。
    fn la(&self, o: usize) -> i32 {
        self.c(self.i + o)
    }

    /// 原文 `n`：前进一个码点。
    fn adv(&mut self) {
        let o = self.c(self.i);
        self.i += 1;
        if (0..128).contains(&o) {
            self.b += 1;
        } else if (128..2048).contains(&o) {
            self.b += 2;
        } else if (0xD800..=0xDBFF).contains(&o) {
            self.b += 4;
            self.i += 1;
        } else {
            self.b += 3;
        }
    }

    fn save(&self) -> (usize, usize) {
        (self.i, self.b)
    }

    fn restore(&mut self, s: (usize, usize)) {
        (self.i, self.b) = s;
    }

    /// 原文 `je`：跳到字节偏移 `o`。
    fn seek(&mut self, o: usize) {
        self.i = lower_bound(&self.table, 0, self.len, o);
        self.b = o;
    }

    /// 原文 `e.src.slice(a, b)`（越界截断）。
    fn slice(&self, a: usize, b: usize) -> &[u16] {
        let b = b.min(self.len);
        if a >= b { &[] } else { &self.src[a..b] }
    }

    fn z(&self, o: usize, e: usize) -> &[u16] {
        slice_bytes(&self.src, &self.table, o, e)
    }

    fn val(&self, t: &Tok) -> &[u16] {
        self.slice(t.vs, t.ve)
    }

    fn val_is(&self, t: &Tok, lit: &str) -> bool {
        eq(self.val(t), lit)
    }

    fn val_in(&self, t: &Tok, lits: &[&str]) -> bool {
        lits.iter().any(|l| self.val_is(t, l))
    }

    fn vkind(&self, t: &Tok) -> Cow<'static, str> {
        owned(self.val(t))
    }

    /// 原文 `a`（含 `We` 的节点上限与时限）。
    fn mk(
        &mut self,
        kind: impl Into<Cow<'static, str>>,
        start: usize,
        end: usize,
        children: Vec<Id>,
    ) -> R<Id> {
        self.node_count += 1;
        if self.node_count > NODE_BUDGET {
            return Err(Abort);
        }
        if self.node_count & 127 == 0 && Instant::now() > self.deadline {
            return Err(Abort);
        }
        self.nodes.push(Node { kind: kind.into(), start, end, children });
        Ok(self.nodes.len() - 1)
    }

    /// 原文 `y`。
    fn mk_tok(&mut self, kind: impl Into<Cow<'static, str>>, t: &Tok) -> R<Id> {
        self.mk(kind, t.start, t.end, Vec::new())
    }

    fn start(&self, id: Id) -> usize {
        self.nodes[id].start
    }
    fn end(&self, id: Id) -> usize {
        self.nodes[id].end
    }
    fn kind(&self, id: Id) -> &str {
        &self.nodes[id].kind
    }
    fn kids(&self, id: Id) -> Vec<Id> {
        self.nodes[id].children.clone()
    }
    fn text(&self, id: Id) -> &[u16] {
        self.z(self.nodes[id].start, self.nodes[id].end)
    }

    /// 递归枢纽（语句表、命令、词、`$`、算术一元、条件一元 / 括号）进门时量一下栈。
    fn guard(&self) -> R<()> {
        if stack_addr().abs_diff(self.stack_base) > STACK_BUDGET { Err(Abort) } else { Ok(()) }
    }

    /// 原文 `Ie`：`${` / `$` 之后（从码元下标 `o` 起）跳过运算符再读名字，名字里有非 ASCII 时返回其后下标。
    fn nonascii_name_end(&self, o: usize) -> Option<usize> {
        let mut s = o;
        while s < self.len && is_param_op(self.c(s)) {
            s += 1;
        }
        let mut found = false;
        while s < self.len {
            let r = self.c(s);
            if is_alnum(r) {
                s += 1;
            } else if r >= 0x80 {
                found = true;
                s += 1;
            } else {
                break;
            }
        }
        found.then_some(s)
    }

    // ---- 词法 ----

    /// 原文 `h`：跳过空白、`\r` 与续行。
    fn h(&mut self) {
        while self.i < self.len {
            let o = self.c(self.i);
            if o == u(b' ') || o == u(b'\t') || o == u(b'\r') {
                self.adv();
            } else if o == u(b'\\') && self.c(self.i + 1) == u(b'\n') {
                self.adv();
                self.adv();
            } else {
                break;
            }
        }
    }

    /// 原文 `Ce`：跳过空格、制表与续行。
    fn skip_blank(&mut self) {
        while self.i < self.len {
            let o = self.c(self.i);
            if o == u(b' ') || o == u(b'\t') {
                self.adv();
            } else if o == u(b'\\') && self.c(self.i + 1) == u(b'\n') {
                self.adv();
                self.adv();
            } else {
                break;
            }
        }
    }

    fn tok_from(&self, ty: Tk, vs: usize, start: usize) -> Tok {
        Tok { ty, vs, ve: self.i, start, end: self.b }
    }

    fn op(&mut self, n: usize, vs: usize, start: usize) -> Tok {
        for _ in 0..n {
            self.adv();
        }
        self.tok_from(Tk::Op, vs, start)
    }

    /// 原文 `T`：取下一个词法单元；`cmd` 为真时按命令位置认 `[[` / `[` / `{` / `}` / `!`。
    fn tok(&mut self, cmd: bool) -> Tok {
        self.h();
        let t = self.b;
        let vs = self.i;
        if self.i >= self.len {
            return Tok { ty: Tk::Eof, vs, ve: vs, start: t, end: t };
        }
        let (s, i, r) = (self.la(0), self.la(1), self.la(2));
        if s == u(b'\n') {
            self.adv();
            return self.tok_from(Tk::Newline, vs, t);
        }
        if s == u(b'#') {
            let l = if self.i > 0 { self.c(self.i - 1) } else { EOF };
            if l == EOF || in_set(l, b" \t\n;&|<>()`") {
                while self.i < self.len && self.c(self.i) != u(b'\n') {
                    self.adv();
                }
                return self.tok_from(Tk::Comment, vs, t);
            }
        }
        let (amp, bar, semi, gt, lt) = (u(b'&'), u(b'|'), u(b';'), u(b'>'), u(b'<'));
        if s == amp && i == amp {
            return self.op(2, vs, t);
        }
        if s == bar && i == bar {
            return self.op(2, vs, t);
        }
        if s == bar && i == amp {
            return self.op(2, vs, t);
        }
        if s == semi && i == semi && r == amp {
            return self.op(3, vs, t);
        }
        if s == semi && i == semi {
            return self.op(2, vs, t);
        }
        if s == semi && i == amp {
            return self.op(2, vs, t);
        }
        if s == gt && i == gt {
            return self.op(2, vs, t);
        }
        if s == gt && i == amp && r == u(b'-') {
            return self.op(3, vs, t);
        }
        if s == gt && i == amp {
            return self.op(2, vs, t);
        }
        if s == gt && i == bar {
            return self.op(2, vs, t);
        }
        if s == amp && i == gt && r == gt {
            return self.op(3, vs, t);
        }
        if s == amp && i == gt {
            return self.op(2, vs, t);
        }
        if s == lt && i == lt && r == lt {
            return self.op(3, vs, t);
        }
        if s == lt && i == lt && r == u(b'-') {
            return self.op(3, vs, t);
        }
        if s == lt && i == lt {
            return self.op(2, vs, t);
        }
        if s == lt && i == amp && r == u(b'-') {
            return self.op(3, vs, t);
        }
        if s == lt && i == amp {
            return self.op(2, vs, t);
        }
        if s == lt && i == u(b'(') {
            self.adv();
            self.adv();
            return self.tok_from(Tk::LtParen, vs, t);
        }
        if s == gt && i == u(b'(') {
            self.adv();
            self.adv();
            return self.tok_from(Tk::GtParen, vs, t);
        }
        if s == u(b'(') && i == u(b'(') {
            return self.op(2, vs, t);
        }
        if s == u(b')') && i == u(b')') {
            return self.op(2, vs, t);
        }
        if in_set(s, b"|&;><()") {
            return self.op(1, vs, t);
        }
        if cmd {
            if s == u(b'[') {
                let mut l = false;
                let skip_cont = |src: &P, mut x: usize, l: &mut bool| {
                    while src.c(x) == u(b'\\') {
                        if src.c(x + 1) == u(b'\n') {
                            x += 2;
                            *l = true;
                        } else if src.c(x + 1) == u(b'\r') && src.c(x + 2) == u(b'\n') {
                            x += 3;
                            *l = true;
                        } else {
                            break;
                        }
                    }
                    x
                };
                let f = skip_cont(self, self.i + 1, &mut l);
                if self.c(f) == u(b'[') {
                    let x = skip_cont(self, f + 1, &mut l);
                    if l && self.c(x) == u(b'(') {
                        self.zsh_cond_paren_diff = true;
                    }
                }
            }
            if s == u(b'[') && i == u(b'[') && (r == EOF || in_set(r, b" \t\n(")) {
                if r == u(b'(') {
                    self.zsh_cond_paren_diff = true;
                }
                return self.op(2, vs, t);
            }
            if s == u(b'[') {
                return self.op(1, vs, t);
            }
            if s == u(b'{') && in_set(i, b" \t\n") {
                return self.op(1, vs, t);
            }
            if s == u(b'}') {
                return self.op(1, vs, t);
            }
            if s == u(b'!') && in_set(i, b" \t") {
                return self.op(1, vs, t);
            }
        }
        if s == u(b'"') {
            self.adv();
            return self.tok_from(Tk::Dquote, vs, t);
        }
        if s == u(b'\'') {
            self.adv();
            while self.i < self.len && self.c(self.i) != u(b'\'') {
                self.adv();
            }
            if self.i < self.len {
                self.adv();
            }
            return self.tok_from(Tk::Squote, vs, t);
        }
        if s == u(b'$') {
            if i == u(b'(') && r == u(b'(') {
                for _ in 0..3 {
                    self.adv();
                }
                return self.tok_from(Tk::DollarDparen, vs, t);
            }
            if i == u(b'(') {
                self.adv();
                self.adv();
                return self.tok_from(Tk::DollarParen, vs, t);
            }
            if i == u(b'{') {
                self.adv();
                self.adv();
                return self.tok_from(Tk::DollarBrace, vs, t);
            }
            if i == u(b'\'') {
                self.adv();
                self.adv();
                while self.i < self.len && self.c(self.i) != u(b'\'') {
                    if self.c(self.i) == u(b'\\') && self.i + 1 < self.len {
                        self.adv();
                    }
                    self.adv();
                }
                if self.i < self.len {
                    self.adv();
                }
                return self.tok_from(Tk::AnsiC, vs, t);
            }
            self.adv();
            return self.tok_from(Tk::Dollar, vs, t);
        }
        if s == u(b'`') {
            self.adv();
            return self.tok_from(Tk::Backtick, vs, t);
        }
        if is_digit(s) {
            let mut l = self.i;
            while l < self.len && is_digit(self.c(l)) {
                l += 1;
            }
            let c = self.c(l);
            if c == gt || c == lt {
                while self.i < l {
                    self.adv();
                }
                return self.tok_from(Tk::Word, vs, t);
            }
        }
        if is_word_char(s) || s == u(b'\\') || s == u(b'{') || s == u(b'}') {
            let l = self.i;
            while self.i < self.len {
                let c = self.c(self.i);
                if c == u(b'\\') {
                    if self.i + 1 >= self.len {
                        break;
                    }
                    self.adv();
                    self.adv();
                    continue;
                }
                if !is_word_char(c) && c != u(b'{') && c != u(b'}') && c != u(b'#') {
                    break;
                }
                self.adv();
            }
            if self.i > l {
                let ty = if is_int(self.slice(l, self.i)) { Tk::Number } else { Tk::Word };
                return self.tok_from(ty, vs, t);
            }
        }
        self.adv();
        self.tok_from(Tk::Word, vs, t)
    }

    // ---- 语句 ----

    /// 原文 `ze`。
    fn program(&mut self) -> R<Id> {
        let mut o = Vec::new();
        self.h();
        loop {
            let i = self.save();
            if self.tok(true).ty == Tk::Newline {
                self.h();
                continue;
            }
            self.restore(i);
            break;
        }
        let t = self.b;
        while self.i < self.len {
            let i = self.save();
            let r = self.tok(true);
            match r.ty {
                Tk::Eof => break,
                Tk::Newline => continue,
                Tk::Comment => {
                    o.push(self.mk_tok("comment", &r)?);
                    continue;
                }
                _ => {}
            }
            self.restore(i);
            let l = self.stmts(None)?;
            let empty = l.is_empty();
            o.extend(l);
            if empty {
                let c = self.tok(true);
                if c.ty == Tk::Eof {
                    break;
                }
                if c.ty == Tk::Op && self.val_is(&c, ";;") && !o.is_empty() {
                    continue;
                }
                o.push(self.mk("ERROR", c.start, c.end, Vec::new())?);
            }
        }
        let s = if o.is_empty() { t } else { self.src_bytes };
        self.mk("program", t, s, o)
    }

    /// 原文 `D`：一串语句，碰到 `stop`（或收尾的保留字 / 括号）停下、不吃掉它。
    fn stmts(&mut self, stop: Option<&str>) -> R<Vec<Id>> {
        self.guard()?;
        let mut t = Vec::new();
        loop {
            self.h();
            let s = self.save();
            let i = self.tok(true);
            if i.ty == Tk::Eof {
                self.restore(s);
                break;
            }
            if i.ty == Tk::Newline {
                if !self.heredocs.is_empty() {
                    self.read_heredocs()?;
                }
                continue;
            }
            if i.ty == Tk::Comment {
                t.push(self.mk_tok("comment", &i)?);
                continue;
            }
            if let Some(o) = stop
                && i.ty == Tk::Op
                && self.val_is(&i, o)
            {
                self.restore(s);
                break;
            }
            if i.ty == Tk::Op && self.val_in(&i, &[")", "}", ";;", ";&", ";;&", "))", "]]", "]"]) {
                self.restore(s);
                break;
            }
            if i.ty == Tk::Backtick && self.in_backtick > 0 {
                self.restore(s);
                break;
            }
            if i.ty == Tk::Word && self.val_in(&i, &CLOSE_WORDS) {
                self.restore(s);
                break;
            }
            self.restore(s);
            let Some(r) = self.and_or()? else { break };
            t.push(r);
            self.h();
            let l = self.save();
            let c = self.tok(true);
            if c.ty == Tk::Op && self.val_in(&c, &[";", "&"]) {
                let f = self.save();
                let _ = self.tok(true);
                self.restore(f);
                let k = self.vkind(&c);
                t.push(self.mk_tok(k, &c)?);
            } else if c.ty == Tk::Newline {
                if !self.heredocs.is_empty() {
                    self.read_heredocs()?;
                }
            } else {
                self.restore(l);
            }
        }
        Ok(t)
    }

    /// 原文 `ke`：`&&` / `||` 连起来的一串管道。
    fn and_or(&mut self) -> R<Option<Id>> {
        let Some(mut o) = self.pipeline()? else { return Ok(None) };
        loop {
            let t = self.save();
            let s = self.tok(true);
            if s.ty == Tk::Op && self.val_in(&s, &["&&", "||"]) {
                let k = self.vkind(&s);
                let i = self.mk_tok(k, &s)?;
                self.skip_newlines();
                let Some(r) = self.pipeline()? else {
                    o = self.mk("ERROR", self.start(o), self.end(i), vec![o, i])?;
                    break;
                };
                if self.kind(r) == "redirected_statement" && self.nodes[r].children.len() >= 2 {
                    let ch = self.kids(r);
                    let l = ch[0];
                    let f = self.mk("list", self.start(o), self.end(l), vec![o, i, l])?;
                    let last = *ch.last().unwrap_or(&l);
                    let mut kids = vec![f];
                    kids.extend_from_slice(&ch[1..]);
                    o = self.mk("redirected_statement", self.start(f), self.end(last), kids)?;
                } else {
                    o = self.mk("list", self.start(o), self.end(r), vec![o, i, r])?;
                }
            } else {
                self.restore(t);
                break;
            }
        }
        Ok(Some(o))
    }

    /// 原文 `F`：跳过空行。
    fn skip_newlines(&mut self) {
        loop {
            let o = self.save();
            if self.tok(true).ty != Tk::Newline {
                self.restore(o);
                break;
            }
        }
    }

    /// 原文 `de`：管道。
    fn pipeline(&mut self) -> R<Option<Id>> {
        let Some(o) = self.command()? else { return Ok(None) };
        let mut t = vec![o];
        loop {
            let i = self.save();
            let r = self.tok(true);
            if r.ty == Tk::Op && self.val_in(&r, &["|", "|&"]) {
                let k = self.vkind(&r);
                let l = self.mk_tok(k, &r)?;
                self.skip_newlines();
                let Some(c) = self.command()? else {
                    t.push(l);
                    break;
                };
                if self.kind(c) == "redirected_statement"
                    && self.nodes[c].children.len() >= 2
                    && !t.is_empty()
                {
                    let ch = self.kids(c);
                    let f = ch[0];
                    let mut d = t.clone();
                    d.extend([l, f]);
                    let b = self.mk("pipeline", self.start(d[0]), self.end(f), d)?;
                    let p = *ch.last().unwrap_or(&f);
                    let mut kids = vec![b];
                    kids.extend_from_slice(&ch[1..]);
                    let x = self.mk("redirected_statement", self.start(b), self.end(p), kids)?;
                    t = vec![x];
                    continue;
                }
                t.extend([l, c]);
            } else {
                self.restore(i);
                break;
            }
        }
        if t.len() == 1 {
            return Ok(Some(t[0]));
        }
        let s = t[t.len() - 1];
        Ok(Some(self.mk("pipeline", self.start(t[0]), self.end(s), t)?))
    }

    /// 原文 `K`：一条命令（复合命令、条件、`!`、或简单命令）。
    fn command(&mut self) -> R<Option<Id>> {
        self.guard()?;
        self.command_inner()
    }

    fn command_inner(&mut self) -> R<Option<Id>> {
        self.h();
        let o = self.save();
        let t = self.tok(true);
        if t.ty == Tk::Eof {
            self.restore(o);
            return Ok(None);
        }
        if t.ty == Tk::Op && self.val_is(&t, "!") {
            let s = self.mk_tok("!", &t)?;
            let Some(i) = self.command()? else {
                return Ok(Some(self.mk(
                    "negated_command",
                    self.start(s),
                    self.end(s),
                    vec![s],
                )?));
            };
            if self.kind(i) == "redirected_statement" && self.nodes[i].children.len() >= 2 {
                let ch = self.kids(i);
                let r = ch[0];
                let c = self.mk("negated_command", self.start(s), self.end(r), vec![s, r])?;
                let f = *ch.last().unwrap_or(&r);
                let mut kids = vec![c];
                kids.extend_from_slice(&ch[1..]);
                return Ok(Some(self.mk(
                    "redirected_statement",
                    self.start(c),
                    self.end(f),
                    kids,
                )?));
            }
            return Ok(Some(self.mk("negated_command", self.start(s), self.end(i), vec![s, i])?));
        }
        if t.ty == Tk::Op && self.val_is(&t, "(") {
            let s = self.mk_tok("(", &t)?;
            let i = self.stmts(Some(")"))?;
            let r = self.tok(true);
            let l = if r.ty == Tk::Op && self.val_is(&r, ")") {
                self.mk_tok(")", &r)?
            } else {
                self.mk(")", self.end(s), self.end(s), Vec::new())?
            };
            let mut kids = vec![s];
            kids.extend(i);
            kids.push(l);
            let c = self.mk("subshell", self.start(s), self.end(l), kids)?;
            return Ok(Some(self.redirects_after(c, false)?));
        }
        if t.ty == Tk::Op && self.val_is(&t, "((") {
            let s = self.mk_tok("((", &t)?;
            let i = self.arith_list("))", Ar::Var)?;
            let r = self.tok(true);
            let l = if self.val_is(&r, "))") {
                self.mk_tok("))", &r)?
            } else {
                self.mk("))", self.end(s), self.end(s), Vec::new())?
            };
            let mut kids = vec![s];
            kids.extend(i);
            kids.push(l);
            return Ok(Some(self.mk("compound_statement", self.start(s), self.end(l), kids)?));
        }
        if t.ty == Tk::Op && self.val_is(&t, "{") {
            let s = self.mk_tok("{", &t)?;
            let i = self.stmts(Some("}"))?;
            let r = self.tok(true);
            let l = if r.ty == Tk::Op && self.val_is(&r, "}") {
                self.mk_tok("}", &r)?
            } else {
                self.mk("}", self.end(s), self.end(s), Vec::new())?
            };
            let mut kids = vec![s];
            kids.extend(i);
            kids.push(l);
            let c = self.mk("compound_statement", self.start(s), self.end(l), kids)?;
            return Ok(Some(self.redirects_after(c, false)?));
        }
        if t.ty == Tk::Op && (self.val_is(&t, "[") || self.val_is(&t, "[[")) {
            let single = self.val_is(&t, "[");
            let close = if single { "]" } else { "]]" };
            let s = self.mk_tok(if single { "[" } else { "[[" }, &t)?;
            let r = self.save();
            let mut l = self.test_or(close)?;
            self.h();
            if single && self.la(0) != u(b']') {
                self.restore(r);
                let b = self.stop_bracket;
                self.stop_bracket = true;
                let p = self.command()?;
                self.stop_bracket = b;
                match p {
                    Some(p) if self.kind(p) == "redirected_statement" => l = Some(p),
                    _ => {
                        self.restore(r);
                        l = self.test_or(close)?;
                    }
                }
                self.h();
            }
            let c = self.save();
            let f = self.tok(false);
            let un = if self.val_is(&f, close) && is_boundary(self.la(0)) && self.la(0) != u(b'(') {
                self.mk_tok(close, &f)?
            } else {
                self.restore(c);
                self.mk(close, self.end(s), self.end(s), Vec::new())?
            };
            let d = match l {
                Some(l) => vec![s, l, un],
                None => vec![s, un],
            };
            let tc = self.mk("test_command", self.start(s), self.end(un), d)?;
            return Ok(Some(self.redirects_after(tc, false)?));
        }
        if t.ty == Tk::Word && is_boundary(self.la(0)) {
            let v = self.val(&t).to_vec();
            if eq(&v, "if") {
                let n = self.if_stmt(&t)?;
                return Ok(Some(self.redirects_after(n, true)?));
            }
            if eq(&v, "while") || eq(&v, "until") {
                let n = self.while_stmt(&t)?;
                return Ok(Some(self.redirects_after(n, true)?));
            }
            if eq(&v, "for") || eq(&v, "select") {
                let n = self.for_stmt(&t)?;
                return Ok(Some(self.redirects_after(n, true)?));
            }
            if eq(&v, "case") {
                let n = self.case_stmt(&t)?;
                return Ok(Some(self.redirects_after(n, true)?));
            }
            if eq(&v, "function") {
                return Ok(Some(self.function_def(&t)?));
            }
            if DECL.iter().any(|d| eq(&v, d)) {
                let n = self.declaration(&t)?;
                return Ok(Some(self.redirects_after(n, false)?));
            }
            if eq(&v, "unset") || eq(&v, "unsetenv") {
                let s = self.save();
                self.h();
                let mut i = false;
                if self.la(0) == u(b'(') {
                    self.tok(true);
                    self.h();
                    i = self.la(0) == u(b')');
                }
                self.restore(s);
                if !i {
                    let n = self.unset(&t)?;
                    return Ok(Some(self.redirects_after(n, false)?));
                }
            }
        }
        self.restore(o);
        self.simple_command()
    }

    /// 原文 `Fe`：简单命令（前缀赋值 / 重定向、命令名、参数），以及 `name() …` 函数定义。
    fn simple_command(&mut self) -> R<Option<Id>> {
        let o = self.b;
        let mut t = Vec::new();
        let mut s = Vec::new();
        loop {
            self.h();
            if let Some(m) = self.assignment()? {
                t.push(m);
                continue;
            }
            if let Some(r) = self.redirect(false)? {
                s.push(r);
                continue;
            }
            break;
        }
        self.h();
        let i = self.save();
        let r = self.tok(true);
        let stop = matches!(r.ty, Tk::Eof | Tk::Newline | Tk::Comment | Tk::Backtick)
            || (r.ty == Tk::Op && !self.val_in(&r, &["{", "[", "[["]))
            || (r.ty == Tk::Word && self.val_in(&r, &RESERVED) && !self.val_is(&r, "in"));
        if stop {
            self.restore(i);
            if t.len() == 1 && s.is_empty() {
                return Ok(Some(t[0]));
            }
            if !s.is_empty() && t.is_empty() {
                let m = s[s.len() - 1];
                return Ok(Some(self.mk(
                    "redirected_statement",
                    self.start(s[0]),
                    self.end(m),
                    s,
                )?));
            }
            if t.len() > 1 && s.is_empty() {
                let m = t[t.len() - 1];
                return Ok(Some(self.mk(
                    "variable_assignments",
                    self.start(t[0]),
                    self.end(m),
                    t,
                )?));
            }
            if !t.is_empty() || !s.is_empty() {
                let mut m = t;
                m.extend(s);
                let last = m[m.len() - 1];
                return Ok(Some(self.mk("command", o, self.end(last), m)?));
            }
            return Ok(None);
        }
        self.restore(i);
        let l = self.save();
        let c = self.word()?;
        if let Some(c) = c
            && self.kind(c) == "word"
        {
            self.h();
            let mut m = false;
            if self.la(0) == u(b'(') {
                let sv = self.save();
                self.tok(true);
                self.h();
                m = self.la(0) == u(b')');
                self.restore(sv);
            }
            if m {
                let open = self.tok(true);
                self.h();
                let close = self.tok(true);
                let n = self.mk_tok("(", &open)?;
                let rr = self.mk_tok(")", &close)?;
                self.h();
                self.skip_newlines();
                if let Some(b) = self.command()? {
                    let mut hh = vec![b];
                    if self.kind(b) == "redirected_statement"
                        && self.nodes[b].children.len() >= 2
                        && self.kind(self.nodes[b].children[0]) == "compound_statement"
                    {
                        hh = self.kids(b);
                    }
                    let re = hh[hh.len() - 1];
                    let mut kids = vec![c, n, rr];
                    kids.extend(hh);
                    return Ok(Some(self.mk(
                        "function_definition",
                        self.start(c),
                        self.end(re),
                        kids,
                    )?));
                }
            }
        }
        self.restore(l);
        let Some(f) = self.word()? else {
            if t.len() == 1 {
                return Ok(Some(t[0]));
            }
            return Ok(None);
        };
        let un = self.mk("command_name", self.start(f), self.end(f), vec![f])?;
        let mut d = Vec::new();
        let mut b = Vec::new();
        let mut p = None;
        loop {
            self.h();
            if let Some(m) = self.redirect(true)? {
                match self.kind(m) {
                    "heredoc_redirect" => p = Some(m),
                    "herestring_redirect" => d.push(m),
                    _ => b.push(m),
                }
                continue;
            }
            if !b.is_empty() {
                break;
            }
            if self.stop_bracket && self.la(0) == u(b']') {
                break;
            }
            let sv = self.save();
            let st = self.tok(false);
            if matches!(st.ty, Tk::Eof | Tk::Newline | Tk::Comment)
                || (st.ty == Tk::Op
                    && self.val_in(
                        &st,
                        &["|", "|&", "&&", "||", ";", ";;", ";&", ";;&", "&", ")", "}", "))"],
                    ))
            {
                self.restore(sv);
                break;
            }
            self.restore(sv);
            let Some(nn) = self.word()? else {
                if self.la(0) == u(b'(') {
                    let rt = self.tok(true);
                    let bb = self.mk_tok("(", &rt)?;
                    let hh = self.stmts(Some(")"))?;
                    let re = self.save();
                    let oe = self.tok(true);
                    let g = if oe.ty == Tk::Op && self.val_is(&oe, ")") {
                        self.mk_tok(")", &oe)?
                    } else {
                        self.restore(re);
                        self.mk(")", self.end(bb), self.end(bb), Vec::new())?
                    };
                    let mut kids = vec![bb];
                    kids.extend(hh);
                    kids.push(g);
                    d.push(self.mk("subshell", self.start(bb), self.end(g), kids)?);
                    continue;
                }
                break;
            };
            if self.kind(nn) == "word" && eq(self.text(nn), "=") {
                d.push(self.mk("ERROR", self.start(nn), self.end(nn), vec![nn])?);
                continue;
            }
            if (self.kind(nn) == "word" || self.kind(nn) == "concatenation")
                && self.la(0) == u(b'(')
                && self.b == self.end(nn)
            {
                d.push(self.mk("ERROR", self.start(nn), self.end(nn), vec![nn])?);
                continue;
            }
            d.push(nn);
        }
        let mut x = t;
        x.extend(s.iter().copied());
        x.push(un);
        x.extend(d);
        let last = self.end(x[x.len() - 1]);
        let w = self.start(x[0]);
        let g = self.mk("command", w, last, x)?;
        if let Some(p) = p {
            self.read_heredocs()?;
            let m = if self.heredocs.is_empty() { None } else { Some(self.heredocs.remove(0)) };
            if let Some(m) = m
                && self.nodes[p].children.len() >= 2
            {
                let body = if m.quoted {
                    Vec::new()
                } else {
                    self.heredoc_body(m.body_start, m.body_end)?
                };
                let nb = self.mk("heredoc_body", m.body_start, m.body_end, body)?;
                let ne = self.mk("heredoc_end", m.end_start, m.end_end, Vec::new())?;
                self.nodes[p].children.extend([nb, ne]);
                self.nodes[p].end = m.end_end;
            }
            let mut rest = s.clone();
            rest.push(p);
            rest.extend(b);
            let st = if s.is_empty() { self.start(g) } else { self.start(g).min(self.start(s[0])) };
            let mut kids = vec![g];
            kids.extend(rest);
            return Ok(Some(self.mk("redirected_statement", st, self.end(p), kids)?));
        }
        if !b.is_empty() {
            let m = b[b.len() - 1];
            let mut kids = vec![g];
            kids.extend(b);
            return Ok(Some(self.mk("redirected_statement", self.start(g), self.end(m), kids)?));
        }
        Ok(Some(g))
    }

    /// 原文 `M`：复合命令之后的重定向。
    fn redirects_after(&mut self, o: Id, herestring: bool) -> R<Id> {
        let mut s = Vec::new();
        loop {
            self.h();
            let r = self.save();
            let Some(l) = self.redirect(false)? else { break };
            if self.kind(l) == "herestring_redirect" && !herestring {
                self.restore(r);
                break;
            }
            s.push(l);
        }
        if s.is_empty() {
            return Ok(o);
        }
        let i = s[s.len() - 1];
        let mut kids = vec![o];
        kids.extend(s);
        self.mk("redirected_statement", self.start(o), self.end(i), kids)
    }

    /// 原文 `Se`：`name=value` / `name+=value` / `name[i]=value` / `name=(…)`。
    fn assignment(&mut self) -> R<Option<Id>> {
        let o = self.save();
        self.h();
        let t = self.b;
        if !is_letter(self.la(0)) {
            self.restore(o);
            return Ok(None);
        }
        while is_alnum(self.la(0)) {
            self.adv();
        }
        let s = self.b;
        let mut i = s;
        if self.la(0) == u(b'[') {
            self.adv();
            let mut g = 1;
            while self.i < self.len && g > 0 {
                let m = self.la(0);
                if m == u(b'[') {
                    g += 1;
                } else if m == u(b']') {
                    g -= 1;
                }
                self.adv();
            }
            i = self.b;
        }
        let (r, l) = (self.la(0), self.la(1));
        let op = if r == u(b'=') && l != u(b'=') {
            "="
        } else if r == u(b'+') && l == u(b'=') {
            "+="
        } else {
            self.restore(o);
            return Ok(None);
        };
        let f = self.mk("variable_name", t, s, Vec::new())?;
        let mut un = f;
        if i > s {
            let g = self.mk("[", s, s + 1, Vec::new())?;
            let m = self.subscript_simple(s + 1, i - 1)?;
            let cl = self.mk("]", i - 1, i, Vec::new())?;
            un = self.mk("subscript", t, i, vec![f, g, m, cl])?;
        }
        let d = self.b;
        self.adv();
        if op == "+=" {
            self.adv();
        }
        let b = self.b;
        let p = self.mk(op, d, b, Vec::new())?;
        let mut x = None;
        if self.la(0) == u(b'(') {
            let g = self.tok(true);
            let m = self.mk_tok("(", &g)?;
            let mut items = vec![m];
            loop {
                self.h();
                if self.la(0) == u(b')') {
                    break;
                }
                let Some(rr) = self.word()? else { break };
                items.push(rr);
            }
            let st = self.tok(true);
            let n = if self.val_is(&st, ")") {
                self.mk_tok(")", &st)?
            } else {
                self.mk(")", self.end(m), self.end(m), Vec::new())?
            };
            items.push(n);
            x = Some(self.mk("array", self.start(m), self.end(n), items)?);
        } else {
            let g = self.la(0);
            if g != EOF && !in_set(g, b" \t\n;&|)}") {
                x = self.word()?;
            }
        }
        let (kids, w) = match x {
            Some(x) => (vec![un, p, x], self.end(x)),
            None => (vec![un, p], b),
        };
        Ok(Some(self.mk("variable_assignment", t, w, kids)?))
    }

    /// 原文 `qe`：`${name[…]}` 的下标。
    fn subscript_index(&mut self) -> R<Option<Id>> {
        self.h();
        let o = self.la(0);
        if (o == u(b'@') || o == u(b'*')) && self.la(1) == u(b']') {
            let t = self.b;
            self.adv();
            return Ok(Some(self.mk("word", t, self.b, Vec::new())?));
        }
        if o == u(b'(') && self.la(1) == u(b'(') {
            let t = self.b;
            self.adv();
            self.adv();
            let s = self.mk("((", t, self.b, Vec::new())?;
            let i = self.arith("))", Ar::Var)?;
            self.h();
            let r = if self.la(0) == u(b')') && self.la(1) == u(b')') {
                let c = self.b;
                self.adv();
                self.adv();
                self.mk("))", c, self.b, Vec::new())?
            } else {
                self.mk("))", self.b, self.b, Vec::new())?
            };
            let kids = match i {
                Some(i) => vec![s, i, r],
                None => vec![s, r],
            };
            return Ok(Some(self.mk("compound_statement", self.start(s), self.end(r), kids)?));
        }
        self.arith("]", Ar::Word)
    }

    /// 原文 `Ke`：赋值左边 `name[…]` 的下标。
    fn subscript_simple(&mut self, o: usize, t: usize) -> R<Id> {
        let s = self.z(o, t).to_vec();
        if is_uint(&s) {
            return self.mk("number", o, t, Vec::new());
        }
        let is_var = s.len() >= 2
            && s[0] == u16::from(b'$')
            && is_letter(i32::from(s[1]))
            && s[2..].iter().all(|c| is_alnum(i32::from(*c)));
        if is_var {
            let r = self.mk("$", o, o + 1, Vec::new())?;
            let l = self.mk("variable_name", o + 1, t, Vec::new())?;
            return self.mk("simple_expansion", o, t, vec![r, l]);
        }
        if s.len() == 2 && s[0] == u16::from(b'$') && is_special(i32::from(s[1])) {
            let r = self.mk("$", o, o + 1, Vec::new())?;
            let l = self.mk("special_variable_name", o + 1, t, Vec::new())?;
            return self.mk("simple_expansion", o, t, vec![r, l]);
        }
        self.mk("word", o, t, Vec::new())
    }

    /// 原文 `be`：重定向之后能不能再接一个目标词。
    fn redirect_target_ok(&self) -> bool {
        let o = self.la(0);
        if o == EOF || o == u(b'\n') {
            return false;
        }
        if in_set(o, b"|&;()") {
            return false;
        }
        if o == u(b'<') || o == u(b'>') {
            return self.la(1) == u(b'(');
        }
        if is_digit(o) {
            let mut t = self.i;
            while t < self.len && is_digit(self.c(t)) {
                t += 1;
            }
            let s = self.c(t);
            if s == u(b'>') || s == u(b'<') {
                return false;
            }
        }
        if o == u(b'}') {
            return false;
        }
        if self.stop_bracket && o == u(b']') {
            return false;
        }
        true
    }

    /// 原文 `U`：一个重定向；`multi` 为真时（命令名之后）目标可以连着好几个词。
    fn redirect(&mut self, multi: bool) -> R<Option<Id>> {
        let t = self.save();
        self.h();
        let mut s = None;
        if is_digit(self.la(0)) {
            let l = self.b;
            let mut c = self.i;
            while c < self.len && is_digit(self.c(c)) {
                c += 1;
            }
            let f = self.c(c);
            if f == u(b'>') || f == u(b'<') {
                while self.i < c {
                    self.adv();
                }
                s = Some(self.mk("file_descriptor", l, self.b, Vec::new())?);
            }
        }
        if s.is_none() && self.la(0) == u(b'{') {
            let mut l = self.i + 1;
            if l < self.len && is_letter(self.c(l)) {
                while l < self.len && is_alnum(self.c(l)) {
                    l += 1;
                }
                if self.c(l) == u(b'[') {
                    let (mut c, mut f, mut q) = (0, false, false);
                    while l < self.len {
                        let d = self.c(l);
                        if f {
                            if d == u(b'\'') {
                                f = false;
                            }
                        } else if q {
                            if d == u(b'\\') && l + 1 < self.len {
                                l += 1;
                            } else if d == u(b'"') {
                                q = false;
                            }
                        } else if d == u(b'\\') && l + 1 < self.len {
                            l += 1;
                        } else if d == u(b'\'') {
                            f = true;
                        } else if d == u(b'"') {
                            q = true;
                        } else if d == u(b'[') {
                            c += 1;
                        } else if d == u(b']') {
                            c -= 1;
                            if c == 0 {
                                l += 1;
                                break;
                            }
                        } else if d == u(b'\n') {
                            break;
                        }
                        l += 1;
                    }
                }
                if self.c(l) == u(b'}') {
                    let c = self.c(l + 1);
                    if c == u(b'>') || c == u(b'<') {
                        let f = self.b;
                        while self.i <= l {
                            self.adv();
                        }
                        s = Some(self.mk("variable_name", f, self.b, Vec::new())?);
                    }
                }
            }
        }
        let i = self.tok(false);
        if i.ty != Tk::Op {
            self.restore(t);
            return Ok(None);
        }
        if self.val_is(&i, "<<<") {
            let l = self.mk_tok("<<<", &i)?;
            self.h();
            let c = self.word()?;
            let f = c.map_or(self.end(l), |c| self.end(c));
            let mut kids: Vec<Id> = s.into_iter().collect();
            kids.push(l);
            kids.extend(c);
            let st = s.map_or(self.start(l), |s| self.start(s));
            return Ok(Some(self.mk("herestring_redirect", st, f, kids)?));
        }
        if self.val_is(&i, "<<") || self.val_is(&i, "<<-") {
            let strip_tabs = self.val_is(&i, "<<-");
            let k = self.vkind(&i);
            let l = self.mk_tok(k, &i)?;
            self.skip_blank();
            let c = self.b;
            let mut quoted = false;
            let mut delim: Vec<u16> = Vec::new();
            let d = self.la(0);
            if d == u(b'\'') || d == u(b'"') {
                quoted = true;
                self.adv();
                while self.i < self.len && self.la(0) != d {
                    delim.push(self.src[self.i]);
                    self.adv();
                }
                if self.i < self.len {
                    self.adv();
                }
            } else if d == u(b'\\') {
                quoted = true;
                self.adv();
                if self.i < self.len && self.la(0) != u(b'\n') {
                    delim.push(self.src[self.i]);
                    self.adv();
                }
                while self.i < self.len && is_alnum(self.la(0)) {
                    delim.push(self.src[self.i]);
                    self.adv();
                }
            } else {
                while self.i < self.len && is_delim_char(self.la(0)) {
                    delim.push(self.src[self.i]);
                    self.adv();
                }
            }
            let b = self.b;
            if d == u(b'"') && has_unit(&delim, b"`$\\\n") {
                return Err(Abort);
            }
            if self.i < self.len && !in_set(self.la(0), b" \t\n<>|&;()") {
                return Err(Abort);
            }
            if delim.iter().any(|c| (0xD800..=0xDFFF).contains(c)) {
                return Err(Abort);
            }
            let p = self.mk("heredoc_start", c, b, Vec::new())?;
            self.heredocs.push(Heredoc {
                delim,
                strip_tabs,
                quoted,
                body_start: 0,
                body_end: 0,
                end_start: 0,
                end_end: 0,
            });
            let mut x: Vec<Id> = s.into_iter().collect();
            x.extend([l, p]);
            let st = s.map_or(self.start(l), |s| self.start(s));
            loop {
                self.h();
                let w = self.la(0);
                if w == u(b'\n') || w == EOF || self.i >= self.len {
                    break;
                }
                if w == u(b'>') || w == u(b'<') || is_digit(w) {
                    let sv = self.save();
                    let r = self.redirect(false)?;
                    if let Some(r) = r
                        && self.kind(r) == "file_redirect"
                    {
                        x.push(r);
                        continue;
                    }
                    self.restore(sv);
                }
                if w == u(b'|') && self.la(1) != u(b'|') {
                    let b0 = self.b;
                    self.adv();
                    self.h();
                    let mut ss = Vec::new();
                    while let Some(n) = self.command()? {
                        ss.push(n);
                        self.h();
                        if self.la(0) == u(b'|') && self.la(1) != u(b'|') {
                            let rb = self.b;
                            self.adv();
                            ss.push(self.mk("|", rb, self.b, Vec::new())?);
                            self.h();
                            continue;
                        }
                        break;
                    }
                    if ss.is_empty() {
                        x.push(self.mk("ERROR", b0, self.b, Vec::new())?);
                    } else {
                        let n = ss[ss.len() - 1];
                        x.push(self.mk("pipeline", self.start(ss[0]), self.end(n), ss)?);
                    }
                    continue;
                }
                if (w == u(b'&') && self.la(1) == u(b'&'))
                    || (w == u(b'|') && self.la(1) == u(b'|'))
                {
                    let b0 = self.b;
                    self.adv();
                    self.adv();
                    self.h();
                    match self.command()? {
                        Some(n) => x.push(n),
                        None => x.push(self.mk("ERROR", b0, self.b, Vec::new())?),
                    }
                    continue;
                }
                if in_set(w, b"&;()") {
                    let b0 = self.b;
                    while self.i < self.len && self.la(0) != u(b'\n') {
                        self.adv();
                    }
                    x.push(self.mk("ERROR", b0, self.b, Vec::new())?);
                    break;
                }
                if let Some(g) = self.word()? {
                    x.push(g);
                    continue;
                }
                let m = self.b;
                while self.i < self.len && self.la(0) != u(b'\n') {
                    self.adv();
                }
                if self.b > m {
                    x.push(self.mk("ERROR", m, self.b, Vec::new())?);
                }
                break;
            }
            return Ok(Some(self.mk("heredoc_redirect", st, self.b, x)?));
        }
        if self.val_is(&i, "<&-") || self.val_is(&i, ">&-") {
            let k = self.vkind(&i);
            let l = self.mk_tok(k, &i)?;
            let mut c: Vec<Id> = s.into_iter().collect();
            c.push(l);
            self.h();
            let f = self.save();
            let un = if self.redirect_target_ok() { self.word()? } else { None };
            match un {
                Some(w) => c.push(w),
                None => self.restore(f),
            }
            let d = s.map_or(self.start(l), |s| self.start(s));
            let b = un.map_or(self.end(l), |w| self.end(w));
            return Ok(Some(self.mk("file_redirect", d, b, c)?));
        }
        if self.val_in(&i, &[">", ">>", ">&", ">|", "&>", "&>>", "<", "<&"]) {
            let k = self.vkind(&i);
            let l = self.mk_tok(k, &i)?;
            let mut c: Vec<Id> = s.into_iter().collect();
            c.push(l);
            let mut f = self.end(l);
            let mut n = 0;
            loop {
                self.h();
                if !self.redirect_target_ok() {
                    break;
                }
                if !multi && n >= 1 {
                    break;
                }
                let (b0, p) = (self.la(0), self.la(1));
                let x = if (b0 == u(b'<') || b0 == u(b'>')) && p == u(b'(') {
                    self.process_sub()?
                } else {
                    self.word()?
                };
                let Some(x) = x else { break };
                c.push(x);
                f = self.end(x);
                n += 1;
            }
            let d = s.map_or(self.start(l), |s| self.start(s));
            return Ok(Some(self.mk("file_redirect", d, f, c)?));
        }
        self.restore(t);
        Ok(None)
    }

    /// 原文 `te`：`<(…)` / `>(…)`。
    fn process_sub(&mut self) -> R<Option<Id>> {
        let o = self.la(0);
        if (o != u(b'<') && o != u(b'>')) || self.la(1) != u(b'(') {
            return Ok(None);
        }
        let t = self.b;
        self.adv();
        self.adv();
        let s = self.mk(if o == u(b'<') { "<(" } else { ">(" }, t, self.b, Vec::new())?;
        let i = self.stmts(Some(")"))?;
        self.h();
        let r = if self.la(0) == u(b')') {
            let l = self.b;
            self.adv();
            self.mk(")", l, self.b, Vec::new())?
        } else {
            self.mk(")", self.b, self.b, Vec::new())?
        };
        let mut kids = vec![s];
        kids.extend(i);
        kids.push(r);
        Ok(Some(self.mk("process_substitution", t, self.end(r), kids)?))
    }

    /// 原文 `ce`：跳到下一行，读 here-doc 正文到定界符行。注意原文读到第一个定界符就整个返回。
    fn read_heredocs(&mut self) -> R<()> {
        while self.i < self.len && self.c(self.i) != u(b'\n') {
            self.adv();
        }
        if self.i < self.len {
            self.adv();
        }
        for k in 0..self.heredocs.len() {
            self.heredocs[k].body_start = self.b;
            let delim = self.heredocs[k].delim.clone();
            let strip = self.heredocs[k].strip_tabs;
            let t = delim.len();
            if strip && delim.first() == Some(&u16::from(b'\t')) {
                return Err(Abort);
            }
            while self.i < self.len {
                let (s, ib) = (self.i, self.b);
                let mut r = s;
                if strip {
                    while r < self.len && self.c(r) == u(b'\t') {
                        r += 1;
                    }
                }
                if self.src.get(r..).is_some_and(|rest| rest.starts_with(&delim)) {
                    let l = r + t;
                    let c = self.c(l);
                    if c == EOF || c == u(b'\n') || c == u(b'\r') {
                        self.heredocs[k].body_end = ib;
                        while self.i < r {
                            self.adv();
                        }
                        self.heredocs[k].end_start = self.b;
                        for _ in 0..t {
                            self.adv();
                        }
                        self.heredocs[k].end_end = self.b;
                        if self.i < self.len && self.c(self.i) == u(b'\n') {
                            self.adv();
                        }
                        return Ok(());
                    }
                    let mut f = l;
                    while f < self.len {
                        let x = self.c(f);
                        if x == u(b'\n') {
                            break;
                        }
                        if in_set(x, b")`}") {
                            return Err(Abort);
                        }
                        f += 1;
                    }
                }
                while self.i < self.len && self.c(self.i) != u(b'\n') {
                    self.adv();
                }
                if self.i < self.len {
                    self.adv();
                }
            }
            let b = self.b;
            let h = &mut self.heredocs[k];
            (h.body_end, h.end_start, h.end_end) = (b, b, b);
        }
        Ok(())
    }

    /// 原文 `Ue`：展开型 here-doc 正文里的展开 / 命令替换（反引号按 `$` 一样走 [`Self::dollar`]）。
    fn heredoc_body(&mut self, o: usize, t: usize) -> R<Vec<Id>> {
        let s = self.save();
        self.seek(o);
        let mut i = Vec::new();
        let mut r = self.b;
        let mut l = false;
        while self.b < t {
            let c = self.la(0);
            if c == u(b'\\') {
                let f = self.la(1);
                if in_set(f, b"$`\\") {
                    self.adv();
                    self.adv();
                    continue;
                }
                self.adv();
                continue;
            }
            if c == u(b'$') || c == u(b'`') {
                if c == u(b'$') && self.la(1) == u(b'\'') {
                    self.adv();
                    continue;
                }
                let f = self.b;
                let un = self.dollar()?;
                if matches!(
                    self.kind(un),
                    "simple_expansion"
                        | "expansion"
                        | "command_substitution"
                        | "arithmetic_expansion"
                ) {
                    if l && f > r {
                        i.push(self.mk("heredoc_content", r, f, Vec::new())?);
                    }
                    i.push(un);
                    r = self.b;
                    l = true;
                }
                continue;
            }
            self.adv();
        }
        if l {
            i.push(self.mk("heredoc_content", r, t, Vec::new())?);
        }
        self.restore(s);
        Ok(i)
    }

    // ---- 词 ----

    /// 原文 `A`：一个词（可能由多段拼成 `concatenation`）。
    fn word(&mut self) -> R<Option<Id>> {
        self.guard()?;
        self.word_inner()
    }

    fn word_inner(&mut self) -> R<Option<Id>> {
        self.h();
        let mut t = Vec::new();
        while self.i < self.len {
            let r = self.la(0);
            if r == EOF || in_set(r, b" \t\n\r|&;()") {
                break;
            }
            if r == u(b'<') || r == u(b'>') {
                if self.la(1) == u(b'(') {
                    if let Some(c) = self.process_sub()? {
                        t.push(c);
                    }
                    continue;
                }
                break;
            }
            if r == u(b'"') {
                t.push(self.dquote()?);
                continue;
            }
            if r == u(b'\'') {
                let c = self.tok(false);
                t.push(self.mk_tok("raw_string", &c)?);
                continue;
            }
            if r == u(b'$') {
                let c = self.la(1);
                if c == u(b'\'') {
                    let x = self.tok(false);
                    t.push(self.mk_tok("ansi_c_string", &x)?);
                    continue;
                }
                if c == u(b'"') {
                    let b0 = self.b;
                    self.adv();
                    t.push(self.mk("$", b0, b0 + 1, Vec::new())?);
                    t.push(self.dquote()?);
                    continue;
                }
                if c == u(b'`') {
                    let b0 = self.b;
                    self.adv();
                    t.push(self.mk("$", b0, b0 + 1, Vec::new())?);
                    continue;
                }
                t.push(self.dollar()?);
                continue;
            }
            if r == u(b'`') {
                if self.in_backtick > 0 {
                    break;
                }
                t.push(self.backtick()?);
                continue;
            }
            if r == u(b'{') {
                if let Some(c) = self.brace_range()? {
                    t.push(c);
                    continue;
                }
                let f = self.la(1);
                if f == EOF || in_set(f, b";|&\n) \t") {
                    let d = self.b;
                    self.adv();
                    t.push(self.mk("word", d, self.b, Vec::new())?);
                    continue;
                }
                let un = self.brace_word()?;
                t.extend(un);
                continue;
            }
            if r == u(b'}') || r == u(b'[') || r == u(b']') {
                let c = self.b;
                self.adv();
                t.push(self.mk("word", c, self.b, Vec::new())?);
                continue;
            }
            let Some(l) = self.bare_word()? else { break };
            if self.kind(l) == "word"
                && is_base_prefix(self.text(l))
                && self.la(0) == u(b'$')
                && (self.la(1) == u(b'{') || self.la(1) == u(b'('))
            {
                let c = self.dollar()?;
                t.push(self.mk("number", self.start(l), self.end(c), vec![c])?);
                continue;
            }
            t.push(l);
        }
        if t.is_empty() {
            return Ok(None);
        }
        if t.len() == 1 {
            return Ok(Some(t[0]));
        }
        let (s, i) = (t[0], t[t.len() - 1]);
        Ok(Some(self.mk("concatenation", self.start(s), self.end(i), t)?))
    }

    /// 原文 `He`：不含引号 / 展开的一段裸词。
    fn bare_word(&mut self) -> R<Option<Id>> {
        let (o, t) = (self.b, self.i);
        while self.i < self.len {
            let r = self.la(0);
            if r == u(b'\\') {
                if self.i + 1 >= self.len || self.c(self.i + 1) == u(b'\n') {
                    break;
                }
                self.adv();
                self.adv();
                continue;
            }
            if r == EOF || in_set(r, b" \t\n\r|&;()<>\"'$`{}[]") {
                break;
            }
            self.adv();
        }
        if self.b == o {
            return Ok(None);
        }
        let k = if is_int(self.slice(t, self.i)) { "number" } else { "word" };
        Ok(Some(self.mk(k, o, self.b, Vec::new())?))
    }

    /// 原文 `Qe`：`{a..z}` / `{1..9}`。
    fn brace_range(&mut self) -> R<Option<Id>> {
        let o = self.save();
        if self.la(0) != u(b'{') {
            return Ok(None);
        }
        let t = self.b;
        self.adv();
        let s = self.b;
        let i = self.b;
        while is_digit(self.la(0)) || is_letter(self.la(0)) {
            self.adv();
        }
        let r = self.b;
        if r == i || self.la(0) != u(b'.') || self.la(1) != u(b'.') {
            self.restore(o);
            return Ok(None);
        }
        let l = self.b;
        self.adv();
        self.adv();
        let c = self.b;
        let f = self.b;
        while is_digit(self.la(0)) || is_letter(self.la(0)) {
            self.adv();
        }
        let un = self.b;
        if un == f || self.la(0) != u(b'}') {
            self.restore(o);
            return Ok(None);
        }
        let d = self.b;
        self.adv();
        let b = self.b;
        let p = self.z(i, r).to_vec();
        let x = self.z(f, un).to_vec();
        let (ip, wx) = (is_uint(&p), is_uint(&x));
        if ip != wx {
            self.restore(o);
            return Ok(None);
        }
        if !ip && (p.len() != 1 || x.len() != 1) {
            self.restore(o);
            return Ok(None);
        }
        let g = if ip { "number" } else { "word" };
        let m = if wx { "number" } else { "word" };
        let k1 = self.mk("{", t, s, Vec::new())?;
        let k2 = self.mk(g, i, r, Vec::new())?;
        let k3 = self.mk("..", l, c, Vec::new())?;
        let k4 = self.mk(m, f, un, Vec::new())?;
        let k5 = self.mk("}", d, b, Vec::new())?;
        Ok(Some(self.mk("brace_expression", t, b, vec![k1, k2, k3, k4, k5])?))
    }

    /// 原文 `Xe`：`{` 打头、不成范围的一串词。
    fn brace_word(&mut self) -> R<Vec<Id>> {
        let o = self.b;
        self.adv();
        let t = self.b;
        let mut s = vec![self.mk("word", o, t, Vec::new())?];
        while self.i < self.len {
            let i = self.la(0);
            if in_set(i, b"}\n;|& \t<>()") {
                break;
            }
            if i == u(b'[') || i == u(b']') {
                let c = self.b;
                self.adv();
                s.push(self.mk("word", c, self.b, Vec::new())?);
                continue;
            }
            let r = self.b;
            while self.i < self.len {
                let c = self.la(0);
                if c == u(b'\\') && self.i + 1 < self.len {
                    self.adv();
                    self.adv();
                    continue;
                }
                if in_set(c, b"}\n;|& \t<>()[]") {
                    break;
                }
                self.adv();
            }
            let l = self.b;
            if l > r {
                let k = if is_int(self.z(r, l)) { "number" } else { "word" };
                s.push(self.mk(k, r, l, Vec::new())?);
            } else {
                break;
            }
        }
        if self.la(0) == u(b'}') {
            let i = self.b;
            self.adv();
            s.push(self.mk("word", i, self.b, Vec::new())?);
        }
        Ok(s)
    }

    /// 原文 `W`：双引号字符串。
    fn dquote(&mut self) -> R<Id> {
        let o = self.b;
        self.adv();
        self.in_dquote += 1;
        let t = self.b;
        let mut i = vec![self.mk("\"", o, t, Vec::new())?];
        let mut r = self.b;
        let mut l = self.i;
        macro_rules! content {
            () => {
                if self.b > r {
                    let seg = self.slice(l, self.i);
                    let blank = !seg.is_empty()
                        && seg.iter().all(|c| *c == u16::from(b' ') || *c == u16::from(b'\t'));
                    if !blank {
                        i.push(self.mk("string_content", r, self.b, Vec::new())?);
                    }
                }
            };
        }
        while self.i < self.len {
            let un = self.la(0);
            if un == u(b'"') {
                break;
            }
            if un == u(b'\\') && self.i + 1 < self.len {
                self.adv();
                self.adv();
                continue;
            }
            if un == u(b'\n') {
                content!();
                self.adv();
                r = self.b;
                l = self.i;
                continue;
            }
            if un == u(b'$') {
                let d = self.la(1);
                if d == u(b'(')
                    || d == u(b'{')
                    || is_letter(d)
                    || is_special(d)
                    || is_digit(d)
                    || d >= 0x80
                    || (is_param_op(d) && self.nonascii_name_end(self.i + 1).is_some())
                {
                    content!();
                    i.push(self.dollar()?);
                    r = self.b;
                    l = self.i;
                    continue;
                }
                if d != u(b'"') && d != EOF {
                    content!();
                    let b0 = self.b;
                    self.adv();
                    i.push(self.mk("$", b0, self.b, Vec::new())?);
                    r = self.b;
                    l = self.i;
                    continue;
                }
            }
            if un == u(b'`') {
                content!();
                i.push(self.backtick()?);
                r = self.b;
                l = self.i;
                continue;
            }
            self.adv();
        }
        content!();
        let f = if self.la(0) == u(b'"') {
            let x = self.b;
            self.adv();
            self.mk("\"", x, self.b, Vec::new())?
        } else {
            self.mk("\"", self.b, self.b, Vec::new())?
        };
        i.push(f);
        self.in_dquote -= 1;
        self.mk("string", o, self.end(f), i)
    }

    /// 原文 `q`：`$` 打头的一切（`$((…))`、`$[…]`、`$(…)`、`${…}`、`$name`、`$'…'`）。
    fn dollar(&mut self) -> R<Id> {
        self.guard()?;
        self.dollar_inner()
    }

    /// 跳过一段引号（`'…'` / `"…"`，双引号里认 `\` 转义），从开引号起。
    fn skip_quoted(&mut self, q: i32) {
        self.adv();
        while self.i < self.len && self.la(0) != q {
            if q == u(b'"') && self.la(0) == u(b'\\') && self.i + 1 < self.len {
                self.adv();
            }
            self.adv();
        }
    }

    /// 跳过 `` `…` `` 或 `$'…'` 的内容（认 `\` 转义），从开头的 `` ` `` / `'` 之后起。
    fn skip_escaped_until(&mut self, q: i32) {
        while self.i < self.len && self.la(0) != q {
            if self.la(0) == u(b'\\') && self.i + 1 < self.len {
                self.adv();
            }
            self.adv();
        }
    }

    fn dollar_inner(&mut self) -> R<Id> {
        let o = self.la(1);
        let t = self.b;
        if o == u(b'(') && self.la(2) == u(b'(') {
            for _ in 0..3 {
                self.adv();
            }
            let c = self.mk("$((", t, self.b, Vec::new())?;
            let f = self.save();
            let un = self.arith_list("))", Ar::Var)?;
            self.h();
            let mut bad = false;
            if self.la(0) != u(b')') || self.la(1) != u(b')') {
                self.arith_recover(f, "))");
                bad = true;
            }
            let d = if self.la(0) == u(b')') && self.la(1) == u(b')') {
                let p = self.b;
                self.adv();
                self.adv();
                self.mk("))", p, self.b, Vec::new())?
            } else {
                self.mk("))", self.b, self.b, Vec::new())?
            };
            let mut kids = vec![c];
            kids.extend(un);
            kids.push(d);
            return self.mk(
                if bad { "ERROR" } else { "arithmetic_expansion" },
                t,
                self.end(d),
                kids,
            );
        }
        if o == u(b'[') {
            self.adv();
            self.adv();
            let c = self.mk("$[", t, self.b, Vec::new())?;
            let f = self.save();
            let un = self.arith_list("]", Ar::Var)?;
            self.h();
            let mut bad = false;
            if self.la(0) != u(b']') {
                self.arith_recover(f, "]");
                bad = true;
            }
            let d = if self.la(0) == u(b']') {
                let p = self.b;
                self.adv();
                self.mk("]", p, self.b, Vec::new())?
            } else {
                self.mk("]", self.b, self.b, Vec::new())?
            };
            let mut kids = vec![c];
            kids.extend(un);
            kids.push(d);
            return self.mk(
                if bad { "ERROR" } else { "arithmetic_expansion" },
                t,
                self.end(d),
                kids,
            );
        }
        if o == u(b'(') {
            self.adv();
            self.adv();
            let c = self.mk("$(", t, self.b, Vec::new())?;
            let f = self.in_dquote;
            self.in_dquote = 0;
            let mut un = self.stmts(Some(")"))?;
            self.in_dquote = f;
            self.h();
            let mut bad = false;
            let d;
            if self.la(0) == u(b')') {
                let p = self.b;
                self.adv();
                d = self.mk(")", p, self.b, Vec::new())?;
            } else {
                bad = true;
                let p = self.b;
                let mut x = 1;
                while self.i < self.len {
                    let ch = self.la(0);
                    if ch == u(b'\\') && self.i + 1 < self.len {
                        self.adv();
                        self.adv();
                        continue;
                    }
                    if ch == u(b'"') || ch == u(b'\'') {
                        self.skip_quoted(ch);
                        if self.i < self.len {
                            self.adv();
                        }
                        continue;
                    }
                    if ch == u(b'`') {
                        self.adv();
                        self.skip_escaped_until(u(b'`'));
                        if self.i < self.len {
                            self.adv();
                        }
                        continue;
                    }
                    if ch == u(b'$') && self.la(1) == u(b'$') {
                        self.adv();
                        self.adv();
                        continue;
                    }
                    if ch == u(b'$') && self.la(1) == u(b'\'') {
                        self.adv();
                        self.adv();
                        self.skip_escaped_until(u(b'\''));
                        if self.i < self.len {
                            self.adv();
                        }
                        continue;
                    }
                    if ch == u(b'(') {
                        x += 1;
                    } else if ch == u(b')') {
                        x -= 1;
                        if x == 0 {
                            break;
                        }
                    }
                    self.adv();
                }
                un.push(self.mk("ERROR", p, self.b, Vec::new())?);
                if self.la(0) == u(b')') {
                    let i0 = self.b;
                    self.adv();
                    d = self.mk(")", i0, self.b, Vec::new())?;
                } else {
                    d = self.mk("ERROR", self.b, self.b, Vec::new())?;
                }
            }
            if !bad
                && un.len() == 1
                && self.kind(un[0]) == "redirected_statement"
                && self.nodes[un[0]].children.len() == 1
                && self.kind(self.nodes[un[0]].children[0]) == "file_redirect"
            {
                un = self.kids(un[0]);
            }
            let mut kids = vec![c];
            kids.extend(un);
            kids.push(d);
            return self.mk(
                if bad { "ERROR" } else { "command_substitution" },
                t,
                self.end(d),
                kids,
            );
        }
        if o == u(b'{') {
            self.adv();
            self.adv();
            let c = self.mk("${", t, self.b, Vec::new())?;
            let mut f = self.expansion_body()?;
            let mut bad = false;
            while self.la(0) == u(b'\n') {
                self.adv();
            }
            let un;
            if self.la(0) == u(b'}') {
                let b0 = self.b;
                self.adv();
                un = self.mk("}", b0, self.b, Vec::new())?;
            } else {
                bad = true;
                let b0 = self.b;
                let mut p = 1;
                while self.i < self.len {
                    let ch = self.la(0);
                    if ch == u(b'\\') && self.i + 1 < self.len {
                        self.adv();
                        self.adv();
                        continue;
                    }
                    if ch == u(b'"') || ch == u(b'\'') {
                        self.skip_quoted(ch);
                        if self.i < self.len {
                            self.adv();
                        }
                        continue;
                    }
                    if ch == u(b'$') && self.la(1) == u(b'(') {
                        let mut w = 1;
                        self.adv();
                        self.adv();
                        while self.i < self.len && w > 0 {
                            let g = self.la(0);
                            if (g == u(b'\\') && self.i + 1 < self.len)
                                || (g == u(b'$') && self.la(1) == u(b'$'))
                            {
                                self.adv();
                            } else if g == u(b'$') && self.la(1) == u(b'\'') {
                                self.adv();
                                self.adv();
                                self.skip_escaped_until(u(b'\''));
                            } else if g == u(b'"') || g == u(b'\'') {
                                self.skip_quoted(g);
                            } else if g == u(b'`') {
                                self.adv();
                                self.skip_escaped_until(u(b'`'));
                            } else if g == u(b'(') {
                                w += 1;
                            } else if g == u(b')') {
                                w -= 1;
                            }
                            self.adv();
                        }
                        continue;
                    }
                    if ch == u(b'`') {
                        self.adv();
                        self.skip_escaped_until(u(b'`'));
                        if self.i < self.len {
                            self.adv();
                        }
                        continue;
                    }
                    if ch == u(b'$') && self.la(1) == u(b'$') {
                        self.adv();
                        self.adv();
                        continue;
                    }
                    if ch == u(b'$') && self.la(1) == u(b'\'') {
                        self.adv();
                        self.adv();
                        self.skip_escaped_until(u(b'\''));
                        if self.i < self.len {
                            self.adv();
                        }
                        continue;
                    }
                    if ch == u(b'$') && self.la(1) == u(b'{') {
                        p += 1;
                        self.adv();
                    } else if ch == u(b'}') {
                        p -= 1;
                        if p == 0 {
                            break;
                        }
                    }
                    self.adv();
                }
                let x = self.mk("ERROR", b0, self.b, Vec::new())?;
                if self.la(0) == u(b'}') {
                    let i0 = self.b;
                    self.adv();
                    un = self.mk("}", i0, self.b, Vec::new())?;
                } else {
                    un = self.mk("ERROR", self.b, self.b, Vec::new())?;
                }
                f.push(x);
            }
            if !bad
                && self.in_dquote > 0
                && self.z(self.end(c), self.start(un)).contains(&u16::from(b'\''))
            {
                bad = true;
            }
            if !bad && her(self.z(t, self.start(un))) {
                bad = true;
            }
            let mut kids = vec![c];
            kids.extend(f);
            kids.push(un);
            let k = if bad || self.zsh_brace_diff { "ERROR" } else { "expansion" };
            return self.mk(k, t, self.end(un), kids);
        }
        self.adv();
        let s = self.b;
        let i = self.mk("$", t, s, Vec::new())?;
        let r = self.la(0);
        if r >= 0x80 {
            while is_alnum(self.la(0)) || self.la(0) >= 0x80 {
                self.adv();
            }
            return self.mk("ERROR", t, self.b, Vec::new());
        }
        if r == u(b'_') && !is_alnum(self.la(1)) && self.la(1) < 0x80 {
            let c = self.b;
            self.adv();
            let f = self.mk("special_variable_name", c, self.b, Vec::new())?;
            return self.mk("simple_expansion", t, self.b, vec![i, f]);
        }
        if is_letter(r) {
            let c = self.b;
            while is_alnum(self.la(0)) {
                self.adv();
            }
            if self.la(0) >= 0x80 {
                while is_alnum(self.la(0)) || self.la(0) >= 0x80 {
                    self.adv();
                }
                return self.mk("ERROR", t, self.b, Vec::new());
            }
            let f = self.mk("variable_name", c, self.b, Vec::new())?;
            return self.mk("simple_expansion", t, self.b, vec![i, f]);
        }
        let hash_len = r == u(b'#') && !is_param_op(self.la(1));
        if is_param_op(r)
            && !hash_len
            && let Some(c) = self.nonascii_name_end(self.i)
        {
            while self.i < c {
                self.adv();
            }
            return self.mk("ERROR", t, self.b, Vec::new());
        }
        if is_digit(r) {
            let c = self.b;
            self.adv();
            let f = self.mk("variable_name", c, self.b, Vec::new())?;
            return self.mk("simple_expansion", t, self.b, vec![i, f]);
        }
        if is_special(r) {
            let c = self.b;
            self.adv();
            let f = self.mk("special_variable_name", c, self.b, Vec::new())?;
            return self.mk("simple_expansion", t, self.b, vec![i, f]);
        }
        if r == u(b'\'') {
            self.adv();
            self.skip_escaped_until(u(b'\''));
            if self.la(0) == u(b'\'') {
                self.adv();
            }
            return self.mk("ansi_c_string", t, self.b, Vec::new());
        }
        Ok(i)
    }

    /// 原文 `Ze`：`${` 之后的内容（名字、下标、运算符与操作数）。
    fn expansion_body(&mut self) -> R<Vec<Id>> {
        let mut o = Vec::new();
        self.h();
        {
            let (r, l) = (self.la(0), self.la(1));
            if r == u(b'#') && l == u(b'!') && self.la(2) == u(b'}') {
                self.adv();
                self.adv();
                return Ok(o);
            }
            if r == u(b'!') && l == u(b'#') {
                let mut c = 2;
                if self.la(c) == u(b'#') {
                    c += 1;
                }
                if self.la(c) == u(b' ') {
                    c += 1;
                }
                if self.la(c) == u(b'}') {
                    for _ in 0..c {
                        self.adv();
                    }
                    return Ok(o);
                }
            }
        }
        if self.la(0) == u(b'#') {
            let r = self.b;
            self.adv();
            o.push(self.mk("#", r, self.b, Vec::new())?);
        }
        let t = self.la(0);
        if (t == u(b'!') || t == u(b'=') || t == u(b'~'))
            && (is_letter(self.la(1)) || is_digit(self.la(1)))
        {
            let r = self.b;
            self.adv();
            let k: &'static str = if t == u(b'!') {
                "!"
            } else if t == u(b'=') {
                "="
            } else {
                "~"
            };
            o.push(self.mk(k, r, self.b, Vec::new())?);
        }
        self.h();
        if is_letter(self.la(0)) {
            let r = self.b;
            while is_alnum(self.la(0)) {
                self.adv();
            }
            o.push(self.mk("variable_name", r, self.b, Vec::new())?);
        } else if is_digit(self.la(0)) {
            let r = self.b;
            while is_digit(self.la(0)) {
                self.adv();
            }
            o.push(self.mk("variable_name", r, self.b, Vec::new())?);
        } else if is_special(self.la(0)) {
            let r = self.b;
            self.adv();
            o.push(self.mk("special_variable_name", r, self.b, Vec::new())?);
        }
        if self.la(0) == u(b'[') {
            let r = o.last().copied();
            let l = self.b;
            self.adv();
            let c = self.mk("[", l, self.b, Vec::new())?;
            let f = self.subscript_index()?;
            self.h();
            let un = self.b;
            if self.la(0) == u(b']') {
                self.adv();
            }
            let d = self.mk("]", un, self.b, Vec::new())?;
            match r {
                Some(r) => {
                    let kids = match f {
                        Some(f) => vec![r, c, f, d],
                        None => vec![r, c, d],
                    };
                    let n = self.mk("subscript", self.start(r), self.b, kids)?;
                    let last = o.len() - 1;
                    o[last] = n;
                }
                None => {
                    let kids = match f {
                        Some(f) => vec![c, f, d],
                        None => vec![c, d],
                    };
                    o.push(self.mk("ERROR", l, self.b, kids)?);
                }
            }
        }
        self.h();
        let s = self.la(0);
        if (s == u(b'*') || s == u(b'@')) && self.la(1) == u(b'}') {
            let r = self.b;
            self.adv();
            o.push(self.mk(if s == u(b'*') { "*" } else { "@" }, r, self.b, Vec::new())?);
            return Ok(o);
        }
        if s == u(b'@') && is_letter(self.la(1)) {
            let r = self.b;
            self.adv();
            o.push(self.mk("@", r, self.b, Vec::new())?);
            while is_alnum(self.la(0)) {
                self.adv();
            }
            return Ok(o);
        }
        let i = self.la(0);
        if i == u(b':') {
            let r = self.la(1);
            if r == u(b'}') {
                self.adv();
                return Ok(o);
            }
            if !in_set(r, b"-=?+") {
                self.adv();
                self.h();
                let c = if self.la(0) == u(b'-') && is_digit(self.la(1)) {
                    let f = self.b;
                    self.adv();
                    while is_digit(self.la(0)) {
                        self.adv();
                    }
                    Some(self.mk("number", f, self.b, Vec::new())?)
                } else {
                    self.arith(":}", Ar::Var)?
                };
                o.extend(c);
                self.h();
                if self.la(0) == u(b':') {
                    self.adv();
                    self.h();
                    let un = if self.la(0) == u(b'-') && is_digit(self.la(1)) {
                        let d = self.b;
                        self.adv();
                        while is_digit(self.la(0)) {
                            self.adv();
                        }
                        Some(self.mk("number", d, self.b, Vec::new())?)
                    } else {
                        self.arith("}", Ar::Var)?
                    };
                    o.extend(un);
                }
                return Ok(o);
            }
        }
        if in_set(i, b":#%/^,-=?+") {
            let r = self.b;
            let l = self.la(1);
            let op = if i == u(b':') && in_set(l, b"-=?+") {
                self.adv();
                self.adv();
                format!(":{}", l as u8 as char)
            } else if in_set(i, b"#%/^,") && l == i {
                self.adv();
                self.adv();
                format!("{0}{0}", i as u8 as char)
            } else {
                self.adv();
                (i as u8 as char).to_string()
            };
            o.push(self.mk(op.clone(), r, self.b, Vec::new())?);
            let is_pat = matches!(
                op.as_str(),
                "#" | "##" | "%" | "%%" | "/" | "//" | "^" | "^^" | "," | ",,"
            );
            if op == "/" || op == "//" {
                let un = self.la(0);
                if un == u(b'#') || un == u(b'%') {
                    let d = self.b;
                    self.adv();
                    o.push(self.mk(
                        if un == u(b'#') { "#" } else { "%" },
                        d,
                        self.b,
                        Vec::new(),
                    )?);
                }
                if self.la(0) == u(b'"') {
                    o.push(self.dquote()?);
                    if let Some(d) = self.expansion_word("regex", true)? {
                        o.push(d);
                    }
                } else if let Some(d) = self.expansion_word("regex", true)? {
                    o.push(d);
                }
                if self.la(0) == u(b'/') {
                    let d = self.b;
                    self.adv();
                    o.push(self.mk("/", d, self.b, Vec::new())?);
                    if let Some(b) = self.expansion_word("replword", false)? {
                        let ch = self.kids(b);
                        if self.kind(b) == "concatenation"
                            && ch.len() == 2
                            && self.kind(ch[0]) == "command_substitution"
                        {
                            o.extend(ch);
                        } else {
                            o.push(b);
                        }
                    }
                }
            } else if matches!(op.as_str(), "#" | "##" | "%" | "%%") {
                let un = self.expansion_pattern()?;
                o.extend(un);
            } else if let Some(un) =
                self.expansion_word(if is_pat { "regex" } else { "word" }, false)?
            {
                o.push(un);
            }
        }
        Ok(o)
    }

    /// 跳过 `${…}` 里嵌的一段 `${…}`（从 `${` 起），`$(`、`` ` ``、`{` 会标记 zsh 差异。
    fn skip_nested_brace(&mut self) {
        let mut b = 0;
        self.adv();
        self.adv();
        b += 1;
        while self.i < self.len && b > 0 {
            let p = self.la(0);
            if (p == u(b'\\') && self.i + 1 < self.len) || (p == u(b'$') && self.la(1) == u(b'$')) {
                self.adv();
            } else if p == u(b'$') && self.la(1) == u(b'\'') {
                self.adv();
                self.adv();
                self.skip_escaped_until(u(b'\''));
            } else if p == u(b'"') || p == u(b'\'') {
                self.skip_quoted(p);
            } else if p == u(b'`') {
                self.zsh_brace_diff = true;
                self.adv();
                self.skip_escaped_until(u(b'`'));
            } else if p == u(b'$') && self.la(1) == u(b'{') {
                b += 1;
                self.adv();
            } else if (p == u(b'$') && self.la(1) == u(b'(')) || p == u(b'{') {
                self.zsh_brace_diff = true;
            } else if p == u(b'}') {
                b -= 1;
            }
            self.adv();
        }
    }

    /// 跳过 `${…}` 里嵌的一段 `$(…)`（从 `$(` 起）。
    fn skip_nested_paren(&mut self) {
        let mut b = 0;
        self.adv();
        self.adv();
        b += 1;
        while self.i < self.len && b > 0 {
            let p = self.la(0);
            if (p == u(b'\\') && self.i + 1 < self.len) || (p == u(b'$') && self.la(1) == u(b'$')) {
                self.adv();
            } else if p == u(b'$') && self.la(1) == u(b'\'') {
                self.adv();
                self.adv();
                self.skip_escaped_until(u(b'\''));
            } else if p == u(b'"') || p == u(b'\'') {
                self.skip_quoted(p);
            } else if p == u(b'`') {
                self.adv();
                self.skip_escaped_until(u(b'`'));
            } else if p == u(b'(') {
                b += 1;
            } else if p == u(b')') {
                b -= 1;
            }
            self.adv();
        }
    }

    /// 原文 `Y`：`${…}` 运算符之后的操作数（`word` / `regex` / `replword` 三种模式）。
    fn expansion_word(&mut self, mode: &str, slash_stops: bool) -> R<Option<Id>> {
        let s = self.b;
        if mode == "word" && self.la(0) == u(b'(') {
            self.adv();
            let mut un = vec![self.mk("(", s, self.b, Vec::new())?];
            while self.i < self.len {
                self.h();
                let d = self.la(0);
                if d == u(b')') || d == u(b'}') || d == u(b'\n') || d == EOF {
                    break;
                }
                let b = self.b;
                while self.i < self.len {
                    let p = self.la(0);
                    if p == EOF || in_set(p, b")} \t\n") {
                        break;
                    }
                    if p == u(b'\\') && self.i + 1 < self.len {
                        self.adv();
                        self.adv();
                        continue;
                    }
                    if p == u(b'$') && self.la(1) == u(b'$') {
                        self.adv();
                        self.adv();
                        continue;
                    }
                    if p == u(b'$') && self.la(1) == u(b'\'') {
                        self.adv();
                        self.adv();
                        self.skip_escaped_until(u(b'\''));
                        if self.la(0) == u(b'\'') {
                            self.adv();
                        }
                        continue;
                    }
                    if p == u(b'$') && self.la(1) == u(b'(') {
                        self.zsh_brace_diff = true;
                    }
                    if p == u(b'"') || p == u(b'\'') {
                        self.skip_quoted(p);
                        if self.la(0) == p {
                            self.adv();
                        }
                        continue;
                    }
                    if p == u(b'`') {
                        self.zsh_brace_diff = true;
                        self.adv();
                        self.skip_escaped_until(u(b'`'));
                        if self.la(0) == u(b'`') {
                            self.adv();
                        }
                        continue;
                    }
                    if p == u(b'{') {
                        self.zsh_brace_diff = true;
                    }
                    self.adv();
                }
                if self.b > b {
                    un.push(self.mk("word", b, self.b, Vec::new())?);
                } else {
                    break;
                }
            }
            if self.la(0) == u(b')') {
                let d = self.b;
                self.adv();
                un.push(self.mk(")", d, self.b, Vec::new())?);
            }
            while self.la(0) == u(b'\n') {
                self.adv();
            }
            return Ok(Some(self.mk("array", s, self.b, un)?));
        }
        if mode == "regex" {
            while self.i < self.len {
                let un = self.la(0);
                if un == u(b'{') {
                    self.zsh_brace_diff = true;
                }
                if un == u(b'}') {
                    break;
                }
                if slash_stops && un == u(b'/') {
                    break;
                }
                if un == u(b'\\') && self.i + 1 < self.len {
                    self.adv();
                    self.adv();
                    continue;
                }
                if un == u(b'"') || un == u(b'\'') {
                    self.adv();
                    self.skip_escaped_until(un);
                    if self.la(0) == un {
                        self.adv();
                    }
                    continue;
                }
                if un == u(b'`') {
                    self.zsh_brace_diff = true;
                    self.adv();
                    self.skip_escaped_until(u(b'`'));
                    if self.la(0) == u(b'`') {
                        self.adv();
                    }
                    continue;
                }
                if un == u(b'$') {
                    let d = self.la(1);
                    if d == u(b'{') {
                        self.skip_nested_brace();
                        continue;
                    }
                    if d == u(b'(') {
                        self.zsh_brace_diff = true;
                        self.skip_nested_paren();
                        continue;
                    }
                }
                self.adv();
            }
            let f = self.b;
            if f == s {
                return Ok(None);
            }
            return Ok(Some(self.mk("regex", s, f, Vec::new())?));
        }
        let mut i = Vec::new();
        let mut r = self.b;
        macro_rules! flush {
            () => {
                if self.b > r {
                    i.push(self.mk("word", r, self.b, Vec::new())?);
                }
            };
        }
        while self.i < self.len {
            let f = self.la(0);
            if f == u(b'}') {
                break;
            }
            if f == u(b'{') {
                self.zsh_brace_diff = true;
            }
            if slash_stops && f == u(b'/') {
                break;
            }
            if f == u(b'\\') && self.i + 1 < self.len {
                self.adv();
                self.adv();
                continue;
            }
            let un = self.la(1);
            if f == u(b'$') {
                if un == u(b'{') || un == u(b'(') || un == u(b'[') {
                    flush!();
                    i.push(self.dollar()?);
                    r = self.b;
                    continue;
                }
                if un == u(b'\'') {
                    flush!();
                    let d = self.b;
                    self.adv();
                    self.adv();
                    self.skip_escaped_until(u(b'\''));
                    if self.la(0) == u(b'\'') {
                        self.adv();
                    }
                    i.push(self.mk("ansi_c_string", d, self.b, Vec::new())?);
                    r = self.b;
                    continue;
                }
                if is_letter(un) || is_digit(un) || is_special(un) || un >= 0x80 {
                    flush!();
                    i.push(self.dollar()?);
                    r = self.b;
                    continue;
                }
            }
            if f == u(b'"') {
                flush!();
                i.push(self.dquote()?);
                r = self.b;
                continue;
            }
            if f == u(b'\'') {
                flush!();
                let d = self.b;
                self.adv();
                while self.i < self.len && self.la(0) != u(b'\'') {
                    self.adv();
                }
                if self.la(0) == u(b'\'') {
                    self.adv();
                }
                i.push(self.mk("raw_string", d, self.b, Vec::new())?);
                r = self.b;
                continue;
            }
            if (f == u(b'<') || f == u(b'>')) && un == u(b'(') {
                self.zsh_brace_diff = true;
                flush!();
                if let Some(d) = self.process_sub()? {
                    i.push(d);
                }
                r = self.b;
                continue;
            }
            if f == u(b'`') {
                flush!();
                i.push(self.backtick()?);
                r = self.b;
                continue;
            }
            self.adv();
        }
        flush!();
        if i.len() > 1 && self.kind(i[0]) == "word" {
            let t0 = self.text(i[0]);
            if !t0.is_empty() && t0.iter().all(|c| *c == u16::from(b' ') || *c == u16::from(b'\t'))
            {
                i.remove(0);
            }
        }
        if i.is_empty() {
            return Ok(None);
        }
        if i.len() == 1 {
            return Ok(Some(i[0]));
        }
        let c = i[i.len() - 1];
        Ok(Some(self.mk("concatenation", self.start(i[0]), self.end(c), i)?))
    }

    /// 原文 `Ve`：`${v#pat}` / `${v%pat}` 的模式。
    fn expansion_pattern(&mut self) -> R<Vec<Id>> {
        let mut o = Vec::new();
        let mut t = self.b;
        macro_rules! flush {
            () => {
                if self.b > t {
                    o.push(self.mk("regex", t, self.b, Vec::new())?);
                }
            };
        }
        while self.i < self.len {
            let i = self.la(0);
            if i == u(b'}') {
                break;
            }
            if i == u(b'{') {
                self.zsh_brace_diff = true;
            }
            if i == u(b'\\') && self.i + 1 < self.len {
                self.adv();
                self.adv();
                continue;
            }
            if i == u(b'"') {
                flush!();
                o.push(self.dquote()?);
                t = self.b;
                continue;
            }
            if i == u(b'\'') {
                flush!();
                let r = self.b;
                self.adv();
                while self.i < self.len && self.la(0) != u(b'\'') {
                    self.adv();
                }
                if self.la(0) == u(b'\'') {
                    self.adv();
                }
                o.push(self.mk("raw_string", r, self.b, Vec::new())?);
                t = self.b;
                continue;
            }
            if i == u(b'`') {
                self.zsh_brace_diff = true;
                self.adv();
                self.skip_escaped_until(u(b'`'));
                if self.la(0) == u(b'`') {
                    self.adv();
                }
                continue;
            }
            if i == u(b'$') {
                let r = self.la(1);
                if r == u(b'$') {
                    self.adv();
                    self.adv();
                    continue;
                }
                if r == u(b'\'') {
                    flush!();
                    let l = self.b;
                    self.adv();
                    self.adv();
                    self.skip_escaped_until(u(b'\''));
                    if self.la(0) == u(b'\'') {
                        self.adv();
                    }
                    o.push(self.mk("ansi_c_string", l, self.b, Vec::new())?);
                    t = self.b;
                    continue;
                }
                if r == u(b'{') {
                    self.skip_nested_brace();
                    continue;
                }
                if r == u(b'(') {
                    self.zsh_brace_diff = true;
                    self.skip_nested_paren();
                    continue;
                }
            }
            self.adv();
        }
        flush!();
        Ok(o)
    }

    /// 原文 `se`：反引号命令替换。
    fn backtick(&mut self) -> R<Id> {
        let o = self.b;
        self.adv();
        let t = self.mk("`", o, self.b, Vec::new())?;
        let mut s = self.i;
        {
            let mut esc = false;
            while s < self.len {
                let d = self.c(s);
                if d == u(b'\\') {
                    let b = self.c(s + 1);
                    if in_set(b, b"`$\\") {
                        esc = true;
                    }
                    s += 2;
                    continue;
                }
                if d == u(b'`') {
                    break;
                }
                s += 1;
            }
            if esc {
                let d = self.b;
                while self.i < s {
                    self.adv();
                }
                let b = self.mk("backtick_escape_unsupported", d, self.b, Vec::new())?;
                let p = if self.la(0) == u(b'`') {
                    let x = self.b;
                    self.adv();
                    self.mk("`", x, self.b, Vec::new())?
                } else {
                    self.mk("`", self.b, self.b, Vec::new())?
                };
                return self.mk("command_substitution", o, self.end(p), vec![t, b, p]);
            }
        }
        self.in_backtick += 1;
        let i = self.in_dquote;
        self.in_dquote = 0;
        let r = self.save();
        let l = std::mem::take(&mut self.heredocs);
        let mut c = Vec::new();
        loop {
            self.h();
            if self.la(0) == u(b'`') || self.la(0) == EOF {
                break;
            }
            let un = self.save();
            let d = self.tok(true);
            if d.ty == Tk::Eof || d.ty == Tk::Backtick {
                self.restore(un);
                break;
            }
            if d.ty == Tk::Newline {
                continue;
            }
            self.restore(un);
            let Some(b) = self.and_or()? else { break };
            c.push(b);
            self.h();
            if self.la(0) == u(b'`') {
                break;
            }
            let p = self.save();
            let x = self.tok(true);
            if x.ty == Tk::Op && self.val_in(&x, &[";", "&"]) {
                let k = self.vkind(&x);
                c.push(self.mk_tok(k, &x)?);
            } else if x.ty != Tk::Newline {
                self.restore(p);
            }
        }
        self.heredocs = l;
        self.in_backtick -= 1;
        self.in_dquote = i;
        if self.i != s {
            self.restore(r);
            while self.i < s {
                self.adv();
            }
            c.clear();
            c.push(self.mk("backtick_body_overrun", self.end(t), self.b, Vec::new())?);
        }
        let f = if self.la(0) == u(b'`') {
            let x = self.b;
            self.adv();
            self.mk("`", x, self.b, Vec::new())?
        } else {
            self.mk("`", self.b, self.b, Vec::new())?
        };
        if c.is_empty() {
            c.push(self.mk("backtick_body_empty", self.end(t), self.b, Vec::new())?);
        }
        let mut kids = vec![t];
        kids.extend(c);
        kids.push(f);
        self.mk("command_substitution", o, self.end(f), kids)
    }

    // ---- 复合命令 ----

    /// 原文 `Q`：期望下一个词是 `w`，是就收进 `list`。
    fn expect(&mut self, w: &'static str, list: &mut Vec<Id>) -> R<()> {
        self.skip_newlines();
        let s = self.save();
        let i = self.tok(true);
        if i.ty == Tk::Word && self.val_is(&i, w) {
            list.push(self.mk_tok(w, &i)?);
        } else {
            self.restore(s);
        }
        Ok(())
    }

    /// 原文 `Ge`。
    fn if_stmt(&mut self, o: &Tok) -> R<Id> {
        let t = self.mk_tok("if", o)?;
        let mut s = vec![t];
        let i = self.stmts(None)?;
        s.extend(i);
        self.expect("then", &mut s)?;
        let r = self.stmts(None)?;
        s.extend(r);
        loop {
            let c = self.save();
            let f = self.tok(true);
            if f.ty == Tk::Word && self.val_is(&f, "elif") {
                let un = self.mk_tok("elif", &f)?;
                let d = self.stmts(None)?;
                let mut b = vec![un];
                b.extend(d);
                self.expect("then", &mut b)?;
                let p = self.stmts(None)?;
                b.extend(p);
                let x = b[b.len() - 1];
                s.push(self.mk("elif_clause", self.start(un), self.end(x), b)?);
            } else if f.ty == Tk::Word && self.val_is(&f, "else") {
                let un = self.mk_tok("else", &f)?;
                let d = self.stmts(None)?;
                let b = d.last().copied().unwrap_or(un);
                let mut kids = vec![un];
                kids.extend(d);
                s.push(self.mk("else_clause", self.start(un), self.end(b), kids)?);
            } else {
                self.restore(c);
                break;
            }
        }
        self.expect("fi", &mut s)?;
        let l = s[s.len() - 1];
        self.mk("if_statement", self.start(t), self.end(l), s)
    }

    /// 原文 `Ye`。
    fn while_stmt(&mut self, o: &Tok) -> R<Id> {
        let k = self.vkind(o);
        let t = self.mk_tok(k, o)?;
        let mut s = vec![t];
        let i = self.stmts(None)?;
        s.extend(i);
        if let Some(r) = self.do_group()? {
            s.push(r);
        }
        let l = s[s.len() - 1];
        self.mk("while_statement", self.start(t), self.end(l), s)
    }

    /// 原文 `pe`：`for` / `select`，含 C 风格的 `for ((…))`。
    fn for_stmt(&mut self, o: &Tok) -> R<Id> {
        let k = self.vkind(o);
        let t = self.mk_tok(k, o)?;
        self.h();
        if self.val_is(o, "for") && self.la(0) == u(b'(') && self.la(1) == u(b'(') {
            let b = self.b;
            self.adv();
            self.adv();
            let p = self.mk("((", b, self.b, Vec::new())?;
            let mut x = vec![t, p];
            for n in 0..3 {
                self.h();
                let ss = self.arith_list(if n < 2 { ";" } else { "))" }, Ar::Assign)?;
                x.extend(ss);
                if n < 2 && self.la(0) == u(b';') {
                    let nb = self.b;
                    self.adv();
                    x.push(self.mk(";", nb, self.b, Vec::new())?);
                }
            }
            self.h();
            if self.la(0) == u(b')') && self.la(1) == u(b')') {
                let nb = self.b;
                self.adv();
                self.adv();
                x.push(self.mk("))", nb, self.b, Vec::new())?);
            }
            let sv = self.save();
            let w = self.tok(true);
            if w.ty == Tk::Op && self.val_is(&w, ";") {
                x.push(self.mk_tok(";", &w)?);
            } else if w.ty != Tk::Newline {
                self.restore(sv);
            }
            if let Some(g) = self.do_group()? {
                x.push(g);
            } else {
                self.skip_newlines();
                self.h();
                if self.la(0) == u(b'{') {
                    let nb = self.b;
                    self.adv();
                    let ss = self.mk("{", nb, self.b, Vec::new())?;
                    let n = self.stmts(Some("}"))?;
                    let rr = if self.la(0) == u(b'}') {
                        let bb = self.b;
                        self.adv();
                        self.mk("}", bb, self.b, Vec::new())?
                    } else {
                        self.mk("}", self.b, self.b, Vec::new())?
                    };
                    let mut kids = vec![ss];
                    kids.extend(n);
                    kids.push(rr);
                    x.push(self.mk("compound_statement", self.start(ss), self.end(rr), kids)?);
                }
            }
            let m = x[x.len() - 1];
            return self.mk("c_style_for_statement", self.start(t), self.end(m), x);
        }
        let mut s = vec![t];
        let i = self.tok(false);
        let v = self.val(&i);
        let good = i.ty == Tk::Word
            && v.first().is_some_and(|c| is_letter(i32::from(*c)))
            && v.iter().all(|c| is_alnum(i32::from(*c)));
        s.push(self.mk(
            if good { "variable_name" } else { "ERROR" },
            i.start,
            i.end,
            Vec::new(),
        )?);
        self.h();
        let r = self.save();
        let l = self.tok(false);
        if l.ty == Tk::Word && self.val_is(&l, "in") {
            s.push(self.mk_tok("in", &l)?);
            loop {
                self.h();
                let b = self.la(0);
                if b == u(b';') || b == u(b'\n') || b == EOF {
                    break;
                }
                let Some(p) = self.word()? else { break };
                s.push(p);
            }
        } else {
            self.restore(r);
        }
        let c = self.save();
        let f = self.tok(true);
        if f.ty == Tk::Op && self.val_is(&f, ";") {
            s.push(self.mk_tok(";", &f)?);
        } else if f.ty != Tk::Newline {
            self.restore(c);
        }
        if let Some(un) = self.do_group()? {
            s.push(un);
        }
        let d = s[s.len() - 1];
        self.mk("for_statement", self.start(t), self.end(d), s)
    }

    /// 原文 `le`：`do … done`。
    fn do_group(&mut self) -> R<Option<Id>> {
        self.skip_newlines();
        let o = self.save();
        let t = self.tok(true);
        if t.ty != Tk::Word || !self.val_is(&t, "do") {
            self.restore(o);
            return Ok(None);
        }
        let s = self.mk_tok("do", &t)?;
        let i = self.stmts(None)?;
        let mut r = vec![s];
        r.extend(i);
        self.expect("done", &mut r)?;
        let l = r[r.len() - 1];
        Ok(Some(self.mk("do_group", self.start(s), self.end(l), r)?))
    }

    /// 原文 `Je`。
    fn case_stmt(&mut self, o: &Tok) -> R<Id> {
        let t = self.mk_tok("case", o)?;
        let mut s = vec![t];
        self.h();
        if let Some(i) = self.word()? {
            s.push(i);
        }
        self.h();
        self.expect("in", &mut s)?;
        self.skip_newlines();
        loop {
            self.h();
            self.skip_newlines();
            let l = self.save();
            let c = self.tok(false);
            if c.ty == Tk::Word && self.val_is(&c, "esac") {
                s.push(self.mk_tok("esac", &c)?);
                break;
            }
            if c.ty == Tk::Eof {
                break;
            }
            self.restore(l);
            let Some(f) = self.case_item()? else { break };
            s.push(f);
        }
        let r = s[s.len() - 1];
        self.mk("case_statement", self.start(t), self.end(r), s)
    }

    /// 原文 `Pe`：`pat | pat) …;;`。
    fn case_item(&mut self) -> R<Option<Id>> {
        self.h();
        let o = self.b;
        let mut t = Vec::new();
        if self.la(0) == u(b'(') {
            let f = self.b;
            self.adv();
            t.push(self.mk("(", f, self.b, Vec::new())?);
        }
        let mut first = true;
        loop {
            self.h();
            let f = self.la(0);
            if f == u(b')') || f == EOF {
                break;
            }
            let un = self.case_pattern()?;
            if un.is_empty() {
                break;
            }
            if !first && un.len() > 1 {
                let mut d = Vec::with_capacity(un.len());
                for x in un {
                    if self.kind(x) == "extglob_pattern" {
                        d.push(self.mk("word", self.start(x), self.end(x), Vec::new())?);
                    } else {
                        d.push(x);
                    }
                }
                let (b, p) = (d[0], d[d.len() - 1]);
                t.push(self.mk("concatenation", self.start(b), self.end(p), d)?);
            } else {
                t.extend(un);
            }
            first = false;
            self.h();
            if self.la(0) == u(b'\\') && self.la(1) == u(b'\n') {
                self.adv();
                self.adv();
                self.h();
            }
            if self.la(0) == u(b'|') {
                let d = self.b;
                self.adv();
                t.push(self.mk("|", d, self.b, Vec::new())?);
                if self.la(0) == u(b'\\') && self.la(1) == u(b'\n') {
                    self.adv();
                    self.adv();
                }
            } else {
                break;
            }
        }
        if self.la(0) == u(b')') {
            let f = self.b;
            self.adv();
            t.push(self.mk(")", f, self.b, Vec::new())?);
        }
        let i = self.stmts(None)?;
        let body_empty = i.is_empty();
        t.extend(i);
        let r = self.save();
        let l = self.tok(true);
        if l.ty == Tk::Op && self.val_in(&l, &[";;", ";&", ";;&"]) {
            let k = self.vkind(&l);
            t.push(self.mk_tok(k, &l)?);
        } else {
            self.restore(r);
        }
        if t.is_empty() {
            return Ok(None);
        }
        if body_empty {
            for slot in &mut t {
                let un = *slot;
                if self.kind(un) != "extglob_pattern" {
                    continue;
                }
                let d = self.text(un);
                if extglob_like_word(d) && !has_unit(d, b"*?(") {
                    *slot = self.mk("word", self.start(un), self.end(un), Vec::new())?;
                }
            }
        }
        let c = t[t.len() - 1];
        Ok(Some(self.mk("case_item", o, self.end(c), t)?))
    }

    /// 原文 `en`：case 的一个模式。
    fn case_pattern(&mut self) -> R<Vec<Id>> {
        self.h();
        let o = self.save();
        let (t, s) = (self.b, self.i);
        let mut depth = 0;
        let (mut dollar, mut bracket, mut quoted) = (false, false, false);
        while self.i < self.len {
            let b = self.la(0);
            if b == u(b'\\') && self.i + 1 < self.len {
                self.adv();
                self.adv();
                continue;
            }
            if b == u(b'"') || b == u(b'\'') {
                quoted = true;
                self.adv();
                self.skip_escaped_until(b);
                if self.la(0) == b {
                    self.adv();
                }
                continue;
            }
            if b == u(b'(') {
                depth += 1;
                self.adv();
                continue;
            }
            if depth > 0 {
                if b == u(b')') {
                    depth -= 1;
                    self.adv();
                    continue;
                }
                if b == u(b'\n') {
                    break;
                }
                self.adv();
                continue;
            }
            if in_set(b, b")| \t\n") {
                break;
            }
            if b == u(b'$') {
                dollar = true;
            }
            if b == u(b'[') {
                bracket = true;
            }
            self.adv();
        }
        if self.b == t {
            return Ok(Vec::new());
        }
        let f = self.slice(s, self.i).to_vec();
        let ext = f.windows(2).any(|w| has_unit(&w[..1], b"*?+@!") && w[1] == u16::from(b'('));
        if quoted && !ext {
            self.restore(o);
            return self.case_pattern_quoted();
        }
        if !ext && (dollar || bracket) {
            self.restore(o);
            return Ok(self.word()?.into_iter().collect());
        }
        let k = if ext || has_unit(&f, b"*?") || extglob_like_word(&f) {
            "extglob_pattern"
        } else {
            "word"
        };
        Ok(vec![self.mk(k, t, self.b, Vec::new())?])
    }

    /// 原文 `nn`：带引号的 case 模式。
    fn case_pattern_quoted(&mut self) -> R<Vec<Id>> {
        let mut o = Vec::new();
        let (mut t, mut s) = (self.b, self.i);
        macro_rules! flush {
            () => {
                if self.i > s {
                    let k = if has_unit(self.slice(s, self.i), b"*?") {
                        "extglob_pattern"
                    } else {
                        "word"
                    };
                    o.push(self.mk(k, t, self.b, Vec::new())?);
                }
            };
        }
        while self.i < self.len {
            let r = self.la(0);
            if r == u(b'\\') && self.i + 1 < self.len {
                self.adv();
                self.adv();
                continue;
            }
            if r == u(b'"') {
                flush!();
                o.push(self.dquote()?);
                (t, s) = (self.b, self.i);
                continue;
            }
            if r == u(b'\'') {
                flush!();
                let l = self.tok(false);
                o.push(self.mk_tok("raw_string", &l)?);
                (t, s) = (self.b, self.i);
                continue;
            }
            if in_set(r, b")| \t\n") {
                break;
            }
            self.adv();
        }
        flush!();
        Ok(o)
    }

    /// 原文 `tn`：`function name [()] body`。
    fn function_def(&mut self, o: &Tok) -> R<Id> {
        let t = self.mk_tok("function", o)?;
        self.h();
        let s = self.tok(false);
        let i = self.mk("word", s.start, s.end, Vec::new())?;
        let mut r = vec![t, i];
        self.h();
        if self.la(0) == u(b'(') && self.la(1) == u(b')') {
            let f = self.tok(true);
            r.push(self.mk_tok("(", &f)?);
            let un = self.b;
            self.adv();
            r.push(self.mk(")", un, self.b, Vec::new())?);
        }
        self.h();
        self.skip_newlines();
        if let Some(l) = self.command()? {
            if self.kind(l) == "redirected_statement"
                && self.nodes[l].children.len() >= 2
                && self.kind(self.nodes[l].children[0]) == "compound_statement"
            {
                r.extend(self.kids(l));
            } else {
                r.push(l);
            }
        }
        let c = r[r.len() - 1];
        self.mk("function_definition", self.start(t), self.end(c), r)
    }

    /// 原文 `sn`：`export` / `declare` / `local` …
    fn declaration(&mut self, o: &Tok) -> R<Id> {
        let k = self.vkind(o);
        let t = self.mk_tok(k, o)?;
        let mut s = vec![t];
        let mut i = Vec::new();
        loop {
            self.h();
            if let Some(un) = self.redirect(false)? {
                i.push(un);
                continue;
            }
            let d = self.la(0);
            if d == u(b'(') {
                s.push(self.mk("variable_name", self.b, self.b + 1, Vec::new())?);
                break;
            }
            if d == EOF || in_set(d, b"\n;&|)<>") {
                break;
            }
            if let Some(b) = self.assignment()? {
                s.push(b);
                continue;
            }
            if d == u(b'"') || d == u(b'\'') || d == u(b'$') {
                match self.word()? {
                    Some(w) => {
                        s.push(w);
                        continue;
                    }
                    None => break,
                }
            }
            let p = self.save();
            let x = self.tok(false);
            if x.ty == Tk::Word || x.ty == Tk::Number {
                let v = self.val(&x);
                if v.first() == Some(&u16::from(b'-')) {
                    s.push(self.mk_tok("word", &x)?);
                } else if v.first().is_some_and(|c| is_letter(i32::from(*c))) {
                    s.push(self.mk("variable_name", x.start, x.end, Vec::new())?);
                } else {
                    s.push(self.mk_tok("word", &x)?);
                }
            } else {
                self.restore(p);
                break;
            }
        }
        let r = s[s.len() - 1];
        let l = self.mk("declaration_command", self.start(t), self.end(r), s)?;
        if i.is_empty() {
            return Ok(l);
        }
        let c = i[i.len() - 1];
        let f = self.end(l).max(self.end(c));
        let mut kids = vec![l];
        kids.extend(i);
        self.mk("redirected_statement", self.start(t), f, kids)
    }

    /// 原文 `rn`：`unset …`。
    fn unset(&mut self, o: &Tok) -> R<Id> {
        let t = self.mk_tok("unset", o)?;
        let mut s = vec![t];
        let mut i = Vec::new();
        loop {
            self.h();
            if let Some(un) = self.redirect(false)? {
                i.push(un);
                continue;
            }
            let d = self.la(0);
            if d == u(b'(') {
                s.push(self.mk("variable_name", self.b, self.b + 1, Vec::new())?);
                break;
            }
            if d == EOF || in_set(d, b"\n;&|)<>") {
                break;
            }
            let Some(b) = self.word()? else { break };
            if self.kind(b) == "word" {
                if self.text(b).first() == Some(&u16::from(b'-')) {
                    s.push(b);
                } else {
                    s.push(self.mk("variable_name", self.start(b), self.end(b), Vec::new())?);
                }
            } else {
                s.push(b);
            }
        }
        let r = s[s.len() - 1];
        let l = self.mk("unset_command", self.start(t), self.end(r), s)?;
        if i.is_empty() {
            return Ok(l);
        }
        let c = i[i.len() - 1];
        let f = self.end(l).max(self.end(c));
        let mut kids = vec![l];
        kids.extend(i);
        self.mk("redirected_statement", self.start(t), f, kids)
    }

    // ---- 条件表达式 `[ … ]` / `[[ … ]]` ----

    /// 原文 `he` / `fe`：`||` 连接。
    fn test_or(&mut self, o: &str) -> R<Option<Id>> {
        let Some(mut t) = self.test_and(o)? else { return Ok(None) };
        loop {
            self.h();
            let s = self.save();
            if o == "]]" && self.la(0) == u(b'|') && self.la(1) == u(b'|') {
                let i = self.b;
                self.adv();
                self.adv();
                let r = self.mk("||", i, self.b, Vec::new())?;
                self.test_skip(o);
                let Some(l) = self.test_and(o)? else {
                    self.restore(s);
                    break;
                };
                t = self.mk("binary_expression", self.start(t), self.end(l), vec![t, r, l])?;
            } else {
                break;
            }
        }
        Ok(Some(t))
    }

    /// 原文 `xe`：`&&` 连接。
    fn test_and(&mut self, o: &str) -> R<Option<Id>> {
        let Some(mut t) = self.test_primary(o)? else { return Ok(None) };
        loop {
            self.h();
            let s = self.save();
            if o == "]]" && self.la(0) == u(b'&') && self.la(1) == u(b'&') {
                let i = self.b;
                self.adv();
                self.adv();
                let r = self.mk("&&", i, self.b, Vec::new())?;
                self.test_skip(o);
                let Some(l) = self.test_primary(o)? else {
                    self.restore(s);
                    break;
                };
                t = self.mk("binary_expression", self.start(t), self.end(l), vec![t, r, l])?;
            } else {
                break;
            }
        }
        Ok(Some(t))
    }

    /// 原文 `ie`：`[[` 里跳过空行与注释。
    fn test_skip(&mut self, o: &str) {
        self.h();
        if o == "]]" {
            loop {
                let t = self.la(0);
                if t == u(b'\n') {
                    self.adv();
                    self.h();
                } else if t == u(b'#') {
                    while self.la(0) != EOF && self.la(0) != u(b'\n') {
                        self.adv();
                    }
                } else {
                    break;
                }
            }
        }
    }

    /// 原文 `ve`。
    fn test_primary(&mut self, o: &str) -> R<Option<Id>> {
        self.guard()?;
        self.test_skip(o);
        let r = if self.la(0) == u(b'(') {
            let s = self.b;
            self.adv();
            let i = self.mk("(", s, self.b, Vec::new())?;
            let r = self.test_or(o)?;
            self.h();
            let l = if self.la(0) == u(b')') {
                let f = self.b;
                self.adv();
                self.mk(")", f, self.b, Vec::new())?
            } else {
                self.mk(")", self.b, self.b, Vec::new())?
            };
            let c = match r {
                Some(r) => vec![i, r, l],
                None => vec![i, l],
            };
            Some(self.mk("parenthesized_expression", self.start(i), self.end(l), c)?)
        } else {
            self.test_binary(o)?
        };
        Ok(r)
    }

    /// 原文 `_e`：`!`、括号、`-f x` 之类一元测试。
    fn test_unary(&mut self, o: &str) -> R<Option<Id>> {
        self.guard()?;
        self.test_unary_inner(o)
    }

    fn test_unary_inner(&mut self, o: &str) -> R<Option<Id>> {
        self.test_skip(o);
        let t = self.la(0);
        let blank = |c: i32| c == EOF || in_set(c, b" \t\n");
        if t == u(b'!') && (blank(self.la(1)) || self.la(1) == u(b'(')) {
            let i = self.b;
            self.adv();
            let r = self.mk("!", i, self.b, Vec::new())?;
            let Some(l) = self.test_unary(o)? else { return Ok(Some(r)) };
            return Ok(Some(self.mk(
                "unary_expression",
                self.start(r),
                self.end(l),
                vec![r, l],
            )?));
        }
        if t == u(b'(') {
            let i = self.b;
            self.adv();
            let r = self.mk("(", i, self.b, Vec::new())?;
            let l = self.test_or(o)?;
            self.h();
            let c = if self.la(0) == u(b')') {
                let un = self.b;
                self.adv();
                self.mk(")", un, self.b, Vec::new())?
            } else {
                self.mk(")", self.b, self.b, Vec::new())?
            };
            let f = match l {
                Some(l) => vec![r, l, c],
                None => vec![r, c],
            };
            return Ok(Some(self.mk("parenthesized_expression", self.start(r), self.end(c), f)?));
        }
        if t == u(b'-') && is_letter(self.la(1)) {
            let i = self.save();
            let r = self.b;
            self.adv();
            while is_alnum(self.la(0)) {
                self.adv();
            }
            if !blank(self.la(0)) {
                self.restore(i);
                return self.test_word(o);
            }
            let l = self.mk("test_operator", r, self.b, Vec::new())?;
            let c = self.i;
            self.h();
            let Some(f) = self.test_word(o)? else {
                let gap = self.slice(c, self.i);
                if !only_blank_or_continuation(gap) {
                    let d = self.mk("test_rhs_missing", self.end(l), self.b, Vec::new())?;
                    return Ok(Some(self.mk(
                        "unary_expression",
                        self.start(l),
                        self.b,
                        vec![l, d],
                    )?));
                }
                return Ok(Some(l));
            };
            return Ok(Some(self.mk(
                "unary_expression",
                self.start(l),
                self.end(f),
                vec![l, f],
            )?));
        }
        self.test_word(o)
    }

    /// 原文 `J`：二元测试缺右边。
    fn test_missing_rhs(&mut self, o: Id, t: Id) -> R<Id> {
        let s = self.mk("test_rhs_missing", self.end(t), self.b, Vec::new())?;
        self.mk("binary_expression", self.start(o), self.b, vec![o, t, s])
    }

    /// 原文 `on`：二元测试。
    fn test_binary(&mut self, o: &str) -> R<Option<Id>> {
        self.h();
        let Some(t) = self.test_unary(o)? else { return Ok(None) };
        self.h();
        let (s, i) = (self.la(0), self.la(1));
        let l = self.b;
        let mut r = None;
        if s == u(b'=') && i == u(b'=') {
            self.adv();
            self.adv();
            r = Some(self.mk("==", l, self.b, Vec::new())?);
        } else if s == u(b'!') && i == u(b'=') {
            self.adv();
            self.adv();
            r = Some(self.mk("!=", l, self.b, Vec::new())?);
        } else if s == u(b'=') && i == u(b'~') {
            self.adv();
            self.adv();
            r = Some(self.mk("=~", l, self.b, Vec::new())?);
        } else if s == u(b'=') && i != u(b'=') {
            self.adv();
            r = Some(self.mk("=", l, self.b, Vec::new())?);
        } else if o == "]]" && s == u(b'<') && i != u(b'<') {
            self.adv();
            r = Some(self.mk("<", l, self.b, Vec::new())?);
        } else if o == "]]" && s == u(b'>') && i != u(b'>') {
            self.adv();
            r = Some(self.mk(">", l, self.b, Vec::new())?);
        } else if s == u(b'-') && is_letter(i) {
            self.adv();
            while is_alnum(self.la(0)) {
                self.adv();
            }
            r = Some(self.mk("test_operator", l, self.b, Vec::new())?);
        }
        let Some(r) = r else { return Ok(Some(t)) };
        self.h();
        if o == "]]" {
            let f = self.kind(r).to_string();
            if f == "=~" {
                self.h();
                let un = self.la(0);
                let mut d = None;
                if un == u(b'"') || un == u(b'\'') {
                    let b = self.save();
                    let p = if un == u(b'"') {
                        self.dquote()?
                    } else {
                        let tk = self.tok(false);
                        self.mk_tok("raw_string", &tk)?
                    };
                    let x = self.i;
                    let mut ii = x;
                    while ii < self.len && (self.c(ii) == u(b' ') || self.c(ii) == u(b'\t')) {
                        ii += 1;
                    }
                    let w = self.c(ii);
                    let mut g = ii + 1;
                    while self.c(g) == u(b'\\') {
                        if self.c(g + 1) == u(b'\n') {
                            g += 2;
                        } else if self.c(g + 1) == u(b'\r') && self.c(g + 2) == u(b'\n') {
                            g += 3;
                        } else {
                            break;
                        }
                    }
                    let m = self.c(g);
                    if (w == u(b']') && m == u(b']') && ii > x)
                        || (w == u(b'&') && m == u(b'&'))
                        || (w == u(b'|') && m == u(b'|') && ii > x)
                        || w == u(b'\n')
                        || w == EOF
                    {
                        d = Some(p);
                    } else {
                        self.restore(b);
                    }
                }
                if d.is_none() {
                    d = self.test_regex(true)?;
                }
                let Some(d) = d else { return Ok(Some(self.test_missing_rhs(t, r)?)) };
                return Ok(Some(self.mk(
                    "binary_expression",
                    self.start(t),
                    self.end(d),
                    vec![t, r, d],
                )?));
            }
            if f == "=" {
                let Some(un) = self.test_regex(false)? else {
                    return Ok(Some(self.test_missing_rhs(t, r)?));
                };
                return Ok(Some(self.mk(
                    "binary_expression",
                    self.start(t),
                    self.end(un),
                    vec![t, r, un],
                )?));
            }
            if f == "==" || f == "!=" {
                let un = self.test_pattern()?;
                if un.is_empty() {
                    return Ok(Some(self.test_missing_rhs(t, r)?));
                }
                let d = un[un.len() - 1];
                let mut kids = vec![t, r];
                kids.extend(un);
                return Ok(Some(self.mk("binary_expression", self.start(t), self.end(d), kids)?));
            }
        }
        let Some(c) = self.test_word(o)? else { return Ok(Some(self.test_missing_rhs(t, r)?)) };
        Ok(Some(self.mk("binary_expression", self.start(t), self.end(c), vec![t, r, c])?))
    }

    /// 原文 `me`：`=~` / `=` 右边的正则。
    fn test_regex(&mut self, allow_pipe: bool) -> R<Option<Id>> {
        self.h();
        let t = self.b;
        let mut s = 0;
        while self.i < self.len {
            let i = self.la(0);
            if i == u(b'\\') && self.i + 1 < self.len {
                self.adv();
                self.adv();
                continue;
            }
            if i == u(b'\n') {
                if s == 0 {
                    break;
                }
                self.adv();
                continue;
            }
            if i == u(b'"') || i == u(b'\'') {
                self.skip_quoted(i);
                if self.i < self.len {
                    self.adv();
                }
                continue;
            }
            if i == u(b'`') {
                self.adv();
                self.skip_escaped_until(u(b'`'));
                if self.i < self.len {
                    self.adv();
                }
                continue;
            }
            if s == 0 {
                if i == u(b' ') || i == u(b'\t') {
                    break;
                }
                if i == u(b'&') || (!allow_pipe && i == u(b'|')) {
                    break;
                }
                if i == u(b')') {
                    break;
                }
            }
            if i == u(b'(') {
                s += 1;
            } else if i == u(b')') && s > 0 {
                s -= 1;
            }
            self.adv();
        }
        if self.b == t {
            return Ok(None);
        }
        Ok(Some(self.mk("regex", t, self.b, Vec::new())?))
    }

    /// 原文 `Ln`：`==` / `!=` 右边的模式。
    fn test_pattern(&mut self) -> R<Vec<Id>> {
        self.h();
        let mut o = Vec::new();
        let (mut t, mut s) = (self.b, self.i);
        let mut depth = 0;
        macro_rules! flush {
            () => {
                if self.i > s {
                    let k =
                        if is_uint(self.slice(s, self.i)) { "number" } else { "extglob_pattern" };
                    o.push(self.mk(k, t, self.b, Vec::new())?);
                }
            };
        }
        while self.i < self.len {
            let l = self.la(0);
            if l == u(b'\\') && self.i + 1 < self.len {
                self.adv();
                self.adv();
                continue;
            }
            if l == u(b'\n') {
                if depth == 0 {
                    break;
                }
                self.adv();
                continue;
            }
            if depth == 0 {
                if l == u(b'&') || l == u(b'|') {
                    break;
                }
                if l == u(b' ') || l == u(b'\t') {
                    let mut c = self.i;
                    loop {
                        let b = self.c(c);
                        if b == u(b' ') || b == u(b'\t') {
                            c += 1;
                        } else if b == u(b'\\') && self.c(c + 1) == u(b'\n') {
                            c += 2;
                        } else if b == u(b'\\')
                            && self.c(c + 1) == u(b'\r')
                            && self.c(c + 2) == u(b'\n')
                        {
                            c += 3;
                        } else {
                            break;
                        }
                    }
                    let f = self.c(c);
                    let mut un = c + 1;
                    while self.c(un) == u(b'\\') {
                        if self.c(un + 1) == u(b'\n') {
                            un += 2;
                        } else if self.c(un + 1) == u(b'\r') && self.c(un + 2) == u(b'\n') {
                            un += 3;
                        } else {
                            break;
                        }
                    }
                    let d = self.c(un);
                    if (f == u(b']') && d == u(b']'))
                        || (f == u(b'&') && d == u(b'&'))
                        || (f == u(b'|') && d == u(b'|'))
                        || f == u(b'#')
                        || f == u(b'\n')
                    {
                        break;
                    }
                    self.adv();
                    continue;
                }
            }
            if l == u(b'$') {
                let c = self.la(1);
                if c == u(b'\'') {
                    flush!();
                    let f = self.tok(false);
                    o.push(self.mk_tok("ansi_c_string", &f)?);
                    (t, s) = (self.b, self.i);
                    continue;
                }
                if c == u(b'"') {
                    flush!();
                    let b0 = self.b;
                    self.adv();
                    o.push(self.mk("$", b0, b0 + 1, Vec::new())?);
                    o.push(self.dquote()?);
                    (t, s) = (self.b, self.i);
                    continue;
                }
                if c == u(b'(') || c == u(b'{') || is_letter(c) || is_special(c) || c >= 0x80 {
                    flush!();
                    o.push(self.dollar()?);
                    (t, s) = (self.b, self.i);
                    continue;
                }
            }
            if l == u(b'"') {
                flush!();
                o.push(self.dquote()?);
                (t, s) = (self.b, self.i);
                continue;
            }
            if l == u(b'\'') {
                flush!();
                let c = self.tok(false);
                o.push(self.mk_tok("raw_string", &c)?);
                (t, s) = (self.b, self.i);
                continue;
            }
            if l == u(b'`') {
                flush!();
                o.push(self.backtick()?);
                (t, s) = (self.b, self.i);
                continue;
            }
            if (l == u(b'<') || l == u(b'>')) && self.la(1) == u(b'(') {
                flush!();
                if let Some(c) = self.process_sub()? {
                    o.push(c);
                }
                (t, s) = (self.b, self.i);
                continue;
            }
            if l == u(b')') && depth == 0 {
                break;
            }
            if l == u(b'(') {
                depth += 1;
            } else if l == u(b')') && depth > 0 {
                depth -= 1;
            }
            self.adv();
        }
        flush!();
        Ok(o)
    }

    /// 原文 `P`：条件表达式里的一个词（碰到收尾的 `]` / `]]` 返回空）。
    fn test_word(&mut self, o: &str) -> R<Option<Id>> {
        self.h();
        if o == "]" && self.la(0) == u(b']') && is_boundary(self.la(1)) && self.la(1) != u(b'(') {
            return Ok(None);
        }
        if o == "]]"
            && self.la(0) == u(b']')
            && self.la(1) == u(b']')
            && is_boundary(self.la(2))
            && self.la(2) != u(b'(')
        {
            return Ok(None);
        }
        self.word()
    }

    // ---- 算术 ----

    /// 原文 `ye`：算术没收尾时回到起点，跳到配对的收尾符号前。
    fn arith_recover(&mut self, o: (usize, usize), t: &str) {
        self.restore(o);
        let mut s = 0;
        while self.i < self.len {
            let i = self.la(0);
            if t == "))" || t == ")" {
                if i == u(b'(') {
                    s += 1;
                } else if i == u(b')') {
                    if s == 0 {
                        if t == ")" || self.la(1) == u(b')') {
                            return;
                        }
                    } else {
                        s -= 1;
                    }
                }
            } else if t == "]" {
                if i == u(b'[') {
                    s += 1;
                } else if i == u(b']') {
                    if s == 0 {
                        return;
                    }
                    s -= 1;
                }
            } else if self.arith_at_stop(t) {
                return;
            }
            self.adv();
        }
    }

    /// 原文 `X`：逗号分隔的算术表达式。
    fn arith_list(&mut self, o: &str, t: Ar) -> R<Vec<Id>> {
        let mut s = Vec::new();
        loop {
            if let Some(i) = self.arith(o, t)? {
                s.push(i);
            }
            self.h();
            if self.la(0) == u(b',') && !self.arith_at_stop(o) {
                self.adv();
                continue;
            }
            break;
        }
        Ok(s)
    }

    /// 原文 `Z` / `ne`：带三元的算术表达式。
    fn arith(&mut self, o: &str, t: Ar) -> R<Option<Id>> {
        let Some(s) = self.arith_binary(o, 0, t)? else { return Ok(None) };
        self.h();
        if self.la(0) == u(b'?') {
            let i = self.b;
            self.adv();
            let r = self.mk("?", i, self.b, Vec::new())?;
            let l = self.arith_binary(":", 0, t)?;
            self.h();
            let c = if self.la(0) == u(b':') {
                let b = self.b;
                self.adv();
                self.mk(":", b, self.b, Vec::new())?
            } else {
                self.mk(":", self.b, self.b, Vec::new())?
            };
            let f = self.arith(o, t)?;
            let un = f.unwrap_or(c);
            let mut d = vec![s, r];
            d.extend(l);
            d.push(c);
            d.extend(f);
            return Ok(Some(self.mk("ternary_expression", self.start(s), self.end(un), d)?));
        }
        Ok(Some(s))
    }

    /// 原文 `an`：算术二元运算符（及其长度）。
    fn arith_op(&self) -> Option<(&'static str, usize)> {
        let (o, t, s) = (self.la(0), self.la(1), self.la(2));
        let c = |x: u8| u(x);
        Some(if o == c(b'<') && t == c(b'<') && s == c(b'=') {
            ("<<=", 3)
        } else if o == c(b'>') && t == c(b'>') && s == c(b'=') {
            (">>=", 3)
        } else if o == c(b'*') && t == c(b'*') {
            ("**", 2)
        } else if o == c(b'<') && t == c(b'<') {
            ("<<", 2)
        } else if o == c(b'>') && t == c(b'>') {
            (">>", 2)
        } else if o == c(b'=') && t == c(b'=') {
            ("==", 2)
        } else if o == c(b'!') && t == c(b'=') {
            ("!=", 2)
        } else if o == c(b'<') && t == c(b'=') {
            ("<=", 2)
        } else if o == c(b'>') && t == c(b'=') {
            (">=", 2)
        } else if o == c(b'&') && t == c(b'&') {
            ("&&", 2)
        } else if o == c(b'|') && t == c(b'|') {
            ("||", 2)
        } else if o == c(b'+') && t == c(b'=') {
            ("+=", 2)
        } else if o == c(b'-') && t == c(b'=') {
            ("-=", 2)
        } else if o == c(b'*') && t == c(b'=') {
            ("*=", 2)
        } else if o == c(b'/') && t == c(b'=') {
            ("/=", 2)
        } else if o == c(b'%') && t == c(b'=') {
            ("%=", 2)
        } else if o == c(b'&') && t == c(b'=') {
            ("&=", 2)
        } else if o == c(b'^') && t == c(b'=') {
            ("^=", 2)
        } else if o == c(b'|') && t == c(b'=') {
            ("|=", 2)
        } else if o == c(b'+') && t != c(b'+') {
            ("+", 1)
        } else if o == c(b'-') && t != c(b'-') {
            ("-", 1)
        } else if o == c(b'*') {
            ("*", 1)
        } else if o == c(b'/') {
            ("/", 1)
        } else if o == c(b'%') {
            ("%", 1)
        } else if o == c(b'<') {
            ("<", 1)
        } else if o == c(b'>') {
            (">", 1)
        } else if o == c(b'&') {
            ("&", 1)
        } else if o == c(b'|') {
            ("|", 1)
        } else if o == c(b'^') {
            ("^", 1)
        } else if o == c(b'=') {
            ("=", 1)
        } else {
            return None;
        })
    }

    /// 原文 `ae`：按优先级（`cn`）的二元表达式。
    fn arith_binary(&mut self, o: &str, t: i32, s: Ar) -> R<Option<Id>> {
        let Some(mut i) = self.arith_unary(o, s)? else { return Ok(None) };
        loop {
            self.h();
            if self.arith_at_stop(o) {
                break;
            }
            if self.la(0) == u(b',') {
                break;
            }
            let Some((l, c)) = self.arith_op() else { break };
            let f = arith_prec(l);
            if f < t {
                break;
            }
            let un = self.b;
            for _ in 0..c {
                self.adv();
            }
            let d = self.mk(l, un, self.b, Vec::new())?;
            let b = if ARITH_RIGHT.contains(&l) { f } else { f + 1 };
            let Some(p) = self.arith_binary(o, b, s)? else { break };
            i = self.mk("binary_expression", self.start(i), self.end(p), vec![i, d, p])?;
        }
        Ok(Some(i))
    }

    /// 原文 `ue`：前缀运算。
    fn arith_unary(&mut self, o: &str, t: Ar) -> R<Option<Id>> {
        self.guard()?;
        self.arith_unary_inner(o, t)
    }

    fn arith_unary_inner(&mut self, o: &str, t: Ar) -> R<Option<Id>> {
        self.h();
        if self.arith_at_stop(o) {
            return Ok(None);
        }
        let (s, i) = (self.la(0), self.la(1));
        if (s == u(b'+') && i == u(b'+')) || (s == u(b'-') && i == u(b'-')) {
            let r = self.b;
            self.adv();
            self.adv();
            let l = self.mk(if s == u(b'+') { "++" } else { "--" }, r, self.b, Vec::new())?;
            let Some(c) = self.arith_unary(o, t)? else { return Ok(Some(l)) };
            return Ok(Some(self.mk(
                "unary_expression",
                self.start(l),
                self.end(c),
                vec![l, c],
            )?));
        }
        if in_set(s, b"-+!~") {
            if t != Ar::Var && s == u(b'-') && is_digit(i) {
                let f = self.b;
                self.adv();
                while is_digit(self.la(0)) {
                    self.adv();
                }
                return Ok(Some(self.mk("number", f, self.b, Vec::new())?));
            }
            let r = self.b;
            self.adv();
            let k: &'static str = match s as u8 {
                b'-' => "-",
                b'+' => "+",
                b'!' => "!",
                _ => "~",
            };
            let l = self.mk(k, r, self.b, Vec::new())?;
            let Some(c) = self.arith_unary(o, t)? else { return Ok(Some(l)) };
            return Ok(Some(self.mk(
                "unary_expression",
                self.start(l),
                self.end(c),
                vec![l, c],
            )?));
        }
        self.arith_postfix(o, t)
    }

    /// 原文 `un`：后缀 `++` / `--`。
    fn arith_postfix(&mut self, o: &str, t: Ar) -> R<Option<Id>> {
        let Some(s) = self.arith_primary(o, t)? else { return Ok(None) };
        let (i, r) = (self.la(0), self.la(1));
        if (i == u(b'+') && r == u(b'+')) || (i == u(b'-') && r == u(b'-')) {
            let l = self.b;
            self.adv();
            self.adv();
            let c = self.mk(if i == u(b'+') { "++" } else { "--" }, l, self.b, Vec::new())?;
            return Ok(Some(self.mk(
                "postfix_expression",
                self.start(s),
                self.end(c),
                vec![s, c],
            )?));
        }
        Ok(Some(s))
    }

    /// 原文 `fn`：括号、字符串、`$…`、数字、名字（含 `name=`、`name[…]`）。
    fn arith_primary(&mut self, o: &str, t: Ar) -> R<Option<Id>> {
        self.h();
        if self.arith_at_stop(o) {
            return Ok(None);
        }
        let s = self.la(0);
        if s == u(b'(') {
            let i = self.b;
            self.adv();
            let r = self.mk("(", i, self.b, Vec::new())?;
            let l = self.arith_list(")", t)?;
            self.h();
            let c = if self.la(0) == u(b')') {
                let f = self.b;
                self.adv();
                self.mk(")", f, self.b, Vec::new())?
            } else {
                self.mk(")", self.b, self.b, Vec::new())?
            };
            let mut kids = vec![r];
            kids.extend(l);
            kids.push(c);
            return Ok(Some(self.mk(
                "parenthesized_expression",
                self.start(r),
                self.end(c),
                kids,
            )?));
        }
        if s == u(b'"') {
            return Ok(Some(self.dquote()?));
        }
        if s == u(b'$') {
            return Ok(Some(self.dollar()?));
        }
        if is_digit(s) {
            let i = self.b;
            while is_digit(self.la(0)) {
                self.adv();
            }
            if self.b - i == 1 && s == u(b'0') && (self.la(0) == u(b'x') || self.la(0) == u(b'X')) {
                self.adv();
                while is_hex(self.la(0)) {
                    self.adv();
                }
            } else if self.la(0) == u(b'#') {
                self.adv();
                while is_base_digit(self.la(0)) {
                    self.adv();
                }
            }
            return Ok(Some(self.mk("number", i, self.b, Vec::new())?));
        }
        if is_letter(s) {
            let i = self.b;
            while is_alnum(self.la(0)) {
                self.adv();
            }
            let r = self.la(0);
            if t == Ar::Assign {
                self.h();
                let (c, f) = (self.la(0), self.la(1));
                if c == u(b'=') && f != u(b'=') {
                    let un = self.mk("variable_name", i, self.b, Vec::new())?;
                    let d = self.b;
                    self.adv();
                    let b = self.mk("=", d, self.b, Vec::new())?;
                    let p = self.arith(o, t)?;
                    let x = p.map_or(self.end(b), |p| self.end(p));
                    let kids = match p {
                        Some(p) => vec![un, b, p],
                        None => vec![un, b],
                    };
                    return Ok(Some(self.mk("variable_assignment", i, x, kids)?));
                }
            }
            if r == u(b'[') {
                let c = self.mk("variable_name", i, self.b, Vec::new())?;
                let f = self.b;
                self.adv();
                let un = self.mk("[", f, self.b, Vec::new())?;
                let d = match self.arith("]", Ar::Var)? {
                    Some(d) => Some(d),
                    None => Some(self.dollar()?),
                };
                self.h();
                let b = if self.la(0) == u(b']') {
                    let x = self.b;
                    self.adv();
                    self.mk("]", x, self.b, Vec::new())?
                } else {
                    self.mk("]", self.b, self.b, Vec::new())?
                };
                let kids = match d {
                    Some(d) => vec![c, un, d, b],
                    None => vec![c, un, b],
                };
                return Ok(Some(self.mk("subscript", i, self.end(b), kids)?));
            }
            return Ok(Some(self.mk(
                if t == Ar::Var { "variable_name" } else { "word" },
                i,
                self.b,
                Vec::new(),
            )?));
        }
        Ok(None)
    }

    /// 原文 `V`：到没到算术表达式的收尾。
    fn arith_at_stop(&self, o: &str) -> bool {
        let t = self.la(0);
        match o {
            "))" => t == u(b')') && self.la(1) == u(b')'),
            ")" => t == u(b')'),
            ";" => t == u(b';'),
            ":" => t == u(b':'),
            "]" => t == u(b']'),
            "}" => t == u(b'}'),
            ":}" => t == u(b':') || t == u(b'}'),
            _ => t == EOF || t == u(b'\n'),
        }
    }
}

/// 原文 `cn`：算术运算符优先级。
fn arith_prec(op: &str) -> i32 {
    match op {
        "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "^=" | "|=" => 2,
        "||" => 4,
        "&&" => 5,
        "|" => 6,
        "^" => 7,
        "&" => 8,
        "==" | "!=" => 9,
        "<" | ">" | "<=" | ">=" => 10,
        "<<" | ">>" => 11,
        "+" | "-" => 12,
        "*" | "/" | "%" => 13,
        _ => 14,
    }
}

/// 原文 `ln`：右结合的算术运算符。
const ARITH_RIGHT: [&str; 12] =
    ["=", "+=", "-=", "*=", "/=", "%=", "<<=", ">>=", "&=", "^=", "|=", "**"];

/// `/^-?(0x)?[0-9]+#$/`：`16#` 这种进制前缀。
fn is_base_prefix(s: &[u16]) -> bool {
    let s = s.strip_prefix(&[u16::from(b'-')]).unwrap_or(s);
    let s = s.strip_prefix(&[u16::from(b'0'), u16::from(b'x')]).unwrap_or(s);
    let Some(d) = s.strip_suffix(&[u16::from(b'#')]) else { return false };
    is_uint(d)
}

/// `/^[-+?*@!][a-zA-Z]/`。
fn extglob_like_word(s: &[u16]) -> bool {
    s.len() >= 2
        && has_unit(&s[..1], b"-+?*@!")
        && is_letter(i32::from(s[1]))
        && s[1] != u16::from(b'_')
}

/// `/^(?:[ \t]|\\\n)*$/`。
fn only_blank_or_continuation(s: &[u16]) -> bool {
    let mut i = 0;
    while i < s.len() {
        if s[i] == u16::from(b' ') || s[i] == u16::from(b'\t') {
            i += 1;
        } else if s[i] == u16::from(b'\\') && s.get(i + 1) == Some(&u16::from(b'\n')) {
            i += 2;
        } else {
            return false;
        }
    }
    true
}

/// 原文 `HEr`：`/\$(?:\{(?:\([^)}]{0,32}\))?)?[#^=~+!]*(?:[A-Za-z_]\w*)?[\u0080-￿]/`——`${` 里的名字带
/// 非 ASCII 字符（zsh 与 bash 认法不同）。
fn her(s: &[u16]) -> bool {
    let at = |i: usize| s.get(i).map_or(EOF, |c| i32::from(*c));
    let tail = |mut i: usize| {
        while in_set(at(i), b"#^=~+!") {
            i += 1;
        }
        if is_letter(at(i)) {
            i += 1;
            while is_alnum(at(i)) {
                i += 1;
            }
        }
        at(i) >= 0x80
    };
    for p in 0..s.len() {
        if s[p] != u16::from(b'$') {
            continue;
        }
        if tail(p + 1) {
            return true;
        }
        if at(p + 1) == u(b'{') {
            if tail(p + 2) {
                return true;
            }
            if at(p + 2) == u(b'(') {
                let mut j = p + 3;
                let mut n = 0;
                while n <= 32 && j < s.len() && s[j] != u16::from(b')') && s[j] != u16::from(b'}') {
                    j += 1;
                    n += 1;
                }
                if n <= 32 && at(j) == u(b')') && tail(j + 1) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同一条连着要两次取同一棵；一段额度用完之后的解析按放弃处理（单次时限也压在额度以内），出了那一段照常。
    #[test]
    fn repeats_are_cached_and_a_spent_budget_gives_up() {
        let a = parse("echo hi").unwrap();
        let b = parse("echo hi").unwrap();
        assert!(Rc::ptr_eq(&a, &b), "同一条只解析一次");
        {
            let _budget = ParseBudget::start(Duration::ZERO);
            {
                let _inner = ParseBudget::start(Duration::from_secs(60));
                assert!(parse("ls -la").is_none(), "嵌套那段不另开额度，外层用完了就是用完了");
            }
            assert!(parse("git status").is_none());
            assert_eq!(super::super::bash_kinds::shell_run("git status", false).kinds, "unparsed");
        }
        assert!(parse("git status").is_some(), "那一段过去就不再限");
    }
}
