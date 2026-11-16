//! 服务端开关 `tengu_review_bughunter_config`：`enabled` 为真的号，官方客户端的提示词多出 ultrareview 那两处。
//!
//! - system 第四块 `# Session-specific guidance` 在「When the user types `/<skill-name>`…」那条之后多一条 ultrareview 说明
//!   （官方 `explainsUltrareview` = `SB()`：开关里 `enabled` 为真且能用 claude.ai 账号）；
//! - 首轮环境说明的技能清单里 `code-review` 那行换成带 `ultra` 档与 `--post` 的长版（官方 `Ks()` 看 `RUt()`，同一个 `enabled`）。
//!
//! 值按号下发：同一天 Team 号拿到 `{"enabled": false}`（`source: force`），个人 Max 号拿到 `enabled: true` 加一串
//! 配置（`source: defaultValue`；`cap/auto-2.1.325-20261114-full/00009` 与 `cap/auto-max-2.1.325-20261116-full/00004` 的 eval），
//! 抓包里两边的主线程正好差这两处。只能按号从 eval 里记（保活与会话启动握手拉 eval 时顺带记）；没见过 eval 的号按关算，
//! 即沿用此前的团队号形态。只存在内存里。
//!
//! 2.1.322 起的三个可执行文件都有这两处（更早的版本手头没有，不补）。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 开关名。
pub(super) const BUGHUNTER_FEATURE: &str = "tengu_review_bughunter_config";

/// 最早确认有这两处的版本。
const SINCE: (u64, u64, u64) = (2, 1, 322);

/// system 第四块里插在它后面的那一条（模板里逐字就是这样，含行首空格与行尾换行）。
const SKILL_LINE: &str = " - When the user types `/<skill-name>`, invoke it via Skill. Only use skills listed in the \
user-invocable skills section — don't guess.\n";

/// ultrareview 说明（`cap/auto-max-2.1.325-20261116-full/00045` 第四块）。
const ULTRAREVIEW_LINE: &str = " - If the user asks about \"ultrareview\" or how to run it, explain that /code-review ultra \
launches a multi-agent cloud review of the current branch (or /code-review ultra <PR#> for a GitHub PR); /ultrareview is a \
deprecated alias for the same command. It is user-triggered and billed; you cannot launch it yourself, so do not attempt to \
via Bash or otherwise. It needs a git repository (offer to \"git init\" if not in one); the no-arg form bundles the local \
branch and does not need a GitHub remote.\n";

/// 技能清单里 `code-review` 那行的两版（资产 [`crate::config::CC_ENV_SKILLS`] 里是短版）。
const CODE_REVIEW_PLAIN: &str = "(low|medium|high|xhigh|max: from few high-confidence findings to many, some uncertain); \
with no level given, it reuses the level you typed last. Pass --comment to post findings as inline PR comments, or --fix to \
apply them to the working tree after the review. Pass --max-findings <n> to report up to n findings, or --max-findings all \
for every finding. The choice stays until you pass --max-findings default. - Use the max level only when the user, or \
instructions such as CLAUDE.md or a skill, explicitly ask for it, never on your own judgment: it is the costliest level and \
starts many reviewer subagents.";

const CODE_REVIEW_ULTRA: &str = "(low|medium|high|xhigh|max: from few high-confidence findings to many, some uncertain; \
ultra: deep multi-agent review in the cloud); with no level given, it reuses the level you typed last. Pass --comment to post \
findings as inline PR comments, or --fix to apply them to the working tree after the review. Pass --max-findings <n> to report \
up to n findings, or --max-findings all for every finding. The choice stays until you pass --max-findings default. For ultra on \
a GitHub.com PR target, --post asks to post the finished review’s findings to the PR as a single comment from the user’s \
GitHub account (not a review; the launch dialog still confirms in interactive sessions, while non-interactive mode posts on \
the flag alone) and --no-post hides that option. - Use the max or ultra level only when the user, or instructions such as \
CLAUDE.md or a skill, explicitly ask for one, never on your own judgment: they are the costliest reviews and start many \
reviewer subagents.";

static ENABLED: LazyLock<parking_lot::RwLock<HashMap<i64, bool>>> = LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的开关；eval 里没有这一项、或值里没有 `enabled: true` 按关记。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let on = features
        .get(BUGHUNTER_FEATURE)
        .and_then(|f| f.get("value"))
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        == Some(true);
    ENABLED.write().insert(cred_id, on);
}

/// 这个号这个版本的提示词带不带 ultrareview 那两处。
pub(crate) fn applies(cred_id: i64, version: &str) -> bool {
    super::parse_version(version).is_some_and(|v| v >= SINCE)
        && ENABLED.read().get(&cred_id).copied().unwrap_or(false)
}

/// 第四块里在 `/<skill-name>` 那条后面补 ultrareview 说明。找不到那条（模板里没有技能那句）或已经有了就原样。
pub(crate) fn fit_system_rest(rest: String, on: bool) -> String {
    if !on || rest.contains(ULTRAREVIEW_LINE) {
        return rest;
    }
    match rest.find(SKILL_LINE) {
        Some(at) => {
            let at = at + SKILL_LINE.len();
            format!("{}{}{}", &rest[..at], ULTRAREVIEW_LINE, &rest[at..])
        }
        None => rest,
    }
}

/// 技能清单里 `code-review` 那行换成带 `ultra` 的长版。资产里没有短版那段就原样。
pub(crate) fn fit_skills(skills: &str, on: bool) -> std::borrow::Cow<'_, str> {
    if on && skills.contains(CODE_REVIEW_PLAIN) {
        std::borrow::Cow::Owned(skills.replacen(CODE_REVIEW_PLAIN, CODE_REVIEW_ULTRA, 1))
    } else {
        std::borrow::Cow::Borrowed(skills)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v["features"].as_object().unwrap().clone()
    }

    #[test]
    fn follows_the_accounts_eval() {
        let id = -93_101;
        assert!(!applies(id, "2.1.325"), "没见过 eval 按关");
        note_eval(
            id,
            &features(
                serde_json::json!({"features": {BUGHUNTER_FEATURE: {"value": {"fleet_size": 5, "enabled": true}}}}),
            ),
        );
        assert!(applies(id, "2.1.325"));
        assert!(applies(id, "2.1.322"));
        assert!(!applies(id, "2.1.321"), "2.1.322 之前不补");
        note_eval(
            id,
            &features(
                serde_json::json!({"features": {BUGHUNTER_FEATURE: {"value": {"enabled": false}}}}),
            ),
        );
        assert!(!applies(id, "2.1.325"), "团队号那份是 enabled:false");
        note_eval(id, &features(serde_json::json!({"features": {}})));
        assert!(!applies(id, "2.1.325"));
    }

    /// 资产里确实有那两处锚点：技能那句在各份第四块模板里、`code-review` 短版在技能清单里。
    #[test]
    fn anchors_exist_in_the_assets() {
        for rest in [
            crate::config::CC_SYSTEM_REST,
            crate::config::CC_SYSTEM_REST_HAIKU_5_5,
            crate::config::CC_SYSTEM_REST_FABLE_5_2,
        ] {
            assert!(rest.contains(SKILL_LINE));
        }
        assert!(crate::config::CC_ENV_SKILLS.contains(CODE_REVIEW_PLAIN));
    }

    /// 两段正文与 `cap/auto-max-2.1.325-20261116-full/00045`（个人 Max 号 opus 主线程首轮）逐字相同：第四块里技能那句之后紧跟
    /// ultrareview 那句，首条消息的技能清单里是 `code-review` 长版；同版团队号那条（`cap/auto-2.1.325-20261114-full/00068`）
    /// 两处都是短版、没有那句。抓包目录不在仓库里（`.gitignore`），没有就跳过。
    #[test]
    fn matches_the_2_1_325_max_and_team_captures() {
        let body = |sub: &str, idx: &str| -> Option<serde_json::Value> {
            let dir = format!("{}/cap/{sub}", env!("CARGO_MANIFEST_DIR"));
            let path =
                std::fs::read_dir(&dir).ok()?.filter_map(|e| Some(e.ok()?.path())).find(|p| {
                    p.file_name()
                        .and_then(|f| f.to_str())
                        .is_some_and(|f| f.starts_with(idx) && f.ends_with(".req.raw"))
                })?;
            let raw = std::fs::read(path).ok()?;
            let sep = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
            serde_json::from_slice(&raw[sep + 4..]).ok()
        };
        let (Some(max), Some(team)) = (
            body("auto-max-2.1.325-20261116-full", "00045_"),
            body("auto-2.1.325-20261114-full", "00068_"),
        ) else {
            eprintln!("skipped: 2.1.325 captures not present");
            return;
        };
        let rest = |v: &serde_json::Value| v["system"][3]["text"].as_str().unwrap().to_string();
        let msgs = |v: &serde_json::Value| v["messages"].to_string();
        assert!(rest(&max).contains(&format!("{SKILL_LINE}{ULTRAREVIEW_LINE}")));
        assert!(!rest(&team).contains(ULTRAREVIEW_LINE));
        assert_eq!(
            fit_system_rest(rest(&team), true).len(),
            rest(&team).len() + ULTRAREVIEW_LINE.len()
        );
        let json = |s: &str| serde_json::Value::from(s).to_string().trim_matches('"').to_string();
        assert!(msgs(&max).contains(&json(CODE_REVIEW_ULTRA)));
        assert!(msgs(&team).contains(&json(CODE_REVIEW_PLAIN)));
    }

    /// 补上之后与 Max 号抓包里那一段逐字相同：技能那句之后紧跟 ultrareview 那句，再接 `# Memory`。
    #[test]
    fn inserts_after_the_skill_line() {
        let rest = format!("# Session-specific guidance\n{SKILL_LINE}\n# Memory\n");
        let out = fit_system_rest(rest.clone(), true);
        assert_eq!(
            out,
            format!("# Session-specific guidance\n{SKILL_LINE}{ULTRAREVIEW_LINE}\n# Memory\n")
        );
        assert_eq!(fit_system_rest(out.clone(), true), out, "已有的不重复补");
        assert_eq!(fit_system_rest(rest.clone(), false), rest);
        let skills = fit_skills(crate::config::CC_ENV_SKILLS, true);
        assert!(skills.contains(CODE_REVIEW_ULTRA));
        assert!(!skills.contains(CODE_REVIEW_PLAIN));
        assert_eq!(fit_skills(crate::config::CC_ENV_SKILLS, false), crate::config::CC_ENV_SKILLS);
    }
}
