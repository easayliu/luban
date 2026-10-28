//! 服务端 bootstrap 回包里的 `client_data`：拆给按它走的几个开关，并按（号, 模型）落库。
//!
//! 官方客户端把 `client_data` 按槽位存在本机配置里（`clientDataCacheSlots`，2.1.311 `xh()`），进程一启动就读得到：
//! `<total_tokens>` 那一族（[`super::lapis_anchor`]）与 `tengu_heron_brook`（[`super::heron_brook`]）在这次 bootstrap
//! 回来之前就按上一份定了。luban 原先只记在内存，重启后到这个号这个模型下一次握手之前都是空的——进行中的会话中途
//! 换档（system 末块、首轮环境说明、每轮提醒一起变，整段缓存重写），每个（号, 模型）重启后的第一个新会话也定错。
//! 现在整份落库（`client_data` 表）：回包与上次落库的那份不同才写（[`save_bootstrap`]），启动时读回（[`restore`]）。
//!
//! 模型名是来访给的（握手拿请求体里的 `model` 去拉 bootstrap），换着名字开会话就能造出任意多份。官方本机最多存 12 个
//! 槽位（`rM`，槽位还分入口与版本，满了丢最旧的）；luban 只分模型，每个号留最近拉过的 [`MODELS_PER_CRED`] 个，挤出去的
//! 那份内存里几个开关的条目与库里那行一起清掉。拉 bootstrap 用的模型名也先规整（[`bootstrap_model`]），不合规的不往
//! URL 里拼。
//!
//! 只落 bootstrap 那份；eval 那份照旧只在内存（这几个开关现在都只从 bootstrap 下发）。

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crate::store::CredentialStore;

/// 每个号最多记几个模型的 `client_data`（内存与库同步），见模块说明。
const MODELS_PER_CRED: usize = 16;

/// 握手没拿到合规的模型名时拉 bootstrap 用的那个：同保活没有真实会话时的默认模型（`oauth::KeepaliveCtx`）。
const BOOTSTRAP_FALLBACK_MODEL: &str = "claude-sonnet-5";

/// 一个号记着的一个模型：上次落库的那份（回包与它相同就不写），与上回拉到它的时刻（按它挤最旧的）。
struct Slot {
    model: String,
    persisted: Option<serde_json::Value>,
    seen: Instant,
}

/// 号 → 它记着的那几个模型（至多 [`MODELS_PER_CRED`] 个）。
static SLOTS: LazyLock<parking_lot::Mutex<HashMap<i64, Vec<Slot>>>> =
    LazyLock::new(Default::default);

/// 模型键：小写、去掉 `[1m]`（bootstrap 的 `model` 参数是规范名）。
fn model_key(model: &str) -> String {
    let m = model.trim().to_ascii_lowercase();
    m.strip_suffix("[1m]").unwrap_or(&m).to_string()
}

/// 拉 bootstrap 时 `?model=` 填什么：规范名（小写、去掉 `[1m]`），只许字母数字与 `.` `_` `-`、字母数字打头、
/// 不超过 64 个字符——官方的模型名都在这个范围里，拼进 URL 不用转义。不合规的（来访乱填、带 `&` `#` 空白之类）
/// 换成 [`BOOTSTRAP_FALLBACK_MODEL`]：上游看到的仍是一条正常的 bootstrap，也不会按这个名字记一份。
pub(crate) fn bootstrap_model(model: &str) -> Cow<'static, str> {
    let key = model_key(model);
    let ok = key.len() <= 64
        && key.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok { Cow::Owned(key) } else { Cow::Borrowed(BOOTSTRAP_FALLBACK_MODEL) }
}

/// 交给按 `client_data` 走的那几个开关（只动内存）。
fn apply(cred_id: i64, model: &str, data: &serde_json::Value) {
    super::heron_brook::note_client_data(cred_id, model, data);
    super::lapis_anchor::note_client_data(cred_id, model, data);
}

/// 那几个开关里这个（号, 模型）的条目一并清掉。
fn forget(cred_id: i64, model: &str) {
    super::heron_brook::forget_client_data(cred_id, model);
    super::lapis_anchor::forget_client_data(cred_id, model);
}

/// 记下一份 bootstrap 回包（`GET /api/claude_cli/bootstrap?model=<model>` 的 200 体）里的 `client_data`（只动内存），
/// 返回拆出来的那份：回包里没有这一项（或不是对象）时是空对象，各开关都按没下发记。解析不了的体什么都不动、
/// 返回 `None`。
pub(crate) fn note_bootstrap(cred_id: i64, model: &str, body: &[u8]) -> Option<serde_json::Value> {
    let v = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let data = v
        .get("client_data")
        .filter(|d| d.is_object())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    apply(cred_id, model, &data);
    Some(data)
}

/// 登记这个号刚拉到 `model` 的那份：返回要不要落库（与上次落库的不同、或还没落过），以及挤出去的那几个模型。
fn touch(cred_id: i64, model: &str, data: &serde_json::Value, now: Instant) -> (bool, Vec<String>) {
    let mut slots = SLOTS.lock();
    let list = slots.entry(cred_id).or_default();
    let write = match list.iter_mut().find(|s| s.model == model) {
        Some(s) => {
            s.seen = now;
            s.persisted.as_ref() != Some(data)
        }
        None => {
            list.push(Slot { model: model.to_string(), persisted: None, seen: now });
            true
        }
    };
    let mut evicted = Vec::new();
    while list.len() > MODELS_PER_CRED {
        let Some(oldest) = list.iter().enumerate().min_by_key(|(_, s)| s.seen).map(|(i, _)| i)
        else {
            break;
        };
        evicted.push(list.swap_remove(oldest).model);
    }
    (write, evicted)
}

/// 同 [`note_bootstrap`]，并且与上次落库的那份不同就写进 `client_data` 表；这个号记着的模型超过
/// [`MODELS_PER_CRED`] 个就挤掉最久没拉过的那个（内存与库一起）。写失败只告警，下一份回包再试。
pub(crate) async fn save_bootstrap(
    store: &CredentialStore,
    cred_id: i64,
    model: &str,
    body: &[u8],
) {
    let Some(data) = note_bootstrap(cred_id, model, body) else { return };
    let key = model_key(model);
    let (write, evicted) = touch(cred_id, &key, &data, Instant::now());
    if !evicted.is_empty() {
        for m in &evicted {
            forget(cred_id, m);
        }
        if let Err(e) = store.delete_client_data(cred_id, &evicted).await {
            tracing::warn!(cred_id, error = %e, "failed to drop evicted bootstrap client_data");
        }
    }
    if !write {
        return;
    }
    match store.save_client_data(cred_id, &key, &data.to_string()).await {
        Ok(()) => {
            let mut slots = SLOTS.lock();
            if let Some(s) =
                slots.get_mut(&cred_id).and_then(|l| l.iter_mut().find(|s| s.model == key))
            {
                s.persisted = Some(data);
            }
        }
        Err(e) => {
            tracing::warn!(cred_id, model = %key, error = %e, "failed to persist bootstrap client_data")
        }
    }
}

/// 启动时（开始转发之前）把 `client_data` 表读回内存，返回读回的条数。每个号只读最近更新的 [`MODELS_PER_CRED`] 个
/// 模型，多出来的（限条数之前留下的）顺手从库里删掉。
pub(crate) async fn restore(store: &CredentialStore) -> anyhow::Result<usize> {
    let (n, overflow) = restore_rows(store.all_client_data().await?);
    let mut by_cred: HashMap<i64, Vec<String>> = HashMap::new();
    for (cred_id, model) in overflow {
        by_cred.entry(cred_id).or_default().push(model);
    }
    for (cred_id, models) in by_cred {
        store.delete_client_data(cred_id, &models).await?;
    }
    Ok(n)
}

/// [`restore`] 的本体：行按号、按最近更新在前排好（[`CredentialStore::all_client_data`]），逐条交给各开关并登记成
/// 「已落库」，越靠后越早拉过。返回读回的条数与超出每号上限、该删的那几行。JSON 坏掉的那条跳过。
fn restore_rows(rows: Vec<(i64, String, String)>) -> (usize, Vec<(i64, String)>) {
    let now = Instant::now();
    let mut n = 0;
    let mut overflow = Vec::new();
    let mut slots = SLOTS.lock();
    for (cred_id, model, text) in rows {
        let list = slots.entry(cred_id).or_default();
        if list.len() >= MODELS_PER_CRED {
            overflow.push((cred_id, model));
            continue;
        }
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) else {
            tracing::warn!(cred_id, %model, "skipping an unreadable stored client_data");
            continue;
        };
        apply(cred_id, &model, &data);
        let key = model_key(&model);
        let seen = now.checked_sub(Duration::from_millis(list.len() as u64)).unwrap_or(now);
        match list.iter_mut().find(|s| s.model == key) {
            Some(s) => s.persisted = Some(data),
            None => list.push(Slot { model: key, persisted: Some(data), seen }),
        }
        n += 1;
    }
    (n, overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{heron_brook, lapis_anchor};

    fn persisted(cred_id: i64, model: &str) -> Option<serde_json::Value> {
        SLOTS
            .lock()
            .get(&cred_id)
            .and_then(|l| l.iter().find(|s| s.model == model))
            .and_then(|s| s.persisted.clone())
    }

    /// 回包里的 `client_data` 拆给两个开关；没有这一项按没下发记；解析不了的体什么都不动。
    #[test]
    fn a_bootstrap_body_feeds_both_switches() {
        let id = -93_401;
        let body = br#"{"client_data":{"cedar_basin":"2027-08-31","tengu_lapis_anchor":"off",
            "tengu_heron_brook":"  hi  "}}"#;
        let data = note_bootstrap(id, "claude-opus-5-5", body).unwrap();
        assert_eq!(data["tengu_lapis_anchor"], "off");
        assert!(!lapis_anchor::session_mode(id, "s-1", "claude-opus-5-5").reminders());
        assert_eq!(heron_brook::applied(id, "claude-opus-5-5"), Some((2, true)));

        // 回包里没有 `client_data`：空对象，两个开关都按没下发记（覆盖掉上一份）。
        let bare = note_bootstrap(id, "claude-opus-5-5", br#"{"oauth_account":{}}"#);
        assert_eq!(bare, Some(serde_json::json!({})));
        assert_eq!(heron_brook::applied(id, "claude-opus-5-5"), None);
        assert!(lapis_anchor::session_mode(id, "s-2", "claude-opus-5-5").reminders());

        // 解析不了的体不动旧值。
        note_bootstrap(id, "claude-opus-5-5", body);
        assert_eq!(note_bootstrap(id, "claude-opus-5-5", b"<html>"), None);
        assert!(!lapis_anchor::session_mode(id, "s-3", "claude-opus-5-5").reminders());
    }

    /// 启动时读回的那份与刚拉到的一样算数：重启前定成 off 的号，重启后的会话（含进行中会话的下一条）照样 off；
    /// 读回的记成已落库，同一份回包再来不重写。每个号只读最近的那几个，多出来的交出去删。
    #[test]
    fn restored_rows_count_like_a_fresh_bootstrap() {
        let id = -93_402;
        let row = |model: &str, data: &str| (id, model.to_string(), data.to_string());
        let rows = vec![
            row("claude-opus-5-5", r#"{"tengu_lapis_anchor":"off"}"#),
            row("claude-haiku-5-5", "{}"),
            row("claude-sonnet-5-5", "not json"),
        ];
        assert_eq!(restore_rows(rows), (2, vec![]), "坏掉的那条跳过");
        let mode = lapis_anchor::session_mode(id, "s-1", "claude-opus-5-5");
        assert!(!mode.reminders() && mode.from_client_data);
        assert!(lapis_anchor::session_mode(id, "s-2", "claude-haiku-5-5").reminders());
        assert!(persisted(id, "claude-opus-5-5").is_some());
        assert!(persisted(id, "claude-sonnet-5-5").is_none());

        let other = -93_404;
        let rows: Vec<_> = (0..MODELS_PER_CRED + 2)
            .map(|i| (other, format!("claude-test-{i}"), "{}".to_string()))
            .collect();
        let (n, overflow) = restore_rows(rows);
        assert_eq!(n, MODELS_PER_CRED);
        let want: Vec<_> = (MODELS_PER_CRED..MODELS_PER_CRED + 2)
            .map(|i| (other, format!("claude-test-{i}")))
            .collect();
        assert_eq!(overflow, want, "排在后面（更早更新）的那几行删掉");
    }

    /// `?model=` 只拼规整过的名字：规范名照用，带 `[1m]` 的去掉、大写转小写；乱填的换成默认那个。
    #[test]
    fn bootstrap_model_is_canonical_and_url_safe() {
        assert_eq!(bootstrap_model("claude-opus-5-5"), "claude-opus-5-5");
        assert_eq!(bootstrap_model(" Claude-Opus-5-5[1m] "), "claude-opus-5-5");
        assert_eq!(bootstrap_model("claude-haiku-4-5-20251001"), "claude-haiku-4-5-20251001");
        for bad in ["", "claude-opus-5-5&entrypoint=sdk-ts", "a b", "#x", "-opus", "模型"] {
            assert_eq!(bootstrap_model(bad), BOOTSTRAP_FALLBACK_MODEL, "{bad:?}");
        }
        assert_eq!(bootstrap_model(&"x".repeat(65)), BOOTSTRAP_FALLBACK_MODEL);
    }

    /// 回包与上次落库的那份不同才写：同一份再来不写，服务端撤了那一项就写成新的。
    #[sqlx::test]
    async fn only_a_changed_bootstrap_is_written(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool.clone()).await;
        // 号 id 显式给一个不与别的用例撞的：几个开关的内存表是进程级的。
        let id = 93_403;
        sqlx::query(
            "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
             VALUES ($1, 'a', 'r', 0)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        let stored = || async {
            let rows = store.all_client_data().await.unwrap();
            rows.into_iter()
                .map(|(cred, model, text)| {
                    (cred, model, serde_json::from_str::<serde_json::Value>(&text).unwrap())
                })
                .collect::<Vec<_>>()
        };
        let off = br#"{"client_data":{"cedar_basin":"2027-08-31","tengu_lapis_anchor":"off"}}"#;
        save_bootstrap(&store, id, "claude-opus-5-5", off).await;
        let want = serde_json::json!({ "cedar_basin": "2027-08-31", "tengu_lapis_anchor": "off" });
        assert_eq!(stored().await, vec![(id, "claude-opus-5-5".to_string(), want)]);

        // 同一份再来不写：库里那条删掉之后再来一遍，也不会补回来。
        sqlx::query("DELETE FROM client_data").execute(&pool).await.unwrap();
        save_bootstrap(&store, id, "claude-opus-5-5", off).await;
        assert!(stored().await.is_empty());

        let plain = br#"{"client_data":{"cedar_basin":"2027-08-31"}}"#;
        save_bootstrap(&store, id, "claude-opus-5-5", plain).await;
        let want = serde_json::json!({ "cedar_basin": "2027-08-31" });
        assert_eq!(stored().await, vec![(id, "claude-opus-5-5".to_string(), want)]);
        assert!(lapis_anchor::session_mode(id, "s-1", "claude-opus-5-5").reminders());
    }

    /// 一个号换着模型名拉 bootstrap：只留最近拉过的 [`MODELS_PER_CRED`] 个，最久没拉过的那个连同内存里开关的条目
    /// 与库里那行一起清掉；常用的那个一直在拉，不会被挤出去。
    #[sqlx::test]
    async fn each_account_keeps_only_its_recent_models(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool.clone()).await;
        let id = 93_405;
        sqlx::query(
            "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
             VALUES ($1, 'a', 'r', 0)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        let off = br#"{"client_data":{"tengu_lapis_anchor":"off"}}"#;
        save_bootstrap(&store, id, "claude-junk-0", off).await;
        save_bootstrap(&store, id, "claude-opus-5-5", off).await;
        for i in 1..MODELS_PER_CRED {
            save_bootstrap(&store, id, &format!("claude-junk-{i}"), off).await;
            // 常用的那个夹在中间一直有人拉。
            save_bootstrap(&store, id, "claude-opus-5-5", off).await;
        }
        let models = || async {
            let mut m: Vec<String> =
                store.all_client_data().await.unwrap().into_iter().map(|(_, m, _)| m).collect();
            m.sort();
            m
        };
        let kept = models().await;
        assert_eq!(kept.len(), MODELS_PER_CRED);
        assert!(!kept.contains(&"claude-junk-0".to_string()), "最久没拉过的挤掉: {kept:?}");
        assert!(kept.contains(&"claude-opus-5-5".to_string()));
        assert!(persisted(id, "claude-junk-0").is_none());
        let gone = lapis_anchor::session_mode(id, "s-junk", "claude-junk-0");
        assert!(gone.reminders() && !gone.from_client_data, "内存里那份也清了");
        assert!(!lapis_anchor::session_mode(id, "s-opus", "claude-opus-5-5").reminders());
    }
}
