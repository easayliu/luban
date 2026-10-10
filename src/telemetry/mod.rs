//! 官方 Claude Code 客户端的遥测模拟：**逐请求**那一半。
//!
//! 官方客户端每发一条 `/v1/messages`，都会在本地攒下一串 `tengu_*` 事件——发出前的
//! `tengu_api_query`、首字节到达时的 `tengu_feature_ok{api_request}`、结束时的
//! `tengu_api_success`（带上游 `request-id`、逐项 token 数、花费、TTFT）与 `tengu_turn_end`
//! ——然后分三路上报：一方事件 `POST /api/event_logging/v2/batch`（每 ~30s 一批）、Datadog
//! 日志（每 ~10s 一批）、OTel 指标 `POST /api/claude_code/metrics`（每 5 分钟）。
//!
//! 此前 luban 只有 [`crate::oauth`] 里的保活遥测：每张凭证每 30 分钟报一组「空闲版本检查」
//! 事件，`session.count` 恒为 1、`cost.usage` 恒为 0.042。于是上游看到的是一个账号有大量
//! `/v1/messages` 用量、遥测里却一条 API 调用都没有——这是比任何单个字段都显眼的破绽。
//! 本模块补上这一半：转发路径在响应流结束时把这条请求的形态与用量交给 [`Telemetry::record`]，
//! 由它按 `cap/2.1.258` 的事件链造出事件、攒批、按官方节奏发出。
//!
//! **身份取自实际发往上游的那份请求**：`metadata.user_id` 里的 `device_id`/`account_uuid`/
//! `session_id`（经过 [`crate::proxy`] 的身份改写之后的值）、出站 `anthropic-beta`、出站 UA
//! 的版本号，以及上游响应头里的 `anthropic-organization-id`。遥测那一侧与 `/v1/messages`
//! 那一侧必须是同一个人、同一台设备、同一个会话，否则两边一比对就是矛盾。
//!
//! 事件字段的取法逐项对照 `cap/2.1.258/00020`、`00032`（event_logging）与 `00019`、`00029`
//! （Datadog）。拿不到的量（客户端内部的消息条数、渲染路径等）按抓包里的规律估，见各处注释。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Bytes;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config;

mod call;
mod events;
mod identity;
mod kind;
mod process;
mod session;
mod shape;
mod template;
#[cfg(test)]
mod tests;
mod tools;
mod upload;

pub use call::*;
use events::*;
pub use identity::*;
use kind::*;
pub use session::*;
pub use shape::*;
use template::*;
use tools::*;
pub use upload::*;

/// 一个新会话要做的启动握手：真实客户端每次拉起进程都会用当前账号打这一串端点
/// （`cap/2.1.260-1` 17:14:56–17:15:05），luban 替它补上，身份取该会话的。
///
/// 由转发路径在该会话**首条请求发出之前**构造并 spawn，见
/// [`crate::proxy::spawn_session_handshake`]；凭证本身作为单独的参数传给
/// [`crate::oauth::HandshakeRunner`]，不放进这个结构。
pub struct Handshake {
    pub snapshot: SessionSnapshot,
    /// bootstrap 的 `model=` 参数：规范名。
    pub model: String,
}

/// 逐请求遥测的汇聚点：转发路径往里 [`Telemetry::record`]，[`run_flusher`] 定时取走发出。
#[derive(Clone, Default)]
pub struct Telemetry(Arc<Shared>);

#[derive(Default)]
struct Shared {
    state: parking_lot::Mutex<State>,
    ingest: IngestQueue,
}

/// 待处理的调用队列：**入队是同步的，出队只有一个消费者**，故处理顺序恒等于
/// [`crate::proxy::ReqLog`] 的析构顺序，也就是响应完成的先后。
///
/// 为什么不能一条一个 `spawn_blocking`：那是往一个最多 512 线程的池子里扔任务，前后脚
/// 提交的两条谁先跑没有任何保证。而 [`Telemetry::process`] 里一多半状态是**按顺序**累积的
/// （`last_main_request_id` 那条链、`turn_depth`、`prev_total_input`、扣住侧查询要看的
/// 「这个会话在不在」……），顺序一乱，报出去的就是一份自相矛盾的会话历史。
///
/// 同一会话上真会并发的是「主线程 + 标题/安全分类/子代理」这几对，两条响应在同一毫秒内
/// 结束并不稀奇；最难看的一种是标题那条抢在会话首条主请求前面被处理——扣留分支要求会话
/// **已经存在**，抢先了就会以一条 haiku 标题请求为起点铺开整串启动事件。
#[derive(Default)]
struct IngestQueue {
    calls: parking_lot::Mutex<VecDeque<ApiCall>>,
    /// 已经有一个消费者在跑。只用来保证「同时最多一个」，队列本身的顺序由 `calls` 保证。
    draining: AtomicBool,
}

/// 一次要发出去的东西（一张凭证下的一个会话）。
pub struct Flush {
    pub cred_id: i64,
    /// 这一批属于哪个会话（日志里只展示前 8 位）。
    pub session_id: String,
    pub version: String,
    pub events: Vec<Value>,
    pub dd: Vec<Value>,
    pub metrics: Option<Value>,
}

impl Telemetry {
    /// 记一条已完成的 API 调用。解析请求体要几毫秒（100KB+ 的 JSON），且调用方在 `Drop`
    /// 里——扔到运行时上做，拿不到运行时（测试）就就地做。
    ///
    /// **入队这一步是同步的**：队列顺序 = `Drop` 顺序 = 响应完成顺序，随后由唯一的消费者
    /// 按序处理，见 [`IngestQueue`]。
    pub fn record(&self, call: ApiCall) {
        {
            let mut q = self.0.ingest.calls.lock();
            // 消费慢过生产时封顶：每条 `ApiCall` 拎着一份出站体（100KB+ 是常态），
            // 无上限的队列在上游长时间挂起时能把内存吃穿。丢**新**的而不是旧的——
            // 旧的丢掉会把已经排好的那条链从中间截断。
            if q.len() >= config::TELEMETRY_INGEST_QUEUE_MAX {
                tracing::warn!(
                    queued = q.len(),
                    "the telemetry ingest queue is full; dropping this call's events"
                );
                return;
            }
            q.push_back(call);
        }
        // 已经有消费者在跑就交给它——顺序正是靠「同时只有一个」保住的。
        if self.0.ingest.draining.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        let work = move || me.drain();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn_blocking(work);
            }
            Err(_) => work(),
        }
    }

    /// 唯一的消费者：把队列按 FIFO 排空。
    ///
    /// 队列锁**不跨** `process`（那是几毫秒的 JSON 解析加事件构造）：先 `pop_front` 拿到
    /// 手里再去锁状态，两把锁不嵌套，入队方也就从不被处理阻塞。
    fn drain(&self) {
        loop {
            let next = self.0.ingest.calls.lock().pop_front();
            let Some(call) = next else {
                // 空了：先放掉标志再复查一次。这中间入队的那条看到的是「有人在跑」，
                // 不会自己起一个消费者，复查就是接住它的那一手。
                self.0.ingest.draining.store(false, Ordering::Release);
                if self.0.ingest.calls.lock().is_empty() {
                    return;
                }
                // 又有了：抢回消费者身份接着跑；抢不到说明刚入队那条已经起了一个，让给它。
                if self.0.ingest.draining.swap(true, Ordering::AcqRel) {
                    return;
                }
                continue;
            };
            let mut st = self.0.state.lock();
            Self::process(&mut st, call, true, None);
        }
    }

    /// 某凭证最近一次响应头里的 `anthropic-organization-id`（保活事件的 `auth` 块用）。
    pub fn org_uuid(&self, cred_id: i64) -> Option<String> {
        self.0.state.lock().org_uuid.get(&cred_id).cloned()
    }

    /// 用凭证上存的组织 id（profile 的 `organization.uuid`）**垫底**：还没从响应头学到时先用它，
    /// 学到了以响应头为准（那是同一个值，只是更新鲜）。转发路径在建遥测材料时调一次。
    ///
    /// 没有这一步，一张刚登录、或久没转发过请求的号发出去的头几条事件 `auth` 块里就没有
    /// `organization_uuid`——官方 345/345 条都带。
    pub fn seed_org_uuid(&self, cred_id: i64, org_uuid: Option<&str>) {
        let Some(org) = org_uuid.map(str::trim).filter(|o| !o.is_empty()) else { return };
        self.0.state.lock().org_uuid.entry(cred_id).or_insert_with(|| org.to_string());
    }

    /// 某凭证最近活跃的真实会话（`max_idle` 内有过请求的那些里最新的一个）；没有则 `None`。
    /// 保活拿它把空闲事件挂到真实会话上，见 [`SessionSnapshot`]。
    pub fn latest_session(&self, cred_id: i64, max_idle: Duration) -> Option<SessionSnapshot> {
        let st = self.0.state.lock();
        let now = Instant::now();
        st.sessions
            .iter()
            .filter(|((c, _), s)| *c == cred_id && now.duration_since(s.last_seen) < max_idle)
            .max_by_key(|(_, s)| s.last_seen)
            .map(|((_, sid), s)| SessionSnapshot {
                session_id: sid.clone(),
                device_id: s.device_id.clone(),
                account_uuid: s.account_uuid.clone(),
                version: s.version.clone(),
                model: s.last_model.clone().unwrap_or_else(|| s.default_model.clone()),
                betas: s.betas.clone(),
                prompt_id: s.prompt_id.clone(),
                started_wall: s.started_wall,
            })
    }

    /// 测试用：等 [`Self::record`] 交出去的调用都处理完（队列空、消费者已退出）。有运行时的
    /// 测试里 `record` 走阻塞线程池，`Drop` 返回时事件还没生成。
    #[cfg(test)]
    pub(crate) async fn settle(&self) {
        for _ in 0..300 {
            if self.0.ingest.calls.lock().is_empty()
                && !self.0.ingest.draining.load(Ordering::Acquire)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("telemetry ingest did not settle");
    }

    /// 就地处理一条调用（测试用）。转发路径走 [`Self::record`]——那条要经队列才能保住
    /// 顺序，而测试本来就是单线程按序调用，直接进 `process` 少一层异步。
    #[cfg(test)]
    fn ingest(&self, call: ApiCall) {
        let mut st = self.0.state.lock();
        Self::process(&mut st, call, true, None);
        tests::dump_pending(&st);
    }

    /// 取走到期该发的：事件攒满 [`config::TELEMETRY_EVENT_FLUSH_SECS`]、Datadog 攒满
    /// [`config::TELEMETRY_DATADOG_FLUSH_SECS`]、指标攒满 [`config::TELEMETRY_METRICS_FLUSH_SECS`]，
    /// 或任一路条数到了 [`config::TELEMETRY_BATCH_MAX`]。每个会话一份 [`Flush`]，同一张凭证
    /// 的多个会话各发各的。
    pub fn take_due(&self, now: Instant) -> Vec<Flush> {
        let mut st = self.0.state.lock();
        let org_uuids = st.org_uuid.clone();
        let mut out = Vec::new();
        for ((cred_id, session), p) in st.pending.iter_mut() {
            let due = |since: Option<Instant>, secs: u64, n: usize| {
                n >= config::TELEMETRY_BATCH_MAX
                    || since.is_some_and(|s| now.duration_since(s) >= Duration::from_secs(secs))
            };
            let mut f = Flush {
                cred_id: *cred_id,
                session_id: session.clone(),
                version: p.version.clone(),
                events: Vec::new(),
                dd: Vec::new(),
                metrics: None,
            };
            // 指标先算：导出会顺手往事件/日志队列里各塞一条 `internal_metrics_export`，退出
            // 收尾时三路同时到期，那两条得赶上同一批（真实退出批次里它就在队尾那串中间）。
            // 指标只按时间到期（`0` 条永远不触发按条数那一路），攒多少条都是一发聚合。
            if !p.metrics.is_empty()
                && due(p.metrics_since, config::TELEMETRY_METRICS_FLUSH_SECS, 0)
            {
                let calls = std::mem::take(&mut p.metrics);
                p.metrics_since = None;
                f.metrics = Some(metrics_body(
                    &calls,
                    &p.version,
                    &p.subscription_type,
                    org_uuids.get(cred_id).map(String::as_str),
                ));
                if let Some(id) = p.identity.clone() {
                    let t = p.export_at.take().unwrap_or_else(Utc::now);
                    let start: DateTime<Utc> = p.started_wall.map(Into::into).unwrap_or(t);
                    let (model, betas, prompt_id) =
                        (p.model.clone(), p.betas.clone(), p.prompt_id.clone());
                    let ctx = EventCtx {
                        model: &model,
                        betas: &betas,
                        prompt_id: &prompt_id,
                        uptime_secs: ((t - start).num_milliseconds().max(0) as f64) / 1000.0,
                    };
                    let extra = json!({ "feature_name": "internal_metrics_export" });
                    p.events.push((t, id.event("tengu_feature_ok", t, &ctx, extra.clone())));
                    p.events_since.get_or_insert(now);
                    p.dd.push(id.dd_entry(
                        "tengu_feature_ok",
                        &ctx,
                        model.trim_end_matches("[1m]"),
                        extra,
                    ));
                    p.dd_since.get_or_insert(now);
                }
            }
            if !p.events.is_empty()
                && due(p.events_since, config::TELEMETRY_EVENT_FLUSH_SECS, p.events.len())
            {
                let mut evs = std::mem::take(&mut p.events);
                evs.sort_by_key(|(t, _)| *t);
                f.events = evs.into_iter().map(|(_, v)| v).collect();
                p.events_since = None;
            }
            if !p.dd.is_empty() && due(p.dd_since, config::TELEMETRY_DATADOG_FLUSH_SECS, p.dd.len())
            {
                // Datadog 那份官方也是按发生顺序排的；补发的侧查询会晚于后来的主线程入队，
                // 按各条自带的 `process_metrics.uptime` 排回去。
                let mut dd = std::mem::take(&mut p.dd);
                let uptime = |v: &Value| {
                    v.get("process_metrics")
                        .and_then(|m| m.get("uptime"))
                        .and_then(|u| u.as_f64())
                        .unwrap_or(0.0)
                };
                dd.sort_by(|a, b| uptime(a).total_cmp(&uptime(b)));
                f.dd = dd;
                p.dd_since = None;
            }
            if !f.events.is_empty() || !f.dd.is_empty() || f.metrics.is_some() {
                out.push(f);
            }
        }
        st.pending.retain(|_, p| !p.events.is_empty() || !p.dd.is_empty() || !p.metrics.is_empty());
        out
    }

    /// 久无请求的会话按「客户端退出」收尾：补上退出那一串事件，并把这个会话攒着的三路
    /// 全部标成立刻到期。
    ///
    /// 真实客户端退出时（`cap/2.1.260-1`，17:10:24）会在一秒内连发三样：metrics、event_logging
    /// 批次（队尾是 `tengu_config_cache_stats` → `lsp_shutdown` → `swarm_session_cleanup` →
    /// `internal_metrics_export` → `tengu_cache_eviction_hint{scope:session_end,last_request_id}`）、
    /// Datadog（那三条 feature_ok）。luban 看不见客户端退出，只能以
    /// [`config::TELEMETRY_SESSION_IDLE_SECS`] 没有请求为准——真实用户也常把会话开着几小时
    /// 再关，这期间保活挂在这个会话上的空闲事件正好把这段空白填成「开着没说话」。
    pub fn gc(&self, now: Instant) {
        let idle = Duration::from_secs(config::TELEMETRY_SESSION_IDLE_SECS);
        let mut st = self.0.state.lock();
        // 扣住太久的侧查询：没等到主线程请求，按会话现有的 prompt id 补发。
        let hold = Duration::from_secs(config::TELEMETRY_SIDE_QUERY_HOLD_SECS);
        let stale: Vec<(i64, String)> = st
            .sessions
            .iter()
            .filter(|(_, s)| s.deferred.iter().any(|(_, _, at)| now.duration_since(*at) >= hold))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            Self::replay_deferred(&mut st, &k);
        }
        let stale_pre: Vec<(i64, String)> = st
            .presession
            .iter()
            .filter(|(_, v)| v.iter().any(|(_, at)| now.duration_since(*at) >= hold))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale_pre {
            Self::replay_presession(&mut st, &k);
        }
        let expired: Vec<((i64, String), Session)> = {
            let keys: Vec<(i64, String)> = st
                .sessions
                .iter()
                .filter(|(_, s)| now.duration_since(s.last_seen) >= idle)
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter().filter_map(|k| st.sessions.remove(&k).map(|s| (k, s))).collect()
        };
        // 记住这些 id 已经「退出」过，再来按 resume；太久的忘掉，免得这张表只增不减。
        let memory = Duration::from_secs(config::TELEMETRY_ENDED_SESSION_MEMORY_SECS);
        st.ended.retain(|_, t| now.duration_since(*t) < memory);
        // 设备级的两张表按设备算，客户端一多（或设备 id 在轮换）会一直长：启动探测的记号过了
        // 「已退出会话」的记忆期就没用了；默认模型那张也按同样的期限，最近没再更新、设备上也没有
        // 会话在跑的就忘掉（下次再以那台设备第一条主线程请求的模型为准）。
        let horizon = std::time::SystemTime::now()
            .checked_sub(memory)
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        st.process_starts.retain(|_, (_, at)| *at >= horizon);
        let live: std::collections::HashSet<(i64, String)> =
            st.sessions.iter().map(|((c, _), s)| (*c, s.device_id.clone())).collect();
        st.device_default_model
            .retain(|k, (_, at)| live.contains(k) || now.duration_since(*at) < memory);
        for (k, _) in &expired {
            st.ended.insert(k.clone(), now);
        }
        for ((cred_id, session_id), s) in expired {
            // 被 `/clear` 换掉的会话：进程还在，没有退出那一串。`-p` 的退出收尾在它那份模板的
            // turn 段里，跑完那一轮就报过了。
            if s.cleared || s.sdk {
                continue;
            }
            let identity = Identity {
                session_id: session_id.clone(),
                device_id: s.device_id,
                account_uuid: s.account_uuid,
                organization_uuid: st.org_uuid.get(&cred_id).cloned(),
                subscription_type: s.subscription_type,
                version: s.version.clone(),
                agent_id: None,
                vcs: s.git_repo.filter(|r| *r).map(|_| "git"),
                parent_session_id: s.parent_session_id.clone(),
                sdk: s.sdk,
            };
            let model = s.last_model.unwrap_or(s.default_model);
            let dd_model = model.trim_end_matches("[1m]").to_string();
            let t0 = Utc::now();
            let ms = |d: i64| t0 + chrono::Duration::milliseconds(d);
            let start: DateTime<Utc> = s.started_wall.into();
            let uptime =
                |dt: DateTime<Utc>| ((dt - start).num_milliseconds().max(0) as f64) / 1000.0;
            let ctx = |dt: DateTime<Utc>| EventCtx {
                model: &model,
                betas: &s.betas,
                prompt_id: &s.prompt_id,
                uptime_secs: uptime(dt),
            };
            let feature = |name: &str| json!({ "feature_name": name });
            let mut events: Vec<(DateTime<Utc>, Value)> = Vec::with_capacity(5);
            let mut dd: Vec<Value> = Vec::with_capacity(3);
            // 配置缓存命中数随会话长短走：抓包里 7 秒的会话 3779、一小时的 11054。
            let cache_hits = 3_000 + u64::from(s.prompt_index) * 2_500;
            events.push((
                t0,
                identity.event(
                    "tengu_config_cache_stats",
                    t0,
                    &ctx(t0),
                    json!({ "cache_hits": cache_hits, "cache_misses": 0, "hit_rate": 1 }),
                ),
            ));
            // `internal_metrics_export` 不在这里：它只在真有指标要导出时才出现（由
            // [`Self::take_due`] 导出时按 `export_at` 的时间戳补进来）。
            for (offset, name) in [(1, "lsp_shutdown"), (6, "swarm_session_cleanup")] {
                let t = ms(offset);
                events.push((t, identity.event("tengu_feature_ok", t, &ctx(t), feature(name))));
                dd.push(identity.dd_entry("tengu_feature_ok", &ctx(t), &dd_model, feature(name)));
            }
            let t = ms(544);
            events.push((
                t,
                identity.event(
                    "tengu_cache_eviction_hint",
                    t,
                    &ctx(t),
                    json!({
                        "scope": "session_end",
                        "last_request_id": s.last_main_request_id.as_deref().unwrap_or("")
                    }),
                ),
            ));

            // 入队并强制到期：把三路的起算点拨到很久以前，下一次 `take_due` 就全发出去。
            let long_ago = now.checked_sub(Duration::from_secs(86_400)).unwrap_or(now);
            let p = st.pending.entry((cred_id, session_id)).or_default();
            if p.version.is_empty() {
                p.version = identity.version.clone();
                p.subscription_type = identity.subscription_type.clone();
            }
            // 导出事件要用的上下文以这个会话为准（pending 里那份可能是空的——比如指标早已
            // 导出、事件也早已发完，这里是重新建的条目）。
            p.identity = Some(identity);
            p.model = model;
            p.betas = s.betas;
            p.prompt_id = s.prompt_id;
            p.started_wall = Some(s.started_wall);
            p.events.extend(events);
            p.events_since = Some(long_ago);
            p.dd.extend(dd);
            p.dd_since = Some(long_ago);
            if !p.metrics.is_empty() {
                p.metrics_since = Some(long_ago);
                p.export_at = Some(ms(542));
            }
        }
    }
}
