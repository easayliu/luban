//! 2.1.315 起 Bash 执行 / 失败事件末尾那组「重复命令」统计（官方 `d_n` 计量器与它拆语句的 `FG`），逐函数移植。
//!
//! 官方每次 Bash 调用开始时把命令拆成一条条语句（[`pieces`]：`cd` 换目录、只读或 `echo` 一类的跳过，剩下的记
//! 「在哪个目录跑了什么」），拿去和这个会话里先前跑完的语句比：同目录同词干（连同 Bash 其余入参与子代理）算重复，
//! 报这次比了几条（`compared_statements`）、几条重复的期间什么都没改（`unchanged_repeats`），挑一条改动最少的那次报
//! 它距今多久、期间改了多少行、可能写过几次、跑过几次测试 / 检查、它的结果还在不在上下文里。调用结束再把这次的语句
//! 记进去（[`BashMeter::settle`]）。
//!
//! 期间「改了多少行」是成本账上的增删行数（`Edit` / `Write` 的 `tengu_file_changed` 那两项）；别的非只读工具（`Read`、
//! `Grep` 这类只读的、`TodoWrite` 一类记事的不算）每调一次记一次可能写过，`NotebookEdit` 记一行改动。
//!
//! 与原文不同处：消息历史代理看不全（线程续轮只有增量），「之后有没有新消息」「结果还在不在上下文里」改按会话状态判——
//! 先前那次之后用户有没有新输入、期间有没有压缩过。

use std::time::SystemTime;

use serde_json::{Map, Value, json};

use super::bash_ast;

/// `wG`：一条命令拆出超过这么多条语句就不拆，整条当一条。
const MAX_STATEMENTS: usize = 20;
/// `_G`：记着的语句最多这么多条，再多从最早的忘起。
const MAX_ENTRIES: usize = 500;
/// `bG`：同一条语句最多记最近这么多种原文。
const MAX_TEXTS: usize = 8;
/// `EG` / `kG` / `ME`：秒数、行数、次数报的上限。
const MAX_SECONDS: i64 = 86_400;
const MAX_LINES: u64 = 100_000;
const MAX_COUNT: u64 = 1_000;
/// `QA`：超过这么多 UTF-16 码元的命令不解析。
const MAX_PARSE_LEN: usize = 10_000;
/// `cj`（2.1.317）：同一项活最近由哪几个代理跑完，最多记这么多个。
const MAX_AGENTS: usize = 4;
/// `dj`（2.1.317）：不分代理的那把「活」键里代理那一段，官方拿 `"\0"` 占位。
const ANY_AGENT: &str = "\0";

/// `NG`：语句之间的连接符。
const JOINERS: &[&str] = &["&&", "||", ";", "&"];
/// `CG`：输出一下、不干活的命令。
const LIGHT: &[&str] = &["echo", "true", "false"];
/// `AG`：改 shell 自己状态的内建命令。
const SHELL_STATE: &[&str] = &[
    "cd", "pushd", "popd", "source", ".", "set", "shopt", "ulimit", "umask", "alias", "unalias",
    "exit", "return",
];
/// `PG`：声明 / 赋值类语句。
const DECLARATIONS: &[&str] =
    &["declaration_command", "unset_command", "variable_assignment", "variable_assignments"];
/// `DG`：写文件的重定向符。
const WRITE_REDIRECTS: &[&str] = &[">", ">>", ">&", "&>", "&>>", ">|"];
/// `vG`：管道尾巴上只筛输出的命令（剥掉它们才是词干）。
const FILTERS: &[&str] = &[
    "tail", "head", "grep", "egrep", "fgrep", "rg", "sed", "awk", "gawk", "wc", "cut", "sort",
    "uniq", "tr", "column", "nl", "jq", "tee", "less", "more", "cat",
];

/// `TG` 与 `xG`：不算「别的工具调用」的那几个（`Edit` / `Write` 的改动走成本账；后几个只是记事或换模式）。
const NOT_OTHER_TOOLS: &[&str] = &[
    "Bash",
    "Edit",
    "Write",
    "TodoWrite",
    "TaskCreate",
    "TaskUpdate",
    "TaskStop",
    "ExitPlanMode",
    "Skill",
];

/// 官方各工具 `isReadOnly()` 为真的那些（只读的调用不记可能写过）。MCP 工具看服务端的 `readOnlyHint`，代理看不到，
/// 按不只读算。
const READ_ONLY_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "WebFetch",
    "WebSearch",
    "ToolSearch",
    "AskUserQuestion",
    "Agent",
    "Task",
    "TaskOutput",
    "TaskGet",
    "TaskList",
    "ListMcpResourcesTool",
    "ReadMcpResourceTool",
    "LSP",
    "EnterPlanMode",
];

/// `FG` 拆出的一条语句。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Piece {
    /// 跑在哪：目录与它前面改 shell 状态的那些语句，`\0` 连起来。
    pub(super) place: String,
    /// 词干：去掉写文件重定向与管道尾巴上的筛选后的命令。
    pub(super) stem: String,
    /// 语句原文（连同它的重定向）。
    pub(super) text: String,
    /// 构建、装包、跑测试或检查（`builds` / `installs_packages` / `runs_tests` / `lints`）。
    pub(super) heavy: bool,
    /// 跑测试或检查、且不是带 `--fix` 一类会改文件的。
    pub(super) tests_or_lints: bool,
    /// 接在 `||` 后面，只在前面失败时才跑。
    pub(super) only_on_failure: bool,
    /// 接在 `&&` 后面时它跑没跑的条件。
    pub(super) ran_only_if: Option<&'static str>,
    /// `&` 放到后台、且后面没有 `wait` 等它。
    pub(super) goes_on_running: bool,
}

/// `FG(e, n, r)`：把命令拆成语句。`cwd` 是会话目录，`read_only` 判一条语句是不是只读（官方 Bash 工具的
/// `isReadOnly`）。解析不了、拆不出或超过 [`MAX_STATEMENTS`] 条的整条算一条。
pub(super) fn pieces(command: &str, cwd: &str, read_only: impl Fn(&str) -> bool) -> Vec<Piece> {
    let whole = || {
        let t = js_trim(command).to_string();
        vec![Piece {
            place: cwd.to_string(),
            stem: t.clone(),
            text: t,
            heavy: false,
            tests_or_lints: false,
            only_on_failure: false,
            ran_only_if: None,
            goes_on_running: false,
        }]
    };
    let tree = (command.encode_utf16().count() <= MAX_PARSE_LEN)
        .then(|| bash_ast::parse(command))
        .flatten();
    let Some(tree) = tree.filter(|t| t.kind(t.root) == "program") else { return whole() };
    let t = &*tree;
    let text = |id: bash_ast::Id| String::from_utf16_lossy(t.text(id));
    struct Stmt {
        node: bash_ast::Id,
        redirects: Vec<bash_ast::Id>,
        then: String,
    }
    let mut h: Vec<Stmt> = Vec::new();
    for d in flatten(t, t.root) {
        let k = t.kind(d);
        if JOINERS.contains(&k) {
            if let Some(last) = h.last_mut() {
                last.then = k.to_string();
            }
        } else if k.ends_with("_redirect") {
            if let Some(last) = h.last_mut() {
                last.redirects.push(d);
            }
        } else {
            h.push(Stmt { node: d, redirects: Vec::new(), then: ";".into() });
        }
    }
    if h.is_empty() || h.len() > MAX_STATEMENTS {
        return whole();
    }
    let mut y = cwd.to_string();
    let mut b = false;
    let mut w: Vec<String> = Vec::new();
    // 最后一段 `&&` 链从哪条起（`R`）、最后一条前面是 `;` / `&` 的 `wait` 在哪（`x`）。
    let r_start = h[..h.len() - 1].iter().rposition(|s| s.then != "&&").map_or(0, |i| i + 1);
    let wait_at = h.iter().enumerate().rev().find_map(|(q, s)| {
        let prev = q.checked_sub(1).map_or("", |p| h[p].then.as_str());
        (text(s.node) == "wait" && s.then != "&" && (prev == ";" || prev == "&")).then_some(q)
    });
    let (mut p_heavy, mut j_light) = (false, false);
    let mut out = Vec::new();
    for (d, st) in h.iter().enumerate() {
        let (y_node, q, z) = (st.node, &st.redirects, st.then.as_str());
        let te = std::iter::once(y_node)
            .chain(q.iter().copied())
            .map(text)
            .collect::<Vec<_>>()
            .join(" ");
        let ae = d.checked_sub(1).map(|p| h[p].then.as_str());
        if ae != Some("&&") {
            p_heavy = false;
            j_light = false;
        }
        let parts: Vec<bash_ast::Id> = if t.kind(y_node) == "redirected_statement" {
            t.children(y_node).to_vec()
        } else {
            vec![y_node]
        };
        let se = parts.first().copied();
        let he = parts.iter().skip(1).chain(q.iter()).any(|&x| writes_file(t, x));
        let mut be = false;
        if let Some(se) = se
            && (DECLARATIONS.contains(&t.kind(se))
                || SHELL_STATE.contains(&command_name(t, se).as_str()))
        {
            let ch = t.children(se);
            let re_ = ch.first().copied();
            let fe = ch.get(1).copied();
            let rest = ch.len().saturating_sub(2);
            let quoted = fe.is_some_and(|f| {
                t.kind(f) == "raw_string"
                    || (t.kind(f) == "string"
                        && t.children(f)
                            .iter()
                            .all(|&g| matches!(t.kind(g), "\"" | "string_content")))
            });
            let te_arg = fe.map(|f| {
                let s = text(f);
                if quoted {
                    s.chars().skip(1).take(s.chars().count().saturating_sub(2)).collect()
                } else {
                    s
                }
            });
            let head = re_.map(text).unwrap_or_default();
            let ze = matches!(head.as_str(), "cd" | "pushd" | "popd");
            if head == "cd"
                && !b
                && !he
                && z != "||"
                && z != "&"
                && ae != Some("||")
                && rest == 0
                && (quoted || fe.is_some_and(|f| t.kind(f) == "word"))
                && let Some(dir) = te_arg.as_deref()
                && plain_path(dir)
            {
                y = resolve(&y, dir);
                continue;
            }
            if z != "&" {
                w.push(te.clone());
                b |= ze;
                be = true;
            }
            if z != "&"
                && !he
                && !substitutes(&te)
                && ((head != "source" && head != ".") || activates(te_arg.as_deref().unwrap_or("")))
            {
                continue;
            }
        }
        let kinds = super::bash_kinds::shell_run(&text(y_node), true).kinds;
        let ce: Vec<&str> = kinds.split(',').collect();
        let we = ce.contains(&"builds") || ce.contains(&"installs_packages");
        let ie = !we && (ce.contains(&"runs_tests") || ce.contains(&"lints"));
        let ee = we || ie;
        let ytext = text(y_node);
        if !ee
            && !he
            && ((LIGHT.contains(&command_name(t, y_node).as_str()) && !substitutes(&ytext))
                || read_only(&ytext))
        {
            if mentions_cd(&ytext) {
                w.push(te.clone());
                b = true;
            }
            j_light = true;
            continue;
        }
        let kept = keep_redirects(t, y_node, !ee);
        let (oe, ke) = match kept.split_first() {
            Some((o, k)) => (Some(*o), k.to_vec()),
            None => (None, Vec::new()),
        };
        let ie_nodes: Vec<bash_ast::Id> = match oe {
            Some(o) if t.kind(o) == "pipeline" => t.children(o).to_vec(),
            Some(o) => vec![o],
            None => Vec::new(),
        };
        let mut we_n = ie_nodes.len();
        while we_n >= 3 {
            let re_ = ie_nodes[we_n - 1];
            let fe = command_name(t, re_);
            if !FILTERS.contains(&fe.as_str()) {
                break;
            }
            let writes_back = simple_command(t, re_)
                .is_some_and(|c| t.children(c).iter().any(|&lt| filter_writes(&fe, &text(lt))));
            if writes_back {
                break;
            }
            we_n -= 2;
        }
        if !ee && we_n == 1 && ie_nodes.len() > 1 && read_only(&text(ie_nodes[0])) {
            we_n = ie_nodes.len();
        }
        let mut stem_nodes: Vec<bash_ast::Id> = Vec::new();
        if we_n >= 1 {
            stem_nodes.extend_from_slice(&ie_nodes[..we_n - 1]);
            stem_nodes.extend(keep_redirects(t, ie_nodes[we_n - 1], !ee));
        }
        stem_nodes.extend(ke);
        stem_nodes.extend(
            q.iter().copied().filter(|&x| !is_write_redirect(t, x) || (!ee && writes_file(t, x))),
        );
        let mut words: Vec<String> = stem_nodes.into_iter().map(text).collect();
        if z == "&" {
            words.push("&".into());
        }
        let nt = words.join(" ");
        let be_words: Vec<&str> = split_js_ws(&nt);
        out.push(Piece {
            place: std::iter::once(y.clone())
                .chain(w.iter().cloned())
                .collect::<Vec<_>>()
                .join("\0"),
            stem: nt.clone(),
            text: te.clone(),
            heavy: ee,
            tests_or_lints: ie
                && (!be_words.iter().any(|x| writes_fix(x))
                    || be_words.iter().any(|x| check_only(x))),
            only_on_failure: ae == Some("||"),
            ran_only_if: if p_heavy {
                Some(if d >= r_start { "the line exited 0" } else { "unknown" })
            } else if j_light {
                Some("light work succeeded")
            } else {
                None
            },
            goes_on_running: z == "&" && wait_at.is_none_or(|x| d > x),
        });
        p_heavy |= ee;
        j_light = true;
        if !be && t.kind(y_node) != "subshell" && mentions_cd(&ytext) {
            w.push(te);
            b = true;
        }
    }
    out
}

/// `_p`：把 `program` / `list`（含套在重定向里的 `list`）摊平成语句与连接符，注释丢掉。
fn flatten(t: &bash_ast::Tree, id: bash_ast::Id) -> Vec<bash_ast::Id> {
    match t.kind(id) {
        "program" | "list" => t.children(id).iter().flat_map(|&c| flatten(t, c)).collect(),
        "comment" => Vec::new(),
        "redirected_statement" if t.children(id).first().is_some_and(|&c| t.kind(c) == "list") => {
            t.children(id).iter().flat_map(|&c| flatten(t, c)).collect()
        }
        _ => vec![id],
    }
}

/// `wp`：写文件的重定向。
fn is_write_redirect(t: &bash_ast::Tree, id: bash_ast::Id) -> bool {
    t.kind(id) == "file_redirect"
        && t.children(id).iter().any(|&c| WRITE_REDIRECTS.contains(&t.kind(c)))
}

/// `bp`：真写到一个文件里（不是 `/dev/null`，也不是 `>&2` 一类转给别的描述符）。
fn writes_file(t: &bash_ast::Tree, id: bash_ast::Id) -> bool {
    let last =
        t.children(id).last().map(|&c| String::from_utf16_lossy(t.text(c))).unwrap_or_default();
    let dup = t.children(id).iter().any(|&c| t.kind(c) == ">&")
        && (last == "-" || (!last.is_empty() && last.bytes().all(|c| c.is_ascii_digit())));
    is_write_redirect(t, id) && last != "/dev/null" && !dup
}

/// `HE`：重定向语句去掉写文件的重定向（`keep_writes` 时真写文件的留着）。
fn keep_redirects(t: &bash_ast::Tree, id: bash_ast::Id, keep_writes: bool) -> Vec<bash_ast::Id> {
    if t.kind(id) != "redirected_statement" {
        return vec![id];
    }
    t.children(id)
        .iter()
        .copied()
        .filter(|&r| !is_write_redirect(t, r) || (keep_writes && writes_file(t, r)))
        .collect()
}

/// `$E`：语句里的那条简单命令（套在重定向里的取第一个子节点）。
fn simple_command(t: &bash_ast::Tree, id: bash_ast::Id) -> Option<bash_ast::Id> {
    let n = if t.kind(id) == "redirected_statement" { *t.children(id).first()? } else { id };
    (t.kind(n) == "command").then_some(n)
}

/// `Sp`：简单命令的命令名，取最后一个 `/` 之后。
fn command_name(t: &bash_ast::Tree, id: bash_ast::Id) -> String {
    let Some(c) = simple_command(t, id) else { return String::new() };
    let Some(&n) = t.children(c).iter().find(|&&x| t.kind(x) == "command_name") else {
        return String::new();
    };
    let s = String::from_utf16_lossy(t.text(n));
    s.rsplit('/').next().unwrap_or("").to_string()
}

/// `NE`：管道尾巴上这个筛选命令带了会写文件的参数（`sed -i`、`sort -o`、`awk` 里 `system` / 重定向……）。
fn filter_writes(cmd: &str, word: &str) -> bool {
    match cmd {
        "sed" => short_with_prefix(word, 'i') || word.starts_with("--in-place"),
        "sort" => short_with_prefix(word, 'o') || word.starts_with("--output"),
        // `DE`：`/^-[if]|system|getline|[>|]/`。
        "awk" | "gawk" => {
            word.starts_with("-i")
                || word.starts_with("-f")
                || word.contains("system")
                || word.contains("getline")
                || word.contains('>')
                || word.contains('|')
        }
        _ => false,
    }
}

/// `/^-[A-Za-z]*c/`：一串短开关里含 `c`（如 `-ni`）。
fn short_with_prefix(word: &str, c: char) -> bool {
    let Some(r) = word.strip_prefix('-') else { return false };
    for ch in r.chars() {
        if ch == c {
            return true;
        }
        if !ch.is_ascii_alphabetic() {
            return false;
        }
    }
    false
}

/// `FE`：有命令替换或进程替换（`` ` ``、`$(`、`<(`、`>(`）。
fn substitutes(s: &str) -> bool {
    s.contains('`') || s.contains("$(") || s.contains("<(") || s.contains(">(")
}

/// `LE`：`/\b(cd|pushd|popd)\b/`。
fn mentions_cd(s: &str) -> bool {
    let b = s.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    ["cd", "pushd", "popd"].iter().any(|w| {
        s.match_indices(w).any(|(i, _)| {
            (i == 0 || !word(b[i - 1])) && b.get(i + w.len()).is_none_or(|&c| !word(c))
        })
    })
}

/// `OG`：`/(^|\/)activate(\.\w+)?$/`（`source .venv/bin/activate`）。
fn activates(s: &str) -> bool {
    let base = s.rsplit('/').next().unwrap_or(s);
    let Some(rest) = base.strip_prefix("activate") else { return false };
    rest.is_empty()
        || rest.strip_prefix('.').is_some_and(|x| {
            !x.is_empty() && x.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
}

/// `LG`：`/^[\w@%+=:,./][\w@%+=:,./ -]*$/`，`cd` 后面能照字面拼出的路径。
fn plain_path(s: &str) -> bool {
    let ok = |c: u8, first: bool| {
        c.is_ascii_alphanumeric()
            || b"_@%+=:,./".contains(&c)
            || (!first && (c == b' ' || c == b'-'))
    };
    let b = s.as_bytes();
    !b.is_empty() && ok(b[0], true) && b[1..].iter().all(|&c| ok(c, false))
}

/// `path.resolve(base, p)`（POSIX）：拼上、规整 `.` 与 `..`。
fn resolve(base: &str, p: &str) -> String {
    let joined = if p.starts_with('/') { p.to_string() } else { format!("{base}/{p}") };
    let mut out: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// `IG`：会改文件的检查开关（`--fix`、`--write`、`-u`、`UPDATE_SNAPSHOTS=1`、`fmt`……）。
fn writes_fix(w: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"^(--fix.*|--write|-w|-i|--in-place|--apply.*|--unsafe-fixes|-a|-A|--autocorrect.*|-F|-u|-update|",
            r"--update.*|--snapshot-update|--force-regen|--bless|--accept|",
            r"[A-Z0-9_]*(UPDATE|ACCEPT|BLESS|REGEN|OVERWRITE|RECORD)[A-Z0-9_]*=.*|",
            r"(.*[-_:/])?(fix|fmt|format|write)([-_:].*)?|(.*/)?(black|isort|rustfmt|autopep8|yapf|pre-commit))$"
        ))
        .expect("IG")
    });
    RE.is_match(w)
}

/// `MG`：`/^(--check.*|--diff|--dry-run)$/`。
fn check_only(w: &str) -> bool {
    w.starts_with("--check") || w == "--diff" || w == "--dry-run"
}

/// JS `split(/\s+/)` 拆出的词（只拿来逐个测 [`writes_fix`] / [`check_only`]，空串两者都不中，略去）。
fn split_js_ws(s: &str) -> Vec<&str> {
    s.split(js_space).filter(|w| !w.is_empty()).collect()
}

fn js_space(c: char) -> bool {
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
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

/// `Bun.hash(s)`：Zig 标准库的 Wyhash（种子 0），对字符串的 UTF-8 字节算。`Bun.hash("")` 是 `0x0409638ee2bde459`，与
/// Zig 的测试向量一致；其余值与 Bun 1.3.11 实算逐个核过（见测试）。
pub(super) fn bun_hash(input: &[u8]) -> u64 {
    const SECRET: [u64; 4] = [
        0xa076_1d64_78bd_642f,
        0xe703_7ed1_a0b4_28db,
        0x8ebc_6af0_9c88_c6e3,
        0x5899_65cc_7537_4cc3,
    ];
    fn mum(a: u64, b: u64) -> (u64, u64) {
        let x = u128::from(a).wrapping_mul(u128::from(b));
        (x as u64, (x >> 64) as u64)
    }
    fn mix(a: u64, b: u64) -> u64 {
        let (a, b) = mum(a, b);
        a ^ b
    }
    fn read(n: usize, b: &[u8]) -> u64 {
        let mut buf = [0u8; 8];
        buf[..n].copy_from_slice(&b[..n]);
        u64::from_le_bytes(buf)
    }
    let seed = 0u64;
    let mut state = [seed ^ mix(seed ^ SECRET[0], SECRET[1]); 3];
    let (a, b);
    let len = input.len();
    if len <= 16 {
        if len >= 4 {
            let end = len - 4;
            let quarter = (len >> 3) << 2;
            a = (read(4, input) << 32) | read(4, &input[quarter..]);
            b = (read(4, &input[end..]) << 32) | read(4, &input[end - quarter..]);
        } else if len > 0 {
            a = (u64::from(input[0]) << 16)
                | (u64::from(input[len >> 1]) << 8)
                | u64::from(input[len - 1]);
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut i = 0;
        if len >= 48 {
            while i + 48 < len {
                for k in 0..3 {
                    let x = read(8, &input[i + 16 * k..]);
                    let y = read(8, &input[i + 16 * k + 8..]);
                    state[k] = mix(x ^ SECRET[k + 1], y ^ state[k]);
                }
                i += 48;
            }
            state[0] ^= state[1] ^ state[2];
        }
        let rest = &input[i..];
        let mut j = 0;
        while j + 16 < rest.len() {
            state[0] = mix(read(8, &rest[j..]) ^ SECRET[1], read(8, &rest[j + 8..]) ^ state[0]);
            j += 16;
        }
        a = read(8, &input[len - 16..]);
        b = read(8, &input[len - 8..]);
    }
    let (a, b) = mum(a ^ SECRET[1], b ^ state[0]);
    mix(a ^ SECRET[0] ^ len as u64, b ^ SECRET[1])
}

/// `JYo(sessionId)`（2.1.317）：这个会话的 Bash 调用记不记重复命令统计——`Bun.hash("bash repeat meter\0" + 会话 id)`
/// 取低 32 位除以 2³² 小于 `rate`（`tengu_vivid_peach`，读不到 0.1）。没抽中的会话每次都走 `skip()`：不比、不记，
/// 事件里那组键一个都没有。官方另在不发遥测（`DISABLE_TELEMETRY` 等）时一律不抽，代理这边恒发。
pub(super) fn metered(session_id: &str, rate: f64) -> bool {
    let h = bun_hash(format!("bash repeat meter\0{session_id}").as_bytes());
    ((h & 0xffff_ffff) as f64) / 4_294_967_296.0 < rate
}

/// 一次调用结束时 `ended()` 交来的那几项（2.1.317）：这条命令算不算「输出交给 `tail`」的候选（`check` / `other`，
/// 见 `super::tools::tail_input_line`），输出有没有另存一份（转到后台的不记，是 `None`），以及输出有没有落盘
/// （`longOutput.persisted`：代理看到的那次结果是 `<persisted-output>` 预览）。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Ended {
    pub(super) line: Option<&'static str>,
    pub(super) kept: Option<bool>,
    pub(super) persisted: bool,
}

/// 支线上下文里一条 `tool_result` 现在的正文状态（[`super::shape::ResultSeen`]）。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Readable {
    /// 官方 `VJo(content)`：没落盘的那次看这个。
    pub(super) plain: bool,
    /// `content !== "[Old tool result content cleared]"`：落过盘的那次看这个。
    pub(super) not_cleared: bool,
}

/// 先前跑完的一条语句（官方 `#e` 里的一项）。
#[derive(Clone, Debug)]
struct Entry {
    text_hashes: Vec<String>,
    returned_at: SystemTime,
    lines_changed: u64,
    possible_writes: u64,
    tests_and_lints: u64,
    unseen_writes: u64,
    background_at_start: u64,
    outcome: &'static str,
    /// 记下时用户已经输入到第几次（判之后有没有新消息）。
    prompt_seq: u32,
    /// 记下时的上下文代数（之后压缩过就不在上下文里了）。
    epoch: u32,
    /// 那次调用的 [`Ended`]（2.1.317 报 `earlier_tail_input_*`）。
    tail: Ended,
    /// 那次调用的 tool_use id（2.1.317 判别的代理那次的结果在不在当前支线的上下文里）。
    tool_use_id: Option<String>,
}

/// 这个会话的计量器（官方 `d_n`，挂在 `toolState` 上，`/clear` 换会话即清空）。
#[derive(Clone, Default, Debug)]
pub(crate) struct BashMeter {
    entries: Vec<(String, Entry)>,
    /// `#t` 改动行数、`#i` 可能写过、`#l` 测试与检查、`#o` 没看到 diff 的写。
    lines: u64,
    writes: u64,
    checks: u64,
    unseen: u64,
    /// `#a` 正在跑的会写的调用（含没有 tool_use id 的）、`#c` 放到后台还在跑的。
    running: u64,
    background: u64,
    /// 成本账上的增删行累计（`oye() + sye()`）与上次开始时读到的值（`#n`）。
    ledger: u64,
    ledger_seen: u64,
    /// `#n`（2.1.317）：不分代理的「活」键 → 正在跑它的代理（开始时记、结束时摘）。
    running_by: Vec<(String, Vec<String>)>,
    /// `#t`（2.1.317）：不分代理的「活」键 → 最近跑完它的代理（至多 [`MAX_AGENTS`] 个，越靠后越新）。
    done_by: Vec<(String, Vec<String>)>,
    /// 各代理支线（主线程是空串）上下文里的 `tool_result`：tool_use id → 正文状态。完整请求体整份换掉，线程续轮的增量体
    /// 追加，**那条支线**压缩后清空（[`Self::note_visible`]、[`Self::compacted`]）。
    visible: Vec<(String, std::collections::HashMap<String, Readable>)>,
    /// 各代理支线压缩过几次（没有的是 0）。只有被压缩的那条支线变，别的子代理照旧。
    epochs: Vec<(String, u32)>,
}

/// 一次调用开始时算好的东西，结束时交回 [`BashMeter::settle`]（官方 `ic`）。
#[derive(Debug)]
pub(super) struct Begun {
    /// 事件里那几项（`repeat` 起）。
    pub(super) fields: Map<String, Value>,
    call: Option<Call>,
    counts_at_start: [u64; 4],
    counted_as_running: bool,
}

#[derive(Debug)]
struct Call {
    pieces: Vec<PieceRec>,
    tool_use_id: Option<String>,
    /// 调用方代理的 id（主线程是空串）。
    agent: String,
    writes: u64,
    checks: u64,
    goes_on_running: bool,
    background_at_start: u64,
}

/// 一条参与比较的语句（官方 `Z` 里的一项）。
#[derive(Debug)]
struct PieceRec {
    key: String,
    /// 不分代理的那把键（2.1.317 `workKey`）。
    work_key: String,
    text_hash: String,
    writes: u64,
    checks: u64,
    ran_only_if: Option<&'static str>,
}

/// `#f` 算出来的一次比较。
struct Diff {
    idx: usize,
    same_text: bool,
    lines: u64,
    writes: u64,
    /// 2.1.317：期间还在跑的会写的调用（`R`），`writes` 里已含。
    running: u64,
    checks: u64,
    unseen: u64,
}

impl Diff {
    /// `pE(e, n)`：比 `other` 改动少（行数 + 写），一样多时检查少。
    fn better_than(&self, other: Option<&Diff>) -> bool {
        other.is_none_or(|b| {
            self.lines + self.writes < b.lines + b.writes
                || (self.lines + self.writes == b.lines + b.writes && self.checks < b.checks)
        })
    }

    /// `gE`（2.1.317）：期间改了什么，按轻重取一样。
    fn changed(&self) -> &'static str {
        if self.lines > 0 {
            "lines_changed"
        } else if self.writes > self.running {
            "possible_writes"
        } else if self.running > 0 {
            "running_commands_only"
        } else if self.checks > 0 {
            "tests_and_lints_only"
        } else {
            "nothing"
        }
    }
}

/// [`BashMeter::begin`] 的入参。
pub(super) struct BeginArgs<'a> {
    pub(super) command: &'a str,
    /// Bash 入参里除 `command` / `description` / `timeout` / `run_in_background` 外的那些（`dangerouslyDisableSandbox`
    /// 已归成布尔）。
    pub(super) rest_input: &'a Value,
    pub(super) cwd: &'a str,
    pub(super) agent_id: &'a str,
    pub(super) tool_use_id: Option<&'a str>,
    pub(super) now: SystemTime,
    pub(super) prompt_seq: u32,
    /// 会话版本 ≥2.1.317：按会话抽样（[`metered`]），事件里那组键换成 2.1.317 的布局（多跨代理那组等）。
    pub(super) v317: bool,
    /// 这个会话抽中了（[`metered`]），只在 `v317` 时看。
    pub(super) metered: bool,
    /// 调用方子代理跑在自己的 worktree 里（Agent 的 `isolation: "worktree"`）；主线程不报。
    pub(super) in_agent_worktree: bool,
}

/// 有序表里取 `key` 那一项。
fn slot<'a>(map: &'a [(String, Vec<String>)], key: &str) -> Option<&'a Vec<String>> {
    map.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

impl BashMeter {
    /// 成本账记一笔增删行（`Edit` / `Write` 的 `tengu_file_changed`）。
    pub(super) fn note_lines(&mut self, added: usize, removed: usize) {
        self.ledger += (added + removed) as u64;
    }

    /// `agent`（主线程是空串）那条支线压缩成功：它先前的结果都不在它的上下文里了；别的支线（各自独立的子代理）不受影响。
    pub(super) fn compacted(&mut self, agent: &str) {
        match self.epochs.iter_mut().find(|(a, _)| a == agent) {
            Some((_, n)) => *n += 1,
            None => self.epochs.push((agent.to_string(), 1)),
        }
        self.visible.retain(|(a, _)| a != agent);
    }

    /// `agent` 那条支线压缩过几次。
    fn epoch_of(&self, agent: &str) -> u32 {
        self.epochs.iter().find(|(a, _)| a == agent).map_or(0, |(_, n)| *n)
    }

    /// 记下 `agent`（主线程是空串）这条支线这次请求看得见的 `tool_result`：完整体整份换掉，增量体追加。每条主线程 /
    /// 子代理请求都调（[`super::Session`] 推进一轮时），`ids` 已按官方正文有效性滤过
    /// （[`super::shape::RequestShape::result_ids`]）。
    pub(super) fn note_visible(
        &mut self,
        agent: &str,
        seen: &[super::shape::ResultSeen],
        delta: bool,
    ) {
        let at = match self.visible.iter().position(|(a, _)| a == agent) {
            Some(i) => i,
            None => {
                self.visible.push((agent.to_string(), Default::default()));
                self.visible.len() - 1
            }
        };
        let set = &mut self.visible[at].1;
        if !delta {
            set.clear();
        }
        set.extend(seen.iter().map(|r| (r.id.clone(), r.readable)));
    }

    /// `mE(messages, earlier) >= 0`（跨代理那项）：别的代理那次的结果在 `agent` 这条支线看得见的结果里。独立的子代理与
    /// 主线程互相看不到对方的 Bash 结果，只有带着父线程上下文分出来的才看得到。
    ///
    /// 那次落没落盘按调用结束时记下的（[`Ended::persisted`]），不看现在的正文：原本没落盘的旧输出后来被换成
    /// `<persisted-output>` 预览时，官方走 `VJo`、判不在。
    fn visible_to(&self, agent: &str, e: &Entry) -> bool {
        let Some(id) = &e.tool_use_id else { return false };
        let Some(r) =
            self.visible.iter().find(|(a, _)| a == agent).and_then(|(_, set)| set.get(id))
        else {
            return false;
        };
        if e.tail.persisted { r.not_cleared } else { r.plain }
    }

    /// `noteOtherToolCall`：别的工具调用一次（只读的不算）。
    pub(super) fn note_other_tool(&mut self, name: &str) {
        if NOT_OTHER_TOOLS.contains(&name) || READ_ONLY_TOOLS.contains(&name) {
            return;
        }
        if name == "NotebookEdit" {
            self.lines += 1;
        } else {
            self.writes += 1;
            self.unseen += 1;
        }
    }

    fn counts(&self) -> [u64; 4] {
        [self.lines, self.writes, self.checks, self.unseen]
    }

    /// `begin`：一次 Bash 调用开始。
    pub(super) fn begin(&mut self, a: BeginArgs<'_>, read_only: impl Fn(&str) -> bool) -> Begun {
        let n = self.ledger;
        self.lines += if n < self.ledger_seen { n } else { n - self.ledger_seen };
        self.ledger_seen = n;
        // 没有 tool_use id（用户 `!` 跑的），或 2.1.317 起这个会话没抽中：`skip()`，只记一次在跑。
        if a.tool_use_id.is_none() || (a.v317 && !a.metered) {
            self.running += 1;
            return Begun {
                fields: Map::new(),
                call: None,
                counts_at_start: self.counts(),
                counted_as_running: true,
            };
        };
        let x = pieces(a.command, a.cwd, &read_only);
        let any_heavy = x.iter().any(|p| p.heavy && !p.only_on_failure);
        let key_of = |agent: &str, ie: &Piece| {
            format!("{agent}\0{}\0{}\0{}", a.rest_input, ie.place, ie.stem)
        };
        let g = self.counts();
        let (mut te, mut ae, mut re) = (0u64, 0u64, 0u64);
        let mut se: Vec<PieceRec> = Vec::new();
        let mut he: Option<Diff> = None;
        // 2.1.317：同一项活别的代理正跑着（`se`），或别的代理跑完过、挑改动最少的那次（`re`）。
        let mut running_other: Option<String> = None;
        let mut done_other: Option<(Diff, String)> = None;
        let fallback;
        let list: &[Piece] = if x.is_empty() {
            let t = js_trim(a.command).to_string();
            fallback = [Piece {
                place: a.cwd.to_string(),
                stem: t.clone(),
                text: t,
                heavy: false,
                tests_or_lints: false,
                only_on_failure: false,
                ran_only_if: None,
                goes_on_running: false,
            }];
            &fallback
        } else {
            &x
        };
        for ie in list {
            let ee = !ie.only_on_failure && (!any_heavy || ie.heavy);
            let key = key_of(a.agent_id, ie);
            let work_key = key_of(ANY_AGENT, ie);
            let text_hash = ie.text.clone();
            if ee && let Some(idx) = self.entries.iter().position(|(k, _)| *k == key) {
                let nt = self.diff(idx, &text_hash, g, te, ae);
                if nt.lines + nt.writes == 0 {
                    re += 1;
                }
                if nt.better_than(he.as_ref()) {
                    he = Some(nt);
                }
            }
            if ee {
                if running_other.is_none() {
                    running_other = slot(&self.running_by, &work_key)
                        .and_then(|v| v.iter().find(|x| **x != a.agent_id).cloned());
                }
                let done: Vec<String> = slot(&self.done_by, &work_key).cloned().unwrap_or_default();
                for agent in done.iter().rev() {
                    if agent == a.agent_id {
                        continue;
                    }
                    let k = key_of(agent, ie);
                    let Some(idx) = self.entries.iter().position(|(x, _)| *x == k) else {
                        continue;
                    };
                    let d = self.diff(idx, &text_hash, g, te, ae);
                    if d.better_than(done_other.as_ref().map(|(d, _)| d)) {
                        done_other = Some((d, agent.clone()));
                    }
                }
            }
            if ie.tests_or_lints {
                ae += 1;
            } else if !x.is_empty() {
                te += 1;
            }
            if ee {
                se.push(PieceRec {
                    key,
                    work_key,
                    text_hash,
                    writes: te,
                    checks: ae,
                    ran_only_if: ie.ran_only_if,
                });
            }
        }
        let mut f = Map::new();
        f.insert("repeat".into(), json!(he.is_some()));
        f.insert("compared_statements".into(), json!(se.len()));
        f.insert("unchanged_repeats".into(), json!(re));
        if a.v317 {
            if !a.agent_id.is_empty() {
                f.insert("subagent_in_agent_worktree".into(), json!(a.in_agent_worktree));
            }
            let other =
                running_other.clone().or_else(|| done_other.as_ref().map(|(_, a)| a.clone()));
            f.insert("other_agent_repeat".into(), json!(other.is_some()));
            if let Some(other) = &other {
                let direction = if a.agent_id.is_empty() {
                    "main_after_subagent"
                } else if other.is_empty() {
                    "subagent_after_main"
                } else {
                    "subagent_after_subagent"
                };
                f.insert("other_agent_direction".into(), json!(direction));
                // 官方另把「那次是放到后台、现在还在跑」也算进来，代理分不清后台任务归哪次调用，只认正跑着的。
                f.insert(
                    "other_agent_call_running_at_start".into(),
                    json!(running_other.is_some()),
                );
            }
            if running_other.is_none()
                && let Some((d, _)) = &done_other
            {
                let e = &self.entries[d.idx].1;
                f.insert("other_agent_same_text_as_earlier_call".into(), json!(d.same_text));
                f.insert("other_agent_seconds_since_that_call".into(), json!(secs_since(a.now, e)));
                if d.lines > 0 || d.unseen == 0 {
                    f.insert(
                        "other_agent_lines_changed_since_that_call".into(),
                        json!(d.lines.min(MAX_LINES)),
                    );
                }
                f.insert(
                    "other_agent_possible_writes_since_that_call".into(),
                    json!(d.writes.min(MAX_COUNT)),
                );
                f.insert(
                    "other_agent_possible_writes_by_running_commands".into(),
                    json!(d.running.min(MAX_COUNT)),
                );
                f.insert(
                    "other_agent_tests_and_lints_since_that_call".into(),
                    json!(d.checks.min(MAX_COUNT)),
                );
                f.insert("other_agent_changed_since_that_call".into(), json!(d.changed()));
                f.insert("other_agent_earlier_call_outcome".into(), json!(e.outcome));
                f.insert(
                    "other_agent_earlier_result_in_context".into(),
                    json!(self.visible_to(a.agent_id, e)),
                );
            }
        }
        if let Some(b) = &he {
            let e = &self.entries[b.idx].1;
            f.insert("same_text_as_earlier_call".into(), json!(b.same_text));
            f.insert("seconds_since_that_call".into(), json!(secs_since(a.now, e)));
            if a.v317 {
                f.insert("changed_since_that_call".into(), json!(b.changed()));
            }
            if b.lines > 0 || b.unseen == 0 {
                f.insert("lines_changed_since_that_call".into(), json!(b.lines.min(MAX_LINES)));
            }
            f.insert("possible_writes_since_that_call".into(), json!(b.writes.min(MAX_COUNT)));
            f.insert("tests_and_lints_since_that_call".into(), json!(b.checks.min(MAX_COUNT)));
            let in_context = e.epoch == self.epoch_of(a.agent_id);
            if in_context {
                f.insert("new_message_since_that_call".into(), json!(a.prompt_seq > e.prompt_seq));
            }
            f.insert("earlier_call_outcome".into(), json!(e.outcome));
            f.insert("earlier_result_in_context".into(), json!(in_context));
            // 2.1.317：那次的输出另存与「交给 tail」状态（落盘的 `earlier_output_*` 代理看不到，不报；
            // `earlier_tail_input_flag_on` 只在 Linux 上有，模拟的是 macOS）。
            if a.v317 {
                f.insert(
                    "earlier_tail_input_candidate".into(),
                    json!(e.tail.line == Some("check")),
                );
                f.insert(
                    "earlier_tail_input_candidate_other".into(),
                    json!(e.tail.line == Some("other")),
                );
                if let Some(kept) = e.tail.kept {
                    f.insert("earlier_tail_input_kept".into(), json!(kept));
                }
            }
        }
        let counted_as_running = te + ae > 0;
        if counted_as_running {
            self.running += 1;
        }
        // 2.1.317：这些活记上「正在跑」，结束时摘掉。
        for p in &se {
            match self.running_by.iter_mut().find(|(k, _)| *k == p.work_key) {
                Some((_, v)) => v.push(a.agent_id.to_string()),
                None => self.running_by.push((p.work_key.clone(), vec![a.agent_id.to_string()])),
            }
        }
        Begun {
            fields: f,
            call: Some(Call {
                pieces: se,
                tool_use_id: a.tool_use_id.map(str::to_string),
                agent: a.agent_id.to_string(),
                writes: te,
                checks: ae,
                goes_on_running: x.iter().any(|p| p.goes_on_running),
                background_at_start: 0,
            }),
            counts_at_start: g,
            counted_as_running,
        }
    }

    /// `#f`：拿记着的第 `idx` 条与现在比。`te` / `ae` 是这条命令里排在前面的语句已经算上的写与检查。
    fn diff(&self, idx: usize, text_hash: &str, g: [u64; 4], te: u64, ae: u64) -> Diff {
        let [d, y, q, z] = g;
        let e = &self.entries[idx].1;
        let we = e.background_at_start + self.running + self.background;
        Diff {
            idx,
            same_text: e.text_hashes.iter().any(|t| t == text_hash),
            lines: d.saturating_sub(e.lines_changed),
            writes: (y + we + te).saturating_sub(e.possible_writes),
            running: we,
            checks: (q + ae).saturating_sub(e.tests_and_lints),
            unseen: (z + we + te + ae).saturating_sub(e.unseen_writes),
        }
    }

    /// `settle`：调用结束。`outcome` 是 `ended` 判的结果（`succeeded` / `failed` / `interrupted` / 转后台那几种），
    /// `exited_zero` 退出码为 0，`tail` 是这次的 [`Ended`]（2.1.317 记进去，下回报 `earlier_tail_input_*`）。
    pub(super) fn settle(
        &mut self,
        b: Begun,
        outcome: &'static str,
        exited_zero: bool,
        now: SystemTime,
        prompt_seq: u32,
        tail: Ended,
    ) {
        if b.counted_as_running {
            self.running = self.running.saturating_sub(1);
        }
        let Some(call) = b.call else { return };
        // 2.1.317：每条语句的活上摘掉这个代理一次。
        for p in &call.pieces {
            if let Some(i) = self.running_by.iter().position(|(k, _)| *k == p.work_key) {
                let v = &mut self.running_by[i].1;
                if let Some(at) = v.iter().position(|x| *x == call.agent) {
                    v.remove(at);
                }
                if v.is_empty() {
                    self.running_by.remove(i);
                }
            }
        }
        // 有结果（`r !== void 0`）就按这次的写与检查记；改动行没看到 diff（`diffTaken` 假）记一次没看到的写。
        let (bb, w) = (call.checks, call.writes);
        let r = u64::from(w + bb > 0);
        self.writes += w;
        self.checks += bb;
        self.unseen += r;
        if call.goes_on_running {
            self.background += 1;
        }
        // 转到后台的不记输出另存那项。
        let tail =
            Ended { kept: if outcome.contains("background") { None } else { tail.kept }, ..tail };
        let [x, p, j, g] = b.counts_at_start;
        for pc in call.pieces {
            let keep = if outcome == "succeeded" && exited_zero {
                pc.ran_only_if != Some("unknown")
            } else {
                pc.ran_only_if.is_none()
                    && (outcome == "succeeded" || outcome == "failed" || pc.writes + pc.checks <= 1)
            };
            if !keep {
                continue;
            }
            let same = pc.writes == call.writes && pc.checks == call.checks;
            let mut texts: Vec<String> = match self.entries.iter().position(|(k, _)| *k == pc.key) {
                Some(i) => {
                    let (_, e) = self.entries.remove(i);
                    e.text_hashes.into_iter().filter(|t| *t != pc.text_hash).collect()
                }
                None => Vec::new(),
            };
            texts.push(pc.text_hash);
            let skip = texts.len().saturating_sub(MAX_TEXTS);
            texts.drain(..skip);
            self.entries.push((
                pc.key,
                Entry {
                    text_hashes: texts,
                    returned_at: now,
                    lines_changed: x,
                    possible_writes: p + pc.writes,
                    tests_and_lints: j + pc.checks,
                    unseen_writes: g + if same { r } else { 0 },
                    background_at_start: call.background_at_start,
                    outcome,
                    prompt_seq,
                    epoch: self.epoch_of(&call.agent),
                    tail,
                    tool_use_id: call.tool_use_id.clone(),
                },
            ));
            // 2.1.317：这项活最近由谁跑完（挪到最新，至多 [`MAX_AGENTS`] 个）。
            let mut agents: Vec<String> =
                match self.done_by.iter().position(|(k, _)| *k == pc.work_key) {
                    Some(i) => {
                        self.done_by.remove(i).1.into_iter().filter(|a| *a != call.agent).collect()
                    }
                    None => Vec::new(),
                };
            agents.push(call.agent.clone());
            let skip = agents.len().saturating_sub(MAX_AGENTS);
            agents.drain(..skip);
            self.done_by.push((pc.work_key, agents));
        }
        while self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        while self.done_by.len() > MAX_ENTRIES {
            self.done_by.remove(0);
        }
    }
}

/// `hE`：距那次结束过了几秒（四舍五入、封顶 [`MAX_SECONDS`]）。
fn secs_since(now: SystemTime, e: &Entry) -> i64 {
    let secs = now.duration_since(e.returned_at).map_or(0.0, |d| d.as_secs_f64()).round() as i64;
    secs.clamp(0, MAX_SECONDS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stems(cmd: &str) -> Vec<(String, String, bool)> {
        pieces(cmd, "/w", |c| {
            c.starts_with("ls") || c.starts_with("git status") || c.starts_with("cat")
        })
        .into_iter()
        .map(|p| (p.place.replace('\0', "|"), p.stem, p.heavy))
        .collect()
    }

    #[test]
    fn statements_split_like_the_official_fg() {
        // `cd` 换目录不成一条，只读的跳过。
        assert_eq!(stems("cd sub && npm test"), vec![("/w/sub".into(), "npm test".into(), true)]);
        assert_eq!(stems("git status && ls"), Vec::<(String, String, bool)>::new());
        // 管道尾巴上的筛选剥掉，`tee` 也是筛选。
        // `2>&1` 是转给描述符、不写文件，也去掉（官方 `FG` 同）。
        assert_eq!(
            stems("cargo test 2>&1 | tail -5"),
            vec![("/w".into(), "cargo test".into(), true)]
        );
        // 写到文件的重定向：重活去掉，轻活留着。
        assert_eq!(
            stems("python x.py > out.txt"),
            vec![("/w".into(), "python x.py > out.txt".into(), false)]
        );
        assert_eq!(stems("make > build.log"), vec![("/w".into(), "make".into(), true)]);
        // `source` 激活虚拟环境记进位置。
        assert_eq!(
            stems("source .venv/bin/activate && pytest"),
            vec![("/w|source .venv/bin/activate".into(), "pytest".into(), true)]
        );
    }

    /// 对拍：逐条比官方 2.1.316 `FG`（node 跑原函数，只读判定两边都用 [`oracle_read_only`]）。路径由环境变量
    /// `LUBAN_BASH_FG_ORACLE` 给，没给或文件不在就跳过。
    #[test]
    #[ignore = "依赖本机 node 产出的对拍语料，手动跑"]
    fn pieces_match_the_official_fg_oracle() {
        use std::io::BufRead;
        let Ok(path) = std::env::var("LUBAN_BASH_FG_ORACLE") else { return };
        let Ok(f) = std::fs::File::open(&path) else { return };
        let (mut total, mut bad) = (0usize, Vec::new());
        for line in std::io::BufReader::new(f).lines() {
            let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
            let cmd = v["cmd"].as_str().unwrap();
            total += 1;
            let got: Vec<Value> = pieces(cmd, "/w", oracle_read_only)
                .into_iter()
                .map(|p| {
                    json!([
                        p.place,
                        p.stem,
                        p.text,
                        p.heavy,
                        p.tests_or_lints,
                        p.only_on_failure,
                        p.ran_only_if,
                        p.goes_on_running
                    ])
                })
                .collect();
            if Value::Array(got.clone()) != v["pieces"] {
                bad.push(format!("{cmd:?}\n  want {}\n  got  {}", v["pieces"], Value::Array(got)));
            }
        }
        eprintln!("FG：{total} 条，{} 条不一致", bad.len());
        assert!(bad.is_empty(), "{}", bad.iter().take(15).cloned().collect::<Vec<_>>().join("\n"));
    }

    /// 对拍用的只读判定：`/^(ls|git status|git diff|cat|grep|find|head|wc)(\s|$)/`。
    fn oracle_read_only(c: &str) -> bool {
        ["ls", "git status", "git diff", "cat", "grep", "find", "head", "wc"].iter().any(|p| {
            c.strip_prefix(p).is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace))
        })
    }

    /// 不同子代理各自头一回跑同一条命令，不算重复（键里带调用方代理的 id）；同一个子代理再跑才算。
    #[test]
    fn different_agents_do_not_share_repeats() {
        let mut m = BashMeter::default();
        let rest = json!({ "dangerouslyDisableSandbox": false });
        let t0 = SystemTime::UNIX_EPOCH;
        let mut go = |agent: &'static str, id: &'static str| {
            let b = m.begin(
                BeginArgs {
                    command: "npm test",
                    rest_input: &rest,
                    cwd: "/w",
                    agent_id: agent,
                    tool_use_id: Some(id),
                    now: t0,
                    prompt_seq: 1,
                    v317: false,
                    metered: false,
                    in_agent_worktree: false,
                },
                |_| false,
            );
            let f = Value::Object(b.fields.clone());
            m.settle(b, "succeeded", true, t0, 1, Ended::default());
            (f["repeat"].clone(), f["unchanged_repeats"].clone())
        };
        assert_eq!(go("agent-1", "t1"), (json!(false), json!(0)));
        assert_eq!(go("agent-2", "t2"), (json!(false), json!(0)), "别的子代理不算重复");
        assert_eq!(go("agent-1", "t3"), (json!(true), json!(1)), "同一个子代理再跑才算");
    }

    #[test]
    fn a_repeat_reports_what_changed_since() {
        let mut m = BashMeter::default();
        let ro = |_: &str| false;
        let rest = json!({ "dangerouslyDisableSandbox": false });
        let t0 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let args = |id: &'static str, at: SystemTime, seq: u32| BeginArgs {
            command: "python3 -m unittest -v",
            rest_input: &rest,
            cwd: "/w",
            agent_id: "",
            tool_use_id: Some(id),
            now: at,
            prompt_seq: seq,
            v317: false,
            metered: false,
            in_agent_worktree: false,
        };
        let b = m.begin(args("t1", t0, 1), ro);
        assert_eq!(
            Value::Object(b.fields.clone()),
            json!({ "repeat": false, "compared_statements": 1, "unchanged_repeats": 0 })
        );
        m.settle(b, "succeeded", true, t0, 1, Ended::default());
        m.note_lines(25, 0);
        m.note_other_tool("Read");
        m.note_other_tool("mcp__x__y");
        let t1 = t0 + std::time::Duration::from_secs(16);
        let b = m.begin(args("t2", t1, 2), ro);
        assert_eq!(
            Value::Object(b.fields),
            json!({
                "repeat": true, "compared_statements": 1, "unchanged_repeats": 0,
                "same_text_as_earlier_call": true, "seconds_since_that_call": 16,
                "lines_changed_since_that_call": 25, "possible_writes_since_that_call": 1,
                "tests_and_lints_since_that_call": 0, "new_message_since_that_call": true,
                "earlier_call_outcome": "succeeded", "earlier_result_in_context": true,
            })
        );
    }

    /// 2.1.317 `other_agent_earlier_result_in_context`（官方 `mE`）按当前支线看得见的消息判：独立子代理看不到主线程那次的
    /// 结果，没压缩也是 false；带着父线程上下文分出来的那种（请求里有那次的 tool_result）是 true；压缩过就都看不到了。
    #[test]
    fn other_agent_results_follow_the_branch_context() {
        let rest = json!({ "dangerouslyDisableSandbox": false });
        let t0 = SystemTime::UNIX_EPOCH;
        let cmd = "npm test";
        let seen = |m: &mut BashMeter, agent: &'static str, id: &'static str| {
            let b = m.begin(args317(cmd, &rest, agent, id, t0, true), |_| false);
            let v = b.fields["other_agent_earlier_result_in_context"].clone();
            m.settle(b, "succeeded", true, t0, 1, Ended::default());
            v
        };
        let r = |id: &str, content: Value| super::super::shape::ResultSeen {
            id: id.into(),
            readable: super::super::shape::readable_of(Some(&content)),
        };
        let mut m = BashMeter::default();
        let main = m.begin(args317(cmd, &rest, "", "m1", t0, true), |_| false);
        m.settle(main, "succeeded", true, t0, 1, Ended::default());
        let base = m.clone();
        m.note_visible("agent-1", &[], false);
        assert_eq!(seen(&mut m, "agent-1", "a1"), false, "独立子代理");
        // 分叉子代理：首条完整请求带着父线程那次的结果，回复里才调 Bash；下一条增量续轮只带新结果。
        let mut m2 = base.clone();
        m2.note_visible("agent-2", &[r("m1", json!("ok"))], false);
        m2.note_visible("agent-2", &[r("x1", json!("ok"))], true);
        let mut m3 = m2.clone();
        assert_eq!(seen(&mut m2, "agent-2", "a2"), true, "增量续轮里那次的结果仍在");
        // 主线程压缩不动这个子代理的上下文；子代理自己压缩才清。
        m3.compacted("");
        let mut m4 = m3.clone();
        assert_eq!(seen(&mut m3, "agent-2", "a3"), true, "主线程压缩不影响子代理");
        m4.compacted("agent-2");
        assert_eq!(seen(&mut m4, "agent-2", "a4"), false, "子代理自己压缩过");
        // 再来一条完整请求（不带那次的结果）就换掉了。
        let mut m5 = base.clone();
        m5.note_visible("agent-5", &[r("m1", json!("ok"))], false);
        m5.note_visible("agent-5", &[r("x1", json!("ok"))], false);
        assert_eq!(seen(&mut m5, "agent-5", "a5"), false, "新的完整请求里没有那次的结果");
        // 那次的正文被清掉了不算；原本没落盘、后来被换成落盘预览也不算（官方按调用时的落盘状态走 `VJo`）。
        for (content, want) in [
            (json!("[Old tool result content cleared]"), false),
            (json!("<persisted-output>\nOutput too large"), false),
            (json!("<truncated-output>…"), false),
        ] {
            let mut m6 = base.clone();
            m6.note_visible("agent-6", &[r("m1", content.clone())], false);
            assert_eq!(seen(&mut m6, "agent-6", "a6"), want, "{content}");
        }
    }

    /// 那次调用结束时输出就落了盘（`longOutput.persisted`）：现在的正文是落盘预览照算，只有恰被清掉才不算。
    #[test]
    fn a_persisted_earlier_call_counts_until_cleared() {
        let rest = json!({ "dangerouslyDisableSandbox": false });
        let t0 = SystemTime::UNIX_EPOCH;
        let cmd = "npm test";
        let r = |content: Value| super::super::shape::ResultSeen {
            id: "m1".into(),
            readable: super::super::shape::readable_of(Some(&content)),
        };
        let mut base = BashMeter::default();
        let main = base.begin(args317(cmd, &rest, "", "m1", t0, true), |_| false);
        base.settle(main, "succeeded", true, t0, 1, Ended { persisted: true, ..Ended::default() });
        for (content, want) in [
            (json!("<persisted-output>\nOutput too large"), true),
            (
                json!("<artifact-content-authored-by-others/>\n[Old tool result content cleared]"),
                true,
            ),
            (json!("[Old tool result content cleared]"), false),
        ] {
            let mut m = base.clone();
            m.note_visible("agent-1", &[r(content.clone())], false);
            let b = m.begin(args317(cmd, &rest, "agent-1", "a1", t0, true), |_| false);
            assert_eq!(b.fields["other_agent_earlier_result_in_context"], want, "{content}");
        }
    }

    /// `Bun.hash` 与 Bun 1.3.11 实算逐个相同（空串是 Zig Wyhash 的测试向量 `0x0409638ee2bde459`）；覆盖不到 4 字节、
    /// 4 ~ 16 字节、17 ~ 48 字节与跨 48 字节分块几种走法。
    #[test]
    fn bun_hash_matches_bun() {
        for (s, want) in [
            ("", 290_873_116_282_709_081u64),
            ("a", 2_941_419_223_392_617_777),
            ("abc", 190_542_993_387_777_138),
            ("x", 4_738_888_789_374_899_184),
            ("message digest", 4_742_346_353_319_458_430),
            ("bash repeat meter\0ca578f14-6c7b-460c-8519-62174777ccb5", 6_928_636_203_526_860_696),
            (
                "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789abcdefghijk",
                14_983_946_105_717_027_472,
            ),
        ] {
            assert_eq!(bun_hash(s.as_bytes()), want, "{s:?}");
        }
        // 低 32 位 / 2³²：这个会话约 0.67，比例 0.1 时没抽中，全抽（1.0）时抽中，0 时谁都不抽。
        let sid = "ca578f14-6c7b-460c-8519-62174777ccb5";
        assert!(!metered(sid, 0.1));
        assert!(metered(sid, 1.0));
        assert!(!metered(sid, 0.0));
    }

    fn args317<'a>(
        command: &'a str,
        rest: &'a Value,
        agent: &'a str,
        id: &'a str,
        at: SystemTime,
        metered: bool,
    ) -> BeginArgs<'a> {
        BeginArgs {
            command,
            rest_input: rest,
            cwd: "/w",
            agent_id: agent,
            tool_use_id: Some(id),
            now: at,
            prompt_seq: 1,
            v317: true,
            metered,
            in_agent_worktree: false,
        }
    }

    /// 2.1.317 没抽中的会话走 `skip()`：不比、不记，那组键一个都没有，之后抽中的调用也比不到它。
    #[test]
    fn an_unmetered_2_1_317_session_reports_nothing() {
        let mut m = BashMeter::default();
        let rest = json!({ "dangerouslyDisableSandbox": false });
        let t0 = SystemTime::UNIX_EPOCH;
        for id in ["t1", "t2"] {
            let b = m.begin(args317("npm test", &rest, "", id, t0, false), |_| false);
            assert!(b.fields.is_empty());
            m.settle(b, "succeeded", true, t0, 1, Ended::default());
        }
        let b = m.begin(args317("npm test", &rest, "", "t3", t0, true), |_| false);
        assert_eq!(b.fields["repeat"], false, "没抽中的那两次没记下");
    }

    /// 2.1.317 的布局：首次是 `other_agent_repeat: false`；同一个代理再跑报 `changed_since_that_call` 与那次的
    /// 「交给 tail」状态；子代理跑主线程跑过的同一条命令，报跨代理那组（方向、那次距今、改了什么）。
    #[test]
    fn the_2_1_317_layout_reports_other_agents() {
        let mut m = BashMeter::default();
        let rest = json!({ "dangerouslyDisableSandbox": false });
        let t0 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let cmd = "npm test 2>&1 | tail -5";
        let b = m.begin(args317(cmd, &rest, "", "t1", t0, true), |_| false);
        assert_eq!(
            Value::Object(b.fields.clone()),
            json!({"repeat": false, "compared_statements": 1, "unchanged_repeats": 0,
                "other_agent_repeat": false})
        );
        m.settle(
            b,
            "succeeded",
            true,
            t0,
            1,
            Ended { line: Some("check"), kept: Some(false), persisted: false },
        );
        let t1 = t0 + std::time::Duration::from_secs(5);
        let b = m.begin(args317(cmd, &rest, "", "t2", t1, true), |_| false);
        let keys: Vec<&str> = b.fields.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "repeat",
                "compared_statements",
                "unchanged_repeats",
                "other_agent_repeat",
                "same_text_as_earlier_call",
                "seconds_since_that_call",
                "changed_since_that_call",
                "lines_changed_since_that_call",
                "possible_writes_since_that_call",
                "tests_and_lints_since_that_call",
                "new_message_since_that_call",
                "earlier_call_outcome",
                "earlier_result_in_context",
                "earlier_tail_input_candidate",
                "earlier_tail_input_candidate_other",
                "earlier_tail_input_kept",
            ]
        );
        assert_eq!(b.fields["changed_since_that_call"], "nothing");
        assert_eq!(b.fields["earlier_tail_input_candidate"], true);
        assert_eq!(b.fields["seconds_since_that_call"], 5);
        m.settle(
            b,
            "succeeded",
            true,
            t1,
            1,
            Ended { line: Some("check"), kept: Some(false), persisted: false },
        );
        // 子代理跑同一条：自己没跑过（`repeat` 假），主线程跑过——跨代理那组。
        m.note_other_tool("mcp__x__y");
        let t2 = t1 + std::time::Duration::from_secs(9);
        let b = m.begin(args317(cmd, &rest, "agent-1", "t3", t2, true), |_| false);
        let f = Value::Object(b.fields.clone());
        assert_eq!(f["repeat"], false);
        assert_eq!(f["subagent_in_agent_worktree"], false);
        assert_eq!(f["other_agent_repeat"], true);
        assert_eq!(f["other_agent_direction"], "subagent_after_main");
        assert_eq!(f["other_agent_call_running_at_start"], false);
        assert_eq!(f["other_agent_seconds_since_that_call"], 9);
        assert_eq!(f["other_agent_possible_writes_since_that_call"], 1);
        assert_eq!(f["other_agent_changed_since_that_call"], "possible_writes");
        assert_eq!(f["other_agent_earlier_call_outcome"], "succeeded");
        // 独立的子代理看不到主线程那次的结果：没压缩过也是 false。
        assert_eq!(f["other_agent_earlier_result_in_context"], false);
        // 主线程那次还在跑时子代理又跑：只报正跑着，不报那次之后的统计。
        let main = m.begin(args317(cmd, &rest, "", "t4", t2, true), |_| false);
        let b2 = m.begin(args317(cmd, &rest, "agent-2", "t5", t2, true), |_| false);
        let f = Value::Object(b2.fields.clone());
        assert_eq!(f["other_agent_call_running_at_start"], true, "{f}");
        assert!(f.get("other_agent_seconds_since_that_call").is_none(), "{f}");
        m.settle(main, "succeeded", true, t2, 1, Ended::default());
        m.settle(b2, "succeeded", true, t2, 1, Ended::default());
        m.settle(b, "succeeded", true, t2, 1, Ended::default());
        assert!(m.running_by.is_empty(), "结束时都摘掉了");
    }
}
