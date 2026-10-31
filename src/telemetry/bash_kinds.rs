//! 官方 2.1.311 起 Bash 命令画像里的 `bash_command_kinds`（`yte` 的那一项），逐函数移植。
//!
//! 官方先把命令解析成语法树（与 [`super::bash_ast`] 同一个解析器），按先序挑出所有简单命令（`BMn`），
//! 每条去掉赋值与重定向、剥掉引号（`WMn`），再剥前缀命令（`jMn`）、认生态与动作（`LB`）；`bash -c` /
//! `eval` 里的脚本再解析一层（`GMn`）。收集到的 kind 去重后按 JS 默认排序用逗号连起来（`ha`）。
//! 解析不了报 `unparsed`，嵌套或脚本超限报 `incomplete`，什么都没认出来报 `shell_word`。
//!
//! 函数名后的注释是原文里的函数名；表（`*_kind`、[`tool`] 等）由原文的对象原样导出生成。
//!
//! 与原文不完全一样的两处，都碰不到真实命令：
//! - `bash -c` / `eval` 里那段脚本，原文把「剩下的时间」（首次进脚本起 20ms）作为解析时限传给解析器；
//!   [`super::bash_ast::parse`] 的时限固定 50ms，这里只在进脚本前看一眼 20ms 是否已经用完。原文解析器每
//!   128 个节点才看一次时限，2000 字符以内的脚本几十微秒就解析完，两者实际上分不出来。
//! - 解析器是 2.1.299 的移植。2.1.311 的解析器多了三道默认开着的防护（`tengu_tingly_pearl` 的「前缀后跟
//!   保留字」、`tengu_calm_nautilus` 的模式引号、`tengu_foamy_heron` 的 `$$` 运算数），只会把整棵树或
//!   其中一段改成 `ERROR` 节点；在对拍语料上开与关得出的 kind 逐条相同。

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use super::bash_ast;

mod v316;

/// 原文 `pM`：超过这么多 UTF-16 码元的命令不解析（报 `unparsed`）。
const MAX_PARSE_LEN: usize = 10_000;
/// 原文 `u1r`：`LB` 递归超过这一层就放弃（报 `incomplete`）。
const MAX_DEPTH: u32 = 6;
/// 原文 `c1r`：`bash -c` / `eval` 里的脚本最多再嵌套这么多层。
const MAX_SCRIPT_DEPTH: u32 = 2;
/// 原文 `p1r`：一条命令里所有内嵌脚本加起来最多这么多 UTF-16 码元。
const SCRIPT_CHARS: i64 = 2_000;
/// 原文 `y1r`：从第一次进内嵌脚本算起的时间预算。
const SCRIPT_TIME: Duration = Duration::from_millis(20);
/// 原文 `LWr`：前缀命令最多剥这么多层。
const WRAPPER_ROUNDS: usize = 4;

/// `Cot` 顺序排好的 `(动作 kind, 词表)`。
type Pairs = &'static [(&'static str, &'static [&'static str])];

/// `IMn` 里一个工具的配置；没列的字段就是原文里没有那个键。
struct Tool {
    verbs: Pairs,
    anywhere: Pairs,
    bare: Option<&'static str>,
    script_after: &'static [&'static str],
    command_after: &'static [&'static str],
    verb_may_be_script: bool,
    value_flags: &'static [&'static str],
    runs_the_rest: bool,
    /// 2.1.316 `ownVerbs`：第一个操作数是这些词时这条命令就到此为止（`c8 report`、`nyc merge` 不是跑测试）。
    own_verbs: &'static [&'static str],
    /// 2.1.316 `verbIsTarget`：动作位置上的词是这个工具自己的目标（`make foo`、`just foo`），再认一层时不算一个程序。
    verb_is_target: bool,
}

/// 原文的 `{}`：没列在 `IMn` 里的工具。
const NO_TOOL: Tool = Tool {
    verbs: &[],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};

/// 一条 shell 命令的 `bash_command_kinds`（`yte(e).bash_command_kinds`）。
#[cfg(test)]
pub(super) fn bash_command_kinds(cmd: &str) -> String {
    shell_run(cmd, false).kinds
}

/// 一条 shell 命令在 Bash 事件里由语法树判出的几样，一次解析得出（[`shell_run`]）。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ShellRun {
    /// `bash_command_kinds`。
    pub(super) kinds: String,
    /// 跑完时 shell 的进程组里还留着进程（`left_process_running`）：官方在 shell 退出后探一下它那个进程组
    /// （`leftRunningPgid` / `oNo(pid)`），还活着就是真。非交互 shell 里 `&` 起的后台任务不另开进程组，`nohup`、
    /// `disown` 也不换组，所以就是「树里（含 `bash -c` / `eval` 里那段）有 `&`、又没有一条 `wait`」。`setsid`
    /// 换了会话的、后台任务恰好在 shell 退出前跑完的分不出来，按留着算；解析不了的按没留算。
    pub(super) leaves_running: bool,
    /// 2.1.315 起的 `bash_programs`：认得的程序名（[`v316::program_of`]），认不得的是 `other`，按 JS 默认排序用逗号
    /// 连起来；一个都没有是 `none`，放弃了或超过 [`v316::MAX_PROGRAMS`] 个是 `incomplete`，解析不了是 `unparsed`。
    /// 按 2.1.311 的规则认（`v316` 为假）时是空串，事件里不报。
    pub(super) programs: String,
    /// 2.1.315 起的 `bash_test_scope`：kind 里有 `runs_tests`（且没放弃）时才有，取各条测试命令范围里
    /// [`v316::SCOPE_ORDER`] 最靠前的那个，一个都没算出来是 `unknown`（[`test_scope`]）。
    pub(super) test_scope: Option<&'static str>,
}

/// 一条 shell 命令的 [`ShellRun`]（`Cfe(e)` / 2.1.311 `yte(e)` 里语法树那几项）。`v316` 是会话版本 ≥2.1.315：
/// 按 2.1.316 的表与规则认，另算 `bash_programs` 与 `bash_test_scope`；否则照 2.1.311（直到 2.1.314 都没变）。
pub(super) fn shell_run(cmd: &str, v316: bool) -> ShellRun {
    let Some(parsed) = scan(cmd) else {
        return ShellRun {
            kinds: "unparsed".into(),
            leaves_running: false,
            programs: if v316 { "unparsed".into() } else { String::new() },
            test_scope: None,
        };
    };
    let mut st = State {
        v316,
        kinds: BTreeSet::new(),
        programs: BTreeSet::new(),
        test_scopes: BTreeSet::new(),
        fed_more_words: false,
        script_depth: 0,
        script_chars_left: SCRIPT_CHARS,
        script_deadline: None,
        gave_up: false,
        background: false,
        waits: false,
    };
    st.walk(&parsed);
    let leaves_running = st.background && !st.waits;
    if st.kinds.is_empty() {
        st.kinds.insert("shell_word");
    }
    // `fa`：JS 默认排序（UTF-16 码元序），kind 与程序名全是 ASCII，与字节序相同。
    let join = |set: &BTreeSet<&'static str>| set.iter().copied().collect::<Vec<_>>().join(",");
    let programs = if !v316 {
        String::new()
    } else if st.gave_up || st.programs.len() > v316::MAX_PROGRAMS {
        "incomplete".into()
    } else if st.programs.is_empty() {
        "none".into()
    } else {
        join(&st.programs)
    };
    if st.gave_up {
        return ShellRun { kinds: "incomplete".into(), leaves_running, programs, test_scope: None };
    }
    let test_scope = (v316 && st.kinds.contains("runs_tests")).then(|| {
        v316::SCOPE_ORDER.iter().copied().find(|s| st.test_scopes.contains(s)).unwrap_or("unknown")
    });
    ShellRun { kinds: join(&st.kinds), leaves_running, programs, test_scope }
}

/// 解析结果里 `bash_command_kinds` 用得到的两样（原文 `BMn` 收集的那个对象）。
struct Parsed {
    /// 先序遍历遇到的每条简单命令，已去掉赋值与重定向、剥好引号（`WMn` 里那一步）。
    commands: Vec<Vec<String>>,
    /// 语法树里出现过 `&`。
    background: bool,
}

/// `zMn` + `BMn`：解析并收集简单命令；空串、过长或解析失败返回 `None`。
fn scan(src: &str) -> Option<Parsed> {
    if src.is_empty() || src.encode_utf16().count() > MAX_PARSE_LEN {
        return None;
    }
    let tree = bash_ast::parse(src)?;
    let mut out = Parsed { commands: Vec::new(), background: false };
    // 先序遍历（原文递归；这里用显式栈，深嵌套也不压线程栈）。
    let mut stack = vec![tree.root];
    while let Some(id) = stack.pop() {
        match tree.kind(id) {
            "command" => out.commands.push(command_words(&tree, id)),
            "&" => out.background = true,
            _ => {}
        }
        stack.extend(tree.children(id).iter().rev());
    }
    Some(out)
}

/// `WMn` 里那一步：命令的子节点去掉 `variable_assignment` 与各种 `*_redirect`，剥一层引号。
fn command_words(tree: &bash_ast::Tree, id: bash_ast::Id) -> Vec<String> {
    tree.children(id)
        .iter()
        .filter(|&&c| {
            let k = tree.kind(c);
            k != "variable_assignment" && !k.ends_with("_redirect")
        })
        .map(|&c| unquote(tree.text(c)))
        .collect()
}

/// 首尾是同一个引号就剥掉：单引号原样取里面；双引号再把 `\"` `\\` `\$` `` \` `` 还原；
/// 不是引号但以 `\` 开头的去掉这一个反斜杠。按 UTF-16 码元比，与原文一致。
fn unquote(y: &[u16]) -> String {
    const SQ: u16 = b'\'' as u16;
    const DQ: u16 = b'"' as u16;
    const BS: u16 = b'\\' as u16;
    let quote = (y.len() >= 2 && y[0] == y[y.len() - 1]).then(|| y[0]);
    let body: Vec<u16> = match quote {
        Some(SQ) => y[1..y.len() - 1].to_vec(),
        Some(DQ) => {
            let s = &y[1..y.len() - 1];
            let mut out = Vec::with_capacity(s.len());
            let mut i = 0;
            while i < s.len() {
                let escaped = s[i] == BS
                    && s.get(i + 1)
                        .is_some_and(|&c| [DQ, BS, b'$' as u16, b'`' as u16].contains(&c));
                if escaped {
                    out.push(s[i + 1]);
                    i += 2;
                } else {
                    out.push(s[i]);
                    i += 1;
                }
            }
            out
        }
        _ if y.first() == Some(&BS) => y[1..].to_vec(),
        _ => y.to_vec(),
    };
    String::from_utf16_lossy(&body)
}

/// 原文 `Cfe` / `yte` 里那个 `d` / `y`：收集中的 kind、程序与测试范围，与内嵌脚本的预算。最后两项是
/// [`ShellRun::leaves_running`] 的材料，原文没有。
struct State {
    /// 按 2.1.316 的表与规则认（[`shell_run`]）。
    v316: bool,
    kinds: BTreeSet<&'static str>,
    /// `programs`（只在 `v316` 时收集）。
    programs: BTreeSet<&'static str>,
    /// `testScopes`（只在 `v316` 时收集）。
    test_scopes: BTreeSet<&'static str>,
    /// `fedMoreWords`：这条简单命令前面有 `xargs` / `parallel` 一类会再塞参数进来的前缀，操作数读不全。每条简单命令
    /// 开始时清掉（`Ve`）。
    fed_more_words: bool,
    script_depth: u32,
    script_chars_left: i64,
    script_deadline: Option<Instant>,
    gave_up: bool,
    /// 走过的树（含内嵌脚本）里有 `&`。
    background: bool,
    /// 走过的树里有一条 `wait`。
    waits: bool,
}

impl State {
    /// `WMn`：整棵树（或一段内嵌脚本）里的每条简单命令。
    fn walk(&mut self, p: &Parsed) {
        if p.background {
            self.kinds.insert("starts_background_job");
            self.background = true;
        }
        for words in &p.commands {
            if words.first().is_some_and(|w| w == "wait") {
                self.waits = true;
            }
            self.fed_more_words = false;
            self.classify(words, 0, None, false);
        }
    }

    /// `GMn`：`bash -c` / `eval` 的脚本再解析一层。超深度、超字数、超时或解析不了都算放弃。
    fn script(&mut self, text: &str) {
        let now = Instant::now();
        let deadline = *self.script_deadline.get_or_insert(now + SCRIPT_TIME);
        self.script_chars_left -= text.encode_utf16().count() as i64;
        let parsed = (self.script_depth < MAX_SCRIPT_DEPTH
            && self.script_chars_left >= 0
            && now <= deadline)
            .then(|| scan(text))
            .flatten();
        let Some(p) = parsed else {
            self.gave_up = true;
            return;
        };
        self.script_depth += 1;
        self.walk(&p);
        self.script_depth -= 1;
    }

    /// 工具名 → 语言 / 生态 kind，按这次用的那一版的表。
    fn kind_of(&self, name: &str) -> Option<&'static str> {
        if self.v316 { v316::kind_of(name) } else { kind_of(name) }
    }

    /// 工具名 → 动作 kind，同上。
    fn extra_kind(&self, name: &str) -> Option<&'static str> {
        if self.v316 { v316::extra_kind(name) } else { extra_kind(name) }
    }

    /// 工具配置，同上。
    fn tool(&self, name: &str) -> Option<&'static Tool> {
        if self.v316 { v316::tool(name) } else { tool(name) }
    }

    /// `H`（2.1.311 `LB`）：一条简单命令（已分好词）。`want` 是原文第四个参数：只认生态 kind 等于它（或是浏览器）的
    /// 命令；`target` 是第五个参数（2.1.316）：这个词是上一层工具的目标，不算一个程序。
    fn classify(&mut self, e: &[String], depth: u32, want: Option<&'static str>, target: bool) {
        if depth > MAX_DEPTH {
            self.gave_up = true;
        }
        if self.gave_up {
            return;
        }
        if e.first().is_some_and(|w| w == "command")
            && e.get(1).is_some_and(|w| w == "-v" || w == "-V")
        {
            self.kinds.insert("shell_word");
            if self.v316 {
                self.programs.insert("command");
            }
            return;
        }
        let y = strip_wrappers(e);
        let Some(first) = y.first() else { return };
        let b = tool_name(first);
        let w = self.kind_of(&b);
        if let Some(want) = want
            && w != Some(want)
            && w != Some("browser")
        {
            return;
        }
        if self.v316 && !target {
            self.programs.insert(v316::program_of(&b).unwrap_or("other"));
        }
        let prefix = &e[..e.len() - y.len()];
        if b == "disown" || prefix.iter().any(|t| DETACHERS.contains(&basename(t))) {
            self.kinds.insert("starts_background_job");
        }
        if self.v316 && prefix.iter().any(|t| v316::FEEDS_MORE_WORDS.contains(&basename(t))) {
            self.fed_more_words = true;
        }
        if SHELLS.contains(&b.as_str()) {
            self.shell(y);
            return;
        }
        if b == "eval" {
            let d = y[1..].join(" ");
            let d = js_trim(&d);
            if !d.is_empty() {
                self.script(d);
                return;
            }
        }
        if b == "find" {
            let mut te = 1;
            while te < y.len() {
                if FIND_EXEC.contains(&y[te].as_str()) {
                    let mut ve = te + 1;
                    while ve < y.len() && y[ve] != ";" && y[ve] != "+" {
                        ve += 1;
                    }
                    self.classify(&y[te + 1..ve], depth + 1, None, false);
                    te = ve;
                }
                te += 1;
            }
        }
        let Some(w) = w else {
            let class = command_class(&normalize_argv0(&b));
            let ext = match b.rfind('.') {
                Some(i) if i > 0 => &b[i + 1..],
                _ => "",
            };
            let k = class.and_then(class_kind).or_else(|| ext_kind(ext)).unwrap_or("other");
            self.kinds.insert(k);
            return;
        };
        self.kinds.insert(w);
        let coverage_run = self.v316 && b == "coverage" && y.get(1).is_some_and(|t| t == "run");
        if b == "python" {
            self.python(y, depth);
        } else if coverage_run {
            // 2.1.316：`coverage run -m pytest` 照 `python -m pytest` 认。
            self.python(&y[1..], depth);
        }
        let q = self.tool(&b).unwrap_or(&NO_TOOL);
        let v = q.value_flags;
        let op = next_operand(y, 1, v);
        let after = next_operand(y, op.index + 1, v);
        if let Some(tok) = op.token
            && q.own_verbs.contains(&tok)
        {
            return;
        }
        let mut we = first_cot(q.verbs, |t| op.token == Some(t));
        // 2.1.316 `R`：测试范围从哪个词起读。
        let mut from = op.index + 1;
        let rest = |i: usize| &y[i.min(y.len())..];
        if q.runs_the_rest {
            self.classify(rest(op.index), depth + 1, None, false);
        } else if let Some(tok) = op.token {
            if q.script_after.contains(&tok) {
                we = script_verb(after.token);
                from = after.index + 1;
            } else if q.command_after.contains(&tok) {
                we = script_verb(after.token);
                from = after.index + 1;
                if we.is_none() {
                    self.classify(rest(after.index), depth + 1, None, false);
                    return;
                }
            } else if we.is_none() && q.verb_may_be_script {
                we = script_verb(Some(tok));
                if we.is_none() {
                    self.classify(rest(op.index), depth + 1, Some(w), q.verb_is_target);
                }
            }
        } else if !INFO_FLAGS.iter().any(|f| y[1..].iter().any(|t| t == f)) {
            we = q.bare;
        }
        if we.is_none() {
            // 2.1.316 起跟在取值开关后面的不算（`gradle -x test` 不是跑测试）。
            we = if self.v316 {
                first_cot(q.anywhere, |t| {
                    (1..y.len()).any(|i| y[i] == t && !v.contains(&y[i - 1].as_str()))
                })
            } else {
                first_cot(q.anywhere, |t| y[1..].iter().any(|a| a == t))
            }
            .or_else(|| self.extra_kind(&b));
            from = 1;
        }
        if let Some(k) = we {
            self.kinds.insert(k);
        }
        if self.v316 && we == Some("runs_tests") && !q.runs_the_rest && !coverage_run {
            let scope = test_scope(&b, y, from, self.fed_more_words);
            self.test_scopes.insert(scope);
        }
        if WATCH_FLAGS.iter().any(|f| y[1..].iter().any(|t| t == f)) {
            self.kinds.insert("serves_or_watches");
        }
    }

    /// `L1r`：`bash` / `sh` / `zsh` / `dash` / `ksh`。带 `-c` 的把那段脚本再认一遍，否则算跑脚本。
    fn shell(&mut self, e: &[String]) {
        let mut r = 1;
        while r < e.len() {
            let s = e[r].as_str();
            if is_short_flags(s) && s.contains('c') {
                let h = if e.get(r + 1).is_some_and(|x| x == "--") {
                    e.get(r + 2)
                } else {
                    e.get(r + 1)
                };
                if let Some(h) = h.filter(|h| !h.is_empty()) {
                    self.script(h);
                    return;
                }
                break;
            }
            if takes_shell_option_arg(s) {
                r += 1;
            } else if !s.starts_with('-') {
                break;
            }
            r += 1;
        }
        self.kinds.insert("shell_script");
    }

    /// `Ne`（2.1.311 `N1r`）：`python` 后面的开关。`-m` 把模块当命令再认（只认生态是 python 的），`-c`、只有开关、
    /// `-` 都算内联代码；碰到脚本文件或 `-V` / `-h` / `--version` 一类就不再报。
    fn python(&mut self, e: &[String], depth: u32) {
        let mut s = 1;
        while s < e.len() && e[s] != "-" {
            let h = e[s].as_str();
            if !h.starts_with('-') || h.starts_with("--version") || h.starts_with("--help") {
                return;
            }
            let (b, w) = if h.starts_with("--") {
                (None, h)
            } else {
                match h.char_indices().find(|&(_, c)| "Vh?cmWX".contains(c)) {
                    Some((i, c)) => (Some(c), &h[i + c.len_utf8()..]),
                    None => (None, h),
                }
            };
            match b {
                Some('V' | 'h' | '?') => return,
                Some('c') => break,
                Some('m') => {
                    let mut d: Vec<String> = Vec::new();
                    if !w.is_empty() {
                        d.push(w.to_string());
                    }
                    d.extend(e[s + 1..].iter().cloned());
                    let module = d.first().map_or("", String::as_str);
                    if let Some(k) = py_module_kind(module) {
                        self.kinds.insert(k);
                        // 2.1.316：`python -m unittest` / `doctest` 本身是一个程序，也算一次测试范围。
                        if self.v316
                            && let Some(m) =
                                v316::PY_TEST_MODULES.iter().copied().find(|m| *m == module)
                        {
                            self.programs.insert(m);
                            let scope = test_scope(m, &d, 1, self.fed_more_words);
                            self.test_scopes.insert(scope);
                        }
                    } else if PY_TOOL_MODULES.contains(&module) {
                        self.classify(&d, depth + 1, None, false);
                    } else {
                        self.classify(&d, depth + 1, Some("python"), false);
                    }
                    return;
                }
                _ => {}
            }
            if matches!(b, Some('W' | 'X')) && w.is_empty() {
                s += 1;
            }
            s += 1;
        }
        self.kinds.insert("python_inline");
    }
}

/// `Ye`（2.1.316）：工具 `e` 这次跑测试的范围。`s` 是剥好前缀的整条命令，从 `n` 起是它的参数；`fed` 是前面有
/// `xargs` 一类会再塞参数的前缀（[`State::fed_more_words`]），读到的操作数不全。
///
/// 一个个读参数：开关按 [`v316::TestRunner::flags`] 定性（`--x=false` 当没说；`takes_expression` 看表达式是不是取反），
/// 会吃值的跳过下一个词；动作词只在第一个操作数位置上认（`verbsOnlyAfter` 要求紧跟在那个词之后）。认出
/// `runs_no_tests` 直接返回；其余操作数看是不是「整体」的写法（`.`、`tests/`……）、变量或通配、测试名、目录。
/// 最后按「只跑改动 / 失败的 > 收窄 > 收窄到目录 > 说不清 > 整体」的先后给一个。
fn test_scope(e: &str, s: &[String], n: usize, fed: bool) -> &'static str {
    let r = v316::test_runner(e);
    let lookup = |pairs: Option<Pairs>, w: &str| -> Option<&'static str> {
        pairs?.iter().find(|(_, words)| words.contains(&w)).map(|(k, _)| *k)
    };
    let (mut d, mut m, mut p, mut g, mut y) = (false, false, false, false, false);
    let mut h = r.is_none() || fed;
    let mut verb_pos = r.is_none_or(|r| {
        r.verbs_only_after.is_none_or(|v| n >= 1 && s.get(n - 1).is_some_and(|w| w == v))
    });
    let (mut dashes, mut after_long) = (false, false);
    let mut a = n;
    while a < s.len() {
        let w = s[a].as_str();
        let after_unknown_long = after_long;
        after_long = false;
        if !w.is_empty() && w.bytes().all(|c| c.is_ascii_digit()) {
            a += 1;
            continue;
        }
        let kind: Option<&'static str>;
        if w.encode_utf16().count() > 1 && w.starts_with('-') {
            let eq = w.find('=');
            let name = eq.map_or(w, |i| &w[..i]);
            let value: Option<&str> = match eq {
                Some(i) => Some(&w[i + 1..]),
                None => s.get(a + 1).map(String::as_str),
            };
            let mut k = if v316::RUNS_NO_TESTS_FLAGS.contains(&name) {
                Some("runs_no_tests")
            } else {
                lookup(r.map(|r| r.flags), name)
            };
            if eq.is_some() && value == Some("false") {
                k = Some("says_nothing");
            } else if k == Some("takes_expression") {
                k = Some(if negated_expression(value.unwrap_or("")) {
                    "takes_value"
                } else {
                    "narrowed"
                });
            }
            if k.is_some_and(|k| k != "says_nothing")
                && eq.is_none()
                && !value.is_some_and(|v| v.starts_with('-'))
            {
                a += 1;
            }
            dashes |= w == "--";
            after_long = k.is_none() && eq.is_none() && long_flag_not_no(w);
            kind = k;
        } else {
            kind = if verb_pos { lookup(r.map(|r| r.verbs), w) } else { None };
            verb_pos = false;
        }
        if kind == Some("runs_no_tests") {
            return "runs_no_tests";
        }
        d |= kind == Some("changed_or_failed_only");
        m |= kind == Some("narrowed");
        p |= kind == Some("narrowed_to_folders");
        if kind.is_some() || w.starts_with('-') {
            a += 1;
            continue;
        }
        let b = w.strip_prefix("./").unwrap_or(w);
        let b = b.strip_suffix('/').unwrap_or(b);
        if r.is_some_and(|r| r.whole_operands.contains(&b)) {
            g = true;
        } else if is_assignment(w) {
            h |= !s[a - 1].starts_with('-');
        } else if w.starts_with(['$', '`'])
            || w.contains('*')
            || w == "{}"
            || r.is_some_and(|r| r.operands == v316::Operands::GoToRecipe)
        {
            h = true;
        } else if let Some(r) = r.filter(|r| {
            r.operands == v316::Operands::NameTests
                || (r.operands == v316::Operands::NameTestsAfterDashes && dashes)
        }) {
            let v = after_unknown_long || fed || r.whole_operands.contains(&basename(b));
            let folder = !has_extension_or_colon(w)
                && (r.bare_operands == v316::BareOperands::Folders
                    || (r.bare_operands == v316::BareOperands::FoldersByForm
                        && (w.contains('/') || v316::FOLDER_NAMES.contains(&b))));
            h |= v;
            p |= !v && folder;
            m |= !v && !folder;
        } else if r.and_then(|r| r.less_than_package).is_some_and(|l| less_than_package(l, b)) {
            y |= !after_unknown_long;
        }
        a += 1;
    }
    if d {
        "changed_or_failed_only"
    } else if m {
        "narrowed"
    } else if p {
        "narrowed_to_folders"
    } else if h {
        "unknown"
    } else if !r.is_some_and(|r| r.whole_needs_operand) || g {
        "whole"
    } else if y {
        "narrowed"
    } else {
        "narrowed_to_folders"
    }
}

/// `/^['"]?(not |~|!)/`：`-k 'not slow'` 一类取反的表达式。
fn negated_expression(v: &str) -> bool {
    let v = v.strip_prefix(['\'', '"']).unwrap_or(v);
    v.starts_with("not ") || v.starts_with('~') || v.starts_with('!')
}

/// `/^--(?!no-)[^-]/`：认不得的长开关（不是 `--no-…`），它后面那个词可能是它的值。
fn long_flag_not_no(w: &str) -> bool {
    w.strip_prefix("--")
        .is_some_and(|r| !r.starts_with("no-") && !r.is_empty() && !r.starts_with('-'))
}

/// `Ft`：`/\.\w+$|:\w/`——带扩展名或 `file:line` 一类的写法，不当目录。
fn has_extension_or_colon(w: &str) -> bool {
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let b = w.as_bytes();
    let ext = b
        .iter()
        .rposition(|&c| c == b'.')
        .is_some_and(|i| i + 1 < b.len() && b[i + 1..].iter().all(|&c| word(c)));
    ext || b.windows(2).any(|p| p[0] == b':' && word(p[1]))
}

/// `lessThanPackage` 的两条正则。go：`/\.go$/`；bazel：
/// `/:(?!all$|all-targets$)\w|^@?[\w.-]*\/\/(?![^:]*\.\.\.$)[^:]*$/`——`//pkg:target`（不是 `:all` / `:all-targets`）
/// 或不带 `:`、不以 `...` 收尾的 `//pkg/dir`。带前瞻，regex 库不支持，照意思手写。
fn less_than_package(l: v316::LessThanPackage, w: &str) -> bool {
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    match l {
        v316::LessThanPackage::GoFile => w.ends_with(".go"),
        v316::LessThanPackage::BazelTarget => {
            let b = w.as_bytes();
            let named = b.iter().enumerate().any(|(i, &c)| {
                c == b':' && b.get(i + 1).is_some_and(|&n| word(n)) && {
                    let rest = &w[i + 1..];
                    rest != "all" && rest != "all-targets"
                }
            });
            if named {
                return true;
            }
            let t = w.strip_prefix('@').unwrap_or(w);
            let head = t.bytes().take_while(|&c| word(c) || c == b'.' || c == b'-').count();
            t[head..]
                .strip_prefix("//")
                .is_some_and(|rest| !rest.contains(':') && !rest.ends_with("..."))
        }
    }
}

/// `Vt`：测试结果只看输出的最后这么多个 UTF-16 码元（从那之前最近的换行起）。
const OUTCOME_TAIL: usize = 8192;

/// `Ze`：各家测试工具的「失败」「通过」判据（pytest、unittest、jest、vitest、mocha、node:test、bun、deno、go、cargo、
/// nextest、maven / gradle、dotnet、ctest、rspec、phpunit……），第三项是「看着像通过、其实有失败」的反证（vitest 的 `›`
/// 行）。原文是带 `m` 标志的 JS 正则，这里照 JS 语义改写：`\d` / `\w` 只认 ASCII，`.` 不跨 `\n` `\r` U+2028 U+2029，
/// `^` / `$` 也认 `\r` 行尾（`R`），`\b` 是 ASCII 词边界。
const OUTCOME_PATTERNS: &[&[&str]] = &[
    &[
        r#"(?mR)^(?:=* ?(?:[0-9]+ [A-Za-z0-9_]+, ){0,9}[1-9][0-9]* (?:failed|errors?)(?-u:\b)[^\n\r\x{2028}\x{2029}]{0,200} in [0-9][0-9.]*s(?-u:\b)|FAIL Required test coverage of [0-9.]+% not reached\.)"#,
        r#"(?mR)^=* ?[0-9]+ passed(?-u:\b)[^\n\r\x{2028}\x{2029}]{0,200} in [0-9][0-9.]*s(?-u:\b)"#,
    ],
    &[
        r#"(?mR)^FAILED \([a-z ]+=[0-9]"#,
        r#"(?mR)^Ran [0-9]+ tests? in [0-9.]+s[\t\n\x{0B}\x{0C}\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]+OK(?-u:\b)"#,
    ],
    &[
        r#"(?mR)^(?:Tests?(?: Suites)?: +(?:[0-9]+ [A-Za-z0-9_]+, ){0,9}[1-9][0-9]* failed(?-u:\b)|Jest: (?:"[^\n\r\x{2028}\x{2029}]{0,200}" coverage threshold for [A-Za-z0-9_]+ \([^\n\r\x{2028}\x{2029}]{0,20}\) not met: |Uncovered count for [A-Za-z0-9_]+ \([0-9]+\) exceeds |Coverage data for [^\n\r\x{2028}\x{2029}]{0,200} was not found\.))"#,
        r#"(?mR)^Tests: +(?:[0-9]+ [A-Za-z0-9_]+, ){0,9}[0-9]+ passed, [0-9]+ total(?-u:\b)"#,
    ],
    &[
        r#"(?mR)^ *(?:Tests?(?: Files)? +(?:[0-9]+ [A-Za-z0-9_]+ \| ){0,9}[1-9][0-9]* failed(?-u:\b)[^\n\r\x{2028}\x{2029}]{0,80}\([0-9]+\)|Errors +[1-9][0-9]* errors?(?-u:\b)|ERROR: (?:Coverage for [A-Za-z0-9_]+ \([0-9.]+%\) does not meet |Uncovered [A-Za-z0-9_]+ \([0-9]+\) exceed )[^\n\r\x{2028}\x{2029}]{0,200}threshold \()"#,
        r#"(?mR)^ *Tests +(?:[0-9]+ [A-Za-z0-9_]+ \| ){0,9}[0-9]+ passed(?-u:\b)[^\n\r\x{2028}\x{2029}]{0,80}\([0-9]+\)"#,
    ],
    &[r#"(?mR)^ *[1-9][0-9]* failing(?-u:\b)"#, r#"(?mR)^ *[0-9]+ passing(?-u:\b)"#],
    &[
        r#"(?mR)^ *[1-9][0-9]* failed$"#,
        r#"(?mR)^ *[0-9]+ passed \([0-9.]+(?:ms|s|m|h)\)"#,
        r#"(?mR)^ {4}\S[^\n\r\x{2028}\x{2029}]{0,400} \x{203A} "#,
    ],
    &[
        r#"(?mR)^ *(?:[1-9][0-9]* fail|[0-9]+ fail\r?\n *[1-9][0-9]* errors?)$"#,
        r#"(?mR)^ *0 fail$"#,
    ],
    &[r#"(?mR)^[#\x{2139}] (?:fail|cancelled) [1-9][0-9]*$"#, r#"(?mR)^[#\x{2139}] fail 0$"#],
    &[r#"(?mR)^FAILED \| [0-9]+ passed(?-u:\b)"#, r#"(?mR)^ok \| [0-9]+ passed(?-u:\b)"#],
    &[r#"(?mR)^(?:--- FAIL: |FAIL(?:\t|$))"#, r#"(?mR)^(?:ok {2}\t|PASS$)"#],
    &[
        r#"(?mR)^(?:test result: FAILED\.|error: (?:test failed|test run failed|[0-9]+ targets? failed|could not compile)(?-u:\b))"#,
        r#"(?mR)^test result: ok\."#,
    ],
    &[
        r#"(?mR)^ *Summary \[[^\]\n]{0,40}\] [0-9/]+ tests? run: [^\n\r\x{2028}\x{2029}]{0,200}(?-u:\b)[1-9][0-9]* (?:failed|timed out|exec failed)(?-u:\b)"#,
        r#"(?mR)^ *Summary \[[^\]\n]{0,40}\] [0-9/]+ tests? run: [0-9]+ passed(?-u:\b)"#,
    ],
    &[
        r#"(?mR)^(?:\[[A-Za-z0-9_]+\] +)?Tests run: [0-9]+, Failures: (?:[1-9][0-9]*, Errors: [0-9]+|0, Errors: [1-9])"#,
        r#"(?mR)^(?:\[[A-Za-z0-9_]+\] +)?Tests run: [0-9]+, Failures: 0, Errors: 0(?-u:\b)"#,
    ],
    &[
        r#"(?mR)^(?:(?:\[[A-Za-z0-9_]+\] +)?BUILD FAIL(?:URE|ED)(?-u:\b)|[0-9]+ tests? completed, [1-9][0-9]* failed(?-u:\b))"#,
        r#"(?mR)^(?:\[[A-Za-z0-9_]+\] +)?BUILD SUCCESS(?:FUL)?(?-u:\b)"#,
    ],
    &[r#"(?mR)^Failed! +- Failed:"#, r#"(?mR)^Passed! +- Failed:"#],
    &[
        r#"(?mR)^[0-9]+% tests passed, [1-9][0-9]* tests? failed out of [0-9]+"#,
        r#"(?mR)^100% tests passed, 0 tests failed out of [0-9]+"#,
    ],
    &[
        r#"(?mR)^[0-9]+ examples?, (?:[1-9][0-9]* failures?|0 failures, [0-9]+ errors? occurred)(?-u:\b)"#,
        r#"(?mR)^[0-9]+ examples?, 0 failures(?-u:\b)"#,
    ],
    &[r#"(?mR)^(?:FAILURES|ERRORS)!$"#, r#"(?mR)^OK \([0-9]+ tests?, [0-9]+ assertions?\)"#],
];

static OUTCOME_RES: std::sync::LazyLock<Vec<Vec<regex::Regex>>> = std::sync::LazyLock::new(|| {
    OUTCOME_PATTERNS
        .iter()
        .map(|g| g.iter().map(|r| regex::Regex::new(r).expect("测试结果正则")).collect())
        .collect()
});

/// `qt`（2.1.316）：一段测试命令输出的结果，`failed` / `passed` / `unknown`。只看最后 [`OUTCOME_TAIL`] 个码元、去掉
/// ANSI 转义；任一组的失败判据命中就是失败，否则有一组通过判据命中、且它的反证没命中是通过。
pub(super) fn test_outcome(output: &str) -> &'static str {
    let u: Vec<u16> = output.encode_utf16().collect();
    let tail = if u.len() > OUTCOME_TAIL {
        let s = u.len() - OUTCOME_TAIL;
        // `e.slice(e.indexOf("\n", s - 1))`：找不到换行时 `indexOf` 是 -1，`slice(-1)` 只剩最后一个码元。
        let from = u[s - 1..]
            .iter()
            .position(|&c| c == u16::from(b'\n'))
            .map_or(u.len() - 1, |i| s - 1 + i);
        String::from_utf16_lossy(&u[from..])
    } else {
        output.to_string()
    };
    let text = strip_ansi(&tail);
    let res = &*OUTCOME_RES;
    if res.iter().any(|g| g[0].is_match(&text)) {
        return "failed";
    }
    let passed =
        res.iter().any(|g| g[1].is_match(&text) && !g.get(2).is_some_and(|r| r.is_match(&text)));
    if passed { "passed" } else { "unknown" }
}

/// `wrr`（2.1.316）：Bash 事件里的 `bash_test_outcome`。只有算出了测试范围（[`ShellRun::test_scope`]）且不是
/// `runs_no_tests`、没转到后台的才有；判出「通过」但命令非零退出、被打断、超时，或者输出过了 `head`、程序没认全，
/// 改报 `unknown`。
pub(super) fn bash_test_outcome(
    run: &ShellRun,
    output: &str,
    exit_code: i64,
    interrupted: bool,
) -> Option<&'static str> {
    let scope = run.test_scope?;
    if scope == "runs_no_tests" {
        return None;
    }
    let a = test_outcome(output);
    let programs: Vec<&str> = run.programs.split(',').collect();
    let doubtful = exit_code != 0
        || interrupted
        || programs.contains(&"head")
        || programs.contains(&"incomplete");
    Some(if a == "passed" && doubtful { "unknown" } else { a })
}

/// `Bun.stripANSI`：去掉 CSI（`ESC [ … 终止字节`）、OSC（`ESC ] … BEL / ESC \`）与其余两字节的 ESC 序列。
fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\u{1b}') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('[') => {
                for d in it.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&d) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(d) = it.next() {
                    if d == '\u{7}' {
                        break;
                    }
                    if d == '\u{1b}' && it.peek() == Some(&'\\') {
                        it.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    std::borrow::Cow::Owned(out)
}

/// `jMn`（配 `t1r`）：剥掉打头的前缀命令（`sudo`、`env`、`timeout 5`……）连同它们的开关，最多 4 层；
/// 剥完什么都不剩就不剥这一层。
fn strip_wrappers(e: &[String]) -> &[String] {
    let mut r = e;
    for _ in 0..WRAPPER_ROUNDS {
        let Some(first) = r.first() else { break };
        let h = basename(first);
        let Some(flags) = wrapper_value_flags(h) else { break };
        if r.len() < 2 {
            break;
        }
        let mut b = 1;
        while b < r.len() {
            let w = r[b].as_str();
            if w == "--" {
                b += 1;
                break;
            }
            if flags.contains(&w) {
                b += 2;
            } else if w.starts_with('-')
                || is_assignment(w)
                || ((h == "timeout" || h == "nice") && is_duration(w))
            {
                b += 1;
            } else {
                break;
            }
        }
        if b >= r.len() {
            break;
        }
        r = &r[b..];
    }
    r
}

/// `sxe` / `$Mn` 的结果：第一个操作数与它的位置（没有时位置可能越过末尾）。
struct Operand<'a> {
    token: Option<&'a str>,
    index: usize,
}

/// `sxe`：从 `n` 起跳过开关（`value_flags` 里的连同后一个值）找第一个操作数。
fn first_operand<'a>(e: &'a [String], n: usize, value_flags: &[&str]) -> Operand<'a> {
    let mut s = n;
    while s < e.len() {
        let h = e[s].as_str();
        if value_flags.contains(&h) {
            s += 2;
        } else if h.starts_with('-') {
            s += 1;
        } else {
            return Operand { token: Some(h), index: s };
        }
    }
    Operand { token: None, index: s }
}

/// `$Mn`：同 [`first_operand`]，再跳过 `NAME=值` 形式的。
fn next_operand<'a>(e: &'a [String], n: usize, value_flags: &[&str]) -> Operand<'a> {
    let mut s = first_operand(e, n, value_flags);
    while let Some(t) = s.token
        && is_assignment(t)
    {
        s = first_operand(e, s.index + 1, value_flags);
    }
    s
}

/// `wot`：按 `Cot` 顺序第一个有词满足 `pred` 的动作 kind。
fn first_cot(pairs: Pairs, pred: impl Fn(&str) -> bool) -> Option<&'static str> {
    pairs.iter().find(|(_, words)| words.iter().any(|w| pred(w))).map(|(k, _)| *k)
}

/// `got`：脚本名（`test:unit`、`build-prod`、`dev_server`……）按打头的词认动作 kind。
fn script_verb(e: Option<&str>) -> Option<&'static str> {
    let e = e?;
    first_cot(SCRIPT_VERBS, |n| {
        e.starts_with(n)
            && (e.len() == n.len() || matches!(e.as_bytes()[n.len()], b':' | b'-' | b'_'))
    })
}

/// `Mot`：取最后一个 `/` 之后的部分。
fn basename(e: &str) -> &str {
    e.rsplit('/').next().unwrap_or(e)
}

/// `UMn`：argv0 归一（取文件名、别名、`python3.12` → `python`、`pip3.12` → `pip`）。
fn normalize_argv0(e: &str) -> String {
    let n = basename(e);
    if let Some(a) = argv0_alias(n) {
        return a.to_string();
    }
    let versioned = |prefix: &str| {
        n.strip_prefix(prefix)
            .is_some_and(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit() || c == b'.'))
    };
    if versioned("python") {
        "python".into()
    } else if versioned("pip") {
        "pip".into()
    } else {
        n.to_string()
    }
}

/// `D1r`：命令名去掉 `@版本`；带 scope 的 npm 包按 [`scoped_package`] 认，认不出是空串。
/// 「从最后一个 `/` 往后两个码元起找 `@`」按 UTF-16 码元算，与原文一致。
fn tool_name(e: &str) -> String {
    let u: Vec<u16> = e.encode_utf16().collect();
    let slash = u.iter().rposition(|&c| c == u16::from(b'/'));
    let from = slash.map_or(1, |i| i + 2);
    let at = u.iter().enumerate().skip(from).find(|&(_, &c)| c == u16::from(b'@')).map(|(i, _)| i);
    let r = match at {
        Some(i) => String::from_utf16_lossy(&u[..i]),
        None => e.to_string(),
    };
    if !r.starts_with('@') {
        return normalize_argv0(&r);
    }
    scoped_package(&r).unwrap_or("").to_string()
}

/// `Pot`：`/^[A-Za-z_]\w*=/`。
fn is_assignment(w: &str) -> bool {
    let b = w.as_bytes();
    if b.is_empty() || !(b[0].is_ascii_alphabetic() || b[0] == b'_') {
        return false;
    }
    let n = b.iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count();
    b.get(n) == Some(&b'=')
}

/// `/^[0-9.]+[smhd]?$/`：`timeout` / `nice` 后面的时长或数值。
fn is_duration(w: &str) -> bool {
    let body = w.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(w);
    !body.is_empty() && body.bytes().all(|c| c.is_ascii_digit() || c == b'.')
}

/// `/^-[A-Za-z]+$/`。
fn is_short_flags(s: &str) -> bool {
    s.strip_prefix('-').is_some_and(|r| !r.is_empty() && r.bytes().all(|c| c.is_ascii_alphabetic()))
}

/// `/^[-+][A-Za-z]*[oO]$|^--(rcfile|init-file)$/`：shell 吃掉下一个参数的那几个开关。
fn takes_shell_option_arg(s: &str) -> bool {
    if s == "--rcfile" || s == "--init-file" {
        return true;
    }
    let Some(r) = s.strip_prefix(['-', '+']) else { return false };
    !r.is_empty() && r.bytes().all(|c| c.is_ascii_alphabetic()) && r.ends_with(['o', 'O'])
}

/// JS `String.prototype.trim` 去掉的空白（与 Rust 的 `char::is_whitespace` 差 U+0085 与 U+FEFF）。
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    })
}

// ---- 以下各表由官方 2.1.311 的对象原样导出生成（原文名见各自注释）。----

/// `_ot`：argv0 → 命令类别（`AWr` × `PMn` 展开，后出现的覆盖先出现的）。
fn command_class(name: &str) -> Option<&'static str> {
    Some(match name {
        "Rscript" | "bash" | "bun" | "deno" | "java" | "node" | "perl" | "php" | "pwsh"
        | "python" | "ruby" | "sh" | "swift" | "ts-node" | "tsx" | "zsh" => "lang_runtime",
        "ag" | "du" | "egrep" | "fd" | "fgrep" | "find" | "grep" | "locate" | "ls" | "rg"
        | "tree" | "whereis" | "which" => "file_search",
        "alias" | "cd" | "date" | "echo" | "env" | "eval" | "exec" | "exit" | "export"
        | "false" | "printenv" | "printf" | "pwd" | "read" | "seq" | "set" | "sleep" | "source"
        | "test" | "trap" | "true" | "type" | "unset" | "wait" | "yes" => "shell_builtin",
        "apk" | "apt" | "apt-get" | "brew" | "bunx" | "cargo" | "composer" | "conda" | "gem"
        | "go" | "gradle" | "gradlew" | "mvn" | "nix" | "nix-build" | "nix-env" | "nix-shell"
        | "npm" | "npx" | "nuget" | "pip" | "pnpm" | "poetry" | "uv" | "uvx" | "yarn" => {
            "package_manager"
        }
        "awk" | "column" | "cut" | "expand" | "fmt" | "fold" | "gawk" | "iconv" | "jq" | "nl"
        | "numfmt" | "paste" | "rev" | "sed" | "sort" | "tac" | "tr" | "unexpand" | "uniq"
        | "xargs" | "yq" => "text_transform",
        "aws" | "az" | "docker" | "docker-compose" | "flyctl" | "gcloud" | "gsutil" | "helm"
        | "heroku" | "kubectl" | "podman" | "terraform" | "vercel" => "container_infra",
        "base64" | "basename" | "cat" | "cmp" | "comm" | "diff" | "dirname" | "file" | "head"
        | "hexdump" | "less" | "md5sum" | "more" | "od" | "readlink" | "realpath" | "sha1sum"
        | "sha256sum" | "shasum" | "stat" | "strings" | "tail" | "wc" | "xxd" => "file_read",
        "bazel" | "clang" | "cmake" | "dotnet" | "eslint" | "gcc" | "jest" | "make" | "msbuild"
        | "mypy" | "nx" | "prettier" | "pytest" | "ruff" | "rustc" | "tsc" | "turbo" | "vite"
        | "vitest" | "webpack" | "xcodebuild" => "build_test",
        "chgrp" | "chmod" | "chown" | "cp" | "gunzip" | "gzip" | "install" | "ln" | "mkdir"
        | "mv" | "patch" | "rm" | "rmdir" | "tar" | "tee" | "touch" | "truncate" | "unzip"
        | "zip" => "fs_mutation",
        "claude" => "claude_cli",
        "convert" | "ffmpeg" | "ffprobe" | "libreoffice" | "magick" | "pandoc" | "pdftotext"
        | "qpdf" | "soffice" => "document_media",
        "crontab" | "df" | "free" | "hostname" | "htop" | "id" | "kill" | "killall" | "lsof"
        | "netstat" | "nohup" | "nproc" | "pgrep" | "pkill" | "ps" | "ss" | "sudo"
        | "systemctl" | "time" | "timeout" | "top" | "ulimit" | "uname" | "uptime" | "watch"
        | "whoami" => "process_system",
        "curl" | "dig" | "nc" | "nslookup" | "ping" | "rsync" | "scp" | "ssh" | "wget" => "network",
        "duckdb" | "mongosh" | "mysql" | "psql" | "redis-cli" | "sqlite3" => "db_client",
        "gh" => "github_cli",
        "git" | "hg" | "svn" => "vcs",
        _ => return None,
    })
}

/// `MMn`：argv0 别名。
fn argv0_alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "." => "source",
        "ggrep" => "grep",
        "gmake" => "make",
        "gsed" => "sed",
        "nodejs" => "node",
        "pip3" => "pip",
        "python2" | "python3" => "python",
        _ => return None,
    })
}

/// `$Wr`：命令类别 → kind（只有这几类有对应）。
fn class_kind(name: &str) -> Option<&'static str> {
    Some(match name {
        "claude_cli" => "claude_cli",
        "container_infra" => "container_infra",
        "db_client" => "db_client",
        "file_read" => "file_read",
        "file_search" => "file_search",
        "fs_mutation" => "file_change",
        "github_cli" => "github_cli",
        "network" => "network",
        "process_system" => "process_system",
        "shell_builtin" => "shell_word",
        "text_transform" => "text_transform",
        "vcs" => "vcs",
        _ => return None,
    })
}

/// `WWr`：工具名 → 语言 / 生态 kind（`FWr` 顺序展开 `BWr`，同名后者覆盖）。
fn kind_of(name: &str) -> Option<&'static str> {
    Some(match name {
        "7z" | "7za" | "7zr" | "brotli" | "bunzip2" | "bzip2" | "cpio" | "gunzip" | "gzip"
        | "lz4" | "pigz" | "tar" | "unar" | "unrar" | "unxz" | "unzip" | "unzstd" | "xz"
        | "zcat" | "zip" | "zipinfo" | "zstd" => "archive",
        ":" | "bg" | "break" | "builtin" | "command" | "continue" | "disown" | "fg" | "hash"
        | "jobs" | "let" | "popd" | "pushd" | "return" | "shift" | "shopt" | "umask" => {
            "shell_word"
        }
        "MP4Box" | "avconv" | "ffmpeg" | "ffplay" | "ffprobe" | "flac" | "lame" | "mediainfo"
        | "mkvmerge" | "sox" | "whisper" | "yt-dlp" => "audio_video",
        "R" | "Rscript" | "bundle" | "composer" | "gem" | "julia" | "perl" | "php" | "phpunit"
        | "rails" | "rake" | "rspec" | "rubocop" | "ruby" => "other_runtime",
        "apk" | "apt" | "apt-cache" | "apt-get" | "brew" | "dnf" | "dpkg" | "nix" | "nix-build"
        | "nix-env" | "nix-shell" | "pacman" | "rpm" | "snap" | "yum" => "os_packages",
        "asciidoctor" | "biber" | "bibtex" | "extract-text" | "hugo" | "latex" | "latexmk"
        | "lualatex" | "markitdown" | "mkdocs" | "pandoc" | "pdflatex" | "quarto"
        | "sphinx-build" | "tectonic" | "typst" | "weasyprint" | "xelatex" => "typeset",
        "astro" | "babel" | "biome" | "bun" | "bunx" | "corepack" | "cypress" | "deno"
        | "esbuild" | "eslint" | "expo" | "gatsby" | "http-server" | "jest" | "mocha" | "next"
        | "ng" | "node" | "nodemon" | "npm" | "npx" | "nuxt" | "nx" | "parcel" | "pm2" | "pnpm"
        | "prettier" | "react-scripts" | "remix" | "rollup" | "serve" | "storybook"
        | "stylelint" | "svelte-kit" | "swc" | "tailwindcss" | "ts-node" | "tsc" | "tsup"
        | "tsx" | "turbo" | "vite" | "vitest" | "vue-cli-service" | "webpack" | "yarn" => "node",
        "bash" | "dash" | "fish" | "ksh" | "pwsh" | "sh" | "shellcheck" | "zsh" => "shell_script",
        "bazel" | "c++" | "cargo" | "cc" | "clang" | "clang++" | "clang-format" | "clang-tidy"
        | "cmake" | "ctest" | "dotnet" | "g++" | "gcc" | "go" | "gofmt" | "golangci-lint"
        | "gradle" | "gradlew" | "java" | "javac" | "kotlin" | "kotlinc" | "ld" | "make"
        | "meson" | "msbuild" | "mvn" | "mvnw" | "ninja" | "nuget" | "rustc" | "rustfmt"
        | "rustup" | "sbt" | "scala" | "scalac" | "swift" | "swiftc" | "xcodebuild" => {
            "compiled_lang"
        }
        "black" | "conda" | "coverage" | "flake8" | "flask" | "gunicorn" | "hatch" | "ipython"
        | "isort" | "jupyter" | "mamba" | "micromamba" | "mypy" | "nox" | "pdm" | "pip"
        | "pipenv" | "pipx" | "poetry" | "pre-commit" | "py.test" | "pyinstaller" | "pylint"
        | "pyright" | "pytest" | "python" | "ruff" | "streamlit" | "tox" | "uv" | "uvicorn"
        | "uvx" | "virtualenv" => "python",
        "chrome"
        | "chrome-headless-shell"
        | "chromedriver"
        | "chromium"
        | "chromium-browser"
        | "firefox"
        | "geckodriver"
        | "google-chrome"
        | "google-chrome-stable"
        | "headless_shell"
        | "lighthouse"
        | "mmdc"
        | "msedge"
        | "playwright"
        | "puppeteer" => "browser",
        "composite" | "convert" | "cwebp" | "dot" | "dwebp" | "exiftool" | "gifsicle" | "gm"
        | "identify" | "inkscape" | "jpegoptim" | "magick" | "mogrify" | "montage" | "optipng"
        | "pngquant" | "rsvg-convert" | "vips" | "vipsthumbnail" | "wkhtmltoimage" => "image",
        "gs" | "img2pdf" | "mutool" | "pdf2ps" | "pdfcrop" | "pdfdetach" | "pdffonts"
        | "pdfgrep" | "pdfimages" | "pdfinfo" | "pdfseparate" | "pdftk" | "pdftocairo"
        | "pdftohtml" | "pdftoppm" | "pdftotext" | "pdfunite" | "ps2pdf" | "qpdf"
        | "wkhtmltopdf" => "pdf",
        "libreoffice" | "localc" | "loimpress" | "lowriter" | "soffice" | "soffice.bin"
        | "unoconv" | "unoconvert" | "unoserver" => "office",
        "ocrmypdf" | "tesseract" => "ocr",
        "sleep" | "wait" => "wait",
        _ => return None,
    })
}

/// `XWr`：工具名 → 动作 kind（`Cot` 顺序展开 `UWr`）。
fn extra_kind(name: &str) -> Option<&'static str> {
    Some(match name {
        "babel" | "c++" | "cc" | "clang" | "clang++" | "cmake" | "esbuild" | "g++" | "gcc"
        | "javac" | "kotlinc" | "ld" | "meson" | "msbuild" | "ninja" | "parcel" | "pyinstaller"
        | "rollup" | "rustc" | "scalac" | "sphinx-build" | "swc" | "swiftc" | "tailwindcss"
        | "tsc" | "tsup" | "webpack" | "xcodebuild" => "builds",
        "biome" | "black" | "clang-format" | "clang-tidy" | "eslint" | "flake8" | "gofmt"
        | "golangci-lint" | "isort" | "mypy" | "pre-commit" | "prettier" | "pylint" | "pyright"
        | "rubocop" | "ruff" | "rustfmt" | "shellcheck" | "stylelint" => "lints",
        "corepack" | "rustup" | "virtualenv" => "installs_packages",
        "coverage" | "ctest" | "cypress" | "jest" | "mocha" | "nox" | "phpunit" | "py.test"
        | "pytest" | "rspec" | "tox" | "vitest" => "runs_tests",
        "gunicorn" | "http-server" | "nodemon" | "pm2" | "serve" | "streamlit" | "uvicorn"
        | "vite" => "serves_or_watches",
        _ => return None,
    })
}

/// `DMn`：`python -m <模块>` 直接定性的那几个模块。
fn py_module_kind(name: &str) -> Option<&'static str> {
    Some(match name {
        "build" | "compileall" | "py_compile" => "builds",
        "doctest" | "unittest" => "runs_tests",
        "ensurepip" | "venv" => "installs_packages",
        "http.server" => "serves_or_watches",
        _ => return None,
    })
}

/// `LMn`：带 scope 的 npm 包名 → 工具名。
fn scoped_package(name: &str) -> Option<&'static str> {
    Some(match name {
        "@angular/cli" => "ng",
        "@biomejs/biome" => "biome",
        "@mermaid-js/mermaid-cli" => "mmdc",
        "@playwright/test" => "playwright",
        "@puppeteer/browsers" => "puppeteer",
        _ => return None,
    })
}

/// `NMn`：脚本扩展名 → kind。
fn ext_kind(name: &str) -> Option<&'static str> {
    Some(match name {
        "bash" | "sh" => "shell_script",
        "cjs" | "js" | "mjs" | "ts" => "node",
        "php" | "pl" | "rb" => "other_runtime",
        "py" => "python",
        _ => return None,
    })
}

/// `t1r`：包在命令前面的「前缀命令」与它们吃参数的那几个开关（`jMn` 剥掉它们）。
fn wrapper_value_flags(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "builtin" => &[],
        "caffeinate" => &["-w", "-t"],
        "command" => &[],
        "coproc" => &[],
        "doas" => &["-u", "-C"],
        "env" => &["-u", "-C", "-S", "--unset", "--chdir"],
        "exec" => &["-a"],
        "ionice" => &["-c", "-n", "--class", "--classdata"],
        "nice" => &["-n", "--adjustment"],
        "noglob" => &[],
        "nohup" => &[],
        "parallel" => &["-j", "--jobs"],
        "setsid" => &[],
        "stdbuf" => &["-i", "-o", "-e", "--input", "--output", "--error"],
        "strace" => &["-o", "-e", "-p", "-s"],
        "sudo" => &["-u", "-g", "-U", "-p", "-C", "-D", "-h", "-r", "-t"],
        "taskset" => &["-c", "--cpu-list"],
        "time" => &["-f", "-o", "--format", "--output"],
        "timeout" => &["-k", "-s", "--kill-after", "--signal"],
        "unbuffer" => &[],
        "xargs" => &[
            "-n",
            "-I",
            "-P",
            "-d",
            "-L",
            "-s",
            "-a",
            "-E",
            "--max-args",
            "--max-procs",
            "--delimiter",
            "--arg-file",
            "--replace",
        ],
        "xvfb-run" => &[
            "-n",
            "-s",
            "-e",
            "-f",
            "-p",
            "-w",
            "--server-num",
            "--server-args",
            "--error-file",
            "--auth-file",
            "--wait",
        ],
        _ => return None,
    })
}

/// `JWr`：脚本名打头的那几个词 → 动作 kind（按 `Cot` 顺序排）。
const SCRIPT_VERBS: Pairs = &[
    ("runs_tests", &["test", "tests", "e2e"]),
    ("builds", &["build", "compile", "bundle"]),
    ("lints", &["lint", "typecheck", "type-check", "format", "check"]),
    (
        "serves_or_watches",
        &["dev", "develop", "start", "serve", "server", "watch", "preview", "storybook"],
    ),
];

/// `IMn`：各工具怎么认动作 kind。没列的工具是空配置（原文的 `{}`）。
fn tool(name: &str) -> Option<&'static Tool> {
    Some(match name {
        "apk" | "apt" | "apt-get" | "brew" | "dnf" | "snap" | "yum" => &TOOL_0,
        "astro" | "expo" | "gatsby" | "hugo" | "mkdocs" | "next" | "ng" | "nuxt" | "nx"
        | "react-scripts" | "remix" | "storybook" | "svelte-kit" | "turbo" | "vite"
        | "vue-cli-service" => &TOOL_1,
        "bazel" => &TOOL_2,
        "bun" | "npm" | "pnpm" => &TOOL_3,
        "bundle" => &TOOL_4,
        "bunx" => &TOOL_5,
        "cargo" => &TOOL_6,
        "composer" => &TOOL_7,
        "conda" | "hatch" | "mamba" | "micromamba" | "pdm" | "pipenv" | "pipx" | "poetry"
        | "uv" => &TOOL_8,
        "deno" => &TOOL_9,
        "dotnet" => &TOOL_10,
        "flask" => &TOOL_11,
        "gem" => &TOOL_12,
        "go" => &TOOL_13,
        "gradle" | "gradlew" | "mvn" | "mvnw" | "sbt" => &TOOL_14,
        "jupyter" => &TOOL_15,
        "make" => &TOOL_16,
        "node" => &TOOL_17,
        "npx" => &TOOL_18,
        "pip" => &TOOL_19,
        "playwright" => &TOOL_20,
        "swift" => &TOOL_21,
        "tsc" => &TOOL_22,
        "tsx" => &TOOL_23,
        "uvx" => &TOOL_24,
        "webpack" => &TOOL_25,
        "yarn" => &TOOL_26,
        _ => return None,
    })
}

/// apk, apt, apt-get, brew, dnf, snap, yum
const TOOL_0: Tool = Tool {
    verbs: &[(
        "installs_packages",
        &[
            "install",
            "reinstall",
            "add",
            "update",
            "upgrade",
            "dist-upgrade",
            "full-upgrade",
            "remove",
            "purge",
            "autoremove",
        ],
    )],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// astro, expo, gatsby, hugo, mkdocs, next, ng, nuxt, nx, react-scripts, remix, storybook, svelte-kit, turbo, vite, vue-cli-service
const TOOL_1: Tool = Tool {
    verbs: &[],
    anywhere: &[],
    bare: None,
    script_after: &["run"],
    command_after: &[],
    verb_may_be_script: true,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// bazel
const TOOL_2: Tool = Tool {
    verbs: &[("runs_tests", &["test", "coverage"]), ("builds", &["build"])],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// bun, npm, pnpm
const TOOL_3: Tool = Tool {
    verbs: &[
        ("runs_tests", &["t"]),
        (
            "installs_packages",
            &[
                "install",
                "i",
                "ci",
                "add",
                "update",
                "upgrade",
                "up",
                "remove",
                "rm",
                "uninstall",
                "link",
                "rebuild",
                "dedupe",
                "prune",
            ],
        ),
    ],
    anywhere: &[],
    bare: None,
    script_after: &["run", "run-script"],
    command_after: &["exec", "dlx", "x"],
    verb_may_be_script: true,
    value_flags: &[
        "--prefix",
        "-w",
        "--workspace",
        "workspace",
        "-C",
        "--cwd",
        "--dir",
        "--filter",
        "-F",
    ],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// bundle
const TOOL_4: Tool = Tool {
    verbs: &[("installs_packages", &["install", "update", "add"])],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &["exec"],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// bunx
const TOOL_5: Tool = Tool {
    verbs: &[],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: true,
    own_verbs: &[],
    verb_is_target: false,
};
/// cargo
const TOOL_6: Tool = Tool {
    verbs: &[
        ("runs_tests", &["test", "nextest", "bench"]),
        ("builds", &["build", "doc", "rustc"]),
        ("lints", &["check", "clippy", "fmt"]),
        ("installs_packages", &["install", "add", "update", "fetch"]),
        ("serves_or_watches", &["watch"]),
    ],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &["--manifest-path", "--config", "-Z"],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// composer
const TOOL_7: Tool = Tool {
    verbs: &[("installs_packages", &["install", "update", "require"])],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// conda, hatch, mamba, micromamba, pdm, pipenv, pipx, poetry, uv
const TOOL_8: Tool = Tool {
    verbs: &[
        ("builds", &["build"]),
        (
            "installs_packages",
            &[
                "install",
                "add",
                "update",
                "upgrade",
                "lock",
                "sync",
                "remove",
                "uninstall",
                "create",
                "env",
                "venv",
                "pip",
                "tool",
                "python",
                "inject",
            ],
        ),
    ],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &["run"],
    verb_may_be_script: false,
    value_flags: &[
        "-n",
        "--name",
        "-p",
        "--prefix",
        "--cwd",
        "--directory",
        "--project",
        "--python",
        "--with",
        "--with-requirements",
        "--extra",
        "--group",
        "--env-file",
    ],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// deno
const TOOL_9: Tool = Tool {
    verbs: &[
        ("runs_tests", &["test", "bench"]),
        ("builds", &["compile", "bundle"]),
        ("lints", &["lint", "fmt", "check"]),
        ("installs_packages", &["install", "add", "cache"]),
        ("serves_or_watches", &["serve"]),
    ],
    anywhere: &[],
    bare: None,
    script_after: &["task"],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// dotnet
const TOOL_10: Tool = Tool {
    verbs: &[
        ("runs_tests", &["test"]),
        ("builds", &["build", "publish", "pack"]),
        ("lints", &["format"]),
        ("installs_packages", &["restore", "add"]),
    ],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// flask
const TOOL_11: Tool = Tool {
    verbs: &[("serves_or_watches", &["run"])],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// gem
const TOOL_12: Tool = Tool {
    verbs: &[("installs_packages", &["install", "update"])],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// go
const TOOL_13: Tool = Tool {
    verbs: &[
        ("runs_tests", &["test"]),
        ("builds", &["build", "generate"]),
        ("lints", &["vet", "fmt"]),
        ("installs_packages", &["get", "install", "mod"]),
    ],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &["-C"],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// gradle, gradlew, mvn, mvnw, sbt
const TOOL_14: Tool = Tool {
    verbs: &[],
    anywhere: &[
        ("runs_tests", &["test", "verify", "check", "integration-test"]),
        ("builds", &["compile", "package", "install", "build", "assemble", "jar", "bootJar"]),
    ],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// jupyter
const TOOL_15: Tool = Tool {
    verbs: &[("serves_or_watches", &["notebook", "lab", "server"])],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// make
const TOOL_16: Tool = Tool {
    verbs: &[("runs_tests", &["check"]), ("builds", &["all", "install"])],
    anywhere: &[],
    bare: Some("builds"),
    script_after: &[],
    command_after: &[],
    verb_may_be_script: true,
    value_flags: &["-C", "-f", "-j"],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// node
const TOOL_17: Tool = Tool {
    verbs: &[],
    anywhere: &[("runs_tests", &["--test"])],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// npx
const TOOL_18: Tool = Tool {
    verbs: &[],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &["-p", "--package", "-c", "--call"],
    runs_the_rest: true,
    own_verbs: &[],
    verb_is_target: false,
};
/// pip
const TOOL_19: Tool = Tool {
    verbs: &[("installs_packages", &["install", "uninstall", "download", "wheel"])],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// playwright
const TOOL_20: Tool = Tool {
    verbs: &[("runs_tests", &["test"]), ("installs_packages", &["install", "install-deps"])],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// swift
const TOOL_21: Tool = Tool {
    verbs: &[("runs_tests", &["test"]), ("builds", &["build"])],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// tsc
const TOOL_22: Tool = Tool {
    verbs: &[],
    anywhere: &[("lints", &["--noEmit"]), ("serves_or_watches", &["-w"])],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// tsx
const TOOL_23: Tool = Tool {
    verbs: &[("serves_or_watches", &["watch"])],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// uvx
const TOOL_24: Tool = Tool {
    verbs: &[],
    anywhere: &[],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &["--from", "--with", "--python", "-p"],
    runs_the_rest: true,
    own_verbs: &[],
    verb_is_target: false,
};
/// webpack
const TOOL_25: Tool = Tool {
    verbs: &[("serves_or_watches", &["serve", "watch"])],
    anywhere: &[("serves_or_watches", &["-w"])],
    bare: None,
    script_after: &[],
    command_after: &[],
    verb_may_be_script: false,
    value_flags: &[],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};
/// yarn
const TOOL_26: Tool = Tool {
    verbs: &[
        ("runs_tests", &["t"]),
        (
            "installs_packages",
            &[
                "install",
                "i",
                "ci",
                "add",
                "update",
                "upgrade",
                "up",
                "remove",
                "rm",
                "uninstall",
                "link",
                "rebuild",
                "dedupe",
                "prune",
            ],
        ),
    ],
    anywhere: &[],
    bare: Some("installs_packages"),
    script_after: &["run", "run-script"],
    command_after: &["exec", "dlx", "x"],
    verb_may_be_script: true,
    value_flags: &[
        "--prefix",
        "-w",
        "--workspace",
        "workspace",
        "-C",
        "--cwd",
        "--dir",
        "--filter",
        "-F",
    ],
    runs_the_rest: false,
    own_verbs: &[],
    verb_is_target: false,
};

const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh"];
const FIND_EXEC: &[&str] = &["-exec", "-execdir", "-ok", "-okdir"];
const DETACHERS: &[&str] = &["nohup", "setsid"];
const INFO_FLAGS: &[&str] = &["--version", "--help", "-v", "-V", "-h", "-n", "-q"];
const WATCH_FLAGS: &[&str] = &["--watch", "--watchAll", "--reload"];
const PY_TOOL_MODULES: &[&str] = &["markitdown", "weasyprint", "ocrmypdf", "whisper"];

#[cfg(test)]
mod tests {
    use super::*;

    /// 对拍：逐条比官方 2.1.311 `yte` 的结果（node 跑官方原函数产出的 JSONL，每行
    /// `{"cmd", "on": yte(cmd), "off": 解析器三道防护关掉时的 kind}`）。路径由环境变量
    /// `LUBAN_BASH_KINDS_ORACLE` 给，没给或文件不在就跳过。顺带统计 [`super::super::tools::bash_profile`]
    /// 那几项与官方的差异（只打印，不判）。
    #[test]
    #[ignore = "依赖本机 node 产出的对拍语料，手动跑"]
    fn bash_command_kinds_matches_the_official_oracle() {
        use std::io::BufRead;
        let Ok(path) = std::env::var("LUBAN_BASH_KINDS_ORACLE") else {
            eprintln!("LUBAN_BASH_KINDS_ORACLE 没给，跳过");
            return;
        };
        let Ok(f) = std::fs::File::open(&path) else {
            eprintln!("{path} 不在，跳过");
            return;
        };
        let (mut total, mut bad) = (0usize, Vec::new());
        let mut profile_diff: std::collections::BTreeMap<&str, usize> = Default::default();
        for line in std::io::BufReader::new(f).lines() {
            let v: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            let cmd = v["cmd"].as_str().unwrap();
            let want = v["on"]["bash_command_kinds"].as_str().unwrap();
            total += 1;
            let got = bash_command_kinds(cmd);
            if got != want {
                bad.push(format!("{cmd:?}: want {want} got {got} (off {})", v["off"]));
            }
            let p = super::super::tools::bash_profile(cmd);
            let on = &v["on"];
            let mut cmp = |k: &'static str, ok: bool| {
                if !ok {
                    *profile_diff.entry(k).or_default() += 1;
                }
            };
            cmp("bash_command_class", on["bash_command_class"] == p.class);
            cmp("bash_argv0", on["bash_argv0"] == p.argv0.as_str());
            cmp("bash_last_argv0", on["bash_last_argv0"] == p.last_argv0.as_str());
            cmp("has_pipe", on["has_pipe"] == p.has_pipe);
            cmp("has_redirect", on["has_redirect"] == p.has_redirect);
            cmp("has_chain", on["has_chain"] == p.has_chain);
            cmp("has_subshell", on["has_subshell"] == p.has_subshell);
            cmp("has_heredoc", on["has_heredoc"] == p.has_heredoc);
            cmp("simple_command_count", on["simple_command_count"] == p.simple_commands);
        }
        eprintln!("bash_command_kinds：{total} 条，{} 条不一致", bad.len());
        for (k, n) in &profile_diff {
            eprintln!(
                "bash_profile {k}：{n}/{total} 与官方不同（{:.2}%）",
                *n as f64 * 100.0 / total as f64
            );
        }
        assert!(
            bad.is_empty(),
            "前几条：\n{}",
            bad.iter().take(30).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    /// 对拍 2.1.316：逐条比官方 `Cfe` 的 `bash_command_kinds`、`bash_programs`、`bash_test_scope`（node 跑官方原函数
    /// 产出的 JSONL，每行 `{"cmd", "on": Cfe(cmd)}`）。路径由环境变量 `LUBAN_BASH_KINDS_ORACLE_316` 给，没给或文件不在就跳过。
    #[test]
    #[ignore = "依赖本机 node 产出的对拍语料，手动跑"]
    fn shell_run_matches_the_official_2_1_316_oracle() {
        use std::io::BufRead;
        let Ok(path) = std::env::var("LUBAN_BASH_KINDS_ORACLE_316") else {
            eprintln!("LUBAN_BASH_KINDS_ORACLE_316 没给，跳过");
            return;
        };
        let Ok(f) = std::fs::File::open(&path) else {
            eprintln!("{path} 不在，跳过");
            return;
        };
        let (mut total, mut bad) = (0usize, Vec::new());
        for line in std::io::BufReader::new(f).lines() {
            let v: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            let cmd = v["cmd"].as_str().unwrap();
            let on = &v["on"];
            total += 1;
            let run = shell_run(cmd, true);
            let want = (
                on["bash_command_kinds"].as_str().unwrap(),
                on["bash_programs"].as_str().unwrap(),
                on["bash_test_scope"].as_str(),
            );
            if (run.kinds.as_str(), run.programs.as_str(), run.test_scope) != want {
                bad.push(format!(
                    "{cmd:?}: want {want:?} got {:?}",
                    (&run.kinds, &run.programs, run.test_scope)
                ));
            }
        }
        eprintln!("2.1.316：{total} 条，{} 条不一致", bad.len());
        assert!(
            bad.is_empty(),
            "前几条：\n{}",
            bad.iter().take(40).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    /// 对拍 2.1.316 `qt`：逐条比官方的测试结果判定（node 跑原函数产出的 JSONL，每行 `{"out", "want"}`）。路径由
    /// 环境变量 `LUBAN_BASH_QT_ORACLE` 给，没给或文件不在就跳过。
    #[test]
    #[ignore = "依赖本机 node 产出的对拍语料，手动跑"]
    fn test_outcome_matches_the_official_oracle() {
        use std::io::BufRead;
        let Ok(path) = std::env::var("LUBAN_BASH_QT_ORACLE") else { return };
        let Ok(f) = std::fs::File::open(&path) else { return };
        let (mut total, mut bad) = (0usize, Vec::new());
        for line in std::io::BufReader::new(f).lines() {
            let v: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            let (out, want) = (v["out"].as_str().unwrap(), v["want"].as_str().unwrap());
            total += 1;
            let got = test_outcome(out);
            if got != want {
                bad.push(format!(
                    "{:?}: want {want} got {got}",
                    &out[out.len().saturating_sub(300)..]
                ));
            }
        }
        eprintln!("qt：{total} 条，{} 条不一致", bad.len());
        assert!(bad.is_empty(), "{}", bad.iter().take(10).cloned().collect::<Vec<_>>().join("\n"));
    }

    /// 期望值都是 node 跑官方 2.1.311 `yte` 得出的。
    #[test]
    fn bash_command_kinds_matches_official_samples() {
        let cases: &[(&str, &str)] = &[
            ("ls -la", "file_search"),
            ("python3 -m pytest -q", "python,runs_tests"),
            ("npx vitest run", "node,runs_tests"),
            ("cargo test", "compiled_lang,runs_tests"),
            ("npm run test:unit", "node,runs_tests"),
            ("npm run dev:server", "node,serves_or_watches"),
            ("npm ci", "installs_packages,node"),
            ("npm exec -- eslint .", "lints,node"),
            ("pnpm --filter web build", "builds,node"),
            ("yarn", "installs_packages,node"),
            ("yarn --version", "node"),
            ("make", "builds,compiled_lang"),
            ("make check", "compiled_lang,runs_tests"),
            ("make testing", "compiled_lang"),
            ("tsc --noEmit -w", "lints,node"),
            ("node --test", "node,runs_tests"),
            ("uvicorn app:app --reload", "python,serves_or_watches"),
            ("jest --watchAll", "node,runs_tests,serves_or_watches"),
            ("sudo env A=1 timeout 5 nice -n 3 npm test", "node,runs_tests"),
            ("nohup node server.js &", "node,starts_background_job"),
            ("npm test & disown", "node,runs_tests,shell_word,starts_background_job"),
            ("command -v npm test", "shell_word"),
            ("timeout -k 5 10 cargo build", "builds,compiled_lang"),
            ("bash -c \"npm test\"", "node,runs_tests"),
            ("bash -euo pipefail -c 'make; make check'", "builds,compiled_lang,runs_tests"),
            ("bash script.sh", "shell_script"),
            ("bash -c \"\"", "shell_script"),
            ("eval", "shell_word"),
            ("find . -name '*.js' -exec grep -l foo {} \\;", "file_search"),
            ("python3 -c 'print(1)'", "python,python_inline"),
            ("python3 x.py", "python"),
            ("python3 -m http.server 8000", "python,serves_or_watches"),
            ("python3 -m json.tool", "python"),
            ("python3 -m markitdown x.pdf", "python,typeset"),
            ("python3 -V", "python"),
            ("npx @playwright/test test", "browser,node,runs_tests"),
            ("npx @foo/bar@1.2.3 run", "node,other"),
            ("npx vitest@latest", "node,runs_tests"),
            ("./x.ts", "node"),
            ("ruby x.rb", "other_runtime"),
            ("npx npx npx npx npx npx npx npx vitest", "incomplete"),
            ("git status && git diff", "vcs"),
            ("sleep 5 & wait", "starts_background_job,wait"),
            ("xargs -I{} rm {}", "file_change"),
            ("eval 'cargo test'", "compiled_lang,runs_tests"),
            ("python3 -u", "python,python_inline"),
            ("", "unparsed"),
            ("(", "shell_word"),
            ("bash -c 'bash -c \"bash -c npm\"'", "incomplete"),
            ("FOO=1 if true; then npm test; fi", "node,runs_tests,shell_word"),
        ];
        for (cmd, want) in cases {
            assert_eq!(bash_command_kinds(cmd), *want, "{cmd:?}");
        }
        // 内嵌脚本合计超过 2000 个码元就放弃。
        let long = format!("bash -c \"{}\"", "npm test; ".repeat(250));
        assert_eq!(bash_command_kinds(&long), "incomplete");
        // 超过 1 万个码元整条不解析。
        assert_eq!(bash_command_kinds(&"x".repeat(10_001)), "unparsed");
    }

    /// `left_process_running`：树里（含内嵌脚本）有 `&`、又没有 `wait` 才算留着进程；`&&`、`2>&1` 不算后台任务，
    /// 解析不了的按没留。
    #[test]
    fn background_jobs_left_running() {
        let cases: &[(&str, bool)] = &[
            ("npm run dev &", true),
            ("nohup python -m http.server 8000 > /dev/null 2>&1 &", true),
            ("(sleep 100 &) ; echo started", true),
            ("bash -c 'sleep 9 &'", true),
            ("sleep 5 & wait", false),
            ("cmd1 & cmd2 & wait $!", false),
            ("make && make test", false),
            ("cargo build 2>&1 | tail -3", false),
            ("ls -la", false),
            ("", false),
        ];
        for (cmd, want) in cases {
            assert_eq!(shell_run(cmd, false).leaves_running, *want, "{cmd:?}");
        }
    }
}
