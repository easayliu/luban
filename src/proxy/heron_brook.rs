//! 服务端下发的 `tengu_heron_brook` 那段系统提示词：客户端拼 system 时读到非空文本就报一条
//! `tengu_heron_brook_applied {len, fromClientData}`（2.1.308 `nan()`：先看 bootstrap 回包的
//! `client_data.tengu_heron_brook`，再看 eval 的同名特性；都是空就不报）。
//!
//! 抓包里只有 haiku-5-5 的 bootstrap（`?model=claude-haiku-5-5`）带过：`cap/auto-2.1.293-20261008-full` 与
//! `cap/auto-2.1.296-20261013-full` 是 2790 字，`cap/auto-2.1.299-20261014-full` 起直到
//! `cap/auto-2.1.308-20261022-full` 都没有这一项、也没有那条事件。eval 里一直没出现过。
//!
//! bootstrap 按号、按请求的模型记（`client_data` 随 `model` 参数变，会话启动握手与首次保活拉 bootstrap 时顺带记），
//! eval 按号记。长度按官方 `value.trim().length`（UTF-16 码元）。bootstrap 那份随整份 `client_data` 落库、启动时
//! 读回（[`super::client_data`]）；eval 那份只在内存。两处都没见过的一律当没下发。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 特性名（`client_data` 里与 eval 里同名）。
const HERON_BROOK_FEATURE: &str = "tengu_heron_brook";

/// `(号, 模型键)`。
type ClientDataKey = (i64, String);

/// `(号, 模型)` → bootstrap `client_data` 里那段文本（`trim` 过）；下发了但为空、或没有这一项记 `None`。
static CLIENT_DATA: LazyLock<parking_lot::RwLock<HashMap<ClientDataKey, Option<String>>>> =
    LazyLock::new(Default::default);
/// 号 → eval 里那段文本（同上）。
static EVAL: LazyLock<parking_lot::RwLock<HashMap<i64, Option<String>>>> =
    LazyLock::new(Default::default);

/// 模型键：小写、去掉 `[1m]`（bootstrap 的 `model` 参数是规范名，遥测这边是展示名）。
fn model_key(model: &str) -> String {
    let m = model.to_ascii_lowercase();
    m.strip_suffix("[1m]").unwrap_or(&m).to_string()
}

/// 非空文本（`trim` 之后，官方 `vmr` 同样先 `trim` 再判空）。
fn text_of(v: Option<&serde_json::Value>) -> Option<String> {
    let t = v.and_then(|v| v.as_str())?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// 记下一份 bootstrap 回包里的 `client_data`（[`super::client_data`] 拆好交过来，回包里没有这一项时是空对象）。
pub(crate) fn note_client_data(cred_id: i64, model: &str, data: &serde_json::Value) {
    let text = text_of(data.get(HERON_BROOK_FEATURE));
    CLIENT_DATA.write().insert((cred_id, model_key(model)), text);
}

/// 忘掉这个号这个模型的那份（[`super::client_data`] 按号限条数，挤出去的一并清掉）。
pub(crate) fn forget_client_data(cred_id: i64, model: &str) {
    CLIENT_DATA.write().remove(&(cred_id, model_key(model)));
}

/// 从一份 eval 的 `features` 里记下这个号的文本：一份 eval 是整份特性缓存，没有这一项即按没下发记。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let text = text_of(features.get(HERON_BROOK_FEATURE).and_then(|f| f.get("value")));
    EVAL.write().insert(cred_id, text);
}

/// 这个号用 `model` 当主模型时下发的那段正文与来处（`fromClientData`）：先看 bootstrap 的 `client_data`，再看 eval；
/// 两处都没下发是 `None`。
pub(crate) fn served(cred_id: i64, model: &str) -> Option<(String, bool)> {
    let client = CLIENT_DATA.read().get(&(cred_id, model_key(model))).cloned().flatten();
    client
        .map(|t| (t, true))
        .or_else(|| EVAL.read().get(&cred_id).cloned().flatten().map(|t| (t, false)))
}

/// 这个号用 `model` 当主模型时，那条事件该报的 `(len, fromClientData)`（长度按 UTF-16 码元）；两处都没下发是 `None`。
pub(crate) fn applied(cred_id: i64, model: &str) -> Option<(usize, bool)> {
    served(cred_id, model).map(|(t, from)| (t.encode_utf16().count(), from))
}

/// haiku-5-5 那段内置的「提前收手」说明（官方 `Amr`）：`heron_brook` 这一节没有下发时，带
/// `haiku_5_5_early_stopping_guidance` 能力的模型用它（[`crate::config::CC_SYSTEM_REST_HAIKU_5_5`] 里就是这一段）；
/// 下发了就换成下发的正文。取它在模板里的那一段：从「The reasoning effort setting changes」起到下一个空行。
const EARLY_STOPPING_HEAD: &str =
    "The reasoning effort setting changes how much you think before you act.";

/// 第四块末尾那两节（官方 2.1.293 起 system 段序 `…、original_games、heron_brook、…`，都在 `<total_tokens>` 之前）：`games`
/// 是原创游戏那段（[`super::original_games`] 判开了才给），`served` 是下发的 `heron_brook` 正文。`heron_brook` 这一节在
/// 模板里有内置的「提前收手」那段（haiku-5-5，官方 `Amr`）时就占着它的位置：下发了就整段换掉，没下发保留原样；原创游戏
/// 那段总排在这一节前面。模板里没有那段的，两节依次插在官方段序里后面那几节（[`LATER_HEADS`]：fable-5-2 模板的 `# Finishing work`、fable-5-1
/// 模板的 `# Writing for the user`）最靠前的一节前，没有那些节就插在 `<total_tokens>` 前、两段之间空一行，那行也没有就
/// 接在末尾。**先调 [`super::finishing_notes::fit_system_rest`]**（按号去掉 / 补上那两节）再调这里：此时下发的正文还没拼进来，
/// 按标题找到的只会是模板自己的节，下发的正文里碰巧有同样的标题也不会串。
/// `served` 在 2.1.293 之前不算（没有这一节）。两样都没有原样返回。在 [`super::lapis_anchor::fit_system_rest`] 之前调。
pub(crate) fn fit_system_rest(
    rest: String,
    games: Option<&str>,
    served: Option<&str>,
    version: &str,
) -> String {
    let served =
        served.filter(|_| !crate::proxy::parse_version(version).is_some_and(|v| v < (2, 1, 293)));
    if games.is_none() && served.is_none() {
        return rest;
    }
    let mut out = rest;
    if let Some(at) = out.find(EARLY_STOPPING_HEAD) {
        // 那段自己带空行，是模板里这一节的全部：截到后面那几节（[`LATER_HEADS`]）或 `<total_tokens>` 那行之前，都没有就截到末尾。
        let end = later_section(&out[at..]).map_or(out.len(), |n| at + n - 2);
        let builtin = out[at..end].to_string();
        let heron = served.unwrap_or(&builtin);
        let section = match games {
            Some(g) => format!("{g}\n\n{heron}"),
            None => heron.to_string(),
        };
        out.replace_range(at..end, &section);
        return out;
    }
    let section = [games, served].into_iter().flatten().collect::<Vec<_>>().join("\n\n");
    match later_section(&out) {
        Some(at) => out.insert_str(at, &format!("{section}\n\n")),
        None => {
            out.push_str("\n\n");
            out.push_str(&section);
        }
    }
    out
}

/// 官方段序里排在这两节后面、模板里可能有的几节的标题行：`finishing_work`、`notes_and_feedback`（2.1.321，
/// [`super::finishing_notes`]）、`willow_tern`（`# Writing for the user`，fable-5-1 那份模板有）。
const LATER_HEADS: [&str; 3] =
    ["# Finishing work\n", "# Memory, notes and feedback\n", "# Writing for the user\n"];

/// `rest` 里这两节该插在谁前面：[`LATER_HEADS`] 里最靠前的那一节（标题行顶格、前面是空行），都没有就是末尾那行
/// `<total_tokens>`；返回它的起点，一个都没有是 `None`。调用时下发的正文还没拼进来，找到的只会是模板自己的节。
fn later_section(rest: &str) -> Option<usize> {
    let para_start = |at: &usize| rest[..*at].ends_with("\n\n");
    LATER_HEADS
        .iter()
        .filter_map(|h| rest.match_indices(h).map(|(at, _)| at).find(para_start))
        .min()
        .or_else(|| rest.rfind("<total_tokens>").filter(para_start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_what_the_server_delivered() {
        let id = -93_201;
        assert_eq!(applied(id, "claude-haiku-5-5"), None, "没见过 bootstrap / eval 当没下发");
        let text = "x".repeat(2790);
        let boot = serde_json::json!({ "tengu_heron_brook": format!(" {text}\n") });
        note_client_data(id, "claude-haiku-5-5", &boot);
        assert_eq!(applied(id, "claude-haiku-5-5[1m]"), Some((2790, true)), "按 trim 后的长度");
        assert_eq!(applied(id, "claude-opus-5-5"), None, "client_data 按模型记");

        let eval = serde_json::json!({ "tengu_heron_brook": { "value": "多字节🙂" } });
        note_eval(id, eval.as_object().unwrap());
        assert_eq!(applied(id, "claude-opus-5-5"), Some((5, false)), "UTF-16 码元数");
        assert_eq!(applied(id, "claude-haiku-5-5"), Some((2790, true)), "client_data 优先");

        // 2.1.299 起的 bootstrap：没有这一项。
        note_client_data(id, "claude-haiku-5-5", &serde_json::json!({ "heather_vale": true }));
        note_eval(id, &serde_json::Map::new());
        assert_eq!(applied(id, "claude-haiku-5-5"), None);
    }

    /// 2.1.318 抓包那天 opus 的原创游戏说明改由 `heron_brook` 下发：插在原创游戏那段的位置（`<total_tokens>` 前、
    /// 没有那行接末尾）；haiku-5-5 模板里内置的「提前收手」那段（自带空行）整段换掉；原创游戏那段总在这一节前面，
    /// 替换时不能吞掉它；2.1.293 之前没有这一节。
    #[test]
    fn served_text_takes_the_heron_brook_slot() {
        let text = crate::proxy::original_games::TEXT;
        let rest = "…an exhaustive survey\n\n<total_tokens>15000000 tokens left</total_tokens>";
        assert_eq!(
            fit_system_rest(rest.into(), None, Some(text), "2.1.318"),
            format!(
                "…an exhaustive survey\n\n{text}\n\n<total_tokens>15000000 tokens left</total_tokens>"
            )
        );
        assert_eq!(
            fit_system_rest("…an exhaustive survey".into(), None, Some(text), "2.1.318"),
            format!("…an exhaustive survey\n\n{text}")
        );
        assert_eq!(fit_system_rest(rest.into(), None, None, "2.1.318"), rest);
        assert_eq!(
            fit_system_rest(rest.into(), None, Some(text), "2.1.291"),
            rest,
            "2.1.293 之前没有这一节"
        );
        // 两节都有：原创游戏在前、下发的跟在后面；只有原创游戏的与原先的插法一致。
        assert_eq!(
            fit_system_rest(rest.into(), Some(text), Some("served"), "2.1.318"),
            format!(
                "…an exhaustive survey\n\n{text}\n\nserved\n\n<total_tokens>15000000 tokens left</total_tokens>"
            )
        );
        assert_eq!(
            fit_system_rest(rest.into(), Some(text), None, "2.1.318"),
            crate::proxy::original_games::fit_system_rest(rest.into(), true, "2.1.318")
        );

        let haiku = crate::config::CC_SYSTEM_REST_HAIKU_5_5;
        let at = haiku.find(EARLY_STOPPING_HEAD).expect("haiku-5-5 模板带内置那段");
        let tail = &haiku[haiku.find("\n\n<total_tokens>").unwrap()..];
        let builtin = &haiku[at..haiku.len() - tail.len()];
        let out = fit_system_rest(haiku.into(), None, Some("served"), "2.1.318");
        assert_eq!(out, format!("{}served{tail}", &haiku[..at]), "内置那段整段换掉");
        // haiku-5-5 开了原创游戏、又下发了不含它的正文：两段都在，原创游戏在前。
        let out = fit_system_rest(haiku.into(), Some(text), Some("served"), "2.1.318");
        assert_eq!(out, format!("{}{text}\n\nserved{tail}", &haiku[..at]));
        // 只开原创游戏：排在内置那段前面，内置那段原样。
        let out = fit_system_rest(haiku.into(), Some(text), None, "2.1.318");
        assert_eq!(out, format!("{}{text}\n\n{builtin}{tail}", &haiku[..at]));
    }
}
