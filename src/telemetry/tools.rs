//! 工具调用那几类事件的字段：Bash 命令画像、结果大小、auto 模式判决等。

use super::*;

/// 一条 shell 命令在官方遥测里的画像（`tengu_bash_tool_command_executed` 与 Bash 的
/// `tengu_tool_use_success` 那组字段）。规则照 `cap/auto-2.1.285-20260930` 的 16 条逐条对：
///
/// - 按引号外的 `&&` / `||` / `;` / `|` / 换行切成简单命令；打头的 `cd …` 不算一条（`cd X &&
///   find … | xargs wc -l` 是 2 条、argv0 `find`）；`$(…)` 与反引号里的不另算；
/// - argv0 取文件名，`python3` 记 `python`，`xargs` / `sudo` / `env` / `time` / `nohup` 往后取；
///   `bash_last_argv0` 是最后一条的；
/// - 类别看第一条：解释器 `lang_runtime`、`git` 一类 `vcs`、`grep` / `find` / `ls` 一类
///   `file_search`、`cat` / `head` / `tail` 一类 `file_read`、`echo` / `sleep` 一类 `shell_builtin`；
/// - `command_type` 是 `git` / `python3` 这类认得的命令原名，其余 `other`；`git` 另报子命令；
/// - 重定向到 `/dev/null`、`2>&1` 这类 fd 复制不算 `has_redirect`（`ls; head -5 README* 2>/dev/null`
///   为 false，`echo >> README.md` 为 true）。
#[derive(Debug, Default, PartialEq)]
pub(super) struct BashProfile {
    pub(super) command_type: String,
    pub(super) class: &'static str,
    pub(super) argv0: String,
    pub(super) last_argv0: String,
    pub(super) subcommand: Option<String>,
    pub(super) has_pipe: bool,
    pub(super) has_redirect: bool,
    pub(super) has_chain: bool,
    pub(super) has_subshell: bool,
    pub(super) has_heredoc: bool,
    pub(super) simple_commands: usize,
    /// 命令里出现的绝对路径与 `~` 路径（判「只读」时要看它们是不是在工作目录里）。
    pub(super) paths: Vec<String>,
    /// 每条简单命令的 argv0（与 `argv0` 同一规则）。
    pub(super) argv0s: Vec<String>,
}

pub(super) fn bash_profile(cmd: &str) -> BashProfile {
    // 按引号外的操作符切段。
    let mut segs: Vec<String> = Vec::new();
    let (mut cur, mut quote, mut depth) = (String::new(), None::<char>, 0i32);
    let (mut has_pipe, mut has_chain, mut has_redirect) = (false, false, false);
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                cur.push(c);
            }
            None => match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' if cur.ends_with('$') => {
                    depth += 1;
                    cur.push(c);
                }
                ')' if depth > 0 => {
                    depth -= 1;
                    cur.push(c);
                }
                _ if depth > 0 => cur.push(c),
                '&' if next == Some('&') => {
                    has_chain = true;
                    segs.push(std::mem::take(&mut cur));
                    i += 1;
                }
                '|' if next == Some('|') => {
                    has_chain = true;
                    segs.push(std::mem::take(&mut cur));
                    i += 1;
                }
                '|' => {
                    has_pipe = true;
                    segs.push(std::mem::take(&mut cur));
                }
                ';' | '\n' => {
                    has_chain = true;
                    segs.push(std::mem::take(&mut cur));
                }
                '>' => {
                    // `2>&1` / `>&2`：fd 复制；`> /dev/null`：丢弃。都不算重定向。
                    let rest: String = chars[i + 1..].iter().collect();
                    let rest = rest.trim_start_matches('>');
                    let target = rest.trim_start();
                    if !rest.starts_with('&') && !target.starts_with("/dev/null") {
                        has_redirect = true;
                    }
                    cur.push(c);
                }
                _ => cur.push(c),
            },
        }
        i += 1;
    }
    segs.push(cur);
    let words = |seg: &str| -> Vec<String> {
        seg.split_whitespace()
            .map(|w| w.trim_matches(|c| c == '"' || c == '\'').to_string())
            .collect()
    };
    let argv0_of = |ws: &[String]| -> (String, Option<String>) {
        let mut it = ws.iter().skip_while(|w| w.contains('=') && !w.starts_with('-'));
        let mut first = it.next().cloned().unwrap_or_default();
        while matches!(first.as_str(), "xargs" | "sudo" | "env" | "time" | "nohup" | "exec") {
            first = it.find(|w| !w.starts_with('-')).cloned().unwrap_or_default();
        }
        let base = first.rsplit('/').next().unwrap_or("").to_string();
        let sub = it.find(|w| !w.starts_with('-')).cloned();
        (base, sub)
    };
    let simple: Vec<Vec<String>> = segs
        .iter()
        .map(|s| words(s))
        .filter(|ws| !ws.is_empty())
        .filter(|ws| ws[0] != "cd")
        .collect();
    let norm = |a: &str| -> String {
        if a.starts_with("python") { "python".to_string() } else { a.to_string() }
    };
    let (first_raw, sub) = simple.first().map(|ws| argv0_of(ws)).unwrap_or_default();
    let (last_raw, _) = simple.last().map(|ws| argv0_of(ws)).unwrap_or_default();
    let argv0 = norm(&first_raw);
    let class = match argv0.as_str() {
        "python" | "node" | "ruby" | "perl" | "php" | "java" | "deno" | "bun" => "lang_runtime",
        "git" | "hg" | "svn" | "gh" => "vcs",
        "grep" | "rg" | "find" | "fd" | "ls" | "tree" | "locate" | "du" => "file_search",
        "cat" | "head" | "tail" | "less" | "more" | "wc" | "xxd" | "stat" | "file" => "file_read",
        "echo" | "sleep" | "printf" | "true" | "false" | "export" | "test" | "[" | "pwd" => {
            "shell_builtin"
        }
        "npm" | "npx" | "yarn" | "pnpm" | "pip" | "pip3" | "cargo" | "go" | "make" | "uv" => {
            "build"
        }
        _ => "other",
    };
    let command_type = match first_raw.as_str() {
        "git" | "python3" | "python" | "node" | "npm" | "npx" | "yarn" | "pnpm" | "cargo"
        | "go" | "make" | "docker" | "pip" | "pip3" => first_raw.clone(),
        _ => "other".to_string(),
    };
    BashProfile {
        command_type,
        class,
        argv0,
        last_argv0: norm(&last_raw),
        subcommand: (class == "vcs").then_some(sub).flatten(),
        has_pipe,
        has_redirect,
        has_chain,
        has_subshell: cmd.contains("$(") || cmd.contains('`'),
        has_heredoc: cmd.contains("<<"),
        simple_commands: simple.len(),
        argv0s: simple.iter().map(|ws| norm(&argv0_of(ws).0)).collect(),
        paths: simple
            .iter()
            .flatten()
            .filter(|w| w.starts_with('/') || w.starts_with('~'))
            .cloned()
            .collect(),
    }
}

/// `tengu_bash_tool_command_executed` 的正文（键序照 `cap/auto-2.1.285-20260930`）。用户在输入框里
/// 用 `!` 跑的那种没有 `tool_use_id`、`user_typed_shell_dispatch` 与 `dangerously_disable_sandbox`
/// 为真（`00191`）。
pub(super) fn bash_executed_meta(
    p: &BashProfile,
    stdout_len: usize,
    tool_use_id: Option<&str>,
    backgrounded: bool,
    user_typed: bool,
    permission_mode: &str,
) -> Value {
    let mut m = Map::new();
    m.insert("command_type".into(), json!(&p.command_type));
    m.insert("bash_command_class".into(), json!(p.class));
    m.insert("bash_argv0".into(), json!(&p.argv0));
    m.insert("bash_last_argv0".into(), json!(&p.last_argv0));
    if let Some(sub) = &p.subcommand {
        m.insert("bash_subcommand".into(), json!(sub));
    }
    for (k, v) in [
        ("has_pipe", p.has_pipe),
        ("has_redirect", p.has_redirect),
        ("has_chain", p.has_chain),
        ("has_subshell", p.has_subshell),
        ("has_heredoc", p.has_heredoc),
    ] {
        m.insert(k.into(), json!(v));
    }
    m.insert("simple_command_count".into(), json!(p.simple_commands));
    m.insert("stdout_length".into(), json!(stdout_len));
    m.insert("stderr_length".into(), json!(0));
    m.insert("exit_code".into(), json!(0));
    m.insert("interrupted".into(), json!(false));
    m.insert("executor_shell".into(), json!("zsh"));
    m.insert("executor_shell_overridden".into(), json!(false));
    m.insert("sandboxed".into(), json!(false));
    m.insert("sandbox_enabled".into(), json!(false));
    m.insert("dangerously_disable_sandbox".into(), json!(user_typed));
    m.insert("user_typed_shell_dispatch".into(), json!(user_typed));
    m.insert("filesystem_policy".into(), json!("strict"));
    m.insert("call_origin".into(), json!("local"));
    m.insert("had_sandbox_violation".into(), json!(false));
    m.insert("was_backgrounded".into(), json!(backgrounded));
    if let Some(id) = tool_use_id {
        m.insert("tool_use_id".into(), json!(id));
    }
    m.insert("destructive_category".into(), json!("none"));
    m.insert("destructive_target_scope".into(), json!("none"));
    m.insert("git_destructive_target".into(), json!("none"));
    m.insert("permission_mode".into(), json!(permission_mode));
    Value::Object(m)
}

/// 2.1.293 的 Bash 执行事件在 `executor_shell_overridden` 之后多一项 `zsh_nomatch_error`
/// （`cap/auto-2.1.293-20261008-full` 全部 Bash 执行 / 失败事件都带）：zsh 把没匹配上的通配符当错误
/// 时为 true——那条的工具结果开头是 `Exit code 1\n(eval):1: no matches found: --include=*.py`
/// （A0 会话 `00071`），其余都是 false。`result_head` 是工具结果正文的开头，看不到的（`!` 直接
/// 跑的那种）传空串。判法见 [`zsh_nomatch_line`]。
pub(super) fn bash_meta_v293(meta: &mut Value, result_head: &str) {
    let nomatch = result_head.split(['\n', '\r', '\u{2028}', '\u{2029}']).any(zsh_nomatch_line);
    insert_after(meta, "executor_shell_overridden", vec![("zsh_nomatch_error", json!(nomatch))]);
}

/// 2.1.299 的 Bash 命令画像多几项（`cap/auto-2.1.299-20261014-full` 21 条）：事件开头报这条命令跑在哪个
/// 模型的会话里（`[1m]` 不带）；`interrupted` 之后报 `zsh_note_in_prompt`（提示词里有没有那段 zsh 说明，
/// 抓包恒 false；用户在输入框里 `!` 跑的那种不报）；`zsh_nomatch_error` 之后报 `zsh_equals_error`
/// （输出里有 `(eval):N: xxx not found` 那种行）、`bash_pipestatus_use`（命令真用了 `$PIPESTATUS`，见
/// [`bash_pipestatus_use`]）、`bash_word_split_use`（命令里有会被 zsh / bash 拆词差异影响的无引号展开，见
/// [`bash_word_split_use`]）。先调 [`bash_meta_v293`] 再调它。
pub(super) fn bash_meta_v299(
    meta: &mut Value,
    command: &str,
    result_head: &str,
    model: &str,
    user_typed: bool,
) {
    let Some(o) = meta.as_object_mut() else { return };
    let mut m = Map::new();
    m.insert("model".into(), json!(model.trim_end_matches("[1m]")));
    m.extend(std::mem::take(o));
    *o = m;
    if !user_typed {
        insert_after(meta, "interrupted", vec![("zsh_note_in_prompt", json!(false))]);
    }
    let equals = result_head.split(['\n', '\r', '\u{2028}', '\u{2029}']).any(zsh_equals_line);
    insert_after(
        meta,
        "zsh_nomatch_error",
        vec![
            ("zsh_equals_error", json!(equals)),
            ("bash_pipestatus_use", json!(bash_pipestatus_use(command))),
            ("bash_word_split_use", json!(bash_word_split_use(command))),
        ],
    );
}

/// `bash_pipestatus_use`：命令里真的用了 `$PIPESTATUS`（照官方 2.1.299 的
/// `e.includes("PIPESTATUS") && /\$\{?PIPESTATUS\b/.test(Fdt(e))`）。`Fdt`（[`strip_shell_quotes`]）先把
/// 单引号内容、转义掉的 `$` 去掉，所以 `echo '$PIPESTATUS'`、`echo \$PIPESTATUS`、`echo $PIPESTATUS_x`、
/// `echo ${PIPESTATUS_x}` 都是 false，`echo $PIPESTATUS`、`echo ${PIPESTATUS[0]}` 才是 true。
pub(super) fn bash_pipestatus_use(cmd: &str) -> bool {
    if !cmd.contains("PIPESTATUS") {
        return false;
    }
    let f = strip_shell_quotes(cmd.as_bytes());
    let w = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i < f.len() {
        if f[i] == b'$' {
            let mut j = i + 1;
            if j < f.len() && f[j] == b'{' {
                j += 1;
            }
            if f[j..].starts_with(b"PIPESTATUS") {
                let k = j + b"PIPESTATUS".len();
                if k >= f.len() || !w(f[k]) {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// 官方 2.1.299 的 `Fdt`：去掉引号与转义，只留会影响 `$PIPESTATUS` 判定的字节。单引号内的内容整段
/// 丢掉（只留两个 `'`）；双引号切换状态、保留 `"`；反斜杠转义下一个字符——下一个不是 `$ ' " ` \` 时留它
/// 本身、是则连同一起去掉。只特判 ASCII，多字节字节（≥0x80）原样留下。
fn strip_shell_quotes(e: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(e.len());
    let (mut single, mut double) = (false, false);
    let mut i = 0;
    while i < e.len() {
        let c = e[i];
        if single {
            if c == b'\'' {
                single = false;
                t.push(c);
            }
            i += 1;
            continue;
        }
        if c == b'\\' {
            let l = e.get(i + 1).copied();
            i += 2;
            if let Some(l) = l
                && !matches!(l, b'$' | b'\'' | b'"' | b'`' | b'\\')
            {
                t.push(l);
            }
            continue;
        }
        if c == b'"' {
            double = !double;
            t.push(c);
        } else if c == b'\'' && !double {
            single = true;
            t.push(c);
        } else {
            t.push(c);
        }
        i += 1;
    }
    t
}

/// `bash_word_split_use`：命令里有一处无引号的变量展开，在 zsh（默认不拆词）与 bash（会拆词）下结果
/// 不同——官方 2.1.299 的 `z8r`：先过几道便宜的前置判据，再用官方自带的 bash 解析器（[`bash_ast`]，逐函数
/// 移植）解析成语法树，照官方的遍历（`Qjn`）收集四类变量后求交集。
///
/// 命中条件是下面任一（`split ∩ lists` 或 `iterated ∩ captured`）：
/// - 某变量被赋了**含内部空格的引号字面量**（`v='a b'` / `v="a b"`），又在命令参数位置被**不加引号**展开
///   （`cat $v`；`echo`/`print`/`eval` 这几个「拆不拆都一样」的不算，引号里、重定向目标、条件表达式里的也不算）；
/// - 某变量被赋了**命令替换**（`v=$(...)` / 反引号），又出现在 `for`/`select … in $v` 或单元素数组里。
///
/// 前置判据（任一不过直接 false）：命令里有 `$`、长度 ≤ 10000（按 UTF-16 码元，同 JS 的 `length`）、匹配
/// `=["'$\`]` 或 `\bfor|select\s`、且**不**匹配 `sh_?word_?split` 或 `emulate … sh/ksh/bash`（已经开了 zsh 拆词
/// / sh 模拟，两边一样）。解析器放弃（定界符不合规、超时、超限）时判 false，与官方一致。
pub(super) fn bash_word_split_use(cmd: &str) -> bool {
    if !cmd.contains('$')
        || cmd.encode_utf16().count() > 10_000
        || !ws_prefilter(cmd)
        || disables_word_split(cmd)
    {
        return false;
    }
    let Some(tree) = bash_ast::parse(cmd) else { return false };
    let v = split_vars(&tree);
    !v.split.is_disjoint(&v.lists) || !v.iterated.is_disjoint(&v.captured)
}

/// JS 正则里的 `\s`。
fn js_space(c: u32) -> bool {
    matches!(
        c,
        0x09..=0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

fn is_js_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `B8r = /=["'$`]|\b(?:for|select)\s/`：有 `=` 紧跟引号 / `$` / 反引号，或有 `for` / `select` 单词后跟空白。
fn ws_prefilter(cmd: &str) -> bool {
    let c: Vec<char> = cmd.chars().collect();
    for i in 0..c.len() {
        if c[i] == '=' && matches!(c.get(i + 1), Some('"' | '\'' | '$' | '`')) {
            return true;
        }
        if i > 0 && is_js_word(c[i - 1]) {
            continue;
        }
        for kw in ["for", "select"] {
            let n = kw.len();
            if c[i..].iter().take(n).copied().eq(kw.chars())
                && c.get(i + n).is_some_and(|s| js_space(*s as u32))
            {
                return true;
            }
        }
    }
    false
}

/// `H8r = /sh_?word_?split|\bemulate\s+(?:-\w+\s+)*(?:sh|ksh|bash)\b/i`：已经把 zsh 拆词打开、或在 sh 模拟
/// 里跑——两种 shell 下结果一样，官方据此判 false。
fn disables_word_split(cmd: &str) -> bool {
    let low = cmd.to_ascii_lowercase();
    if ["shwordsplit", "sh_wordsplit", "shword_split", "sh_word_split"]
        .iter()
        .any(|v| low.contains(v))
    {
        return true;
    }
    let c: Vec<char> = low.chars().collect();
    let kw: Vec<char> = "emulate".chars().collect();
    (0..c.len()).any(|p| {
        c[p..].starts_with(&kw)
            && (p == 0 || !is_js_word(c[p - 1]))
            && emulates_posix_shell(&c[p + kw.len()..])
    })
}

/// `emulate` 之后是不是 `\s+(?:-\w+\s+)*(?:sh|ksh|bash)\b`。
fn emulates_posix_shell(b: &[char]) -> bool {
    let ws = |b: &[char]| b.iter().take_while(|c| js_space(**c as u32)).count();
    let n = ws(b);
    if n == 0 {
        return false;
    }
    let mut b = &b[n..];
    while b.first() == Some(&'-') {
        let w = b[1..].iter().take_while(|c| is_js_word(**c)).count();
        let s = ws(&b[1 + w..]);
        if w == 0 || s == 0 {
            return false;
        }
        b = &b[1 + w + s..];
    }
    ["sh", "ksh", "bash"].iter().any(|sh| {
        let sh: Vec<char> = sh.chars().collect();
        b.starts_with(&sh) && b.get(sh.len()).is_none_or(|c| !is_js_word(*c))
    })
}

/// `z8r` 收集的四类变量，命中条件是 `split ∩ lists` 或 `iterated ∩ captured`（见 [`bash_word_split_use`]）。
#[derive(Default)]
struct SplitVars {
    /// 被赋了含内部空格的引号字面量的变量（`v='a b'`）。
    lists: std::collections::HashSet<String>,
    /// 被赋了命令替换的变量（`v=$(...)`）。
    captured: std::collections::HashSet<String>,
    /// 在命令参数位置被无引号展开的变量（`cat $v`）。
    split: std::collections::HashSet<String>,
    /// 在 `for`/`select … in $v` 或单元素数组里被迭代的变量。
    iterated: std::collections::HashSet<String>,
}

/// 遍历时的父节点：节点类型，或「拆不拆都一样」的命令（`echo`/`print`/`eval`，原文 `Vjn`）。
#[derive(Clone, Copy)]
enum Parent<'a> {
    Kind(&'a str),
    SameEither,
}

/// `j8r`：展开的父节点是这几类时才算「命令参数位置」。
const WS_ARG_PARENTS: [&str; 6] =
    ["command", "command_name", "declaration_command", "unset_command", "for_statement", "array"];
/// `G8r`：拆不拆都一样的命令。
const WS_SAME_EITHER: [&str; 3] = ["echo", "print", "eval"];

/// 官方 `Qjn`：遍历语法树收集四类变量（集合运算与顺序无关，这里用显式栈代替递归）。
fn split_vars(t: &bash_ast::Tree) -> SplitVars {
    let name = |id| String::from_utf16_lossy(t.text(id));
    let mut v = SplitVars::default();
    let mut stack = vec![(t.root, Parent::Kind(""))];
    while let Some((e, n)) = stack.pop() {
        let kind = t.kind(e);
        let ch = t.children(e);
        let mut s = Parent::Kind(kind);
        match kind {
            "simple_expansion" | "expansion" => {
                if let Some(h) = expansion_var(t, e)
                    && matches!(n, Parent::Kind(k) if WS_ARG_PARENTS.contains(&k))
                {
                    v.split.insert(h);
                }
                continue;
            }
            "concatenation" => s = n,
            "command" => {
                let name_is_same_either =
                    ch.iter().find(|c| t.kind(**c) == "command_name").is_some_and(|h| {
                        let txt = t.text(*h);
                        WS_SAME_EITHER.iter().any(|w| txt.iter().copied().eq(w.encode_utf16()))
                    });
                if name_is_same_either {
                    s = Parent::SameEither;
                }
            }
            "variable_assignment" => {
                let in_command = matches!(n, Parent::Kind("command") | Parent::SameEither);
                if let (false, Some(&h), Some(&b)) = (in_command, ch.first(), ch.get(2))
                    && t.kind(h) == "variable_name"
                {
                    let y = ch.get(1).map(|y| t.kind(*y));
                    let inner = if t.kind(b) == "string" && t.children(b).len() == 3 {
                        t.children(b)[1]
                    } else {
                        b
                    };
                    if t.kind(inner) == "command_substitution" {
                        v.captured.insert(name(h));
                    } else if space_literal(t, b, y == Some("+=")) {
                        v.lists.insert(name(h));
                    } else if t.kind(b) == "array" && y == Some("=") {
                        // `children.slice(1, -1)`：去掉两头的括号。
                        let items = t.children(b);
                        let hi = items.len().saturating_sub(1);
                        single_item(t, if 1 < hi { &items[1..hi] } else { &[] }, &mut v.iterated);
                    }
                }
            }
            "for_statement" => {
                if let Some(h) = ch.iter().find(|w| t.kind(**w) == "variable_name")
                    && ch.iter().any(|w| space_literal(t, *w, false))
                {
                    v.lists.insert(name(*h));
                }
                let y = ch.iter().position(|w| t.kind(*w) == "in").map_or(0, |p| p + 1);
                // `findIndex` 找不到是 -1，`slice(y, -1)` 取到倒数第二个。
                let b = ch.iter().position(|w| matches!(t.kind(*w), ";" | "do_group"));
                let end = b.unwrap_or(ch.len().saturating_sub(1));
                single_item(t, if y < end { &ch[y..end] } else { &[] }, &mut v.iterated);
            }
            _ => {}
        }
        stack.extend(ch.iter().map(|c| (*c, s)));
    }
    v
}

/// 官方 `Jjn`：`$name` / `${name}`（不带运算符）的变量名。
fn expansion_var(t: &bash_ast::Tree, e: bash_ast::Id) -> Option<String> {
    let n = match t.kind(e) {
        "simple_expansion" => 2,
        "expansion" => 3,
        _ => 0,
    };
    let ch = t.children(e);
    let r = *ch.get(1)?;
    (ch.len() == n && t.kind(r) == "variable_name").then(|| String::from_utf16_lossy(t.text(r)))
}

/// 官方 `Yjn`：只有一个元素、且它就是一个 `$name` 时记下。
fn single_item(
    t: &bash_ast::Tree,
    items: &[bash_ast::Id],
    set: &mut std::collections::HashSet<String>,
) {
    if let [one] = items
        && let Some(v) = expansion_var(t, *one)
    {
        set.insert(v);
    }
}

/// 官方 `Xjn`：引号字面量（`'a b'` / `"a b"`）去掉引号、里面的展开各换成一个 `x` 后，有没有内部空白
/// （`\S\s+\S`）；`+=` 在前面补一个 `x`（接在已有值后面，接缝处可能产生内部空白）。
fn space_literal(t: &bash_ast::Tree, e: bash_ast::Id, plus_eq: bool) -> bool {
    if !matches!(t.kind(e), "string" | "raw_string") {
        return false;
    }
    let txt = t.text(e);
    let mut r: Vec<u16> = if txt.len() > 2 { txt[1..txt.len() - 1].to_vec() } else { Vec::new() };
    for &c in t.children(e) {
        if matches!(t.kind(c), "\"" | "string_content") {
            continue;
        }
        let needle = t.text(c);
        let at = if needle.is_empty() {
            Some(0)
        } else {
            r.windows(needle.len()).position(|w| w == needle)
        };
        if let Some(at) = at {
            r.splice(at..at + needle.len(), [u16::from(b'x')]);
        }
    }
    if plus_eq {
        r.insert(0, u16::from(b'x'));
    }
    has_inner_whitespace(&r)
}

/// `\S\s+\S`（JS 的 `\s`，按 UTF-16 码元）。
fn has_inner_whitespace(s: &[u16]) -> bool {
    let (mut seen, mut gap) = (false, false);
    for &c in s {
        if js_space(u32::from(c)) {
            gap |= seen;
        } else if gap {
            return true;
        } else {
            seen = true;
        }
    }
    false
}

/// 一行是不是 zsh 的「`=cmd` 展开找不到命令」诊断，照 2.1.299 可执行文件里的判据：
/// `/^\(eval\):\d+: \S+ not found$/m`。
pub(super) fn zsh_equals_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("(eval):") else { return false };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let Some(rest) = rest[digits..].strip_prefix(": ") else { return false };
    let Some(word) = rest.strip_suffix(" not found") else { return false };
    !word.is_empty() && !word.chars().any(char::is_whitespace)
}

/// 一行是不是 zsh 的「通配符没匹配上」诊断，照 2.1.293 可执行文件里的判据逐字实现：
/// `/^(?:\(eval\)|[A-Za-z_][\w-]*)(?::\d+)?: no matches found: /m`——行首是 `(eval)` 或一个名字
/// （`zsh`、报错的函数名 `scan_py` 之类），可带 `:行号`，紧跟 `: no matches found: `。只看子串会把
/// 命令自己的输出（`search: no matches found in index`）也算进去。按 JS 的 `m` 标志，`\n`、`\r`、
/// `\u{2028}`、`\u{2029}` 之后都算行首。
pub(super) fn zsh_nomatch_line(line: &str) -> bool {
    let rest = if let Some(r) = line.strip_prefix("(eval)") {
        r
    } else {
        let mut chars = line.char_indices();
        match chars.next() {
            Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }
        let end = chars
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-'))
            .map_or(line.len(), |(i, _)| i);
        &line[end..]
    };
    let rest = match rest.strip_prefix(':') {
        Some(r) => {
            let digits = r.chars().take_while(char::is_ascii_digit).count();
            if digits > 0 && r[digits..].starts_with(':') { &r[digits..] } else { rest }
        }
        None => rest,
    };
    rest.starts_with(": no matches found: ")
}

/// [`tool_success_extras`] 的文件表里记「上一份计划的 Write 入参长度」用的键（不会是真路径）。
pub(super) const PLAN_INPUT_KEY: &str = "\0plan";

/// `tengu_tool_use_success` 里各工具自己那组字段（`cap/auto-2.1.285-20260930` 的键序）：结果之后、
/// `toolInputSizeBytes` 之前是 sidecar 那几项，之后是路径 / 命令那几项。`success` 进来时最后一个
/// 键是 `toolInputSizeBytes`。
pub(super) fn tool_success_extras(
    success: &mut Value,
    tu: &ToolUse,
    sizes: &mut HashMap<String, usize>,
    v277: bool,
    v285: bool,
) {
    let path = tu.input.get("file_path").and_then(|p| p.as_str()).unwrap_or("");
    let path_fields = |v: &mut Value| {
        v["fileExtension"] = json!(file_ext(path));
        v["filePathLen"] = json!(js_len(path));
    };
    let mut sidecar: Vec<(&str, Value)> = Vec::new();
    match tu.name.as_str() {
        "Read" if !path.is_empty() => {
            if tu.image_b64 > 0 {
                sidecar.push(("sidecarFileBase64Bytes", json!(tu.image_b64)));
            } else {
                sidecar.push(("sidecarFileContentBytes", json!(tu.stripped_bytes)));
                if tu.input.get("offset").is_none() && tu.input.get("limit").is_none() {
                    sizes.insert(path.to_string(), tu.stripped_bytes);
                }
            }
        }
        "Edit" if !path.is_empty() => {
            let old = tu.input.get("old_string").and_then(|s| s.as_str()).unwrap_or("");
            let new = tu.input.get("new_string").and_then(|s| s.as_str()).unwrap_or("");
            let original = sizes.get(path).copied().unwrap_or(old.len() * 3);
            sidecar.push(("sidecarOriginalFileBytes", json!(original)));
            sidecar.push(("sidecarStructuredPatchBytes", json!(old.len() + new.len() + 40)));
            sizes.insert(path.to_string(), (original + new.len()).saturating_sub(old.len()));
        }
        "Write" if !path.is_empty() => {
            let content = tu.input.get("content").and_then(|s| s.as_str()).unwrap_or("");
            let original = sizes.get(path).copied();
            if let Some(o) = original {
                sidecar.push(("sidecarOriginalFileBytes", json!(o)));
            }
            sidecar.push(("sidecarContentBytes", json!(js_len(content))));
            let patch = if original.is_some() { js_len(content).saturating_sub(11) } else { 0 };
            sidecar.push(("sidecarStructuredPatchBytes", json!(patch)));
            sizes.insert(path.to_string(), js_len(content));
            // 规划模式写的是计划文件：随后的 `ExitPlanMode` 客户端把这份计划塞进入参，
            // `toolInputSizeBytes` 与这条 Write 的相等（`00162` / `00164` 都是 1589）。
            if path.contains("/plans/") {
                sizes.insert(PLAN_INPUT_KEY.to_string(), tu.input_len);
            }
        }
        _ => {}
    }
    if !sidecar.is_empty() {
        insert_after(success, "toolResultWillPersist", sidecar);
    }
    match tu.name.as_str() {
        "Read" if !path.is_empty() => {
            path_fields(success);
            success["readHasLimit"] = json!(tu.input.get("limit").is_some());
            success["readHasOffset"] = json!(tu.input.get("offset").is_some());
        }
        "Edit" | "Write" if !path.is_empty() => path_fields(success),
        "Bash" => {
            let command = tu.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
            // 命令里提到的文件，都在 `bashCommandLen` 前（`tail … README.md` 两项都是 `md`）：
            // `fileExtension` 是文件命令的参数扩展名（官方 `o8s`；`cap/2.1.277` 那条 `sed … .rs` 报 `rs`，
            // `cap/2.1.280/00176` 两条与复算逐字相同），`bashCommandFileExtensions` 是文档类扩展名（`hjr`；
            // `cap/auto-2.1.285-20260930` 起才见到，更早的抓包里没有能判断的命令）。
            if v277 && let Some(ext) = super::files::bash_command_file_ext(command) {
                success["fileExtension"] = json!(ext);
            }
            if v285 {
                let docs = super::files::bash_mentioned_exts(command);
                if !docs.is_empty() {
                    success["bashCommandFileExtensions"] = json!(docs.join(","));
                }
            }
            success["bashCommandLen"] = json!(tu.command_len);
            if !command.is_empty() {
                let p = bash_profile(command);
                success["bash_command_class"] = json!(p.class);
                success["bash_argv0"] = json!(&p.argv0);
                if let Some(sub) = &p.subcommand {
                    success["bash_subcommand"] = json!(sub);
                }
                success["has_pipe"] = json!(p.has_pipe);
            }
        }
        _ => {}
    }
}

/// 只读的 shell 命令：default 模式下不弹框、auto 模式下服务端判决直接放行
/// （`serverHeldShellAllowFrom: readOnly`）。查找、读文件、`git` 的只读子命令，且不写文件。
///
/// 碰到工作目录外面的路径就不算（`~/.codex/config.toml`：`/init` 那条读配置的命令 default 模式下
/// 照样弹了框，`cap/auto-2.1.285-20260930` 08:02:04.556）；工作目录里的绝对路径照算只读。
pub(super) fn bash_read_only(p: &BashProfile, cwd: Option<&str>) -> bool {
    let inside = p.paths.iter().all(|w| {
        w == "/dev/null" || (!w.starts_with('~') && cwd.is_some_and(|c| w.starts_with(c)))
    });
    if !inside {
        return false;
    }
    let git_ro = p.class == "vcs"
        && p.subcommand
            .as_deref()
            .is_some_and(|s| matches!(s, "status" | "log" | "diff" | "show" | "branch" | "blame"));
    // 每一条都得是只读的：`ls …; which …` 那条（`07:58:12.088` 前）没弹框。
    (matches!(p.class, "file_search" | "file_read") || git_ro)
        && !p.has_redirect
        && p.argv0s.iter().all(|a| {
            matches!(
                a.as_str(),
                "ls" | "cat"
                    | "head"
                    | "tail"
                    | "wc"
                    | "grep"
                    | "rg"
                    | "find"
                    | "tree"
                    | "stat"
                    | "file"
                    | "du"
                    | "xxd"
                    | "echo"
                    | "which"
                    | "pwd"
                    | "sort"
                    | "uniq"
                    | "cut"
                    | "git"
            )
        })
}

/// Bash 失败结果开头的 `Exit code N`；认不出按 1。
pub(super) fn exit_code_of(head: &str) -> i64 {
    head.trim_start()
        .strip_prefix("Exit code ")
        .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(1)
}

/// `tengu_auto_mode_decision` 的正文（键序照 `cap/auto-2.1.285-20260930`）。两种形态：
///
/// - 服务端判了（`not_flagged` / `flagged`）：长的那份，Bash 多破坏性判定三项；只读命令服务端
///   直接放行，报 `serverVerdictOverrodeFastPath: true` + `serverHeldShellAllowFrom: readOnly`；
///   复合命令的判定来源是 `subcommandResults`，SubagentHandback 是 `safetyCheck`，其余工具没有这一项；
///   同一条回复里第二个起的工具 `sameTurnSiblings` 递增、不再等分类器；
/// - 服务端没判（`skipped`，acceptEdits 快路径下的 Edit / Write）：短的那份。
///
/// `session*Tokens` 是会话到上一条回复为止的用量合计。
#[allow(clippy::too_many_arguments)]
pub(super) fn auto_mode_decision_meta(
    tu: &ToolUse,
    verdict: &str,
    profile: Option<&BashProfile>,
    agent_msg_id: &str,
    sibling: usize,
    usage: [i64; 4],
    seed: u32,
    cwd: Option<&str>,
) -> Value {
    let wait = if sibling == 0 { 850 + i64::from(seed % 1400) } else { 0 };
    let decision = if verdict == "flagged" { "denied" } else { "allowed" };
    let mut m = Map::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_string(), v);
    };
    put("decision", json!(decision));
    put("toolName", json!(&tu.name));
    put("isMcp", json!(false));
    put("inProtectedNamespace", json!(false));
    put("chromeAutomode", json!(false));
    if verdict == "skipped" {
        put("mcpAlwaysAllowOverride", json!(false));
        put("mcpServerAskOverride", json!(false));
        put("agentMsgId", json!(agent_msg_id));
        put("confidence", json!("high"));
        put("fastPath", json!("acceptEdits"));
        put("chromePointerPress", json!(false));
        put("hookAllowVouch", json!(false));
        put("classifierSource", json!("server"));
        put("serverClassifierNoVerdict", json!("server_call_skipped"));
        put("classifierTotalWaitMs", json!(if sibling == 0 { wait } else { 0 }));
        return Value::Object(m);
    }
    for k in ["chromeNavigationForced", "chromePointerPress", "chromeClickRouted", "hookAllowVouch"]
    {
        put(k, json!(false));
    }
    put("mcpAlwaysAllowOverride", json!(false));
    put("mcpServerAskOverride", json!(false));
    let read_only = profile.is_some_and(|p| bash_read_only(p, cwd));
    if let Some(p) = profile {
        put("destructive_category", json!("none"));
        put("destructive_target_scope", json!("none"));
        put("git_destructive_target", json!("none"));
        put("stripAllBashFlag", json!(false));
        let compound = p.simple_commands > 1 && !read_only;
        put(
            "originalDecisionReasonType",
            json!(if compound { "subcommandResults" } else { "other" }),
        );
    } else {
        put("stripAllBashFlag", json!(false));
        if tu.name == "SubagentHandback" {
            put("originalDecisionReasonType", json!("safetyCheck"));
        }
    }
    put("editClassificationGated", json!(false));
    put("serverVerdictOverrodeFastPath", json!(read_only));
    if read_only {
        put("serverHeldShellAllowFrom", json!("readOnly"));
    }
    put("agentMsgId", json!(agent_msg_id));
    put("sameTurnSiblings", json!(sibling));
    put("classifierQueueDepth", json!(0));
    put("classifierQueueWaitMs", json!(0));
    put("classifierSource", json!("server"));
    put("classifierTotalWaitMs", json!(wait));
    put("classifierModel", json!("nonconforming"));
    put("consecutiveDenials", json!(0));
    put("totalDenials", json!(0));
    put("classifierDurationMs", json!(0));
    put("sessionInputTokens", json!(usage[0]));
    put("sessionOutputTokens", json!(usage[1]));
    put("sessionCacheReadInputTokens", json!(usage[2]));
    put("sessionCacheCreationInputTokens", json!(usage[3]));
    Value::Object(m)
}

/// 2.1.291 的 `tengu_auto_mode_decision` 多两项（`cap/auto-2.1.291-20261006-full` 每条都有）：
/// `mcpRemoteSessionAllowOverride` 紧跟 `mcpAlwaysAllowOverride`，`fromTurnHandoff` 紧跟 `agentMsgId`。
pub(super) fn auto_mode_decision_v291(v: &mut Value) {
    insert_after(
        v,
        "mcpAlwaysAllowOverride",
        vec![("mcpRemoteSessionAllowOverride", json!(false))],
    );
    insert_after(v, "agentMsgId", vec![("fromTurnHandoff", json!(false))]);
}

/// 2.1.300 的 `tengu_auto_mode_decision` 多一项 `coworkWebAllowOverride`，紧跟
/// `mcpRemoteSessionAllowOverride`（`cap/auto-2.1.300-20261015-full` 31 条都是 `false`）。
pub(super) fn auto_mode_decision_v300(v: &mut Value) {
    insert_after(
        v,
        "mcpRemoteSessionAllowOverride",
        vec![("coworkWebAllowOverride", json!(false))],
    );
}

/// 路径的扩展名（不带点）；没有的为空。
pub(super) fn file_ext(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_string(),
        _ => String::new(),
    }
}

/// Read 结果里文件本身的字节数：去掉每行开头的行号前缀（`     1\t` / `1→`）再按 UTF-8 数，
/// 行与行之间一个 `\n`（`sidecarFileContentBytes`：`cap/auto-2.1.285-20260930` 167 字的结果对
/// 140 字节的 calc.py）。
pub(super) fn read_content_bytes(result: &str) -> usize {
    let lines: Vec<&str> = result
        .lines()
        .take_while(|l| !l.trim_start().starts_with("<system-reminder>"))
        .map(|l| {
            let t = l.trim_start();
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 {
                let rest = &t[digits..];
                rest.strip_prefix('\t').or_else(|| rest.strip_prefix('→')).unwrap_or(rest)
            } else {
                l
            }
        })
        .collect();
    lines.iter().map(|l| l.len()).sum::<usize>() + lines.len().saturating_sub(1)
}

/// 一条调用结束的时刻。
pub(super) fn this_end_wall(call: &ApiCall) -> SystemTime {
    call.started_at + Duration::from_millis(call.total_ms)
}

/// 挂着的建议被用户的下一次输入顶掉：`tengu_prompt_suggestion{outcome: ignored}`，报在提交那一刻
/// （`cap/auto-2.1.285-20260930` 20 条）。`timeToIgnoreMs` 是建议出来到提交的间隔；
/// `timeToFirstKeystrokeMs` 再扣掉敲这段输入的时间（按 [`Telemetry::process`] 里 `user_secs` 的
/// 口径估，间隔不够就按间隔）；`similarity` 是**敲的字数 ÷ 建议的字数**（20 条里认得出原文的
/// 12 条逐位相等：79 / 28 = 2.82、8 / 11 = 0.73……）。
pub(super) fn ignored_suggestion_meta(
    request_id: &str,
    shown_end: SystemTime,
    suggestion_chars: usize,
    submit: DateTime<Utc>,
    typed_chars: usize,
) -> Value {
    let shown: DateTime<Utc> = shown_end.into();
    let to_ignore = (submit - shown).num_milliseconds().max(0);
    let typing = ((800.0 + 100.0 * typed_chars as f64) as i64).min(to_ignore);
    json!({
        "source": "cli",
        "outcome": "ignored",
        "prompt_id": "user_intent",
        "generationRequestId": request_id,
        "timeToIgnoreMs": to_ignore,
        "timeToFirstKeystrokeMs": (to_ignore - typing).max(0),
        "wasFocusedWhenShown": true,
        "similarity": typed_chars as f64 / suggestion_chars.max(1) as f64
    })
}

/// 客户端的权限模式名 → 事件里报的那个（同名）。认不出的不报，退回推断。
pub(super) fn permission_mode_name(mode: &str) -> Option<&'static str> {
    ["default", "auto", "plan", "acceptEdits", "bypassPermissions", "dontAsk"]
        .into_iter()
        .find(|m| *m == mode)
}

/// `estimatedInputTokens`，口径见 [`parse_shape`]。
pub(super) fn estimate_tokens(model: &str, chars: usize) -> usize {
    if model.contains("haiku") { (chars as f64 / 4.0).round() as usize } else { chars.div_ceil(3) }
}

/// 消息里最后一条 auto 模式提示是进入还是退出；一条都没有为 `None`。从末尾往前找，
/// 通常几条之内就停。
///
/// **只认客户端自己注入的那两种位置**（`cap/2.1.277`、`cap/2.1.280` 全部 36 处）：
/// - `role: system` 的消息（环境说明、附件）——用户正文不会落在这种消息里；
/// - user 消息里以 `<system-reminder>` 开头、紧跟着就是提示本身的文本块。
///
/// 用户正文里引用这段话、工具输出（`tool_result`）里恰好有这段文字、assistant 回复里复述
/// 它，都不算：否则一条 `cat` 了 Claude Code 文档的 Bash 结果就能把 permissionMode 翻掉，
/// 连带子代理沿用的模式与 auto 模式那几条事件一起错。
pub(super) fn auto_mode_of_messages(msgs: &[Value]) -> Option<bool> {
    const ENTER: &str = "While auto mode is active:";
    const EXIT: &str = "## Exited Auto Mode";
    const REMINDER: &str = "<system-reminder>";
    // 一段文本里最后出现的是进入还是退出。
    let last_marker = |t: &str| match (t.rfind(ENTER), t.rfind(EXIT)) {
        (None, None) => None,
        (Some(a), Some(e)) => Some(a > e),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
    };
    // user 消息里的提示块：标签之后（跳过空白）直接就是提示。
    let reminder_marker = |t: &str| {
        let rest = t.trim_start().strip_prefix(REMINDER)?.trim_start();
        if rest.starts_with(ENTER) {
            Some(true)
        } else if rest.starts_with(EXIT) {
            Some(false)
        } else {
            None
        }
    };
    for m in msgs.iter().rev() {
        let role = m.get("role").and_then(|r| r.as_str());
        let texts: Vec<&str> = match m.get("content") {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect(),
            _ => Vec::new(),
        };
        for t in texts.iter().rev() {
            let hit = match role {
                Some("system") => last_marker(t),
                Some("user") => reminder_marker(t),
                _ => None,
            };
            if hit.is_some() {
                return hit;
            }
        }
    }
    None
}

/// 报错文案进哈希前的脱敏（官方 `dZe`，逐条照搬）：取前 500 个 UTF-16 码元，依次把 URL、邮箱、密钥、
/// Windows / UNC / Unix 路径、UUID、请求 id、长十六进制、长 base64、IP、四位以上的数字换成占位符。
/// JS 的 `\b` / `\w` / `\d` 只认 ASCII，这里写成 `(?-u:\b)` 与显式字符类；`\s` 是 ECMAScript 的空白集合（含 U+FEFF、
/// 不含 U+0085，与 Rust 的 Unicode `\s` 不同），显式写出；`/i` 没带 `u` 标志，非 ASCII 字符不会折成 ASCII 字母
/// （`ſ`、开尔文符号 `K` 都不算 `s` / `k`），所以忽略大小写只按 ASCII（`(?i-u:…)`）。
pub(super) fn scrub_error_message(message: &str) -> String {
    static RULES: std::sync::LazyLock<Vec<(regex::Regex, &'static str)>> = std::sync::LazyLock::new(
        || {
            const B: &str = r"(?-u:\b)";
            // ECMAScript 的 `\s`（WhiteSpace + LineTerminator）。
            const WS: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
            [
                (format!(r"(?i-u:https?://)[^{WS}]+"), "<url>"),
                (format!(r"{B}[0-9A-Za-z_.+-]+@[0-9A-Za-z_.-]+\.[0-9A-Za-z_]{{2,}}{B}"), "<email>"),
                (
                    format!(
                        r"{B}(?i-u:sk-ant|sk|pk|ghp|gho|ghs|ghu|github_pat|xox[bpoars])[-_][0-9A-Za-z_-]{{8,}}{B}"
                    ),
                    "<key>",
                ),
                (format!(r#"[A-Za-z]:\\[^{WS}"']*"#), "<path>"),
                (format!(r#"\\\\[^{WS}"']+"#), "<path>"),
                (format!(r#"(?:[^{WS}"'\\]+\\){{2,}}[^{WS}"']+"#), "<path>"),
                (format!(r#"(?:/[^{WS}"':]+){{2,}}"#), "<path>"),
                (
                    format!(
                        r"{B}[0-9a-fA-F]{{8}}-[0-9a-fA-F]{{4}}-[0-9a-fA-F]{{4}}-[0-9a-fA-F]{{4}}-[0-9a-fA-F]{{12}}{B}"
                    ),
                    "<id>",
                ),
                (format!(r"{B}req_[A-Za-z0-9_-]{{12,}}"), "<id>"),
                (format!(r"{B}[0-9a-fA-F]{{16,}}{B}"), "<id>"),
                (format!(r"{B}[A-Za-z0-9+/]{{32,}}={{0,2}}"), "<b64>"),
                (format!(r"{B}[0-9]{{1,3}}(?:\.[0-9]{{1,3}}){{3}}{B}"), "<ip>"),
                (format!(r"{B}[0-9]{{4,}}{B}"), "<num>"),
            ]
            .into_iter()
            .map(|(re, to)| (regex::Regex::new(&re).expect("dZe pattern"), to))
            .collect()
        },
    );
    // `slice(0, 500)` 按 UTF-16 码元截：第 500 个落在代理对中间时留下半个代理，哈希前编成 UTF-8 时它变成
    // U+FFFD（`EF BF BD`）。这里照样补一个 U+FFFD，而不是把整个字符丢掉。
    let mut units = 0;
    let mut head = String::new();
    for c in message.chars() {
        let n = c.len_utf16();
        if units + n > 500 {
            if units < 500 {
                head.push('\u{FFFD}');
            }
            break;
        }
        units += n;
        head.push(c);
    }
    RULES.iter().fold(head, |s, (re, to)| re.replace_all(&s, *to).into_owned())
}

/// 已声明的工具在验证这一步就被客户端拦下（`validateInput` 不过）时报的 `errorCode`：按各工具的提示文案认
/// （2.1.306 的 Edit / Write / NotebookEdit）。认不出的不算——别的工具、别的失败形态没有样本，照旧走执行。
pub(super) fn validate_error_code(tool: &str, message: &str) -> Option<u32> {
    let table: &[(&str, u32)] = match tool {
        "Edit" => &[
            ("No changes to make: old_string and new_string are exactly the same.", 1),
            ("File does not exist.", 4),
            ("Cannot create new file - file already exists.", 3),
            ("File is a Jupyter Notebook.", 5),
            ("File has not been read yet.", 6),
            ("File has been modified", 7),
            ("String to replace not found in file.", 8),
            ("Found ", 9),
            ("File is too large to edit", 10),
        ],
        "Write" => &[
            ("File has not been read yet.", 2),
            ("File has been modified", 3),
            // 子代理写 `REPORT*.md` 之类的报告文件（官方另报 `tengu_subagent_md_report_blocked`）。
            ("Subagents should return findings as text, not write report files.", 5),
        ],
        "NotebookEdit" => &[
            ("Notebook file does not exist.", 1),
            ("File must be a Jupyter notebook", 2),
            ("Cell type is required", 5),
            ("Notebook is not valid JSON.", 6),
            // JSON 合法但不是 notebook 的结构（官方 `CRe`）。
            ("Notebook file is not a valid Jupyter notebook", 6),
            ("Cell ID must be specified", 7),
            ("Cell with index ", 7),
            ("Cell with ID ", 8),
            ("File has not been read yet.", 9),
            ("File has been modified since read", 10),
        ],
        _ => &[],
    };
    if tool == "Edit"
        && message.starts_with("Found ")
        && !message.contains(" matches of the string to replace")
    {
        return None;
    }
    // 文件不是 UTF-8 文本（旧编码 / 二进制、前 8KB 有 NUL、UTF-16 大端）：官方 `VSt` 按原因换开头，后面那句固定。
    if matches!(tool, "Edit" | "NotebookEdit")
        && message.starts_with("File ")
        && message.contains("These tools write UTF-8 (or UTF-16LE behind its byte-order mark)")
    {
        return Some(15);
    }
    if tool == "Write" {
        if message.contains(" is a directory, not a file.") {
            return Some(17);
        }
        if message.contains(" exists but is not a regular file") {
            return Some(18);
        }
    }
    table.iter().find(|(p, _)| message.starts_with(p)).map(|(_, c)| *c)
}

/// schema 不过时的 `zodIssueCodes`：从客户端拼的文案反认 zod 的 issue code（官方 `xL` 逐条一句：缺必填参数、
/// 类型不对是 `invalid_type`，多了参数是 `unrecognized_keys`，都不符是 `invalid_union`），去重、按出现顺序、
/// 逗号相连。认不出一条的（只有 zod 自己那句 message）报 `custom`。
pub(super) fn zod_issue_codes(detail: &str) -> String {
    let mut codes: Vec<&str> = Vec::new();
    for line in detail.lines() {
        let code = if line.starts_with("The required parameter `") && line.ends_with("` is missing")
            || line.contains("` type is expected as `")
        {
            "invalid_type"
        } else if line.starts_with("An unexpected parameter `") {
            "unrecognized_keys"
        } else if line.ends_with(" matches none of the accepted forms") {
            "invalid_union"
        } else {
            continue;
        };
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    if codes.is_empty() { "custom".into() } else { codes.join(",") }
}
