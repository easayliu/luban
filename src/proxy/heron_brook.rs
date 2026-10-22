//! 服务端下发的 `tengu_heron_brook` 那段系统提示词：客户端拼 system 时读到非空文本就报一条
//! `tengu_heron_brook_applied {len, fromClientData}`（2.1.308 `nan()`：先看 bootstrap 回包的
//! `client_data.tengu_heron_brook`，再看 eval 的同名特性；都是空就不报）。
//!
//! 抓包里只有 haiku-5-5 的 bootstrap（`?model=claude-haiku-5-5`）带过：`cap/auto-2.1.293-20261008-full` 与
//! `cap/auto-2.1.296-20261013-full` 是 2790 字，`cap/auto-2.1.299-20261014-full` 起直到
//! `cap/auto-2.1.308-20261022-full` 都没有这一项、也没有那条事件。eval 里一直没出现过。
//!
//! bootstrap 按号、按请求的模型记（`client_data` 随 `model` 参数变，会话启动握手与首次保活拉 bootstrap 时顺带记），
//! eval 按号记。长度按官方 `value.trim().length`（UTF-16 码元）。只存在内存里；没见过的一律当没下发。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 特性名（`client_data` 里与 eval 里同名）。
const HERON_BROOK_FEATURE: &str = "tengu_heron_brook";

/// `(号, 模型键)`。
type ClientDataKey = (i64, String);

/// `(号, 模型)` → bootstrap `client_data` 里那段文本的长度；下发了但为空、或没有这一项记 `None`。
static CLIENT_DATA: LazyLock<parking_lot::RwLock<HashMap<ClientDataKey, Option<usize>>>> =
    LazyLock::new(Default::default);
/// 号 → eval 里那段文本的长度（同上）。
static EVAL: LazyLock<parking_lot::RwLock<HashMap<i64, Option<usize>>>> =
    LazyLock::new(Default::default);

/// 模型键：小写、去掉 `[1m]`（bootstrap 的 `model` 参数是规范名，遥测这边是展示名）。
fn model_key(model: &str) -> String {
    let m = model.to_ascii_lowercase();
    m.strip_suffix("[1m]").unwrap_or(&m).to_string()
}

/// 非空文本的长度（`trim` 之后按 UTF-16 码元数）。
fn text_len(v: Option<&serde_json::Value>) -> Option<usize> {
    let t = v.and_then(|v| v.as_str())?.trim();
    (!t.is_empty()).then(|| t.encode_utf16().count())
}

/// 记下一份 bootstrap 回包（`GET /api/claude_cli/bootstrap?model=<model>` 的 200 体）。解析不了的体不动旧值。
pub(crate) fn note_bootstrap(cred_id: i64, model: &str, body: &[u8]) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else { return };
    let len = text_len(v.get("client_data").and_then(|c| c.get(HERON_BROOK_FEATURE)));
    CLIENT_DATA.write().insert((cred_id, model_key(model)), len);
}

/// 从一份 eval 的 `features` 里记下这个号的文本：一份 eval 是整份特性缓存，没有这一项即按没下发记。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let len = text_len(features.get(HERON_BROOK_FEATURE).and_then(|f| f.get("value")));
    EVAL.write().insert(cred_id, len);
}

/// 这个号用 `model` 当主模型时，那条事件该报的 `(len, fromClientData)`；两处都没下发是 `None`。
pub(crate) fn applied(cred_id: i64, model: &str) -> Option<(usize, bool)> {
    let client = CLIENT_DATA.read().get(&(cred_id, model_key(model))).copied().flatten();
    client
        .map(|n| (n, true))
        .or_else(|| EVAL.read().get(&cred_id).copied().flatten().map(|n| (n, false)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_what_the_server_delivered() {
        let id = -93_201;
        assert_eq!(applied(id, "claude-haiku-5-5"), None, "没见过 bootstrap / eval 当没下发");
        let text = "x".repeat(2790);
        let boot =
            serde_json::json!({ "client_data": { "tengu_heron_brook": format!(" {text}\n") } });
        note_bootstrap(id, "claude-haiku-5-5", boot.to_string().as_bytes());
        assert_eq!(applied(id, "claude-haiku-5-5[1m]"), Some((2790, true)), "按 trim 后的长度");
        assert_eq!(applied(id, "claude-opus-5-5"), None, "client_data 按模型记");

        let eval = serde_json::json!({ "tengu_heron_brook": { "value": "多字节🙂" } });
        note_eval(id, eval.as_object().unwrap());
        assert_eq!(applied(id, "claude-opus-5-5"), Some((5, false)), "UTF-16 码元数");
        assert_eq!(applied(id, "claude-haiku-5-5"), Some((2790, true)), "client_data 优先");

        // 2.1.299 起的 bootstrap：没有这一项。
        let boot = serde_json::json!({ "client_data": { "heather_vale": true } });
        note_bootstrap(id, "claude-haiku-5-5", boot.to_string().as_bytes());
        note_eval(id, &serde_json::Map::new());
        assert_eq!(applied(id, "claude-haiku-5-5"), None);
    }
}
