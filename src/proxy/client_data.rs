//! 服务端 bootstrap 回包里的 `client_data`：拆给按它走的几个开关，并按（号, 模型）落库。
//!
//! 官方客户端把 `client_data` 按槽位存在本机配置里（`clientDataCacheSlots`，2.1.311 `xh()`），进程一启动就读得到：
//! `<total_tokens>` 那一族（[`super::lapis_anchor`]）、`tengu_heron_brook`（[`super::heron_brook`]）与 2.1.314 起的
//! `original_games_guidance`（[`super::original_games`]）在这次 bootstrap
//! 回来之前就按上一份定了。luban 原先只记在内存，重启后到这个号这个模型下一次握手之前都是空的——进行中的会话中途
//! 换档（system 末块、首轮环境说明、每轮提醒一起变，整段缓存重写），每个（号, 模型）重启后的第一个新会话也定错。
//! 现在整份落库（`client_data` 表）：回包与上次落库的那份不同才写（[`save_bootstrap`]），启动时读回（[`restore`]）。
//! 同一个号的握手可以并发，登记、挤出删库与写库按号串行（[`CRED_LOCKS`]），免得慢的旧操作后落库盖掉新的。
//!
//! 模型名是来访给的（握手拿请求体里的 `model` 去拉 bootstrap），换着名字开会话就能造出任意多份。官方本机最多存 12 个
//! 槽位（`rM`，槽位还分入口与版本，满了丢最旧的）；luban 只分模型，每个号留最近拉过的 [`MODELS_PER_CRED`] 个，挤出去的
//! 那份内存里几个开关的条目与库里那行一起清掉。拉 bootstrap 用的模型名也先规整（[`bootstrap_model`]），不合规的不往
//! URL 里拼。
//!
//! 只落 bootstrap 那份；eval 那份照旧只在内存（这几个开关现在都只从 bootstrap 下发）。

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
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

/// 号 → 它的落库锁：[`save_bootstrap`] 从记下回包到挤出删库、写库整段拿着。锁不放在槽位里——槽位会被挤掉再建，
/// 新建的槽位若带新锁，挤出前还在库上 await 的旧写入 / 删除就绕过了它（旧值盖新值、删掉刚写的那行）。
/// 号的个数有限，条目不回收。
static CRED_LOCKS: LazyLock<parking_lot::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

fn cred_lock(cred_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    CRED_LOCKS.lock().entry(cred_id).or_default().clone()
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

/// 这个号这个模型有没有记着的 `client_data`（官方 `Iht()`：当前槽位有缓存）。有就不等这次 bootstrap，按缓存组装。
/// 模型名按拉 bootstrap 时那样规整（[`bootstrap_model`]），与落库的槽位同一把键。
pub(crate) fn has_slot(cred_id: i64, model: &str) -> bool {
    let key = fetch_key(cred_id, model).1;
    SLOTS.lock().get(&cred_id).is_some_and(|l| l.iter().any(|s| s.model == key))
}

/// 一个槽位上的那次 bootstrap 拉取（官方 `clientDataFetches` 的值）：进行中（`true` 即回来了，成功失败都算），或已结束
/// （`over`：拉完了，或等它的请求等超时把它放下了，后台照跑）。
enum Fetch {
    Pending(tokio::sync::watch::Receiver<bool>),
    /// 何时结束的：过了 [`OVER_TTL`] 视同没有记录，冷槽位的请求可以再拉一次（官方按进程记，luban 常驻，按时长代替）。
    Over(Instant),
}

/// `over` 记多久：之后同一冷槽位再来请求就重新拉一次。
const OVER_TTL: Duration = Duration::from_secs(60 * 60);

/// 拉取表最多记几条：满了先清过期的 `over`，仍满就清掉全部 `over`（进行中的不动）。
const FETCHES_CAP: usize = 4096;

/// 拉取表与槽位共用的键：模型名按拉 bootstrap 时那样规整（[`bootstrap_model`]，长度与字符集有上限，不合规的落到
/// 同一个默认模型），来访乱填的名字撑不大这张表。
fn fetch_key(cred_id: i64, model: &str) -> (i64, String) {
    (cred_id, model_key(&bootstrap_model(model)))
}

/// 这一条还算不算数：进行中的算，`over` 未过期的算。
fn live(f: &Fetch, now: Instant) -> bool {
    match f {
        Fetch::Pending(_) => true,
        Fetch::Over(at) => now.duration_since(*at) < OVER_TTL,
    }
}

/// 清掉某个槽位的拉取记录（槽位落定了缓存、或被挤出时），见 [`touch`] / [`save_bootstrap`]。
fn forget_fetch(cred_id: i64, model: &str) {
    let key = fetch_key(cred_id, model);
    let mut map = PENDING.lock();
    if matches!(map.get(&key), Some(Fetch::Over(_))) {
        map.remove(&key);
    }
}

/// （号, 模型键）→ 那个槽位的拉取。没有记录与 `over` 不同：没有记录时冷槽位的请求当场发起一次再等（官方 `SIi` 的 `P()`），
/// `over` 时不等也不再拉。
type Fetches = HashMap<(i64, String), Fetch>;

static PENDING: LazyLock<parking_lot::Mutex<Fetches>> = LazyLock::new(Default::default);

/// 一次登记了的 bootstrap 拉取：拉完（或任务半路结束）丢掉它即置完成，槽位记成 `over`。
pub(crate) struct FetchGuard {
    key: (i64, String),
    tx: tokio::sync::watch::Sender<bool>,
}

impl Drop for FetchGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(true);
        let mut map = PENDING.lock();
        // 只动自己那条：等超时已被放下、又有新一次登记了的不动。
        if let Some(Fetch::Pending(rx)) = map.get(&self.key)
            && rx.same_channel(&self.tx.subscribe())
        {
            map.insert(self.key.clone(), Fetch::Over(Instant::now()));
        }
    }
}

/// 要拉这个号 `model` 的 bootstrap 了：槽位上没有进行中的就登记成进行中，返回的守卫随拉取那段走、拉完丢掉；已经有一次
/// 在拉的不再登记（官方 `D7t`：已有进行中的 promise 不覆盖），返回 `None`——等的请求照旧等先登记的那次。
pub(crate) fn begin_fetch(cred_id: i64, model: &str) -> Option<FetchGuard> {
    let key = fetch_key(cred_id, model);
    let now = Instant::now();
    let mut map = PENDING.lock();
    if matches!(map.get(&key), Some(Fetch::Pending(_))) {
        return None;
    }
    if map.len() >= FETCHES_CAP {
        map.retain(|_, f| live(f, now));
        if map.len() >= FETCHES_CAP {
            map.retain(|_, f| matches!(f, Fetch::Pending(_)));
        }
    }
    let (tx, rx) = tokio::sync::watch::channel(false);
    map.insert(key.clone(), Fetch::Pending(rx));
    Some(FetchGuard { key, tx })
}

/// 等超时的那次拉取放下（官方 `_Ii`：标成 `over`）：之后的请求不再等它，拉取本身在后台照跑。
fn give_up(key: &(i64, String), rx: &tokio::sync::watch::Receiver<bool>) {
    let mut map = PENDING.lock();
    if let Some(Fetch::Pending(cur)) = map.get(key)
        && cur.same_channel(rx)
    {
        map.insert(key.clone(), Fetch::Over(Instant::now()));
    }
}

/// 号 → `tengu_deep_shore`（官方首条前等不等这次 `client_data` 拉取，读不到按开）。
static DEEP_SHORE: LazyLock<parking_lot::RwLock<HashMap<i64, bool>>> =
    LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的 `tengu_deep_shore`；没有这一项按开。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let on = features
        .get("tengu_deep_shore")
        .and_then(|f| f.get("value"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    DEEP_SHORE.write().insert(cred_id, on);
}

/// 官方首条前等 `client_data` 的上限（2.1.318 `UWt` 的 `Cf`）。
pub(crate) const WAIT_MS: u64 = 1500;

/// 这次等 `client_data` 的结果，遥测随这条请求报（官方 `UWt`）：等到且槽位有了报 `feature_ok{model_client_data_wait,
/// waited_ms}`，等超时报 `feature_sad{…, timeout}`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientDataWait {
    pub waited_ms: u64,
    pub timed_out: bool,
    /// 等到了且槽位有了缓存（官方 `Iht()`）：只有这时报成功那条。
    pub ready: bool,
}

/// 发出前这个号 `model` 的 `client_data` 要怎么等（官方 2.1.318 `UWt` / `SIi`）。
pub(crate) enum WaitPlan {
    /// 不等：槽位已有缓存（[`has_slot`]）、`tengu_deep_shore` 关了、或那次拉取已经 `over`。
    Skip,
    /// 等槽位上进行中的那次（同一槽位并发的请求等同一次）。
    Wait(tokio::sync::watch::Receiver<bool>),
    /// 冷槽位上什么拉取都没有：调用方当场发起一次 bootstrap（守卫随它走），再等它。
    Start(FetchGuard, tokio::sync::watch::Receiver<bool>),
}

/// 定 [`WaitPlan`]（官方 `SIi`）：槽位已有缓存、或那次已 `over` 不等也不拉；有进行中的就等它；什么都没有就当场发起。
/// `tengu_deep_shore` 不在这里看——官方关掉它只是不等（[`waits`]），拉取照样发起。
pub(crate) fn plan(cred_id: i64, model: &str) -> WaitPlan {
    if has_slot(cred_id, model) {
        return WaitPlan::Skip;
    }
    let key = fetch_key(cred_id, model);
    let now = Instant::now();
    match PENDING.lock().get(&key) {
        Some(Fetch::Pending(rx)) => return WaitPlan::Wait(rx.clone()),
        Some(f @ Fetch::Over(_)) if live(f, now) => return WaitPlan::Skip,
        _ => {}
    }
    match begin_fetch(cred_id, model) {
        Some(guard) => {
            let rx = guard.tx.subscribe();
            WaitPlan::Start(guard, rx)
        }
        // 两次判定之间别人刚登记了一次：等它。
        None => match PENDING.lock().get(&key) {
            Some(Fetch::Pending(rx)) => WaitPlan::Wait(rx.clone()),
            _ => WaitPlan::Skip,
        },
    }
}

/// 这个号发出前等不等 `client_data`（官方 `UWt` 的 `tengu_deep_shore`，读不到按开）：关了只是不等，拉取照发。
pub(crate) fn waits(cred_id: i64) -> bool {
    DEEP_SHORE.read().get(&cred_id).copied().unwrap_or(true)
}

/// 限时等那次拉取回来（[`WAIT_MS`]）。等超时就把它放下（[`give_up`]），之后的请求不再等。等到了但槽位仍是空的（拉取
/// 失败）只报时长不报成功——官方那支也只在 `Iht()` 时才报。
pub(crate) async fn wait(
    cred_id: i64,
    model: &str,
    mut rx: tokio::sync::watch::Receiver<bool>,
) -> ClientDataWait {
    let t0 = Instant::now();
    let timed_out =
        tokio::time::timeout(Duration::from_millis(WAIT_MS), rx.wait_for(|v| *v)).await.is_err();
    let waited_ms = t0.elapsed().as_millis() as u64;
    if timed_out {
        give_up(&fetch_key(cred_id, model), &rx);
    }
    ClientDataWait { waited_ms, timed_out, ready: !timed_out && has_slot(cred_id, model) }
}

/// [`plan`] + [`wait`]，不当场发起拉取（没有记录也不等）：测试与不便发请求的调用方用。
#[cfg(test)]
pub(crate) async fn wait_for(cred_id: i64, model: &str) -> Option<ClientDataWait> {
    match plan(cred_id, model) {
        WaitPlan::Wait(rx) if waits(cred_id) => Some(wait(cred_id, model, rx).await),
        WaitPlan::Skip | WaitPlan::Wait(_) => None,
        WaitPlan::Start(guard, _) => {
            drop(guard);
            PENDING.lock().remove(&fetch_key(cred_id, model));
            None
        }
    }
}

/// `client_data` / 相关 eval 每变一次加一：模拟路径 `detect` 时记下这个数，发出前变了就按新下发重拼第四块
/// （`Simulation::refresh_system_rest`）——bootstrap 赶在 `detect` 之后、发出之前回来的那种不等也要跟上。
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 现在的 [`GENERATION`]。
pub(crate) fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// 下发变了（bootstrap 或 eval）：[`GENERATION`] 加一。
pub(crate) fn bump() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// 交给按 `client_data` 走的那几个开关（只动内存）。
fn apply(cred_id: i64, model: &str, data: &serde_json::Value) {
    super::finishing_notes::note_client_data(cred_id, model, data);
    super::heron_brook::note_client_data(cred_id, model, data);
    super::lapis_anchor::note_client_data(cred_id, model, data);
    super::original_games::note_client_data(cred_id, model, data);
    bump();
}

/// 那几个开关里这个（号, 模型）的条目一并清掉。
fn forget(cred_id: i64, model: &str) {
    super::finishing_notes::forget_client_data(cred_id, model);
    super::heron_brook::forget_client_data(cred_id, model);
    super::lapis_anchor::forget_client_data(cred_id, model);
    super::original_games::forget_client_data(cred_id, model);
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
    drop(slots);
    // 槽位有了缓存：那条 `over` 记录不再有用（[`has_slot`] 已经短路）。
    forget_fetch(cred_id, model);
    (write, evicted)
}

/// 同 [`note_bootstrap`]，并且与上次落库的那份不同就写进 `client_data` 表；这个号记着的模型超过
/// [`MODELS_PER_CRED`] 个就挤掉最久没拉过的那个（内存与库一起）。写失败只告警，下一份回包再试。
///
/// 整段拿着这个号的落库锁（[`CRED_LOCKS`]），内存里的开关、槽位与库按拿到锁的先后一份一份变。不串行的话，库里已是
/// A、旧回包 B 正在写时最新的 A 又到了，比的还是 A 便跳过，B 写完库里就成了旧的；挤出删库与重新写入同理。
pub(crate) async fn save_bootstrap(
    store: &CredentialStore,
    cred_id: i64,
    model: &str,
    body: &[u8],
) {
    let lock = cred_lock(cred_id);
    let _guard = lock.lock().await;
    let Some(data) = note_bootstrap(cred_id, model, body) else { return };
    let key = model_key(model);
    let (write, evicted) = touch(cred_id, &key, &data, Instant::now());
    if !evicted.is_empty() {
        for m in &evicted {
            forget(cred_id, m);
            forget_fetch(cred_id, m);
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
        assert!(!lapis_anchor::session_mode(id, "s-1", "claude-opus-5-5", "2.1.312").reminders());
        assert_eq!(heron_brook::applied(id, "claude-opus-5-5"), Some((2, true)));

        // 回包里没有 `client_data`：空对象，两个开关都按没下发记（覆盖掉上一份）。
        let bare = note_bootstrap(id, "claude-opus-5-5", br#"{"oauth_account":{}}"#);
        assert_eq!(bare, Some(serde_json::json!({})));
        assert_eq!(heron_brook::applied(id, "claude-opus-5-5"), None);
        assert!(lapis_anchor::session_mode(id, "s-2", "claude-opus-5-5", "2.1.312").reminders());

        // 解析不了的体不动旧值。
        note_bootstrap(id, "claude-opus-5-5", body);
        assert_eq!(note_bootstrap(id, "claude-opus-5-5", b"<html>"), None);
        assert!(!lapis_anchor::session_mode(id, "s-3", "claude-opus-5-5", "2.1.312").reminders());
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
        let mode = lapis_anchor::session_mode(id, "s-1", "claude-opus-5-5", "2.1.312");
        assert!(!mode.reminders() && mode.from_client_data);
        assert!(lapis_anchor::session_mode(id, "s-2", "claude-haiku-5-5", "2.1.312").reminders());
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
        assert!(lapis_anchor::session_mode(id, "s-1", "claude-opus-5-5", "2.1.312").reminders());
    }

    /// 同一个号的回包并发落库：按拿到号锁的先后一份一份来，后到的 A 落在先到的 B 之后，库里与内存都是 A。
    #[sqlx::test]
    async fn a_stale_concurrent_bootstrap_does_not_win(pool: sqlx::PgPool) {
        let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
        let id = 93_406;
        sqlx::query(
            "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
             VALUES ($1, 'a', 'r', 0)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        let model = "claude-opus-5-5";
        let a = br#"{"client_data":{"tengu_lapis_anchor":"off"}}"#;
        let b = br#"{"client_data":{}}"#;
        save_bootstrap(&store, id, model, a).await;

        // 占住号锁，相当于前一份正在写；B 先到、A 后到，都排在锁后面。
        let lock = cred_lock(id);
        let guard = lock.lock().await;
        let tb = tokio::spawn({
            let store = store.clone();
            async move { save_bootstrap(&store, id, model, b).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let ta = tokio::spawn({
            let store = store.clone();
            async move { save_bootstrap(&store, id, model, a).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(guard);
        tb.await.unwrap();
        ta.await.unwrap();

        let rows = store.all_client_data().await.unwrap();
        assert_eq!(rows.len(), 1);
        let stored: serde_json::Value = serde_json::from_str(&rows[0].2).unwrap();
        assert_eq!(stored, serde_json::json!({ "tengu_lapis_anchor": "off" }));
        assert_eq!(persisted(id, model), Some(stored));
    }

    /// 挤出与重新拉到同一个模型并发：挤掉 M 的那次（删库）排在前、重新拉到 M 的那次（写库）排在后，库里留着 M，
    /// 内存里记的「已落库」与库一致。
    #[sqlx::test]
    async fn an_eviction_racing_a_reinsert_keeps_the_new_row(pool: sqlx::PgPool) {
        let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
        let id = 93_407;
        sqlx::query(
            "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
             VALUES ($1, 'a', 'r', 0)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        let off = br#"{"client_data":{"tengu_lapis_anchor":"off"}}"#;
        // M 最早拉过，号上正好记满。
        save_bootstrap(&store, id, "claude-m", off).await;
        for i in 1..MODELS_PER_CRED {
            save_bootstrap(&store, id, &format!("claude-other-{i}"), off).await;
        }

        let lock = cred_lock(id);
        let guard = lock.lock().await;
        let evict = tokio::spawn({
            let store = store.clone();
            async move { save_bootstrap(&store, id, "claude-new", off).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let reinsert = tokio::spawn({
            let store = store.clone();
            async move { save_bootstrap(&store, id, "claude-m", off).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(guard);
        evict.await.unwrap();
        reinsert.await.unwrap();

        let models: Vec<String> =
            store.all_client_data().await.unwrap().into_iter().map(|(_, m, _)| m).collect();
        assert!(models.contains(&"claude-m".to_string()), "{models:?}");
        assert!(models.contains(&"claude-new".to_string()), "{models:?}");
        assert_eq!(models.len(), MODELS_PER_CRED);
        assert!(persisted(id, "claude-m").is_some());
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
        let gone = lapis_anchor::session_mode(id, "s-junk", "claude-junk-0", "2.1.312");
        assert!(gone.reminders() && !gone.from_client_data, "内存里那份也清了");
        assert!(
            !lapis_anchor::session_mode(id, "s-opus", "claude-opus-5-5", "2.1.312").reminders()
        );
    }

    /// 官方 2.1.318 `UWt`：槽位没有缓存、又有一次拉取在进行时，组装前最多等 1.5 秒它回来；同一槽位并发的几条等同一次
    /// （放行信号约 274ms 先到、bootstrap 约 651ms 才回来那种也等到）；槽位有缓存、`tengu_deep_shore` 关了、没有进行中的
    /// 拉取都不等；等到了但槽位仍空（拉取失败）不算就绪。
    #[tokio::test]
    async fn waits_only_for_a_cold_slot_with_a_fetch_in_flight() {
        let ms = Duration::from_millis;
        let id = -931_801;
        let model = "claude-haiku-5-5";
        assert_eq!(wait_for(id, model).await, None, "没有进行中的拉取不等");

        let guard = begin_fetch(id, model).unwrap();
        let finish = tokio::spawn(async move {
            tokio::time::sleep(ms(377)).await;
            touch(id, &model_key(model), &serde_json::json!({}), Instant::now());
            drop(guard);
        });
        let t0 = Instant::now();
        let (a, b) = tokio::join!(wait_for(id, model), wait_for(id, "Claude-Haiku-5-5[1m]"));
        finish.await.unwrap();
        for w in [a, b] {
            let w = w.expect("冷槽位、有拉取在进行：等");
            assert!(!w.timed_out && w.ready, "{w:?}");
            assert!(w.waited_ms >= 300, "{w:?}");
        }
        assert!(t0.elapsed() < ms(WAIT_MS), "两条等的是同一次，一起放行");
        assert_eq!(wait_for(id, model).await, None, "槽位有了缓存就不等");

        // 拉取失败：等到了，槽位仍空，只报时长。
        let other = "claude-opus-5-5";
        let guard = begin_fetch(id, other).unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(ms(50)).await;
            drop(guard);
        });
        let w = wait_for(id, other).await.unwrap();
        assert!(!w.timed_out && !w.ready, "{w:?}");

        assert_eq!(wait_for(id, other).await, None, "拉完了（over）不再等，也不再拉");

        // `tengu_deep_shore` 关了不等。
        let _g = begin_fetch(id, "claude-sonnet-5-5").unwrap();
        let off = serde_json::json!({"tengu_deep_shore": {"value": false}});
        note_eval(id, off.as_object().unwrap());
        assert_eq!(wait_for(id, "claude-sonnet-5-5").await, None);
        note_eval(id, &serde_json::Map::new());

        // 一直不回来：1.5 秒后照发，并把它放下——下一条不再等（官方 `_Ii` 标 over），拉取本身还在跑。
        let t1 = Instant::now();
        let w = wait_for(id, "claude-sonnet-5-5").await.unwrap();
        assert!(w.timed_out && !w.ready && t1.elapsed() >= ms(WAIT_MS), "{w:?}");
        let t2 = Instant::now();
        assert_eq!(wait_for(id, "claude-sonnet-5-5").await, None);
        assert!(t2.elapsed() < ms(100), "超时放下之后不再各等 1.5 秒");
    }

    /// 同一槽位已经有一次在拉：后来的不覆盖登记（官方 `D7t`），后来那次先结束也不摘掉先登记的那次；冷槽位上什么都没有时
    /// 由请求当场发起（[`plan`] 给 `Start`）。
    #[test]
    fn a_later_fetch_does_not_override_the_registered_one() {
        let id = -931_802;
        let model = "claude-opus-5-5";
        let first = begin_fetch(id, model).expect("先来的登记");
        assert!(begin_fetch(id, model).is_none(), "已有进行中的不再登记");
        assert!(matches!(plan(id, model), WaitPlan::Wait(_)), "等先登记的那次");
        drop(first);
        assert!(matches!(plan(id, model), WaitPlan::Skip), "拉完了（槽位仍空）不等也不再拉");
        // 新的一次（新会话握手）可以重新登记。
        let again = begin_fetch(id, model).expect("over 之后可以再登记");
        drop(again);

        let cold = "claude-fable-5-1";
        match plan(id, cold) {
            WaitPlan::Start(guard, rx) => {
                assert!(!*rx.borrow());
                assert!(matches!(plan(id, cold), WaitPlan::Wait(_)), "当场发起的那次同样登记");
                drop(guard);
                assert!(*rx.borrow(), "回来即放行");
            }
            _ => panic!("冷槽位、什么都没有：当场发起"),
        }
    }

    /// `tengu_deep_shore` 关了只是不等：冷槽位上什么都没有时照样当场发起（官方 `SIi` 不看它）。拉取表的键按 bootstrap
    /// 那样规整，乱填的模型名落到同一个默认模型；槽位落定缓存后那条 `over` 清掉；`over` 过期视同没有记录。
    #[test]
    fn fetch_table_is_keyed_bounded_and_independent_of_deep_shore() {
        let id = -931_803;
        let off = serde_json::json!({"tengu_deep_shore": {"value": false}});
        note_eval(id, off.as_object().unwrap());
        assert!(!waits(id));
        match plan(id, "claude-haiku-5-5") {
            WaitPlan::Start(guard, _) => drop(guard),
            _ => panic!("关了也照样发起拉取"),
        }
        assert!(matches!(plan(id, "claude-haiku-5-5"), WaitPlan::Skip), "拉完 over");
        note_eval(id, &serde_json::Map::new());

        // 乱填的名字（带 `&`、超长）都落到同一把键，撑不大表。
        let junk_a = format!("x&{}", "a".repeat(200));
        let junk_b = "bad model name";
        assert_eq!(fetch_key(id, &junk_a), fetch_key(id, junk_b));
        assert_eq!(fetch_key(id, "Claude-Opus-5-5[1m]"), fetch_key(id, "claude-opus-5-5"));

        // 槽位落定缓存：那条 over 清掉。
        let model = "claude-haiku-5-5";
        assert!(PENDING.lock().contains_key(&fetch_key(id, model)));
        touch(id, &model_key(model), &serde_json::json!({}), Instant::now());
        assert!(!PENDING.lock().contains_key(&fetch_key(id, model)));

        // over 过期：视同没有记录，冷槽位可以再拉。
        let stale = "claude-fable-5-1";
        PENDING.lock().insert(fetch_key(id, stale), Fetch::Over(Instant::now() - OVER_TTL));
        assert!(matches!(plan(id, stale), WaitPlan::Start(..)), "过期的 over 不挡重拉");
    }
}
