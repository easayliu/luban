//! 2.1.321 起主线程 system 第四块里的 `# Finishing work` 与 `# Memory, notes and feedback` 两节（官方能力
//! `finishing_work_guidance` / `memory_guidance`）。
//!
//! 官方 2.1.321 的能力判定（`kT`）与原创游戏那段（[`super::original_games`]）同一个函数：这两项在「带
//! `fable_5_2_prompt_bundle` 的模型」上默认开，内置目录里只有 fable-5-2 与 mythos-5-2 带那个 bundle；bootstrap 回包的
//! `client_data` 里某一项为 `false` 时那一节关（另报 `tengu_model_capability_off_from_client_data`），默认关的模型
//! `client_data` 显式给 `true` 也会开（另报 `tengu_model_capability_from_client_data`）。两项各判各的。system 段序是
//! `…、original_games、heron_brook、brook_heron、finishing_work、notes_and_feedback、willow_tern、autonomy_append、…`，
//! 之后才是 EndConversation 提示与 `<total_tokens>`。
//!
//! 默认开着时的正文与位置有抓包（`cap/auto-2.1.321-20261110-fable52/00068`，那天 fable-5-2 的 bootstrap 里没有这两项）；
//! 显式下发 `false` / 给别的模型下发 `true` 还没见过，按可执行文件做。两节正文取自 fable-5-2 那份模板
//! （[`crate::config::CC_SYSTEM_REST_FABLE_5_2`]）。
//!
//! `client_data` 按号、按 bootstrap 的模型记，同 [`super::original_games`]；随整份 `client_data` 落库、启动时读回
//! （[`super::client_data`]）。没见过的（号, 模型）、或下发里没有这一项的按模型默认。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 两节，按官方段序。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    FinishingWork,
    MemoryNotes,
}

pub(crate) const SECTIONS: [Section; 2] = [Section::FinishingWork, Section::MemoryNotes];

impl Section {
    /// 能力名（`client_data` 里同名一项）。
    pub(crate) fn capability(self) -> &'static str {
        match self {
            Self::FinishingWork => "finishing_work_guidance",
            Self::MemoryNotes => "memory_guidance",
        }
    }

    /// 这一节的标题行（带换行），正文以它开头。
    pub(crate) fn head(self) -> &'static str {
        match self {
            Self::FinishingWork => "# Finishing work\n",
            Self::MemoryNotes => "# Memory, notes and feedback\n",
        }
    }

    /// 认这一节用的特征：标题行连同正文第一行。只认标题会把别处碰巧同名的标题（`heron_brook` 下发的正文、客户端自己
    /// 写的提示词）当成内置这一节；正文第一行是官方逐字写死的。
    pub(crate) fn signature(self) -> &'static str {
        let t = self.text();
        let first_line =
            t[self.head().len()..].find('\n').map_or(t.len(), |n| self.head().len() + n);
        &t[..first_line]
    }

    /// 这一节的整段正文（不带前后的空行），取自 fable-5-2 那份模板：从标题行起，到下一节标题或 `<total_tokens>` 那行
    /// 之前的空行为止。
    pub(crate) fn text(self) -> &'static str {
        static TEXTS: LazyLock<[&'static str; 2]> = LazyLock::new(|| {
            let t = crate::config::CC_SYSTEM_REST_FABLE_5_2;
            let cut = |from: &str, to: &str| {
                let at = t.find(from).expect("fable-5-2 模板里有这一节");
                let end = t[at..].find(to).map_or(t.len(), |n| at + n);
                &t[at..end]
            };
            [
                cut(Section::FinishingWork.head(), "\n\n# Memory, notes and feedback\n"),
                cut(Section::MemoryNotes.head(), "\n\n<total_tokens>"),
            ]
        });
        TEXTS[self as usize]
    }
}

/// [`CLIENT_DATA`] 的表。
type Served = HashMap<(i64, String), [Option<bool>; 2]>;

/// `(号, 模型键)` → bootstrap `client_data` 里两项显式下发的布尔值（不是布尔、没下发的是 `None`，按模型默认）。
static CLIENT_DATA: LazyLock<parking_lot::RwLock<Served>> = LazyLock::new(Default::default);

/// 模型键：小写、去掉 `[1m]`（bootstrap 的 `model` 参数是规范名）。
fn model_key(model: &str) -> String {
    let m = model.trim().to_ascii_lowercase();
    m.strip_suffix("[1m]").unwrap_or(&m).to_string()
}

/// 记下一份 bootstrap 回包里的 `client_data`（[`super::client_data`] 拆好交过来，回包里没有这两项时都按没下发）。
pub(crate) fn note_client_data(cred_id: i64, model: &str, data: &serde_json::Value) {
    let key = (cred_id, model_key(model));
    let served = SECTIONS.map(|s| data.get(s.capability()).and_then(serde_json::Value::as_bool));
    if served.iter().all(Option::is_none) {
        CLIENT_DATA.write().remove(&key);
    } else {
        CLIENT_DATA.write().insert(key, served);
    }
}

/// 忘掉这个号这个模型的那份（[`super::client_data`] 按号限条数，挤出去的一并清掉）。
pub(crate) fn forget_client_data(cred_id: i64, model: &str) {
    CLIENT_DATA.write().remove(&(cred_id, model_key(model)));
}

/// `client_data` 显式下发的那个值，没下发是 `None`。
fn served(cred_id: i64, model: &str, section: Section) -> Option<bool> {
    CLIENT_DATA.read().get(&(cred_id, model_key(model))).and_then(|v| v[section as usize])
}

/// 规范名（`[1m]`、日期这类尾巴去掉）。
fn canonical(model: &str) -> String {
    let canon = super::curious_tower::canonical_model(model);
    canon.strip_suffix("[1m]").unwrap_or(&canon).to_string()
}

/// 这一版有没有这两节：2.1.321 起（2.1.319 的可执行文件里没有这两个能力名；2.1.320 没抓，按没有）。读不出版本的按最新那版。
fn version_has(version: &str) -> bool {
    !crate::proxy::parse_version(version).is_some_and(|v| v < (2, 1, 321))
}

/// 规范名 `model` 默认带不带这两节：带 `fable_5_2_prompt_bundle` 的 fable-5-2 / mythos-5-2。
fn on_by_default(model: &str) -> bool {
    matches!(model, "claude-fable-5-2" | "claude-mythos-5-2")
}

/// 这个号拿 `model` 当主线程模型、客户端是 `version` 时，第四块带不带 `section` 这一节：`client_data` 显式下发了就照它，
/// 没下发按模型默认。
pub(crate) fn applies(cred_id: i64, model: &str, version: &str, section: Section) -> bool {
    version_has(version)
        && served(cred_id, model, section).unwrap_or_else(|| on_by_default(&canonical(model)))
}

/// `client_data` 改了这个模型的默认时官方报的那几条（同 [`super::original_games::client_data_event`]），按段序；与默认
/// 一致或没下发的不报。
pub(crate) fn client_data_events(
    cred_id: i64,
    model: &str,
    version: &str,
) -> Vec<(Section, &'static str, serde_json::Value)> {
    if !version_has(version) {
        return Vec::new();
    }
    let canon = canonical(model);
    SECTIONS
        .into_iter()
        .filter_map(|s| {
            let on = served(cred_id, model, s)?;
            if on == on_by_default(&canon) {
                return None;
            }
            let name = if on {
                "tengu_model_capability_from_client_data"
            } else {
                "tengu_model_capability_off_from_client_data"
            };
            Some((s, name, serde_json::json!({ "capability": s.capability(), "model": canon })))
        })
        .collect()
}

/// 第四块模板（渲染好、**还没拼**原创游戏那段与 `heron_brook` 的正文——[`super::heron_brook::fit_system_rest`] 在这之后调，
/// 下发的正文里碰巧有同样的标题也不会被当成这两节）按 `on` 调整这两节：模板里有、该关的整节去掉（连同它前面的空行）；
/// 模板里没有、该开的按段序插进去——排在已有的另一节之后 / 之前，都没有就插在 `# Writing for the user`（官方
/// `willow_tern`，fable-5-1 那份模板有）前，再没有插在 `<total_tokens>` 那行前，那行也没有就接在末尾。
pub(crate) fn fit_system_rest(rest: String, on: [bool; 2]) -> String {
    let mut out = rest;
    for s in SECTIONS {
        let at = find_section(&out, s);
        match (at, on[s as usize]) {
            (Some(at), false) => {
                let end = at + section_len(&out[at..], s);
                let from = if out[..at].ends_with("\n\n") { at - 2 } else { at };
                out.replace_range(from..end, "");
            }
            (None, true) => insert_section(&mut out, s),
            _ => {}
        }
    }
    out
}

/// 这一节在 `rest` 里的起点（标题行顶格、前面是空行或在开头）。
fn find_section(rest: &str, s: Section) -> Option<usize> {
    rest.match_indices(s.head())
        .map(|(at, _)| at)
        .find(|at| *at == 0 || rest[..*at].ends_with("\n\n"))
}

/// 从这一节起点算，这一节有多长：官方正文逐字在就是它的长度；被改过的截到下一个顶格标题或 `<total_tokens>` 前。
fn section_len(from: &str, s: Section) -> usize {
    if from.starts_with(s.text()) {
        return s.text().len();
    }
    let body = &from[s.head().len()..];
    let next = ["\n\n# ", "\n\n<total_tokens>"].iter().filter_map(|m| body.find(m)).min();
    s.head().len() + next.unwrap_or(body.len())
}

fn insert_section(out: &mut String, s: Section) {
    let text = s.text();
    // `MemoryNotes` 紧跟在 `FinishingWork` 后面；`FinishingWork` 排在 `MemoryNotes` 前面。
    match s {
        Section::MemoryNotes => {
            if let Some(at) = find_section(out, Section::FinishingWork) {
                let end = at + section_len(&out[at..], Section::FinishingWork);
                out.insert_str(end, &format!("\n\n{text}"));
                return;
            }
        }
        Section::FinishingWork => {
            if let Some(at) = find_section(out, Section::MemoryNotes) {
                out.insert_str(at, &format!("{text}\n\n"));
                return;
            }
        }
    }
    let before = ["# Writing for the user\n", "<total_tokens>"]
        .iter()
        .filter_map(|m| out.rfind(m).filter(|at| out[..*at].ends_with("\n\n")))
        .next();
    match before {
        Some(at) => out.insert_str(at, &format!("{text}\n\n")),
        None => {
            out.push_str("\n\n");
            out.push_str(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const V: &str = "2.1.321";

    #[test]
    fn section_texts_come_from_the_fable_5_2_template() {
        let fin = Section::FinishingWork.text();
        let mem = Section::MemoryNotes.text();
        assert!(fin.starts_with("# Finishing work\nEnding your turn means your work stops there"));
        assert!(fin.ends_with("edit directly."), "{fin}");
        assert!(mem.starts_with("# Memory, notes and feedback\nThis adds to the Memory section"));
        assert!(mem.ends_with("not intermediate files."), "{mem}");
        let t = crate::config::CC_SYSTEM_REST_FABLE_5_2;
        assert!(t.contains(&format!("\n\n{fin}\n\n{mem}\n\n<total_tokens>")));
    }

    #[test]
    fn default_follows_the_model_and_client_data_decides_when_given() {
        let id = 96_321;
        for s in SECTIONS {
            assert!(applies(id, "claude-fable-5-2", V, s));
            assert!(applies(id, "claude-fable-5-2[1m]", V, s));
            assert!(applies(id, "claude-mythos-5-2", V, s));
            assert!(!applies(id, "claude-fable-5-2", "2.1.319", s), "2.1.319 没有这两节");
            assert!(!applies(id, "claude-fable-5-2", "2.1.320", s), "2.1.320 没抓，按没有");
            for m in
                ["claude-opus-5-5", "claude-fable-5-1", "claude-sonnet-5-5", "claude-haiku-5-5"]
            {
                assert!(!applies(id, m, V, s), "{m}");
            }
        }
        assert!(client_data_events(id, "claude-fable-5-2", V).is_empty());

        // 两项各判各的：只关一节。
        note_client_data(id, "claude-fable-5-2", &json!({"memory_guidance": false}));
        assert!(applies(id, "claude-fable-5-2", V, Section::FinishingWork));
        assert!(!applies(id, "claude-fable-5-2[1m]", V, Section::MemoryNotes));
        let ev = client_data_events(id, "claude-fable-5-2", V);
        assert_eq!(ev.len(), 1);
        assert_eq!(
            (ev[0].1, &ev[0].2),
            (
                "tengu_model_capability_off_from_client_data",
                &json!({"capability": "memory_guidance", "model": "claude-fable-5-2"})
            )
        );
        // 与默认一致的不报；不是布尔的按没下发。
        note_client_data(
            id,
            "claude-fable-5-2",
            &json!({"memory_guidance": true, "finishing_work_guidance": "yes"}),
        );
        assert!(applies(id, "claude-fable-5-2", V, Section::MemoryNotes));
        assert!(client_data_events(id, "claude-fable-5-2", V).is_empty());

        // 默认关的模型下发 true 打开。
        note_client_data(id, "claude-opus-5-5", &json!({"finishing_work_guidance": true}));
        assert!(applies(id, "claude-opus-5-5", V, Section::FinishingWork));
        assert!(!applies(id, "claude-opus-5-5", V, Section::MemoryNotes));
        assert!(!applies(id, "claude-opus-5-5", "2.1.319", Section::FinishingWork));
        let ev = client_data_events(id, "claude-opus-5-5", V);
        assert_eq!(ev[0].1, "tengu_model_capability_from_client_data");
        assert!(client_data_events(id, "claude-opus-5-5", "2.1.319").is_empty());

        forget_client_data(id, "claude-opus-5-5");
        assert!(!applies(id, "claude-opus-5-5", V, Section::FinishingWork));
        note_client_data(id, "claude-fable-5-2", &json!({}));
        assert!(served(id, "claude-fable-5-2", Section::MemoryNotes).is_none());
    }

    #[test]
    fn sections_are_removed_and_inserted_in_the_official_order() {
        let fin = Section::FinishingWork.text();
        let mem = Section::MemoryNotes.text();
        let tt = "<total_tokens>15000000 tokens left</total_tokens>";
        let full = format!("intro\n\ngames\n\n{fin}\n\n{mem}\n\n{tt}");
        assert_eq!(fit_system_rest(full.clone(), [true, true]), full);
        assert_eq!(
            fit_system_rest(full.clone(), [false, true]),
            format!("intro\n\ngames\n\n{mem}\n\n{tt}")
        );
        assert_eq!(
            fit_system_rest(full.clone(), [true, false]),
            format!("intro\n\ngames\n\n{fin}\n\n{tt}")
        );
        assert_eq!(
            fit_system_rest(full.clone(), [false, false]),
            format!("intro\n\ngames\n\n{tt}")
        );
        // 去掉再加回来，回到原样。
        for on in [[false, true], [true, false], [false, false]] {
            let cut = fit_system_rest(full.clone(), on);
            assert_eq!(fit_system_rest(cut, [true, true]), full, "{on:?}");
        }
        // 没有这两节的模板：插在 `<total_tokens>` 前；有 `# Writing for the user` 的插在它前面；都没有接在末尾。
        let plain = format!("intro\n\ngames\n\n{tt}");
        assert_eq!(fit_system_rest(plain, [true, true]), full);
        let fable = format!("intro\n\n# Writing for the user\nrules\n\nautonomy\n\n{tt}");
        assert_eq!(
            fit_system_rest(fable, [true, true]),
            format!("intro\n\n{fin}\n\n{mem}\n\n# Writing for the user\nrules\n\nautonomy\n\n{tt}")
        );
        assert_eq!(fit_system_rest("intro".into(), [false, true]), format!("intro\n\n{mem}"));
        // 真模板：fable-5-2 那份关掉两节后不再有这两个标题，别的字节不动。
        let t = crate::config::CC_SYSTEM_REST_FABLE_5_2;
        let off = fit_system_rest(t.to_string(), [false, false]);
        assert!(!off.contains("# Finishing work") && !off.contains("# Memory, notes and feedback"));
        assert_eq!(off.len(), t.len() - fin.len() - mem.len() - 4);
        assert_eq!(fit_system_rest(off, [true, true]), t);
    }
}
