//! 服务端实验 `tengu_lucky_cerf` 按号的分组（2.1.294 起）。
//!
//! 它决定 `Agent` 工具「何时用」那段说明换不换成更长的一版（[`config::cc_agent_uses_cerf_text_in`]），
//! 以及那条实验曝光（`GrowthbookExperimentEvent`）。这是按 `accountUUID` 分组的实验：
//! `cap/auto-2.1.296-20261013-full/00007` 的 eval 里 `variations` 是 `[[], [清单]]`，抓包那个号分在
//! 第 1 组。分组由服务端算、eval 里直接给结果，本地算不出来，只能按号从 eval 读
//! （[`note_eval`]，保活与会话启动握手拉 eval 时顺带记）。
//!
//! **按会话钉住。** 每个会话第一次用到分组时取一份快照（[`session_snapshot`]），之后这个会话的
//! 工具说明与曝光都看这一份，不会一半按旧一半按新。
//!
//! **首条请求确认分组。** 新会话的首条请求先改写出站体、再跑启动握手（握手要拿出站体里的身份），
//! 握手里那次 eval 才是这个会话该用的分组。所以首条请求整形前先占住这个会话（[`begin_first_request`]，
//! 重新按号现在的分组钉一份），握手之后用这次 eval 确认（[`FirstRequest::settle`]）：不论原先钉的是
//! 暂定值还是旧缓存，都换成这次的结果，`Agent` 说明因此变了调用方就在发出前重新整形。同会话这期间
//! 来的别的请求整形前先等确认（[`wait_settled`]）；等超时就照现在钉的那份发、并把快照锁住，首条
//! 那头随后不再改分组——已经发出去的工具说明与曝光始终是同一份。
//!
//! 没见过这个号的 eval（刚加的号、重启后还没保活）时钉的是**暂定**快照，按抓包那个号算：第 1 组、
//! 默认清单（[`config::cc_agent_uses_cerf_text`]）；握手超时没等到 eval 的会话就一直用它。只存在内存里。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config;

/// 实验在模板里的特性名，曝光事件 `experiment_metadata.feature_id` 就是它。
pub(crate) const LUCKY_CERF_FEATURE: &str = "tengu_lucky_cerf";

/// 没见过 eval 时的曝光：`cap/auto-2.1.296-20261013-full` 那个号的实验名与分组。
const DEFAULT_EXPOSURE: (&str, i64) = ("tengu_lucky_cerf_experiment_v3", 1);

/// 会话快照多久没人用就作废：会话本身早就过期了，同一个会话 id 再来（模拟会话的槽位会复用）
/// 就是一场新对话，按号现在的分组重新钉。
const PIN_IDLE: Duration = Duration::from_secs(24 * 3600);

/// 同会话的并发请求最多等首条确认多久：首条自己只等握手 [`config::HANDSHAKE_LEAD_TIMEOUT_MS`]，
/// 再留一秒给它整形与重新整形。
const SETTLE_WAIT: Duration = Duration::from_millis(config::HANDSHAKE_LEAD_TIMEOUT_MS + 1_000);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Assigned {
    /// 这个号拿到的模型清单（`features.tengu_lucky_cerf.value`）；eval 里没有这个开关时为空。
    models: Vec<String>,
    /// 进了实验才有曝光：（实验名, 分组）。值来自强制规则或默认值时官方不发曝光。
    exposure: Option<(String, i64)>,
}

/// 一个会话钉住的分组：`None` 是钉住时还没见过这个号的 eval（按抓包那个号算）。
type Snapshot = Option<Assigned>;

fn uses_cerf(snap: &Snapshot, model: &str) -> bool {
    match snap {
        Some(a) => config::cc_agent_uses_cerf_text_in(model, &a.models),
        None => config::cc_agent_uses_cerf_text(model),
    }
}

static CERF: std::sync::LazyLock<parking_lot::RwLock<HashMap<i64, Assigned>>> =
    std::sync::LazyLock::new(Default::default);

struct Pin {
    /// 最近一次用到的时刻。
    at: Instant,
    snap: Snapshot,
    /// 首条请求正在等握手确认分组：同会话别的请求在这上面等（[`wait_settled`]）。
    pending: Option<Arc<tokio::sync::Notify>>,
    /// 有请求没等到确认就照这份发出去了：首条确认时不再改它。
    locked: bool,
}

impl Pin {
    fn fresh(cred_id: i64, now: Instant) -> Self {
        Pin { at: now, snap: CERF.read().get(&cred_id).cloned(), pending: None, locked: false }
    }
}

/// （号, 会话）→ 钉住的分组。
static PINNED: std::sync::LazyLock<parking_lot::Mutex<HashMap<(i64, String), Pin>>> =
    std::sync::LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的 `tengu_lucky_cerf`。一份 eval 是整份特性缓存，
/// 没有这个键就是没下发（默认值 null：清单为空、无曝光）。已经开始的会话不受影响（钉住的快照）。
pub(super) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let f = features.get(LUCKY_CERF_FEATURE);
    let models = f
        .and_then(|f| f.get("value"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    // 曝光只发给真进了实验的（`experimentResult.inExperiment`），分组取 `variationId`。
    let exposure = f.and_then(|f| {
        let key = f.get("experiment")?.get("key")?.as_str()?;
        let res = f.get("experimentResult")?;
        if res.get("inExperiment").and_then(|b| b.as_bool()) != Some(true) {
            return None;
        }
        Some((key.to_owned(), res.get("variationId")?.as_i64()?))
    });
    CERF.write().insert(cred_id, Assigned { models, exposure });
}

/// 这个会话的分组快照：头一次问（或上次用已过 [`PIN_IDLE`]）时按号现在的分组钉住，之后一直用
/// 这份（顺手丢掉闲太久的）。
fn session_snapshot(cred_id: i64, session: &str) -> Snapshot {
    snapshot_locked(&mut PINNED.lock(), cred_id, session)
}

/// [`session_snapshot`] 的本体，调用方已经拿着 [`PINNED`] 的锁。
fn snapshot_locked(
    pinned: &mut HashMap<(i64, String), Pin>,
    cred_id: i64,
    session: &str,
) -> Snapshot {
    let now = Instant::now();
    if let Some(pin) = pinned.get_mut(&(cred_id, session.to_owned()))
        && now.duration_since(pin.at) < PIN_IDLE
    {
        pin.at = now;
        return pin.snap.clone();
    }
    if pinned.len() >= 1024 {
        pinned.retain(|_, p| now.duration_since(p.at) < PIN_IDLE || p.pending.is_some());
    }
    let pin = Pin::fresh(cred_id, now);
    let snap = pin.snap.clone();
    pinned.insert((cred_id, session.to_owned()), pin);
    snap
}

/// 新会话首条请求（要跑启动握手的那条）整形前调：按号现在的分组重新钉一份，并标成「确认中」，
/// 同会话别的请求先等它（[`wait_settled`]）。拿到的守卫在握手后 [`FirstRequest::settle`]；没走到
/// 那一步（本地拒掉、出错提前返回）就在析构时放行等着的请求，分组保持现在钉的这份。
pub(crate) fn begin_first_request(cred_id: i64, session: &str) -> FirstRequest {
    let notify = Arc::new(tokio::sync::Notify::new());
    let now = Instant::now();
    let mut pinned = PINNED.lock();
    let key = (cred_id, session.to_owned());
    // 同会话别的请求赶在这之前（判定首条与占住之间那一小段）已经照某份整形了：保留那份并锁住，
    // 首条确认时不改。更早留下的（槽位复用的旧对话）照常换成新钉的。
    let mut pin = match pinned.remove(&key) {
        Some(old) if now.duration_since(old.at) < SETTLE_WAIT => Pin { locked: true, ..old },
        _ => Pin::fresh(cred_id, now),
    };
    pin.pending = Some(notify.clone());
    pinned.insert(key, pin);
    FirstRequest { cred_id, session: session.to_owned(), notify }
}

/// 首条请求的「确认中」守卫，见 [`begin_first_request`]。
pub(crate) struct FirstRequest {
    cred_id: i64,
    session: String,
    notify: Arc<tokio::sync::Notify>,
}

impl FirstRequest {
    /// 握手之后、首条请求发出之前调：用这个号现在的分组（握手刚拉的 eval）确认这个会话的快照
    /// ——不论原先钉的是暂定值还是旧缓存，实验名、分组、清单或撤销都照这次的。返回这个模型的
    /// `Agent` 说明是否因此改变，改变了调用方要重新整形。eval 没到、或已有请求没等到确认就照
    /// 原先那份发了（快照锁住），都不改、返回 `false`。
    pub(crate) fn settle(self, model: &str) -> bool {
        let mut pinned = PINNED.lock();
        let Some(pin) = pinned.get_mut(&(self.cred_id, self.session.clone())) else { return false };
        if !pin.pending.as_ref().is_some_and(|n| Arc::ptr_eq(n, &self.notify)) {
            return false;
        }
        let mut changed = false;
        if !pin.locked
            && let Some(real) = CERF.read().get(&self.cred_id).cloned()
        {
            let real = Some(real);
            changed = uses_cerf(&pin.snap, model) != uses_cerf(&real, model);
            pin.snap = real;
        }
        pin.at = Instant::now();
        changed
        // 析构（[`Drop`]）放行等着的请求。
    }
}

impl Drop for FirstRequest {
    fn drop(&mut self) {
        let mut pinned = PINNED.lock();
        if let Some(pin) = pinned.get_mut(&(self.cred_id, self.session.clone()))
            && pin.pending.as_ref().is_some_and(|n| Arc::ptr_eq(n, &self.notify))
        {
            pin.pending = None;
        }
        self.notify.notify_waiters();
    }
}

/// 同会话的其他请求整形前调：首条请求还在确认分组就等它（最多 [`SETTLE_WAIT`]）。等超时就把
/// 快照锁住——这条要照现在钉的那份发出去了，首条那头不能再改。没在确认的立即返回。
pub(crate) async fn wait_settled(cred_id: i64, session: &str) {
    wait_settled_for(cred_id, session, SETTLE_WAIT).await;
}

async fn wait_settled_for(cred_id: i64, session: &str, wait: Duration) {
    let key = (cred_id, session.to_owned());
    // 「没在确认」与「登记这条要用快照了」在同一把锁里做：放行的同时把快照钉上、刷新用到的时刻，
    // 首条随后占住会话时看得见它刚被用过（[`begin_first_request`] 保留并锁住），不会在这条整形
    // 之后再改分组。分两步做就有空档：这边看过没在确认、首条恰好此时占住，这条照暂定的发了，
    // 首条确认时却改成真分组。
    let notify = {
        let mut pinned = PINNED.lock();
        match pinned.get(&key).and_then(|p| p.pending.clone()) {
            Some(n) => n,
            None => {
                snapshot_locked(&mut pinned, cred_id, session);
                return;
            }
        }
    };
    let notified = notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    // 登记好之后再看一眼：首条可能刚好在这之间确认完。
    {
        let mut pinned = PINNED.lock();
        let same = |p: &Pin| p.pending.as_ref().is_some_and(|n| Arc::ptr_eq(n, &notify));
        if !pinned.get(&key).is_some_and(same) {
            snapshot_locked(&mut pinned, cred_id, session);
            return;
        }
    }
    let timed_out = tokio::time::timeout(wait, notified).await.is_err();
    let mut pinned = PINNED.lock();
    if timed_out
        && let Some(pin) = pinned.get_mut(&key)
        && pin.pending.as_ref().is_some_and(|n| Arc::ptr_eq(n, &notify))
    {
        pin.locked = true;
    }
    snapshot_locked(&mut pinned, cred_id, session);
}

/// 这个号、这个会话、这个模型的 `Agent` 说明用不用 `tengu_lucky_cerf` 那一版。
pub(crate) fn agent_uses_cerf_text(cred_id: i64, session: &str, model: &str) -> bool {
    match session_snapshot(cred_id, session) {
        Some(a) => config::cc_agent_uses_cerf_text_in(model, &a.models),
        None => config::cc_agent_uses_cerf_text(model),
    }
}

/// 这个号、这个会话、这个模型该发的那条实验曝光（实验名, 分组）；`None` 即不发。客户端只在
/// 精简提示词的模型上读这个开关（[`config::cc_model_lean_prompt`]），读了且号在实验里才曝光——
/// 不论模型在不在清单里（opus-5-5 的会话同样有）。一个会话只发一次，由遥测那边记。
pub(crate) fn exposure(cred_id: i64, session: &str, model: &str) -> Option<(String, i64)> {
    if !config::cc_model_lean_prompt(model) {
        return None;
    }
    match session_snapshot(cred_id, session) {
        Some(a) => a.exposure,
        None => Some((DEFAULT_EXPOSURE.0.to_owned(), DEFAULT_EXPOSURE.1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(body: &str) -> serde_json::Map<String, serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        v["features"].as_object().unwrap().clone()
    }

    /// `cap/auto-2.1.296-20261013-full/00007` 里那一项（只留用得到的键）。
    const IN_V1: &str = r#"{"features":{"tengu_lucky_cerf":{"value":["claude-opus-5","claude-fable-5",
        "claude-fable-5-1","claude-mythos-5","claude-mythos-5-1","claude-sonnet-5-5","claude-haiku-5-5"],
        "source":"experiment","experiment":{"key":"tengu_lucky_cerf_experiment_v3","variations":[[],[]],
        "hashAttribute":"accountUUID"},"experimentResult":{"inExperiment":true,"variationId":1}}}}"#;

    const S: &str = "sess";

    /// 第 0 组：空清单、进了实验。
    const V0: &str = r#"{"features":{"tengu_lucky_cerf":{"value":[],
        "experiment":{"key":"tengu_lucky_cerf_experiment_v3"},
        "experimentResult":{"inExperiment":true,"variationId":0}}}}"#;

    #[test]
    fn unseen_account_keeps_the_captured_assignment() {
        let id = -91_001;
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"));
        assert!(!agent_uses_cerf_text(id, S, "claude-opus-5-5"));
        assert_eq!(
            exposure(id, S, "claude-opus-5-5"),
            Some(("tengu_lucky_cerf_experiment_v3".into(), 1))
        );
    }

    #[test]
    fn follows_the_accounts_own_eval() {
        let id = -91_002;
        note_eval(id, &features(IN_V1));
        assert!(agent_uses_cerf_text(id, "a", "claude-haiku-5-5"));
        assert!(agent_uses_cerf_text(id, "a", "claude-fable-5-1[1m]"));
        assert!(!agent_uses_cerf_text(id, "a", "claude-opus-5-5"));
        assert_eq!(
            exposure(id, "a", "claude-opus-5-5"),
            Some(("tengu_lucky_cerf_experiment_v3".into(), 1))
        );

        // 第 0 组：空清单，照样曝光（分组 0）——新会话才看到；会话 a 仍是钉住的那份。
        note_eval(
            id,
            &features(
                r#"{"features":{"tengu_lucky_cerf":{"value":[],"source":"experiment",
                "experiment":{"key":"tengu_lucky_cerf_experiment_v3"},
                "experimentResult":{"inExperiment":true,"variationId":0}}}}"#,
            ),
        );
        assert!(!agent_uses_cerf_text(id, "b", "claude-sonnet-5-5"));
        assert_eq!(
            exposure(id, "b", "claude-sonnet-5-5"),
            Some(("tengu_lucky_cerf_experiment_v3".into(), 0))
        );
        assert!(agent_uses_cerf_text(id, "a", "claude-sonnet-5-5"));
        assert_eq!(exposure(id, "a", "claude-sonnet-5-5").map(|e| e.1), Some(1));

        // 这份 eval 不再提它：没下发，不换说明、不曝光。
        note_eval(id, &features(r#"{"features":{"tengu_lantern_spool":{"value":true}}}"#));
        assert!(!agent_uses_cerf_text(id, "c", "claude-sonnet-5-5"));
        assert_eq!(exposure(id, "c", "claude-sonnet-5-5"), None);
    }

    /// 会话钉住之后 eval 才到（握手超时没等到、或保活后来才拉）：这个会话的曝光仍跟工具说明同一份。
    #[test]
    fn eval_landing_mid_session_does_not_split_the_session() {
        let id = -91_005;
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"), "还没 eval：按抓包那组");
        note_eval(
            id,
            &features(
                r#"{"features":{"tengu_lucky_cerf":{"value":[],
                "experiment":{"key":"tengu_lucky_cerf_experiment_v3"},
                "experimentResult":{"inExperiment":true,"variationId":0}}}}"#,
            ),
        );
        assert_eq!(exposure(id, S, "claude-sonnet-5-5").map(|e| e.1), Some(1));
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"));
        assert!(!agent_uses_cerf_text(id, "next", "claude-sonnet-5-5"), "下个会话按 eval");
    }

    /// 首条请求用握手拿到的这次 eval 确认分组：暂定值换成真分组；确认过之后的 eval 不再动它。
    #[test]
    fn first_request_settles_on_the_handshake_eval() {
        let id = -91_006;
        let first = begin_first_request(id, S);
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"), "暂定：按抓包那组");
        note_eval(id, &features(V0));
        assert!(first.settle("claude-sonnet-5-5"), "第 0 组：说明变了");
        assert!(!agent_uses_cerf_text(id, S, "claude-sonnet-5-5"));
        assert_eq!(exposure(id, S, "claude-sonnet-5-5").map(|e| e.1), Some(0));
        note_eval(id, &features(IN_V1));
        assert!(!agent_uses_cerf_text(id, S, "claude-sonnet-5-5"), "确认过：后来的 eval 不动它");

        // eval 没到：不改、不必重新整形。
        let id = -91_009;
        let first = begin_first_request(id, S);
        assert!(!first.settle("claude-sonnet-5-5"));
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"));

        // 分组不改变这个模型的说明（opus-5-5 两组都是老的那版）：换快照，但不必重新整形。
        let id = -91_007;
        let first = begin_first_request(id, S);
        assert!(!agent_uses_cerf_text(id, S, "claude-opus-5-5"));
        note_eval(id, &features(V0));
        assert!(!first.settle("claude-opus-5-5"));
        assert_eq!(exposure(id, S, "claude-opus-5-5").map(|e| e.1), Some(0), "曝光跟真分组");
    }

    /// 号已有旧 eval 缓存：新会话首条先按旧的钉，握手这次 eval 改了分组 / 撤了开关就照这次的。
    #[test]
    fn first_request_takes_this_handshake_over_an_old_cache() {
        let id = -91_010;
        note_eval(id, &features(IN_V1));
        let first = begin_first_request(id, S);
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"), "先按旧缓存的第 1 组");
        note_eval(id, &features(V0));
        assert!(first.settle("claude-sonnet-5-5"));
        assert!(!agent_uses_cerf_text(id, S, "claude-sonnet-5-5"));
        assert_eq!(exposure(id, S, "claude-sonnet-5-5").map(|e| e.1), Some(0));

        // 同一个号的下一个新会话：撤掉开关（eval 里没有这一项）——不换说明、不曝光。
        let first = begin_first_request(id, "next");
        note_eval(id, &features(r#"{"features":{}}"#));
        first.settle("claude-sonnet-5-5");
        assert!(!agent_uses_cerf_text(id, "next", "claude-sonnet-5-5"));
        assert_eq!(exposure(id, "next", "claude-sonnet-5-5"), None);
        assert_eq!(exposure(id, S, "claude-sonnet-5-5").map(|e| e.1), Some(0), "旧会话不受影响");
    }

    /// 首条还在确认时同会话的另一条请求先等：确认完再整形，用的是确认后的那份。
    #[tokio::test]
    async fn a_concurrent_request_waits_for_the_first_to_settle() {
        let id = -91_011;
        let first = begin_first_request(id, S);
        let waiter = tokio::spawn(wait_settled(id, S));
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "首条没确认：等着");
        note_eval(id, &features(V0));
        assert!(first.settle("claude-sonnet-5-5"));
        waiter.await.unwrap();
        assert!(!agent_uses_cerf_text(id, S, "claude-sonnet-5-5"), "等来的是第 0 组");
    }

    /// 等超时的请求照没确认的那份发了：快照锁住，首条随后拿到的 eval 不再改它。
    #[tokio::test]
    async fn a_timed_out_waiter_locks_the_snapshot() {
        let id = -91_012;
        let first = begin_first_request(id, S);
        wait_settled_for(id, S, Duration::from_millis(10)).await;
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"), "超时：照暂定那份发");
        note_eval(id, &features(V0));
        assert!(!first.settle("claude-sonnet-5-5"), "已有请求照暂定发了：不改");
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"));
        assert_eq!(exposure(id, S, "claude-sonnet-5-5").map(|e| e.1), Some(1), "曝光同那份");
    }

    /// 同会话别的请求赶在首条占住之前就钉了快照（整形用了它）：首条保留那份，确认时不改。
    #[test]
    fn a_pin_used_just_before_the_first_request_is_kept() {
        let id = -91_014;
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"), "抢先那条照暂定的发了");
        let first = begin_first_request(id, S);
        note_eval(id, &features(V0));
        assert!(!first.settle("claude-sonnet-5-5"));
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"));
    }

    /// 另一种交错：别的请求先看过没在确认、放行，首条这才占住，那条随后照暂定的整形。放行那一步
    /// 已经登记了用快照，首条占住时保留并锁住，确认时不改——那条发出的说明与之后的曝光一致。
    #[tokio::test]
    async fn a_request_released_just_before_the_first_claims_keeps_its_snapshot() {
        let id = -91_015;
        wait_settled(id, S).await;
        let first = begin_first_request(id, S);
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"), "放行那条照暂定的整形");
        note_eval(id, &features(V0));
        assert!(!first.settle("claude-sonnet-5-5"));
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"));
        assert_eq!(exposure(id, S, "claude-sonnet-5-5").map(|e| e.1), Some(1));
    }

    /// 首条没走到确认（本地拒掉、提前返回）：守卫析构放行等着的请求，分组保持现在这份。
    #[tokio::test]
    async fn dropping_the_first_request_releases_waiters() {
        let id = -91_013;
        let first = begin_first_request(id, S);
        let waiter = tokio::spawn(wait_settled(id, S));
        tokio::task::yield_now().await;
        drop(first);
        waiter.await.unwrap();
        wait_settled(id, S).await;
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"));
    }

    /// 闲置超过 [`PIN_IDLE`] 的快照作废：同一个会话 id 再来时按号现在的分组。
    #[test]
    fn an_idle_pin_is_read_again() {
        let id = -91_008;
        note_eval(id, &features(IN_V1));
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"));
        note_eval(id, &features(V0));
        assert!(agent_uses_cerf_text(id, S, "claude-sonnet-5-5"), "还在用：钉住的那份");
        let stale = Instant::now().checked_sub(PIN_IDLE + Duration::from_secs(1)).unwrap();
        PINNED.lock().get_mut(&(id, S.to_owned())).unwrap().at = stale;
        assert!(!agent_uses_cerf_text(id, S, "claude-sonnet-5-5"), "过期：按现在的第 0 组");
    }

    #[test]
    fn full_prompt_models_never_read_the_flag() {
        let id = -91_003;
        note_eval(id, &features(IN_V1));
        for model in
            ["claude-haiku-4-5-20251001", "claude-sonnet-5", "claude-sonnet-4-6", "claude-opus-4-7"]
        {
            assert_eq!(exposure(id, S, model), None, "{model}");
            assert!(!agent_uses_cerf_text(id, S, model), "{model}");
        }
        assert_eq!(exposure(-91_004, S, "claude-opus-4-7"), None, "没见过 eval 的号同样不发");
    }
}
