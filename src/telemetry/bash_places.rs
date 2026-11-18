//! Bash 命令里提到了哪几类位置（官方 2.1.326 `MEa`，报在 `tengu_tool_use_success` / `tengu_tool_use_error` 的
//! `bash_places`）：十一类路径各一条正则，按固定顺序逐条试，命中的按字母序用逗号连起来，一条都没命中报 `none`，命令超过
//! 一万个 UTF-16 码元报 `too_long`（官方 `sC`）。
//!
//! 官方正则带前后断言（`(?<![\w.-])`、`(?![\w.-])` 之类），`regex` 不支持，这里拆成「锚定在某一位置的核心串 + 前一个字符
//! 与后面的手工判断」，逐位置试，与 JS 的回溯语义等价（核心串里没有能匹配出不同长度的分支）。JS 的 `\w` 没开 `u` 标志，
//! 只认 ASCII 字母数字下划线。

use std::sync::LazyLock;

use regex::Regex;

/// 官方 `sC`：超过这么多个 UTF-16 码元的命令不分类。
const MAX_LEN: usize = 10_000;

/// 前一个字符的限制。
#[derive(Clone, Copy)]
enum Behind {
    /// 不限。
    Anything,
    /// 官方 `ge`：`(?<![\w.-])`。
    NotWordDotDash,
    /// 官方 `Y`：`(?<![\w./~})-])`。
    NotPathy,
}

/// 核心串之后的限制。
#[derive(Clone, Copy)]
enum Ahead {
    /// 不限。
    Any,
    /// 官方 `j`：`(?![\w.-])`。
    End,
    /// `project_files`：`(?!/+(?:\.notes|uploads)j)j`。
    EndNotNotesOrUploads,
    /// `user_data`：`(?!/+uploads j)j`。
    EndNotUploads,
    /// `other_absolute_path`：`(?!(?:tmp|home|root|dev/null|mnt/project-files|mnt/user-data|mnt/skills|opt/claude-plugins-official)j)[\w.-]`。
    OtherAbsolute,
}

struct Alt {
    behind: Behind,
    core: Regex,
    ahead: Ahead,
}

fn alt(behind: Behind, core: &str, ahead: Ahead) -> Alt {
    Alt { behind, core: Regex::new(&format!("^(?:{core})")).expect("bash_places 正则"), ahead }
}

/// JS `\w` 加 `.` `-`。
fn word_dot_dash(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// 官方 `j` 在 `rest` 开头成立：后面没有字符，或不是 `[\w.-]`。
fn ends(rest: &str) -> bool {
    !rest.chars().next().is_some_and(word_dot_dash)
}

/// `rest` 以 `/+` 起、接着 `word` 且其后满足 `j`。
fn slashes_then(rest: &str, words: &[&str]) -> bool {
    let after = rest.trim_start_matches('/');
    if after.len() == rest.len() {
        return false;
    }
    words.iter().any(|w| after.strip_prefix(w).is_some_and(ends))
}

const OTHER_EXCLUDED: &[&str] = &[
    "tmp",
    "home",
    "root",
    "dev/null",
    "mnt/project-files",
    "mnt/user-data",
    "mnt/skills",
    "opt/claude-plugins-official",
];

impl Alt {
    fn matches_at(&self, s: &str, i: usize) -> bool {
        let prev = s[..i].chars().next_back();
        let ok_behind = match self.behind {
            Behind::Anything => true,
            Behind::NotWordDotDash => !prev.is_some_and(word_dot_dash),
            Behind::NotPathy => {
                !prev.is_some_and(|c| word_dot_dash(c) || matches!(c, '/' | '~' | '}' | ')'))
            }
        };
        if !ok_behind {
            return false;
        }
        let Some(m) = self.core.find(&s[i..]) else { return false };
        let rest = &s[i + m.end()..];
        match self.ahead {
            Ahead::Any => true,
            Ahead::End => ends(rest),
            Ahead::EndNotNotesOrUploads => {
                ends(rest) && !slashes_then(rest, &[".notes", "uploads"])
            }
            Ahead::EndNotUploads => ends(rest) && !slashes_then(rest, &["uploads"]),
            Ahead::OtherAbsolute => {
                !OTHER_EXCLUDED.iter().any(|w| rest.strip_prefix(w).is_some_and(ends))
                    && rest.chars().next().is_some_and(word_dot_dash)
            }
        }
    }
}

/// 十一类，顺序同官方 `yt`。
static PLACES: LazyLock<Vec<(&'static str, Vec<Alt>)>> = LazyLock::new(|| {
    use Ahead::*;
    use Behind::*;
    vec![
        ("project_notes", vec![alt(NotWordDotDash, r"\.notes", End)]),
        ("project_uploads", vec![alt(NotPathy, r"/mnt/project-files/+uploads", End)]),
        ("project_files", vec![alt(NotPathy, r"/mnt/project-files", EndNotNotesOrUploads)]),
        (
            "user_uploads",
            vec![
                alt(NotPathy, r"/mnt/user-data/+uploads", End),
                alt(Anything, r"\.claude/+uploads", End),
            ],
        ),
        ("user_data", vec![alt(NotPathy, r"/mnt/user-data", EndNotUploads)]),
        (
            "memory",
            vec![
                alt(Anything, r"/memor(?:y|ies)", End),
                alt(NotWordDotDash, r"memor(?:y|ies)/", Any),
                alt(NotWordDotDash, r"MEMORY\.md", End),
            ],
        ),
        (
            "skills_or_plugins",
            vec![
                alt(NotPathy, r"/mnt/skills", End),
                alt(NotPathy, r"/opt/claude-plugins-official", End),
                alt(Anything, r"\.claude/+(?:skills|plugins)", End),
                alt(NotWordDotDash, r"SKILL\.md", End),
            ],
        ),
        ("tool_results", vec![alt(Anything, r"/tool-results", End)]),
        ("tmp", vec![alt(NotPathy, r"/tmp", End)]),
        (
            "home",
            vec![
                alt(NotPathy, r"~", End),
                alt(NotPathy, r"/home", End),
                alt(NotPathy, r"/root", End),
                alt(Anything, r"\$HOME", End),
                alt(Anything, r"\$\{HOME\}", End),
            ],
        ),
        ("other_absolute_path", vec![alt(NotPathy, r"/", OtherAbsolute)]),
    ]
});

/// 官方 `MEa`：`bash_places` 的取值。
pub(super) fn bash_places(command: &str) -> String {
    if command.encode_utf16().count() > MAX_LEN {
        return "too_long".to_string();
    }
    let hit = |alts: &Vec<Alt>| {
        command.char_indices().any(|(i, _)| alts.iter().any(|a| a.matches_at(command, i)))
    };
    let mut names: Vec<&str> =
        PLACES.iter().filter(|(_, alts)| hit(alts)).map(|(name, _)| *name).collect();
    if names.is_empty() {
        return "none".to_string();
    }
    names.sort_unstable();
    names.join(",")
}

#[cfg(test)]
mod tests {
    use super::bash_places;

    /// 抓包里出现过的几条（`cap/auto-{max,team}-2.1.326-20261117-full`）与几样边角，取值都用 node 跑官方原正则核过。
    #[test]
    fn matches_the_official_regexes() {
        for (cmd, want) in [
            ("python3 calc.py", "none"),
            ("ls ./nonexistent_dir_luban_325", "none"),
            ("cat /etc/hosts", "other_absolute_path"),
            ("ls ~/foo /usr/bin", "home,other_absolute_path"),
            ("echo $HOME ${HOME}", "home"),
            ("ls /tmp /dev/null", "tmp"),
            ("ls /tmpx", "other_absolute_path"),
            ("cat ~/.claude/projects/x/memory/MEMORY.md", "home,memory"),
            ("cat memory/a.md", "memory"),
            ("cat foo/memory", "memory"),
            ("cat amemory/a", "none"),
            ("ls .notes", "project_notes"),
            ("ls a.notes", "none"),
            ("ls /mnt/project-files/uploads /mnt/project-files", "project_files,project_uploads"),
            ("ls /mnt/project-files//.notes", "project_notes"),
            ("ls /mnt/user-data/uploads", "user_uploads"),
            ("ls /mnt/user-data", "user_data"),
            ("ls x/.claude/uploads", "user_uploads"),
            ("cat .claude/skills/a/SKILL.md", "skills_or_plugins"),
            ("ls /opt/claude-plugins-official", "skills_or_plugins"),
            ("cat /a/tool-results/x.txt", "other_absolute_path,tool_results"),
            ("cd ~ && ls", "home"),
            ("a/~", "none"),
            ("cat $(pwd)/x", "none"),
            ("cat ${D}/x", "none"),
        ] {
            assert_eq!(bash_places(cmd), want, "{cmd}");
        }
        assert_eq!(bash_places(&"a".repeat(10_001)), "too_long");
        assert_eq!(bash_places(&"a".repeat(10_000)), "none");
    }

    /// 对拍：`BASH_PLACES_DUMP` 指向 node 跑官方原正则（`MEa` 原文）得出的 `{命令: 取值}`，逐条比。命令取自本机全部抓包里的
    /// Bash 调用加一批边角（11-17 那次 370 条全同）。没有那份文件就跳过。
    #[test]
    #[ignore = "依赖本机 node 导出的对拍文件"]
    fn matches_a_node_dump_of_the_official_function() {
        let Ok(path) = std::env::var("BASH_PLACES_DUMP") else { return };
        let dump: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let bad: Vec<_> = dump.iter().filter(|(c, w)| bash_places(c) != **w).take(10).collect();
        assert!(bad.is_empty(), "{} 条里不一致：{bad:?}", dump.len());
    }
}
