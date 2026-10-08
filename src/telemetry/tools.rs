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
