//! 缓存保活（[`config::CC_BETA_CACHE_KEEPALIVE`]）：记下「哪条回复可以续命、续命该打到哪个号」，
//! 供 [`config::CC_CACHE_TOUCH_PATH`] 那条请求（[`super::handler`] 里的 `touch`）照着发。
//!
//! 官方的协议（2.1.299 / 2.1.300 可执行文件里同一份，服务端开关 `tengu_sequential_cloud` 管开不开）：
//!
//! 1. 交互式会话的主线程请求带上这项 beta；上游开了保活就在回复头上写
//!    `anthropic-cache-keepalive: active`，**这条回复的缓存写入随之按 5 分钟档记**（体里写的仍是
//!    `ttl:"1h"`），客户端看到它才「上膛」（`armed`），记住这条回复的 `request-id`；
//! 2. 之后每 `touchIntervalSeconds`（下发 30）秒，只要最近 `activityWindowMinutes`（下发 60）分钟内
//!    有过动静、这条记录离上次续命不到 5 分钟（减 10 秒），就 `POST cache_touch {"request_id":"req_…"}`，
//!    上游回 `204`；下一条主线程回复到了就换成新的那条；
//! 3. 上游回 404（记录没了）连续 3 次、或 400 / 403，客户端就不再打。
//!
//! 空闲实测（`cap/auto-2.1.300-20261015-idle/_driver/RESULT.md`）：touch 严格每 30 秒一发，挂 9 分钟
//! 缓存照样命中；不续命时同一条请求原样重放，5 分 46 秒后对话那段已经过期重写。
//!
//! touch 只认**发出那条回复的号**：`request_id` 是上游那个号自己的请求，换个 token 去续是另一个
//! 账号在动别人的缓存。所以回复经过 luban 时按 request-id 记下号与那条请求实际出站的身份头
//! （UA、会话 id、Stainless 一组），touch 来了就照原号、原身份发，不选号、不重试、不计费。同一个
//! request-id 距上次转发不到 [`TOUCH_MIN_GAP`] 的就地回 204，不让异常客户端或几个客户端共用一把 Key
//! 时的重复 touch 把官方不会有的频率带到号上。
//!
//! 模拟路径**暂不带**这项 beta（`simulation` 的 `SIM_CACHE_KEEPALIVE`）：luban 不替模拟会话发 touch，
//! 带了就是把它们的缓存从 1 小时缩成 5 分钟。模拟回复里出现的 `active`（来访自己带了 beta）照记，
//! 来访不是 CC、不会来 touch，到期自然清掉。
//!
//! **记录只在内存里**：luban 重启后，在跑的会话 touch 都拿到本地 404，连续三次客户端就停了，要等下一条
//! 主线程回复才重新上膛，这期间缓存只剩 5 分钟——发版尽量别挑高峰重启。将来多实例挂在负载均衡后面，
//! touch 落到别的实例同样 404，到那时得把这张表挪到共享存储（或按会话粘实例）。
//!
//! touch 成功过的那条记录在被下一条主线程回复接替时，官方报一条
//! `tengu_feature_ok{feature_name:"api_cache_keepalive"}`（`cap/auto-2.1.300-20261015-full` A0：
//! 两发 touch 成功，下一条主线程收尾时一条，夹在 `tether_live_outcome` 与 `api_success` 之间）。
//! luban 替号报遥测，[`note_response`] 的返回值就是「这一条要不要补它」。

use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue};

use crate::config;

/// 服务端开关：值是 `{enabled: true, touchIntervalSeconds, activityWindowMinutes, resumeTouches}`，
/// 关着时是 `false`（`cap/auto-2.1.299-20261014-full` 的 eval 是 `defaultValue`）。
pub(super) const SEQUENTIAL_CLOUD_FEATURE: &str = "tengu_sequential_cloud";

/// 回复头：值为 `active` 才能续。
pub(super) const KEEPALIVE_RESPONSE_HEADER: &str = "anthropic-cache-keepalive";

/// 一条记录多久没续命就作废。官方是距上次续命满 5 分钟减 10 秒就放弃这条（`record_expired` /
/// `entry_lapsed`，可执行文件里的 `300000 - 1e4`），上游缓存也只活 5 分钟；这里多留一分钟，晚到的
/// touch 交给上游判 404，不在本地先拒。
const RECORD_TTL: Duration = Duration::from_secs(6 * 60);

/// 记录条数上限。每个活跃会话同一时刻只有一两条要留（上一条与刚到的这条），满了挤掉最早记下的。
const RECORD_MAX: usize = 50_000;

/// 流一直没收尾的记录最长留多久（从上膛算）：正常的流都会走到 [`end_stream`]，这只是兜底，免得哪条
/// 收尾没跑到的记录永远不过期。
const STREAMING_MAX: Duration = Duration::from_secs(2 * 60 * 60);

/// 同一条记录两发 touch 至少隔多久，不到就地回 204（官方 30 秒一发，见模块说明）。
pub(super) const TOUCH_MIN_GAP: Duration = Duration::from_secs(10);

/// 每次插入顺带检查队首的条数：过期的清掉、还活着的挪到队尾，清理均摊到插入上，不整表扫。
const SWEEP_STEPS: usize = 16;

/// 照抄进 touch 的那几个出站头：与那条回复的请求同一个客户端、同一个会话，官方 touch 是同一个
/// SDK 实例发的，这几项逐字相同（`cap/auto-2.1.300-20261015-full/00139` 对 `00095`）。
const CARRIED_HEADERS: &[&str] = &[
    "user-agent",
    "x-claude-code-session-id",
    "x-stainless-arch",
    "x-stainless-lang",
    "x-stainless-os",
    "x-stainless-package-version",
    "x-stainless-runtime",
    "x-stainless-runtime-version",
    "anthropic-dangerous-direct-browser-access",
    "anthropic-version",
    "x-app",
];

/// 号的 eval 里保活开没开。没见过 eval 的号按关处理（与开关的默认值一致）。
static ENABLED: LazyLock<parking_lot::RwLock<HashMap<i64, bool>>> = LazyLock::new(Default::default);

/// 可续命的记录与主线程链，一把锁管着（几样总是一起改）。
static STATE: LazyLock<parking_lot::Mutex<State>> = LazyLock::new(Default::default);

#[derive(Default)]
struct State {
    /// 可续命的回复，键是上游 `request-id`。
    armed: HashMap<String, Armed>,
    /// 记下的先后（`request-id` 与记下时的序号）：超量时挤队首，清过期也从队首看。序号对不上的是
    /// 同一个 id 后来又记过一次留下的旧位置，跳过。
    order: VecDeque<(String, u64)>,
    seq: u64,
    /// 每个（号, 出站会话 id）当前上膛的那条主线程回复的 `request-id`。随它指着的记录一起删，不会
    /// 比 `armed` 多。
    chain: HashMap<(i64, String), String>,
}

struct Armed {
    seq: u64,
    cred_id: i64,
    /// 发出那条请求的接入 Key（没配 Key 时为 `None`）：只有同一把 Key 能续，别的 Key 拿着别人的
    /// request-id 来续一律当不存在。
    key_id: Option<i64>,
    /// [`CARRIED_HEADERS`] 里出站请求带了的那几项，按表序。
    headers: Vec<(&'static str, HeaderValue)>,
    /// 上膛（回复头到达）的时刻。
    armed_at: Instant,
    /// 流结束或上次续命成功的时刻；流还没结束时不按它过期。
    renewed: Instant,
    /// 回复还在流式输出：官方这时不判记录过期（`stream === "open"` 跳过 lapse 检查）、也不理 404，
    /// 流结束那一刻把计时拨到当下（`recordRenewedAt`），见 [`end_stream`]。
    streaming: bool,
    /// 上一发 touch 转去上游的时刻（[`TOUCH_MIN_GAP`] 看它）。
    last_sent: Option<Instant>,
    /// 自上膛以来有没有续命成功过（被下一条接替时据此补那条 `feature_ok`）。
    touched_ok: bool,
    /// 它是哪条主线程链上的（主线程回复才有），删它时连链一起摘。
    chain: Option<(i64, String)>,
}

impl Armed {
    /// 这条记录该不该作废：流没结束的只看兜底上限，结束了的看距上次续命（或流结束）多久。
    fn expired(&self, now: Instant) -> bool {
        if self.streaming {
            now.duration_since(self.armed_at) >= STREAMING_MAX
        } else {
            now.duration_since(self.renewed) >= RECORD_TTL
        }
    }
}

impl State {
    fn remove(&mut self, id: &str) -> Option<Armed> {
        let a = self.armed.remove(id)?;
        if let Some(key) = &a.chain
            && self.chain.get(key).is_some_and(|c| c == id)
        {
            self.chain.remove(key);
        }
        Some(a)
    }

    /// 清队首：过期的删掉，还活着的挪到队尾，最多看 [`SWEEP_STEPS`] 条。
    fn sweep(&mut self, now: Instant) {
        for _ in 0..SWEEP_STEPS {
            let Some((id, seq)) = self.order.pop_front() else { return };
            match self.armed.get(&id) {
                Some(a) if a.seq == seq && a.expired(now) => {
                    self.remove(&id);
                }
                Some(a) if a.seq == seq => self.order.push_back((id, seq)),
                _ => {}
            }
        }
    }

    /// 满了就从队首挤，直到有空位。
    fn make_room(&mut self) {
        while self.armed.len() >= RECORD_MAX {
            let Some((id, seq)) = self.order.pop_front() else { return };
            if self.armed.get(&id).is_some_and(|a| a.seq == seq) {
                self.remove(&id);
            }
        }
    }
}

/// touch 来了该怎么办，见 [`route`]。
pub(super) enum Touch {
    /// 照这份转去上游。
    Forward(TouchRoute),
    /// 距上次转发不到 [`TOUCH_MIN_GAP`]：就地回 204。
    TooSoon,
    /// 没记过、过期了、或不是同一把接入 Key 记下的。
    Unknown,
}

/// touch 要用的那份：号与原请求的身份头。
pub(super) struct TouchRoute {
    pub(super) cred_id: i64,
    pub(super) headers: Vec<(&'static str, HeaderValue)>,
}

/// 从 eval 的 `features` 记下这个号的保活开关。值是对象且 `enabled` 为 `true` 才算开（客户端用
/// `{enabled: literal(true), …}` 的 schema 校验，校验不过等同没有）。
pub(super) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let on = features
        .get(SEQUENTIAL_CLOUD_FEATURE)
        .and_then(|f| f.get("value"))
        .and_then(|v| v.get("enabled"))
        .and_then(|e| e.as_bool())
        == Some(true);
    ENABLED.write().insert(cred_id, on);
}

/// 这个号的保活开没开，模拟路径据此决定主线程带不带 [`config::CC_BETA_CACHE_KEEPALIVE`]。
pub(super) fn account_enabled(cred_id: i64) -> bool {
    ENABLED.read().get(&cred_id).copied().unwrap_or(false)
}

/// 上游 `request-id` 的形态：`req_` + 字母数字。touch 体里的那个也按这个认。
pub(super) fn is_request_id(v: &str) -> bool {
    v.len() > 4
        && v.len() <= 128
        && v.starts_with("req_")
        && v.bytes().skip(4).all(|b| b.is_ascii_alphanumeric())
}

/// 一条 `/v1/messages` 回复到了：开了保活（回复头 `active`）就按它的 `request-id` 记下号与出站身份头。
///
/// `main` 是这条是不是主线程请求：只有主线程会上膛、会接替上一条。返回值是「上一条被接替的主线程
/// 记录续命成功过」——这条主线程收尾时要补 `feature_ok{api_cache_keepalive}`，与官方一样不论这条
/// 自己是不是 `active`（官方在每条主线程回复到达时都会停掉上一条的续命）。
pub(super) fn note_response(
    cred_id: i64,
    key_id: Option<i64>,
    outbound: &HeaderMap,
    response: &HeaderMap,
    main: bool,
) -> bool {
    let session =
        outbound.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()).map(str::to_string);
    let active = response
        .get(KEEPALIVE_RESPONSE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("active"));
    let request_id = response
        .get("request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| is_request_id(v))
        .map(str::to_string);
    let now = Instant::now();
    let mut st = STATE.lock();
    let chain_key = session.filter(|_| main).map(|sid| (cred_id, sid));
    let prev_ok = match &chain_key {
        Some(key) => {
            let prev = match (active, &request_id) {
                (true, Some(id)) => st.chain.insert(key.clone(), id.clone()),
                _ => st.chain.remove(key),
            };
            prev.and_then(|p| st.armed.get_mut(&p))
                .is_some_and(|a| std::mem::take(&mut a.touched_ok))
        }
        None => false,
    };
    if let (true, Some(id)) = (active, request_id) {
        st.sweep(now);
        st.make_room();
        st.seq += 1;
        let seq = st.seq;
        let headers = CARRIED_HEADERS
            .iter()
            .filter_map(|name| outbound.get(*name).map(|v| (*name, v.clone())))
            .collect();
        st.order.push_back((id.clone(), seq));
        st.armed.insert(
            id,
            Armed {
                seq,
                cred_id,
                key_id,
                headers,
                armed_at: now,
                renewed: now,
                streaming: true,
                last_sent: None,
                touched_ok: false,
                chain: chain_key,
            },
        );
    }
    prev_ok
}

/// 首发响应登记时留下的材料，随 `ReqLog` 走到最终那一发（[`note_final`]）：降级、剥字段、改发 thread
/// 这些重试成功后，客户端认的是**最终**那条回复的 `request-id`，得照它再登记一次。
pub(super) struct KeepaliveNote {
    pub(super) cred_id: i64,
    pub(super) key_id: Option<i64>,
    /// 首发那一轮的出站头（会话 id 与身份头取自它；重试只改体与 beta，这几项不变）。
    pub(super) outbound: HeaderMap,
    pub(super) main: bool,
    /// 首发响应的 `request-id`（登记过的那条）。
    pub(super) noted: Option<String>,
}

/// 最终那一发响应：不是首发那条（换过一发）就照它再登记一次。首发那次已经把上一条主线程记录接替掉、
/// 报没报遥测也定了；被拒的首发不是 `active`，这里再走一遍 [`note_response`] 不会重复接替。
pub(super) fn note_final(n: &KeepaliveNote, response: &HeaderMap) {
    let rid = response.get("request-id").and_then(|v| v.to_str().ok()).map(str::trim);
    if rid.is_some() && rid != n.noted.as_deref() {
        note_response(n.cred_id, n.key_id, &n.outbound, response, n.main);
    }
}

/// touch 来了：这条 `request-id` 该打到哪个号、带哪套身份头。要转的那发当场记下转发时刻，并发来的
/// 第二发就落在 [`Touch::TooSoon`] 里。
pub(super) fn route(request_id: &str, key_id: Option<i64>) -> Touch {
    let now = Instant::now();
    let mut st = STATE.lock();
    let Some(a) = st.armed.get_mut(request_id) else { return Touch::Unknown };
    if a.expired(now) {
        st.remove(request_id);
        return Touch::Unknown;
    }
    if a.key_id != key_id {
        return Touch::Unknown;
    }
    if a.last_sent.is_some_and(|t| now.duration_since(t) < TOUCH_MIN_GAP) {
        return Touch::TooSoon;
    }
    a.last_sent = Some(now);
    Touch::Forward(TouchRoute { cred_id: a.cred_id, headers: a.headers.clone() })
}

/// 上游对这一发 touch 的答复：2xx 记一次续命成功。其余（含 404）一概不动、照转给客户端，由它按官方的
/// 重试规矩办：回复还在流式输出时它不理 404、接着续；平常也要连着三次 404 才停。luban 在头一次 404
/// 就删记录，等于把之后的 touch 全变成本地 404，替客户端提前放弃。记录照常按 [`RECORD_TTL`] 到期。
pub(super) fn settle(request_id: &str, status: u16) {
    if (200..300).contains(&status)
        && let Some(a) = STATE.lock().armed.get_mut(request_id)
    {
        a.renewed = Instant::now();
        a.touched_ok = true;
    }
}

/// 这条回复的流结束了（转发收尾，见 `ReqLog` 的 `Drop`）：从这一刻起按 [`RECORD_TTL`] 计时。官方在流
/// 结束时同样把记录的续命时刻拨到当下。没记过的（没开保活的回复）什么都不做。
pub(super) fn end_stream(request_id: &str) {
    if let Some(a) = STATE.lock().armed.get_mut(request_id) {
        a.streaming = false;
        a.renewed = Instant::now();
    }
}

/// 这条记录的号已经停了或删了：缓存在它名下，换号续不了，忘掉它。
pub(super) fn forget(request_id: &str) {
    STATE.lock().remove(request_id);
}

/// touch 出站的 `anthropic-beta`：只有这两项，`oauth` 在前（`cap/auto-2.1.300-20261015-full/00139`）。
pub(super) fn touch_beta() -> String {
    format!("{},{}", config::OAUTH_BETA_HEADER, config::CC_BETA_CACHE_KEEPALIVE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn eval_flag_needs_enabled_true() {
        let on: serde_json::Value = serde_json::json!({
            "tengu_sequential_cloud": {"value": {"enabled": true, "touchIntervalSeconds": 30,
                "activityWindowMinutes": 60, "resumeTouches": true}}
        });
        note_eval(9_001, on.as_object().unwrap());
        assert!(account_enabled(9_001));
        let off: serde_json::Value =
            serde_json::json!({"tengu_sequential_cloud": {"value": false}});
        note_eval(9_001, off.as_object().unwrap());
        assert!(!account_enabled(9_001));
        assert!(!account_enabled(9_002), "没见过 eval 的号按关");
    }

    #[test]
    fn records_active_replies_and_reports_touched_chain() {
        let out = hdrs(&[
            ("user-agent", "claude-cli/2.1.300 (external, cli)"),
            ("x-claude-code-session-id", "3730aa95-50eb-4b01-a271-045e2e06e647"),
            ("x-stainless-os", "MacOS"),
            ("x-app", "cli"),
            ("x-claude-code-prompt-id", "not-carried"),
        ]);
        let active = |id: &str| hdrs(&[("request-id", id), (KEEPALIVE_RESPONSE_HEADER, "active")]);
        let (cred, key) = (9_101, Some(7));
        assert!(!note_response(cred, key, &out, &active("req_011CaaaA1"), true));
        let Touch::Forward(r) = route("req_011CaaaA1", key) else { panic!("上膛了") };
        assert_eq!(r.cred_id, cred);
        let names: Vec<_> = r.headers.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["user-agent", "x-claude-code-session-id", "x-stainless-os", "x-app"]);
        assert!(matches!(route("req_011CaaaA1", Some(8)), Touch::Unknown), "别的 Key 续不了");
        assert!(matches!(route("req_011CaaaA1", None), Touch::Unknown));
        assert!(matches!(route("req_011CaaaA1", key), Touch::TooSoon), "10 秒内再来就地回");
        // 没续过就被接替：不报。
        assert!(!note_response(cred, key, &out, &active("req_011CaaaA2"), true));
        // 续成功一次，再被接替：报一次，且只报一次。
        settle("req_011CaaaA2", 204);
        assert!(note_response(cred, key, &out, &active("req_011CaaaA3"), true));
        settle("req_011CaaaA3", 204);
        // 接替它的这条不是 active，上一条照样停、照样报。
        assert!(note_response(cred, key, &out, &hdrs(&[("request-id", "req_011CaaaA4")]), true));
        assert!(matches!(route("req_011CaaaA4", key), Touch::Unknown), "没 active 的不记");
        // 上游 404 不删：客户端按官方规矩还会再试（流式输出中不理 404、平常连着三次才停）。
        settle("req_011CaaaA3", 404);
        assert!(!matches!(route("req_011CaaaA3", key), Touch::Unknown), "照常还能转");
        // 号没了才忘。
        forget("req_011CaaaA3");
        assert!(matches!(route("req_011CaaaA3", key), Touch::Unknown));
    }

    #[test]
    fn non_main_replies_do_not_touch_the_chain() {
        let out = hdrs(&[("x-claude-code-session-id", "9e467c89-b2ad-4bed-9f32-aa5caf0ed44c")]);
        let active = |id: &str| hdrs(&[("request-id", id), (KEEPALIVE_RESPONSE_HEADER, "active")]);
        let (cred, key) = (9_201, None);
        assert!(!note_response(cred, key, &out, &active("req_011CbbbB1"), true));
        settle("req_011CbbbB1", 204);
        // `/btw`、压缩这类非主线程请求也会 active，但不接替主线程那条。
        assert!(!note_response(cred, key, &out, &active("req_011CbbbB2"), false));
        assert!(matches!(route("req_011CbbbB2", key), Touch::Forward(_)), "辅助请求的回复照记");
        assert!(note_response(cred, key, &out, &active("req_011CbbbB3"), true));
    }

    /// 主线程链随它指着的记录一起删（号没了 / 过期 / 挤出），不会无限长。
    #[test]
    fn chain_entries_go_with_their_records() {
        let out = hdrs(&[("x-claude-code-session-id", "7a0d3c1e-1111-4222-8333-444455556666")]);
        let active = |id: &str| hdrs(&[("request-id", id), (KEEPALIVE_RESPONSE_HEADER, "active")]);
        note_response(9_301, None, &out, &active("req_011CcccC1"), true);
        let key = (9_301, "7a0d3c1e-1111-4222-8333-444455556666".to_string());
        assert!(STATE.lock().chain.contains_key(&key));
        forget("req_011CcccC1");
        assert!(!STATE.lock().chain.contains_key(&key), "记录没了，链跟着摘");
    }

    /// 流没结束的记录不按 TTL 过期；结束时计时拨到当下。
    #[test]
    fn streaming_records_do_not_lapse_until_the_stream_ends() {
        let out = hdrs(&[("x-claude-code-session-id", "5b1c2d3e-1111-4222-8333-444455556666")]);
        let active = |id: &str| hdrs(&[("request-id", id), (KEEPALIVE_RESPONSE_HEADER, "active")]);
        note_response(9_401, None, &out, &active("req_011CddddD1"), true);
        let later = Instant::now() + RECORD_TTL + Duration::from_secs(60);
        {
            let mut st = STATE.lock();
            let a = st.armed.get_mut("req_011CddddD1").unwrap();
            assert!(a.streaming && !a.expired(later), "流还开着，过了 TTL 也不作废");
            // 假装流早就结束、之后再没续过：过了 TTL 就作废。
            a.streaming = false;
            assert!(a.expired(later));
            a.streaming = true;
        }
        end_stream("req_011CddddD1");
        let st = STATE.lock();
        let a = &st.armed["req_011CddddD1"];
        assert!(!a.streaming && !a.expired(Instant::now()), "流结束时计时拨到当下");
        assert!(a.expired(later));
    }

    #[test]
    fn request_id_shape() {
        assert!(is_request_id("req_011Cg34k1PDydFY8V38T9315"));
        for bad in
            ["", "req_", "msg_011C", "req_01-1", "req_01 1", &format!("req_{}", "a".repeat(200))]
        {
            assert!(!is_request_id(bad), "{bad}");
        }
        assert_eq!(touch_beta(), "oauth-2025-04-20,cache-keepalive-2026-09-03");
    }
}
