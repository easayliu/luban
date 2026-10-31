//! 2.1.314 起 opus-5-5 主线程 system 第四块里那段原创游戏的说明（官方能力 `original_games_guidance`）。
//!
//! 官方 2.1.314 的能力判定（`NP`）：这一项在「带 `opus_5_5_prompt_bundle` 的模型」上默认开（同组还有
//! `silent_turn_reminder`、`quizzical_shore`，托管入口——远程、Cowork 这类——不算），内置目录里只有 opus-5-5 带那个
//! bundle；bootstrap 回包的 `client_data` 里这一项为 `false` 时关（官方另报一条
//! `tengu_model_capability_off_from_client_data`，抓包里没有这种下发，不报）；反过来默认关的模型（sonnet 等），`client_data`
//! 显式给 `true` 也会开（另报 `tengu_model_capability_from_client_data`，同样没有抓包，不报）。开着时 system 段序是
//! `act_dont_rederive`、…、`opus5_reduced_delegation`、**`original_games`**、`heron_brook`、…、EndConversation 提示，
//! 之后才是 `total_tokens`；模拟路径的模板不收 EndConversation 那段（[`crate::config::CC_SYSTEM_REST`]），这一段就落在
//! 「…not an exhaustive survey」与 `<total_tokens>` 之间（[`fit_system_rest`]）。
//!
//! `cap/auto-2.1.314-20261030-full` 的 opus 会话第四块比 2.1.312 正好多这 190 字节（交互式 5369→5559，`-p`
//! 4877→5067），sonnet / haiku / fable 不变。2.1.312 的可执行文件里没有这一段。
//!
//! `client_data` 按号、按 bootstrap 的模型记，同 [`super::heron_brook`]；随整份 `client_data` 落库、启动时读回
//! （[`super::client_data`]）。没见过的（号, 模型）、或下发里没有这一项的按模型默认。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 能力名（`client_data` 里同名一项）。
const CAPABILITY: &str = "original_games_guidance";

/// 那一段正文（2.1.314 `Jsn`，逐字取自可执行文件，与抓包一致）。
pub(crate) const TEXT: &str = "Do build original games, games in a genre, and games from the user's own designs and assets. Don't build a recreation of a specific existing game, even if it is renamed or called original.";

/// `(号, 模型键)` → bootstrap `client_data` 里这一项显式下发的布尔值（不是布尔、没下发的不记，按模型默认）。
static CLIENT_DATA: LazyLock<parking_lot::RwLock<HashMap<(i64, String), bool>>> =
    LazyLock::new(Default::default);

/// 模型键：小写、去掉 `[1m]`（bootstrap 的 `model` 参数是规范名）。
fn model_key(model: &str) -> String {
    let m = model.trim().to_ascii_lowercase();
    m.strip_suffix("[1m]").unwrap_or(&m).to_string()
}

/// 记下一份 bootstrap 回包里的 `client_data`（[`super::client_data`] 拆好交过来，回包里没有这一项时是空对象）。
pub(crate) fn note_client_data(cred_id: i64, model: &str, data: &serde_json::Value) {
    let key = (cred_id, model_key(model));
    match data.get(CAPABILITY).and_then(serde_json::Value::as_bool) {
        Some(on) => CLIENT_DATA.write().insert(key, on),
        None => CLIENT_DATA.write().remove(&key),
    };
}

/// 忘掉这个号这个模型的那份（[`super::client_data`] 按号限条数，挤出去的一并清掉）。
pub(crate) fn forget_client_data(cred_id: i64, model: &str) {
    CLIENT_DATA.write().remove(&(cred_id, model_key(model)));
}

/// 这个号拿 `model` 当主线程模型、客户端是 `version` 时，第四块带不带这一段：2.1.314 起，`client_data` 显式下发了就照它，
/// 没下发按模型默认——规范名是 opus-5-5（`[1m]`、日期这类尾巴照认）的开、别的关（官方 `NP`）。读不出版本的按最新那版。
pub(crate) fn applies(cred_id: i64, model: &str, version: &str) -> bool {
    if crate::proxy::parse_version(version).is_some_and(|v| v < (2, 1, 314)) {
        return false;
    }
    served(cred_id, model).unwrap_or_else(|| canonical(model) == "claude-opus-5-5")
}

/// `client_data` 显式下发的那个值，没下发是 `None`。
fn served(cred_id: i64, model: &str) -> Option<bool> {
    CLIENT_DATA.read().get(&(cred_id, model_key(model))).copied()
}

/// 规范名（`[1m]`、日期这类尾巴去掉）。
fn canonical(model: &str) -> String {
    let canon = super::curious_tower::canonical_model(model);
    canon.strip_suffix("[1m]").unwrap_or(&canon).to_string()
}

/// `client_data` 改了这个模型的默认时官方报的那条（2.1.314 `NP` 里的 `y()`）：默认关、下发 `true` 打开报
/// `tengu_model_capability_from_client_data`，默认开、下发 `false` 关掉报 `tengu_model_capability_off_from_client_data`，
/// 内容 `{capability, model}`（`model` 是 `client_data` 那份的规范名）；与默认一致或没下发不报。官方按会话、按
/// 「能力 + 模型」去重，组装 system 时头一回用到才报，调用方按会话报一次。抓包里还没有这种下发。
pub(crate) fn client_data_event(
    cred_id: i64,
    model: &str,
    version: &str,
) -> Option<(&'static str, serde_json::Value)> {
    if crate::proxy::parse_version(version).is_some_and(|v| v < (2, 1, 314)) {
        return None;
    }
    let on = served(cred_id, model)?;
    let model = canonical(model);
    if on == (model == "claude-opus-5-5") {
        return None;
    }
    let name = if on {
        "tengu_model_capability_from_client_data"
    } else {
        "tengu_model_capability_off_from_client_data"
    };
    Some((name, serde_json::json!({ "capability": CAPABILITY, "model": model })))
}

/// 第四块（[`crate::proxy::render_system_rest`] 渲染出来的）插上这一段：模板末尾是 `<total_tokens>` 那行时插在它前面、
/// 两段之间空一行；没有那行就接在末尾。`on` 为假原样返回。先于 [`super::lapis_anchor::fit_system_rest`] 调，后者去掉
/// 那行时连同它前面的空行一起去掉，剩下的正好以这一段收尾（`cap/auto-2.1.314-20261030-full` 的 opus 会话）。
pub(crate) fn fit_system_rest(rest: String, on: bool) -> String {
    if !on {
        return rest;
    }
    let mut out = rest;
    match out.rfind("<total_tokens>").filter(|at| out[..*at].ends_with("\n\n")) {
        Some(at) => out.insert_str(at, &format!("{TEXT}\n\n")),
        None => {
            out.push_str("\n\n");
            out.push_str(TEXT);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_by_default_for_opus_5_5_from_2_1_314_and_client_data_decides_when_given() {
        let id = -93_501;
        for model in ["claude-opus-5-5", "claude-opus-5-5[1m]", "Claude-Opus-5-5"] {
            assert!(applies(id, model, "2.1.314"), "{model}");
            assert!(!applies(id, model, "2.1.312"), "{model}: 2.1.312 没有这一段");
        }
        for model in ["claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1", "claude-opus-5"]
        {
            assert!(!applies(id, model, "2.1.314"), "{model}");
        }
        // `client_data` 只有恰为 `false` 才关。
        note_client_data(id, "claude-opus-5-5", &serde_json::json!({ CAPABILITY: "false" }));
        assert!(applies(id, "claude-opus-5-5", "2.1.314"));
        note_client_data(id, "claude-opus-5-5", &serde_json::json!({ CAPABILITY: false }));
        assert!(!applies(id, "claude-opus-5-5[1m]", "2.1.314"));
        forget_client_data(id, "claude-opus-5-5");
        assert!(applies(id, "claude-opus-5-5", "2.1.314"));
        // 默认关的模型，`client_data` 显式 `true` 打开（官方 `NP`：不在默认集合时看 `data[e] === true`）；只认布尔、
        // 也只认这个号这个模型；2.1.314 之前仍没有这一段；下发里去掉这一项退回默认。
        note_client_data(id, "claude-sonnet-5-5", &serde_json::json!({ CAPABILITY: true }));
        assert!(applies(id, "claude-sonnet-5-5", "2.1.314"));
        assert!(applies(id, "claude-sonnet-5-5[1m]", "2.1.314"));
        assert!(!applies(id, "claude-sonnet-5-5", "2.1.312"));
        assert!(!applies(id, "claude-haiku-5-5", "2.1.314"));
        assert!(!applies(id + 1, "claude-sonnet-5-5", "2.1.314"));
        note_client_data(id, "claude-sonnet-5-5", &serde_json::json!({ CAPABILITY: "true" }));
        assert!(!applies(id, "claude-sonnet-5-5", "2.1.314"));
        note_client_data(id, "claude-sonnet-5-5", &serde_json::json!({ CAPABILITY: true }));
        note_client_data(id, "claude-sonnet-5-5", &serde_json::json!({}));
        assert!(!applies(id, "claude-sonnet-5-5", "2.1.314"));
        note_client_data(id, "claude-opus-5-5", &serde_json::json!({ CAPABILITY: true }));
        assert!(applies(id, "claude-opus-5-5", "2.1.314"));
        forget_client_data(id, "claude-opus-5-5");
    }

    /// 改了默认才报来源事件：sonnet 下发 `true` 报打开那条、opus 下发 `false` 报关掉那条，与默认一致、没下发、2.1.314
    /// 之前都不报；`model` 报规范名。
    #[test]
    fn reports_where_client_data_overrides_the_default() {
        let id = -93_502;
        assert_eq!(client_data_event(id, "claude-sonnet-5-5", "2.1.314"), None, "没下发");
        note_client_data(id, "claude-sonnet-5-5", &serde_json::json!({ CAPABILITY: true }));
        assert_eq!(
            client_data_event(id, "claude-sonnet-5-5[1m]", "2.1.314"),
            Some((
                "tengu_model_capability_from_client_data",
                serde_json::json!({"capability": CAPABILITY, "model": "claude-sonnet-5-5"})
            ))
        );
        assert_eq!(client_data_event(id, "claude-sonnet-5-5", "2.1.312"), None);
        note_client_data(id, "claude-opus-5-5", &serde_json::json!({ CAPABILITY: false }));
        assert_eq!(
            client_data_event(id, "claude-opus-5-5", "2.1.314"),
            Some((
                "tengu_model_capability_off_from_client_data",
                serde_json::json!({"capability": CAPABILITY, "model": "claude-opus-5-5"})
            ))
        );
        note_client_data(id, "claude-opus-5-5", &serde_json::json!({ CAPABILITY: true }));
        assert_eq!(client_data_event(id, "claude-opus-5-5", "2.1.314"), None, "与默认一致");
        note_client_data(id, "claude-sonnet-5-5", &serde_json::json!({ CAPABILITY: false }));
        assert_eq!(client_data_event(id, "claude-sonnet-5-5", "2.1.314"), None, "与默认一致");
        forget_client_data(id, "claude-opus-5-5");
        forget_client_data(id, "claude-sonnet-5-5");
    }

    #[test]
    fn sits_between_the_last_paragraph_and_the_total_tokens_line() {
        let tt = "<total_tokens>15000000 tokens left</total_tokens>";
        let rest = format!("…an exhaustive survey\n\n{tt}");
        assert_eq!(
            fit_system_rest(rest.clone(), true),
            format!("…an exhaustive survey\n\n{TEXT}\n\n{tt}")
        );
        assert_eq!(fit_system_rest(rest.clone(), false), rest);
        // 没有那行（档位已是 off 的模板）就接在末尾。
        assert_eq!(
            fit_system_rest("…an exhaustive survey".into(), true),
            format!("…an exhaustive survey\n\n{TEXT}")
        );
        // 与 `<total_tokens>` 那一档剪裁连用：off 时剩下的以这一段收尾。
        let off = crate::proxy::lapis_anchor::SessionMode {
            mode: crate::proxy::lapis_anchor::Mode::Off,
            ..crate::proxy::lapis_anchor::SessionMode::DEFAULT
        };
        assert_eq!(
            crate::proxy::lapis_anchor::fit_system_rest(fit_system_rest(rest, true), off),
            format!("…an exhaustive survey\n\n{TEXT}")
        );
        assert_eq!(TEXT.len(), 188, "抓包里多出的 190 字节是这一段加前面的空行");
    }
}
