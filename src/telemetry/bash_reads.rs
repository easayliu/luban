//! Bash 命令读了哪些文件（官方 2.1.321 `Gps`）：`cat` / `nl` / `bat`、`sed -n 'N,Mp'`、`head` / `tail`，以及整条只有一个
//! `grep` / `rg` 的。官方 Bash 跑完（不是后台、没被打断）按它把读到的文件记进 `readFileState`（`qSo`），压缩后恢复
//! 文件就从那张表里挑（`x0r`）——只用 `cat` 看过的任务输出文件也会被读回（`cap/auto-2.1.321-20261110-full` 00183 的
//! `cat …/tasks/bqrgrhew6.output`，压缩后 00204 读回、官方批次 00212 报 3 条恢复）。
//!
//! 逐函数照原文：拆子命令 `Jg`、取命令词 `bC`（`oRe` + `Zgt` + `he`），各种命令的认法 `y5s` / `k5s` / `w5s` / `b5s` /
//! `M5s`、`head` / `tail` 的行数 `zSo`。

use super::bash_ast;

/// 官方 `Dv`：超过这么多个 UTF-16 码元的命令不解析。
const MAX_PARSE_LEN: usize = 10_000;
/// 官方 `qC`：路径超过这么长不记。
const MAX_PATH_LEN: usize = 4096;
/// `head` / `tail` 不写行数时的默认（`f5s` / `p5s`）。
const DEFAULT_LINES: usize = 10;

/// 一条命令读到的一个文件（官方 `Gps` 的一项）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BashRead {
    /// 命令里写的路径（相对路径要按工作目录补全，见 [`resolve`]）。
    pub(super) path: String,
    /// 只读了一段（`sed -n`、`head`、`tail`）：官方记的是那一段，压缩后恢复时整份重读，代理不知道全文。
    pub(super) partial: bool,
    /// `cat -n` / `nl` 带行号输出。
    pub(super) numbered: bool,
    /// 要退出码为 0 才记（`grep` / `rg`）。
    pub(super) requires_exit_zero: bool,
    /// 输出不是文件内容（`grep` / `rg` 只出匹配行）：官方照记、标 `contentNotInModelContext`。
    pub(super) content_not_in_context: bool,
    /// 读的是哪一段（官方 `startLine` / `endLine` / `tailLines`）：`None` 是整份。
    pub(super) range: Option<Range>,
    /// 整条命令只有这一条子命令：输出全是它的。夹着别的子命令（`echo marker; cat f`）时输出里混着别的，内容认不出。
    pub(super) sole: bool,
}

/// 官方 `Gps`：命令里有 `|` `<` `>` 的一概不认；拆成子命令后每条都要认得出（`echo` / `printf` / `true` / `:` 开头的
/// 可以夹在中间），只有一条子命令时另认 `grep` / `rg`。
pub(super) fn bash_reads(command: &str) -> Vec<BashRead> {
    if command.contains(['|', '<', '>']) {
        return Vec::new();
    }
    let parts = split_commands(command);
    if parts.is_empty() {
        return Vec::new();
    }
    let single = parts.len() == 1;
    let mut out = Vec::new();
    for part in &parts {
        let words = command_words(part);
        let hit = sed_read(&words)
            .or_else(|| cat_read(&words))
            .or_else(|| head_read(&words))
            .or_else(|| tail_read(&words))
            .or_else(|| if single { grep_read(&words) } else { None });
        match hit {
            Some(r) => out.push(r),
            None if !single && !harmless(part) => return Vec::new(),
            None => {}
        }
    }
    out.retain(|r| r.path.encode_utf16().count() <= MAX_PATH_LEN);
    for r in &mut out {
        r.sole = single;
    }
    out
}

/// 官方 `h5s`：`/^\s*(echo|printf|true|:)\b/`。
fn harmless(part: &str) -> bool {
    let t = part.trim_start();
    ["echo", "printf", "true", ":"].iter().any(|w| {
        t.strip_prefix(w).is_some_and(|rest| {
            // `\b`：`:` 不是单词字符，后面跟单词字符才算边界；`echo` 这类后面不能再是单词字符。
            let next = rest.chars().next();
            if *w == ":" { next.is_some_and(is_word_char) } else { !next.is_some_and(is_word_char) }
        })
    })
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn len16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// 官方 `Jg`：按 `program` / `list` / `pipeline` 展开、跳过分隔符与注释，`redirected_statement` 只看它的语句部分，
/// 其余节点整段文本算一条子命令。空串返回空；过长或解析不了的整条算一条。
fn split_commands(src: &str) -> Vec<String> {
    if src.is_empty() {
        return Vec::new();
    }
    if len16(src) > MAX_PARSE_LEN {
        return vec![src.to_string()];
    }
    let Some(tree) = bash_ast::parse(src) else { return vec![src.to_string()] };
    let mut root = tree.root;
    if tree.kind(root) == "ERROR"
        && let Some(&first) = tree.children(root).first()
        && tree.kind(first) == "program"
    {
        root = first;
    }
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let kind = tree.kind(id);
        if matches!(kind, "&&" | "||" | "|" | ";" | "&" | "|&" | "\n" | "comment") {
            continue;
        }
        if kind == "redirected_statement" {
            let kids: Vec<_> = tree
                .children(id)
                .iter()
                .copied()
                .filter(|&c| !tree.kind(c).ends_with("_redirect"))
                .collect();
            stack.extend(kids.into_iter().rev());
            continue;
        }
        if matches!(kind, "program" | "list" | "pipeline") {
            stack.extend(tree.children(id).iter().rev());
            continue;
        }
        out.push(String::from_utf16_lossy(tree.text(id)));
    }
    out
}

/// 官方 `bC`：解析一条子命令，取它的命令节点（`oRe`）的词（`Zgt`）。
fn command_words(src: &str) -> Vec<String> {
    if src.is_empty() || len16(src) > MAX_PARSE_LEN {
        return Vec::new();
    }
    let Some(tree) = bash_ast::parse(src) else { return Vec::new() };
    let Some(cmd) = find_command(&tree, tree.root, None) else { return Vec::new() };
    words_of(&tree, cmd)
}

fn is_command(tree: &bash_ast::Tree, id: bash_ast::Id) -> bool {
    matches!(tree.kind(id), "command" | "declaration_command")
}

/// 官方 `oRe`。
fn find_command(
    tree: &bash_ast::Tree,
    id: bash_ast::Id,
    parent: Option<bash_ast::Id>,
) -> Option<bash_ast::Id> {
    let kind = tree.kind(id);
    if is_command(tree, id) {
        return Some(id);
    }
    if kind == "variable_assignment"
        && let Some(p) = parent
    {
        let start = tree.nodes[id].start;
        return tree
            .children(p)
            .iter()
            .copied()
            .find(|&c| is_command(tree, c) && tree.nodes[c].start > start);
    }
    if kind == "pipeline" {
        return tree.children(id).iter().find_map(|&c| find_command(tree, c, Some(id)));
    }
    if kind == "redirected_statement" {
        return tree.children(id).iter().copied().find(|&c| is_command(tree, c));
    }
    tree.children(id).iter().find_map(|&c| find_command(tree, c, Some(id)))
}

/// 官方 `Zgt`：命令名与之后的参数词，遇到命令替换 / 进程替换就停。
fn words_of(tree: &bash_ast::Tree, id: bash_ast::Id) -> Vec<String> {
    let text = |c: bash_ast::Id| String::from_utf16_lossy(tree.text(c));
    if tree.kind(id) == "declaration_command" {
        return tree
            .children(id)
            .first()
            .map(|&c| text(c))
            .filter(|t| {
                matches!(
                    t.as_str(),
                    "export" | "declare" | "typeset" | "readonly" | "local" | "unset" | "unsetenv"
                )
            })
            .into_iter()
            .collect();
    }
    let substitution =
        |c: bash_ast::Id| matches!(tree.kind(c), "command_substitution" | "process_substitution");
    let mut out = Vec::new();
    let mut named = false;
    for &c in tree.children(id) {
        let kind = tree.kind(c);
        if kind == "variable_assignment" {
            continue;
        }
        if kind == "command_name" || (!named && kind == "word") {
            named = true;
            let a = tree.children(c).first().copied().unwrap_or(c);
            if tree.kind(a) == "concatenation" {
                if tree.children(a).iter().any(|&x| substitution(x)) {
                    out.push(text(a));
                } else {
                    out.push(tree.children(a).iter().map(|&x| unquote_word(tree, x)).collect());
                }
            } else {
                out.push(unquote_word(tree, a));
            }
            continue;
        }
        if matches!(kind, "word" | "string" | "raw_string" | "number") {
            out.push(unquote_word(tree, c));
        } else if kind == "concatenation" {
            if tree.children(c).iter().any(|&x| substitution(x)) {
                break;
            }
            out.push(tree.children(c).iter().map(|&x| unquote_word(tree, x)).collect());
        } else if substitution(c) {
            break;
        }
    }
    out
}

/// 官方 `he`：`word` 去掉反斜杠转义（`\\(.)` → `$1`），别的剥一层成对的引号（`mn`）。
fn unquote_word(tree: &bash_ast::Tree, id: bash_ast::Id) -> String {
    let t = String::from_utf16_lossy(tree.text(id));
    if tree.kind(id) == "word" {
        let mut out = String::with_capacity(t.len());
        let mut chars = t.chars();
        while let Some(c) = chars.next() {
            // JS 的 `.` 不匹配换行：`\` 后面是换行就原样留着。
            if c == '\\' {
                match chars.clone().next() {
                    Some(n) if !matches!(n, '\n' | '\r' | '\u{2028}' | '\u{2029}') => {
                        out.push(n);
                        chars.next();
                    }
                    _ => out.push(c),
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    strip_quotes(&t).to_string()
}

/// 官方 `mn`：首尾是同一种引号（`"` 或 `'`）就剥掉。
fn strip_quotes(t: &str) -> &str {
    let b = t.as_bytes();
    if b.len() >= 2
        && (b[0] == b'"' && b[b.len() - 1] == b'"' || b[0] == b'\'' && b[b.len() - 1] == b'\'')
    {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// 读了文件的哪一段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Range {
    /// 第 `start` 到第 `end` 行（从 1 数，含两头）。
    Lines(usize, usize),
    /// 最后 `n` 行。
    Tail(usize),
}

fn read(path: &str, partial: bool) -> BashRead {
    BashRead {
        path: path.to_string(),
        partial,
        range: None,
        numbered: false,
        requires_exit_zero: false,
        content_not_in_context: false,
        sole: false,
    }
}

/// 官方 `y5s`：`sed -n 'N,Mp' 文件` / `sed -n 'Np' 文件`（要有 `-n` / `--quiet` / `--silent`，不能就地改、不能 `-e`）。
fn sed_read(w: &[String]) -> Option<BashRead> {
    if w.first()? != "sed" {
        return None;
    }
    let (mut quiet, mut script, mut file) = (false, None::<&str>, None::<&str>);
    for a in &w[1..] {
        if a.starts_with('-') {
            if a.starts_with("--") {
                if a == "--in-place" || a.starts_with("--in-place=") || a == "--expression" {
                    return None;
                }
                if a == "--quiet" || a == "--silent" {
                    quiet = true;
                }
            } else {
                if a.contains('i') || a == "-e" {
                    return None;
                }
                if a.contains('n') {
                    quiet = true;
                }
            }
            continue;
        }
        if script.is_none() {
            script = Some(a);
        } else if file.is_none() {
            file = Some(a);
        } else {
            return None;
        }
    }
    let (script, file) = (script?, file?);
    if !quiet {
        return None;
    }
    let body = script.strip_suffix('p')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let num = |s: &str| s.parse::<usize>().unwrap_or(usize::MAX);
    let range = match body.split_once(',') {
        Some((a, b)) if digits(a) && digits(b) => Range::Lines(num(a), num(b)),
        None if digits(body) => Range::Lines(num(body), num(body)),
        _ => return None,
    };
    Some(BashRead { range: Some(range), ..read(file, true) })
}

/// 官方 `k5s`：`cat` / `nl` / `bat` / `batcat` 一个文件，只许它们各自认的几个开关（`cat -n` 等带行号）。
fn cat_read(w: &[String]) -> Option<BashRead> {
    let allowed: &[&str] = match w.first()?.as_str() {
        "cat" => &["-n", "--number"],
        "nl" => &[],
        "bat" | "batcat" => &["-n", "--number", "-p", "--plain"],
        _ => return None,
    };
    let mut file = None;
    for a in &w[1..] {
        if a.starts_with('-') && a != "-" {
            if !allowed.contains(&a.as_str()) {
                return None;
            }
            continue;
        }
        if file.is_some() {
            return None;
        }
        file = Some(a.as_str());
    }
    let file = file.filter(|f| *f != "-")?;
    let numbered = w[0] == "nl" || w.iter().any(|a| a == "-n" || a == "--number");
    Some(BashRead { numbered, ..read(file, false) })
}

/// 官方 `zSo`：`head` / `tail` 的行数与文件（`-n N`、`--lines N`、`--lines=N`、`-nN`、`-N`），别的开关一概不认。
fn line_count(w: &[String]) -> Option<(usize, &str)> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let num = |s: &str| s.parse::<usize>().unwrap_or(usize::MAX);
    let (mut count, mut file) = (None, None);
    let mut i = 1;
    while i < w.len() {
        let a = w[i].as_str();
        if a == "-n" || a == "--lines" {
            i += 1;
            let v = w.get(i)?;
            if !digits(v) {
                return None;
            }
            count = Some(num(v));
        } else if let Some(v) = a.strip_prefix("--lines=") {
            if !digits(v) {
                return None;
            }
            count = Some(num(v));
        } else if a.strip_prefix("-n").is_some_and(digits) {
            count = Some(num(&a[2..]));
        } else if a.strip_prefix('-').is_some_and(digits) {
            count = Some(num(&a[1..]));
        } else if a.starts_with('-') || file.is_some() {
            // 别的开关、第二个文件都不认。
            return None;
        } else {
            file = Some(a);
        }
        i += 1;
    }
    let file = file.filter(|f| *f != "-")?;
    if count == Some(0) {
        return None;
    }
    Some((count.unwrap_or(DEFAULT_LINES), file))
}

/// 官方 `w5s`。
fn head_read(w: &[String]) -> Option<BashRead> {
    if w.first()? != "head" {
        return None;
    }
    line_count(w).map(|(n, f)| BashRead { range: Some(Range::Lines(1, n)), ..read(f, true) })
}

/// 官方 `b5s`。
fn tail_read(w: &[String]) -> Option<BashRead> {
    if w.first()? != "tail" {
        return None;
    }
    line_count(w).map(|(n, f)| BashRead { range: Some(Range::Tail(n)), ..read(f, true) })
}

/// 官方 `M5s`：整条只有一个 `grep` / `egrep` / `fgrep` / `rg`，一个模式一个文件（不带通配），开关只许那几种。
fn grep_read(w: &[String]) -> Option<BashRead> {
    let rg = w.first()? == "rg";
    if !rg && !matches!(w[0].as_str(), "grep" | "egrep" | "fgrep") {
        return None;
    }
    let short_ok = |a: &str| {
        let Some(body) = a.strip_prefix('-') else { return false };
        let set: &str = if rg { "iSswxFnNHUP" } else { "niwxEFGPHh" };
        !body.is_empty() && body.chars().all(|c| set.contains(c))
    };
    let long_ok: &[&str] = if rg {
        &[
            "--ignore-case",
            "--smart-case",
            "--case-sensitive",
            "--word-regexp",
            "--line-regexp",
            "--fixed-strings",
            "--line-number",
            "--no-line-number",
            "--with-filename",
            "--no-filename",
            "--multiline",
            "--pcre2",
        ]
    } else {
        &[
            "--line-number",
            "--ignore-case",
            "--word-regexp",
            "--line-regexp",
            "--extended-regexp",
            "--fixed-strings",
            "--basic-regexp",
            "--perl-regexp",
            "--with-filename",
            "--no-filename",
            "--color=never",
            "--color=auto",
        ]
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let context = |a: &str| {
        ["-A", "-B", "-C"].iter().any(|p| a.strip_prefix(p).is_some_and(digits))
            || ["--after-context=", "--before-context=", "--context="]
                .iter()
                .any(|p| a.strip_prefix(p).is_some_and(digits))
    };
    let (mut pattern, mut file) = (None::<&str>, None::<&str>);
    let mut i = 1;
    while i < w.len() {
        let a = w[i].as_str();
        if a.starts_with('-') && a != "-" {
            let takes_value = matches!(a, "-A" | "-B" | "-C")
                || rg && matches!(a, "--after-context" | "--before-context" | "--context");
            if takes_value {
                i += 1;
                if !w.get(i).is_some_and(|v| digits(v)) {
                    return None;
                }
            } else if !(context(a) || short_ok(a) || long_ok.contains(&a)) {
                return None;
            }
            i += 1;
            continue;
        }
        if pattern.is_none() {
            pattern = Some(a);
        } else if file.is_none() {
            file = Some(a);
        } else {
            return None;
        }
        i += 1;
    }
    pattern?;
    let file = file.filter(|f| *f != "-" && !f.contains(['*', '?', '[', '{']))?;
    Some(BashRead { requires_exit_zero: true, content_not_in_context: true, ..read(file, true) })
}

/// 官方 `ht`：相对路径按工作目录补成绝对路径并规整（`.`、`..`、重复的 `/`）。没有工作目录的相对路径、`~` 开头的不认。
/// 文件账本的键（官方 `readFileState` 的键是 `ht` 规范化过的）：绝对路径与按工作目录补全的相对路径规整好；`~/` 开头的家目录
/// 代理不知道，规整后面那段、留着 `~` 开头；没有工作目录的相对路径只规整 `.`、`..`、重复的 `/`。同一个文件不管怎么写都落到
/// 同一个键上。
pub(super) fn ledger_key(path: &str, cwd: Option<&str>) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let inner = resolve(&format!("/{rest}"), None).unwrap_or_default();
        return format!("~{inner}");
    }
    if path == "~" {
        return "~".to_string();
    }
    if let Some(r) = resolve(path, cwd) {
        return r;
    }
    // 相对路径、工作目录不知道：只规整，不补成绝对路径。
    let r = resolve(&format!("/{path}"), None).unwrap_or_default();
    r.trim_start_matches('/').to_string()
}

/// 工具入参里的文件路径（Read / Edit / Write）的账本键：官方解析前先裁掉首尾空白（JS `trim()` 的那套）。Bash 命令里的路径
/// 是 shell 词，空白是文件名的一部分，不裁（用 [`ledger_key`]）。
pub(super) fn tool_path_key(path: &str, cwd: Option<&str>) -> String {
    ledger_key(path.trim_matches(super::files::is_js_space), cwd)
}

pub(super) fn resolve(path: &str, cwd: Option<&str>) -> Option<String> {
    let joined = if path.starts_with('/') {
        path.to_string()
    } else if path.starts_with('~') {
        return None;
    } else {
        format!("{}/{path}", cwd?)
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    Some(format!("/{}", parts.join("/")))
}

/// 官方 `O5s`：文件全文里这次读的那一段（整份就是全文）；行号越界时 `None`。
pub(super) fn slice_of(r: &BashRead, file: &str) -> Option<String> {
    match r.range {
        None => Some(file.to_string()),
        Some(Range::Tail(n)) => {
            let mut lines: Vec<&str> = file.split('\n').collect();
            if lines.last() == Some(&"") {
                lines.pop();
            }
            if lines.is_empty() {
                return None;
            }
            let k = n.min(lines.len());
            Some(lines[lines.len() - k..].join("\n"))
        }
        Some(Range::Lines(start, end)) => {
            let lines: Vec<&str> = file.split('\n').collect();
            let s = start.max(1);
            let e = end.max(s);
            if s > lines.len() {
                return None;
            }
            Some(lines[s - 1..e.min(lines.len())].join("\n"))
        }
    }
}

/// 官方 `Mw`：去掉开头的 BOM，`\r\n` 换成 `\n`（文件读进来先过这一步再取段、再核）。
pub(super) fn normalize_file_text(text: &str) -> String {
    text.strip_prefix('\u{feff}').unwrap_or(text).replace("\r\n", "\n")
}

/// 官方 Read 读整份文件得到的正文（2.1.321 `_vs`）：去开头 BOM，`\r\n` 换成 `\n`，再去掉末尾单独的一个 `\r`。
pub(super) fn read_tool_text(text: &str) -> String {
    let mut t = normalize_file_text(text);
    if t.ends_with('\r') {
        t.pop();
    }
    t
}

/// JS `trim()` 之后是不是空串（空白集合按 JS，[`super::files::is_js_space`]：含 U+FEFF、不含 U+0085）。
fn js_blank(s: &str) -> bool {
    s.trim_matches(super::files::is_js_space).is_empty()
}

/// 官方 `qSo` 里对一个文件的整套核对：全文先 `Mw`，取这次读的那一段（`O5s`，取不到就跳过）；那一段 JS `trim()` 后为空、文件
/// 却不空就跳过；输出不知道（`h === undefined`）不核、照记；否则那一段要包含在输出里（`D5s`）。
pub(super) fn bash_read_verified(r: &BashRead, file: &str, output: Option<&str>) -> bool {
    let file = normalize_file_text(file);
    let Some(part) = slice_of(r, &file) else { return false };
    if js_blank(&part) && !js_blank(&file) {
        return false;
    }
    match output {
        None => true,
        Some(out) => r.content_not_in_context || output_contains(out, &part, r.numbered),
    }
}

/// 官方 `D5s`：文件那一段（去掉开头的空行、`trimEnd()`）是不是包含在输出里（`\r\n` 换成 `\n`）；带行号的输出另把每行的
/// 行号去掉再找一次。
pub(super) fn output_contains(output: &str, part: &str, numbered: bool) -> bool {
    let mut s = part;
    loop {
        let t = s.trim_start_matches(|c: char| c != '\n' && super::files::is_js_space(c));
        match t.strip_prefix('\n') {
            Some(rest) => s = rest,
            None => break,
        }
    }
    let s = s.trim_end_matches(super::files::is_js_space);
    let out = output.replace("\r\n", "\n");
    out.contains(s)
        || numbered
            && out.split('\n').map(strip_line_number).collect::<Vec<_>>().join("\n").contains(s)
}

/// 官方 `I5s`：去掉一行开头的行号（`^[ \t]*\d+[ \t]`），只有空白的行变空。
fn strip_line_number(line: &str) -> &str {
    let t = line.trim_start_matches([' ', '\t']);
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && t[digits..].starts_with([' ', '\t']) {
        return &t[digits + 1..];
    }
    if line.bytes().all(|b| b == b' ' || b == b'\t') { "" } else { line }
}

/// 整份读出的文件内容（只对整条命令就是这一条整份读的：`cat` / `nl` / `bat`）：客户端给模型的输出去掉了末尾的换行，文本文件按以换行
/// 结尾补回；带行号的去掉行号。空输出就是空文件。
pub(super) fn full_content(r: &BashRead, output: &str) -> Option<String> {
    if r.partial || r.content_not_in_context || !r.sole {
        return None;
    }
    if output.is_empty() {
        return Some(String::new());
    }
    let body = if r.numbered {
        output.split('\n').map(strip_line_number).collect::<Vec<_>>().join("\n")
    } else {
        output.replace("\r\n", "\n")
    };
    Some(format!("{body}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(cmd: &str) -> Vec<(String, bool)> {
        bash_reads(cmd).into_iter().map(|r| (r.path, r.partial)).collect()
    }

    /// 认得出的读法与认不出的：管道 / 重定向、混了别的命令的整条不认；只有一条时才认 grep。
    #[test]
    fn reads_follow_the_official_gps() {
        assert_eq!(paths("cat /a/b.output"), vec![("/a/b.output".into(), false)]);
        assert_eq!(paths("cat -n 'x y.txt'"), vec![("x y.txt".into(), false)]);
        assert_eq!(paths("nl f && cat g"), vec![("f".into(), false), ("g".into(), false)]);
        assert_eq!(paths("echo hi; cat g"), vec![("g".into(), false)]);
        assert_eq!(paths("sed -n '3,9p' f.rs"), vec![("f.rs".into(), true)]);
        assert_eq!(paths("head -n 20 f"), vec![("f".into(), true)]);
        assert_eq!(paths("tail -5 f"), vec![("f".into(), true)]);
        assert_eq!(paths("grep -n foo f.py"), vec![("f.py".into(), true)]);
        assert!(paths("grep foo f && cat g").is_empty(), "grep 只在整条只有它时认");
        assert!(paths("cat f | head").is_empty());
        assert!(paths("cat f > g").is_empty());
        assert!(paths("ls && cat g").is_empty(), "混了认不出的命令整条不认");
        assert!(paths("cat a b").is_empty());
        assert!(paths("cat -v f").is_empty());
        assert!(paths("sed 's/a/b/' f").is_empty());
        assert!(paths("sed -i -n '1p' f").is_empty());
        assert!(paths("head -n 0 f").is_empty());
        assert!(paths("grep foo 'src/*.rs'").is_empty());
        assert!(paths("cat $(ls)").is_empty());
        // 非 ASCII 的开关：不认，也不能按字节切坏字符。
        assert!(paths("grep -é x f").is_empty());
        assert!(paths("grep -Aé x f").is_empty());
        assert!(paths("rg -é x f").is_empty());
    }

    #[test]
    fn content_restores_the_trailing_newline_and_drops_line_numbers() {
        let cat = bash_reads("cat f").pop().unwrap();
        assert_eq!(
            full_content(&cat, "bg-done\n\n[exited with code 0]").as_deref(),
            Some("bg-done\n\n[exited with code 0]\n")
        );
        assert_eq!(full_content(&cat, "").as_deref(), Some(""));
        let numbered = bash_reads("cat -n f").pop().unwrap();
        assert_eq!(full_content(&numbered, "     1\ta\n     2\tb").as_deref(), Some("a\nb\n"));
        let head = bash_reads("head f").pop().unwrap();
        assert_eq!(full_content(&head, "a"), None);
        // 夹着别的子命令（输出里混着 marker）：认得出读了这个文件，内容认不出。
        let mixed = bash_reads("echo marker; cat f").pop().unwrap();
        assert_eq!(mixed.path, "f");
        assert_eq!(full_content(&mixed, "marker\nbody"), None);
    }

    /// 账本的键：同一个文件不管怎么写都落到同一个上；`~/` 留着 `~` 开头，没工作目录的相对路径只规整。
    #[test]
    fn ledger_keys_are_normalized() {
        assert_eq!(ledger_key("/a/./b//c", None), "/a/b/c");
        assert_eq!(ledger_key("./x/../y", Some("/w")), "/w/y");
        assert_eq!(ledger_key("~/tool-results//artifact-1", None), "~/tool-results/artifact-1");
        assert_eq!(ledger_key("./a//b", None), "a/b");
        assert_eq!(tool_path_key("\t/a/b \u{feff}", None), "/a/b");
        assert_eq!(ledger_key("/a/b ", None), "/a/b ", "Bash 的路径不裁");
    }

    /// 官方 `O5s` / `D5s`：取那一段、按包含关系核。
    #[test]
    fn slices_and_containment_follow_the_official_check() {
        let file = "a\nb\nc\nd\n";
        let sed = bash_reads("sed -n '2,3p' f").pop().unwrap();
        assert_eq!(slice_of(&sed, file).as_deref(), Some("b\nc"));
        let head = bash_reads("head -n 2 f").pop().unwrap();
        assert_eq!(slice_of(&head, file).as_deref(), Some("a\nb"));
        let tail = bash_reads("tail -n 1 f").pop().unwrap();
        assert_eq!(slice_of(&tail, file).as_deref(), Some("d"));
        let cat = bash_reads("cat f").pop().unwrap();
        assert_eq!(slice_of(&cat, file).as_deref(), Some(file));
        assert!(output_contains("extra header\nshared body", "shared body\n", false));
        assert!(!output_contains("other", "shared body\n", false));
        assert!(output_contains("x\r\n\n  shared", "\n\nshared\n", false));
        assert!(output_contains("     1\tshared body", "shared body\n", true));
        // `Mw`：CRLF 的文件先换成 LF 再核；BOM 去掉。
        let crlf = "extra\r\nshared body\r\n";
        assert!(bash_read_verified(&cat, crlf, Some("extra\nshared body")));
        assert!(bash_read_verified(&cat, "\u{feff}x\n", Some("x")));
        // 那一段按 JS `trim()` 判空：只有 U+0085 不算空（照核，输出里含着就过），只有 U+FEFF 算空（文件不空就跳过）。
        let line2 = bash_reads("sed -n '2p' f").pop().unwrap();
        assert!(bash_read_verified(&line2, "a\n\u{85}\n", Some("\u{85}")));
        assert!(!bash_read_verified(&line2, "a\n\u{feff}\n", Some("a")));
    }

    #[test]
    fn paths_resolve_against_the_working_directory() {
        assert_eq!(resolve("f.txt", Some("/p/q")).as_deref(), Some("/p/q/f.txt"));
        assert_eq!(resolve("../r/./f", Some("/p/q")).as_deref(), Some("/p/r/f"));
        assert_eq!(resolve("/abs//x", None).as_deref(), Some("/abs/x"));
        assert_eq!(resolve("rel", None), None);
        assert_eq!(resolve("~/x", Some("/p")), None);
    }
}
