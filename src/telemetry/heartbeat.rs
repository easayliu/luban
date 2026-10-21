//! 会话活动心跳 `tengu_cache_heartbeat_shadow`（2.1.301 起，见 [`MIN_VERSION`]；服务端开关 `tengu_dapper_dawn` 开着的号才发，
//! 见 [`crate::proxy::dapper_dawn`]）。
//!
//! 官方是一台按进程的小状态机（可执行文件里的 `uue` / `tEt` / `ews` / `iFn` / `nEt`），这里逐条照搬：
//!
//! - **主循环请求成功**（`tEt`，`tengu_api_success` 那一刻、排在它前面）：记下这一刻，报 `main_request`，
//!   `sinceLastMainRequestMs` 是距上一条主循环请求；这是复位后的头一条、且界面已经在「干活」时，紧跟一条
//!   那段活的 `work_start`（`sinceLastEventMs: 0`）。`-p` 的 `sdk` 主线程同样算。
//! - **界面运行态切换**（`ews`）：`work`（`turn_running` / `subagent_running` / `compacting`）与
//!   `awaiting_user`（`permission prompt` / `input needed` 等）之间换一次，先报旧的 `_end`、再报新的
//!   `_start`，同一毫秒。运行态**总是记**，只是复位之后、头一条主循环请求之前报不出来。
//! - **用户输入**（`iFn`，提交与编辑输入框都调）：报 `user_input`，距上一条**报出去的**不到 10 秒的不报。
//! - **清场**（`nEt`：`session_end` / `conversation_clear` / `compaction`）：报那一项，然后整台复位——
//!   时刻、运行态、输入节流、`interactive` 是否已报都清掉，之后要等下一条主循环请求才重新开口。
//!
//! 每条都带 `requestId`（当时最后一条主循环请求的 request-id，复位后没有就整条不报）、
//! `sinceLastEventMs`（距上一条心跳），复位后的头一条再带 `interactive`（交互式 `true`、`-p` `false`）。
//!
//! luban 看不见界面，运行态从它自己排出来的那条时间线上取：新输入开始干活、主线程收尾停手、权限弹框
//! 与点头、`Agent` 工具在跑、`/compact`，各处登记一个 [`Trigger`]，一条调用的事件排完后按时刻折算
//! （[`Heartbeat::fold`]）。界面上别的对话框（`/status` 之类的 `dialog open`）代理看不到，不报。
//!
//! 开关官方每次触发都重读（`WIt()` = `k("tengu_dapper_dawn", false)`，四个入口各读一次），关着时状态机
//! 一步不动：这里每条调用折算前按当时服务这条的号重读一次（[`Heartbeat::refresh`]），会话中途 eval
//! 换了值就跟着开或关。

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

/// 界面运行态（官方 `qio` 的返回值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Run {
    /// `kind: work`：`turn_running` / `subagent_running` / `compacting`。
    Work(&'static str),
    /// `kind: awaiting_user`：`permission prompt` / `input needed`。
    Awaiting(&'static str),
}

pub(crate) const TURN_RUNNING: Run = Run::Work("turn_running");
pub(crate) const SUBAGENT_RUNNING: Run = Run::Work("subagent_running");
pub(crate) const COMPACTING: Run = Run::Work("compacting");

/// 弹框等用户点头时的运行态：`AskUserQuestion` 是 `input needed`，其余（含 `ExitPlanMode`）是
/// `permission prompt`（`cap/auto-2.1.305-20261021-full` A16 / A17 / B1）。
pub(crate) fn awaiting_for(tool: &str) -> Run {
    Run::Awaiting(if tool == "AskUserQuestion" { "input needed" } else { "permission prompt" })
}

/// 一处时间线上的触发。
#[derive(Clone, Debug)]
pub(crate) enum Trigger {
    /// 一条主循环请求成功，带它的 request-id。
    MainRequest(String),
    /// 界面运行态换成这个（`None` 即闲下来）。
    Run(Option<Run>),
    /// 用户在输入框里动了（提交或编辑）。
    UserInput,
    /// 清场：`session_end` / `conversation_clear` / `compaction`。
    Reset(&'static str),
}

/// 按进程的状态（官方 `JTt`）。`/clear` 换会话不换进程，新会话接着用旧会话这一份。
#[derive(Clone, Debug, Default)]
pub(crate) struct Heartbeat {
    /// 这个会话报不报：版本够（[`Self::version_ok`]）且号的开关开着，每次折算前按当时的号重读。
    pub(crate) on: bool,
    /// 出站版本 ≥ [`MIN_VERSION`]（客户端才有这台状态机），会话头一条请求时钉住。
    version_ok: bool,
    /// 交互式（`-p` 为 `false`）。
    pub(crate) interactive: bool,
    last_main_at: Option<DateTime<Utc>>,
    last_event_at: Option<DateTime<Utc>>,
    last_user_input: Option<DateTime<Utc>>,
    run: Option<Run>,
    interactive_logged: bool,
    /// 当时最后一条主循环请求的 request-id（官方 `dV()`）。
    last_main_request_id: Option<String>,
    /// 已经折算过的最晚时刻：晚落地的调用登记的触发早于它就是过时的，见 [`Self::fold`]。
    watermark: Option<DateTime<Utc>>,
}

/// 有这台状态机的最低客户端版本。2.1.301 的可执行文件里已经整套接好（开关、主循环请求、界面运行态、输入框、
/// 清场五个入口，换掉压缩名后与 2.1.305 逐字相同）；303 那份抓包没有这个事件只是那天号的开关是关的
/// （`defaultValue: false`），不是版本没有。手头最早只有 301 的可执行文件，更早的版本没核实，按没有算。
pub(crate) const MIN_VERSION: &str = "2.1.301";

/// 官方 10 秒的输入节流（`zio`）。
const USER_INPUT_GAP_MS: i64 = 10_000;

impl Heartbeat {
    pub(crate) fn new(version_ok: bool, interactive: bool) -> Self {
        Heartbeat { on: version_ok, version_ok, interactive, ..Default::default() }
    }

    /// 按这个号此刻的开关重定报不报（官方 `WIt()`，每次触发都读）。
    pub(crate) fn refresh(&mut self, cred_id: i64) {
        self.on = self.version_ok && crate::proxy::dapper_dawn::account_enabled(cred_id);
    }

    /// 把这一批触发按时刻折算成心跳事件的（时刻, meta）。关着时一条不出，状态也不动。
    pub(crate) fn fold(
        &mut self,
        triggers: Vec<(DateTime<Utc>, Trigger)>,
    ) -> Vec<(DateTime<Utc>, Value)> {
        self.fold_tagged(triggers.into_iter().map(|(t, trig)| (t, trig, ())).collect())
            .into_iter()
            .map(|(t, meta, ())| (t, meta))
            .collect()
    }

    /// 同 [`Self::fold`]，每处触发带一个标签，报出来的事件带着它产生时那处的标签（提交前敲字的
    /// `user_input` 要挂到上一轮 prompt 上，调用方按标签挑上下文）。
    pub(crate) fn fold_tagged<T: Copy>(
        &mut self,
        mut triggers: Vec<(DateTime<Utc>, Trigger, T)>,
    ) -> Vec<(DateTime<Utc>, Value, T)> {
        let mut out = Vec::new();
        triggers.sort_by_key(|(t, _, _)| *t);
        if !self.on {
            // 关着也不是全不动：清场照样整台复位（官方 `nEt` 只把报那一条挂在开关上，`ZTt` 与 `aFn` 里的
            // `bAe()` 不看开关），最后一条主循环请求的 id 照样记（官方 `dV()` 是请求日志里的，与心跳无关）。
            // 运行态、输入与主循环请求的时刻不记（`ews` / `iFn` / `tEt` 开关关着直接返回）。
            for (now, trig, _) in triggers {
                self.watermark = Some(self.watermark.map_or(now, |t| t.max(now)));
                match trig {
                    Trigger::MainRequest(id) => self.last_main_request_id = Some(id),
                    Trigger::Reset(_) => {
                        self.reset();
                        self.last_main_request_id = None;
                    }
                    Trigger::Run(_) | Trigger::UserInput => {}
                }
            }
            return out;
        }
        for (now, trig, tag) in triggers {
            // 时间线是一条条调用拼起来的：子代理那条可能比主线程续轮晚落地，登记的时刻早于已经折算过的。
            // 运行态与输入不能倒回去改，过时的丢掉；主循环请求与清场照报，但记下的时刻只进不退。
            let stale = self.watermark.is_some_and(|t| now < t);
            self.watermark = Some(self.watermark.map_or(now, |t| t.max(now)));
            // 运行态与输入只由界面产生（官方 `ews` 挂在 REPL 的 hook 上、`iFn` 挂在输入框上），`-p` 两样都没有。
            let ui_only = matches!(trig, Trigger::Run(_) | Trigger::UserInput);
            if ui_only && (stale || !self.interactive) {
                continue;
            }
            let mut here = Vec::new();
            match trig {
                Trigger::MainRequest(id) => {
                    self.last_main_request_id = Some(id);
                    let prev = self.last_main_at;
                    self.last_main_at = Some(prev.map_or(now, |p| p.max(now)));
                    // 复位后的头一条：界面已经在干活的话，那段活的开头这时才报得出来。
                    if self.emit(&mut here, now, "main_request", None, Some(prev))
                        && prev.is_none()
                        && let Some(run) = self.run
                    {
                        self.edge(&mut here, now, run, true);
                    }
                }
                Trigger::Run(next) => {
                    if next == self.run {
                        continue;
                    }
                    let old = std::mem::replace(&mut self.run, next);
                    if let Some(old) = old {
                        self.edge(&mut here, now, old, false);
                    }
                    if let Some(next) = next {
                        self.edge(&mut here, now, next, true);
                    }
                }
                Trigger::UserInput => {
                    if self
                        .last_user_input
                        .is_some_and(|t| (now - t).num_milliseconds() < USER_INPUT_GAP_MS)
                    {
                        continue;
                    }
                    if self.emit(&mut here, now, "user_input", None, None) {
                        self.last_user_input = Some(now);
                    }
                }
                Trigger::Reset(kind) => {
                    self.emit(&mut here, now, kind, None, None);
                    self.reset();
                    // 官方清场之后紧跟着把最后一条主循环请求忘掉（`aFn` 里的 `bAe()`）。
                    self.last_main_request_id = None;
                }
            }
            out.extend(here.into_iter().map(|(t, meta)| (t, meta, tag)));
        }
        out
    }

    /// 一段运行态的开始或结束（官方 `due`）。
    fn edge(
        &mut self,
        out: &mut Vec<(DateTime<Utc>, Value)>,
        now: DateTime<Utc>,
        run: Run,
        start: bool,
    ) {
        let (kind, run) = match (run, start) {
            (Run::Work(_), true) => ("work_start", run),
            (Run::Work(_), false) => ("work_end", run),
            (Run::Awaiting(_), true) => ("awaiting_user_start", run),
            (Run::Awaiting(_), false) => ("awaiting_user_end", run),
        };
        self.emit(out, now, kind, Some(run), None);
    }

    /// 报一条（官方 `uue`）。复位后、头一条主循环请求之前不报；`main_prev` 是 `main_request` 那条用的
    /// 「上一条主循环请求」时刻（别的种类用当前记的那个）。
    fn emit(
        &mut self,
        out: &mut Vec<(DateTime<Utc>, Value)>,
        now: DateTime<Utc>,
        kind: &str,
        run: Option<Run>,
        main_prev: Option<Option<DateTime<Utc>>>,
    ) -> bool {
        if self.last_main_at.is_none() {
            return false;
        }
        let Some(request_id) = self.last_main_request_id.clone() else {
            self.reset();
            return false;
        };
        let since_main = main_prev.unwrap_or(self.last_main_at);
        let mut m = serde_json::Map::new();
        m.insert("kind".into(), json!(kind));
        match run {
            Some(Run::Work(w)) => {
                m.insert("work".into(), json!(w));
            }
            Some(Run::Awaiting(a)) => {
                m.insert("awaiting".into(), json!(a));
            }
            None => {}
        }
        if let Some(t) = self.last_event_at {
            m.insert("sinceLastEventMs".into(), json!((now - t).num_milliseconds().max(0)));
        }
        if let Some(t) = since_main {
            m.insert("sinceLastMainRequestMs".into(), json!((now - t).num_milliseconds().max(0)));
        }
        m.insert("requestId".into(), json!(request_id));
        if !self.interactive_logged {
            m.insert("interactive".into(), json!(self.interactive));
        }
        self.interactive_logged = true;
        self.last_event_at = Some(self.last_event_at.map_or(now, |t| t.max(now)));
        out.push((now, Value::Object(m)));
        true
    }

    /// 官方 `ZTt`：交互式与否、开关与最后那条 request-id 不在这里清。
    fn reset(&mut self) {
        self.last_main_at = None;
        self.last_event_at = None;
        self.last_user_input = None;
        self.run = None;
        self.interactive_logged = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_760_000_000_000 + ms).unwrap()
    }

    fn kinds(out: &[(DateTime<Utc>, Value)]) -> Vec<String> {
        out.iter()
            .map(|(_, m)| {
                let k = m["kind"].as_str().unwrap();
                match m.get("work").or(m.get("awaiting")) {
                    Some(w) => format!("{k}:{}", w.as_str().unwrap()),
                    None => k.to_string(),
                }
            })
            .collect()
    }

    /// `cap/auto-2.1.305-20261021-full` B1 前两轮：首条主循环请求之前什么都不报（输入、开工都压着），
    /// 头一条 `main_request` 带 `interactive` 并补上那段活的开头；续轮、收尾、下一次输入照常。
    #[test]
    fn follows_the_b1_capture() {
        let mut hb = Heartbeat::new(true, true);
        let out = hb.fold(vec![
            (at(0), Trigger::UserInput),
            (at(46), Trigger::Run(Some(TURN_RUNNING))),
            (at(2_062), Trigger::MainRequest("req_a".into())),
            (at(5_103), Trigger::MainRequest("req_b".into())),
            (at(5_187), Trigger::Run(None)),
            (at(11_267), Trigger::UserInput),
            (at(12_270), Trigger::Run(Some(TURN_RUNNING))),
        ]);
        assert_eq!(
            kinds(&out),
            [
                "main_request",
                "work_start:turn_running",
                "main_request",
                "work_end:turn_running",
                "user_input",
                "work_start:turn_running"
            ]
        );
        let first = &out[0].1;
        assert_eq!(
            first.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["kind", "requestId", "interactive"]
        );
        assert_eq!(first["interactive"], true);
        assert_eq!(out[1].1["sinceLastEventMs"], 0);
        assert_eq!(out[1].1["sinceLastMainRequestMs"], 0);
        assert!(out[1].1.get("interactive").is_none());
        assert_eq!(
            (&out[2].1["sinceLastEventMs"], &out[2].1["sinceLastMainRequestMs"]),
            (&json!(3_041), &json!(3_041))
        );
        assert_eq!(out[2].1["requestId"], "req_b");
        assert_eq!(
            (&out[4].1["sinceLastEventMs"], &out[4].1["sinceLastMainRequestMs"]),
            (&json!(6_080), &json!(6_164))
        );
        assert_eq!(out[5].1["requestId"], "req_b", "开工报的是当时最后一条主循环请求");
    }

    #[test]
    fn user_input_is_throttled_to_ten_seconds() {
        let mut hb = Heartbeat::new(true, true);
        hb.fold(vec![(at(0), Trigger::MainRequest("r".into()))]);
        let out = hb.fold(vec![
            (at(1_000), Trigger::UserInput),
            (at(10_900), Trigger::UserInput),
            (at(11_000), Trigger::UserInput),
        ]);
        assert_eq!(kinds(&out), ["user_input", "user_input"]);
        assert_eq!(out[1].0, at(11_000));
    }

    /// 弹框：干活 → 等用户（同一毫秒先收后开），点头后回来干活；被拒则停手。
    #[test]
    fn permission_prompts_switch_the_run() {
        let mut hb = Heartbeat::new(true, true);
        let out = hb.fold(vec![
            (at(0), Trigger::Run(Some(TURN_RUNNING))),
            (at(10), Trigger::MainRequest("r".into())),
            (at(21), Trigger::Run(Some(awaiting_for("Edit")))),
            (at(900), Trigger::Run(Some(TURN_RUNNING))),
            (at(2_000), Trigger::Run(Some(awaiting_for("AskUserQuestion")))),
            (at(3_000), Trigger::Run(None)),
        ]);
        assert_eq!(
            kinds(&out),
            [
                "main_request",
                "work_start:turn_running",
                "work_end:turn_running",
                "awaiting_user_start:permission prompt",
                "awaiting_user_end:permission prompt",
                "work_start:turn_running",
                "work_end:turn_running",
                "awaiting_user_start:input needed",
                "awaiting_user_end:input needed",
            ]
        );
    }

    /// 清场：报那一项后整台复位，直到下一条主循环请求才重新开口，`interactive` 再报一次。
    #[test]
    fn a_reset_silences_until_the_next_main_request() {
        let mut hb = Heartbeat::new(true, true);
        let out = hb.fold(vec![
            (at(0), Trigger::MainRequest("r1".into())),
            (at(10), Trigger::Run(Some(COMPACTING))),
            (at(20_000), Trigger::Reset("compaction")),
            (at(25_000), Trigger::UserInput),
            (at(26_000), Trigger::Run(Some(TURN_RUNNING))),
            (at(28_000), Trigger::MainRequest("r2".into())),
        ]);
        assert_eq!(
            kinds(&out),
            [
                "main_request",
                "work_start:compacting",
                "compaction",
                "main_request",
                "work_start:turn_running"
            ]
        );
        assert_eq!(out[2].1["requestId"], "r1");
        assert_eq!(out[3].1["interactive"], true);
        assert!(out[3].1.get("sinceLastMainRequestMs").is_none());
    }

    /// 乱序到达的主循环请求照报，但不把记下的时刻往回拨，之后的过时切换照样拦得住。
    #[test]
    fn a_late_main_request_does_not_rewind_the_clock() {
        let mut hb = Heartbeat::new(true, true);
        hb.fold(vec![
            (at(0), Trigger::Run(Some(TURN_RUNNING))),
            (at(100), Trigger::MainRequest("r1".into())),
        ]);
        hb.fold(vec![(at(500), Trigger::Run(Some(SUBAGENT_RUNNING)))]);
        let out = hb.fold(vec![
            (at(300), Trigger::MainRequest("r2".into())),
            (at(320), Trigger::Run(None)),
        ]);
        assert_eq!(kinds(&out), ["main_request"]);
        assert_eq!(out[0].1["sinceLastEventMs"], 0);
    }

    /// 晚落地的调用登记的运行态切换早于已报出的心跳：丢掉，不倒回去改。
    #[test]
    fn stale_run_changes_are_dropped() {
        let mut hb = Heartbeat::new(true, true);
        hb.fold(vec![
            (at(0), Trigger::Run(Some(TURN_RUNNING))),
            (at(100), Trigger::MainRequest("r".into())),
        ]);
        let out = hb.fold(vec![
            (at(50), Trigger::Run(Some(SUBAGENT_RUNNING))),
            (at(200), Trigger::Run(None)),
        ]);
        assert_eq!(kinds(&out), ["work_end:turn_running"]);
    }

    /// 标签跟着触发走：提交前敲字那条 `user_input` 带它登记时的标签，别的照旧。
    #[test]
    fn tags_follow_the_trigger_that_produced_the_event() {
        let mut hb = Heartbeat::new(true, true);
        let out = hb.fold_tagged(vec![
            (at(0), Trigger::MainRequest("r".into()), "call"),
            (at(5_000), Trigger::UserInput, "prev"),
            (at(6_000), Trigger::Run(Some(TURN_RUNNING)), "call"),
        ]);
        assert_eq!(
            out.iter().map(|(_, m, tag)| (m["kind"].as_str().unwrap(), *tag)).collect::<Vec<_>>(),
            [("main_request", "call"), ("user_input", "prev"), ("work_start", "call")]
        );
    }

    /// 关着期间清场：不报，但整台照样复位；再开之后下一条主循环请求重新带 `interactive`、不接旧计时。
    #[test]
    fn a_reset_while_off_still_resets() {
        let mut hb = Heartbeat::new(true, true);
        hb.fold(vec![(at(0), Trigger::MainRequest("r1".into()))]);
        hb.on = false;
        assert!(
            hb.fold(vec![
                (at(1_000), Trigger::MainRequest("r2".into())),
                (at(2_000), Trigger::Reset("conversation_clear")),
                (at(3_000), Trigger::MainRequest("r3".into())),
            ])
            .is_empty()
        );
        hb.on = true;
        let out = hb.fold(vec![(at(5_000), Trigger::MainRequest("r4".into()))]);
        assert_eq!(kinds(&out), ["main_request"]);
        assert_eq!(out[0].1["interactive"], true);
        assert!(out[0].1.get("sinceLastEventMs").is_none());
        assert!(out[0].1.get("sinceLastMainRequestMs").is_none());
        assert_eq!(out[0].1["requestId"], "r4");
    }

    /// 关着期间的主循环请求不记时刻但记 id：再开后头一条报得出来的事件带的是最新那条的 id。
    #[test]
    fn the_last_request_id_is_kept_while_off() {
        let mut hb = Heartbeat::new(true, true);
        hb.fold(vec![(at(0), Trigger::MainRequest("r1".into()))]);
        hb.on = false;
        hb.fold(vec![(at(1_000), Trigger::MainRequest("r2".into()))]);
        hb.on = true;
        let out = hb.fold(vec![(at(20_000), Trigger::UserInput)]);
        assert_eq!(kinds(&out), ["user_input"]);
        assert_eq!(out[0].1["requestId"], "r2");
        assert_eq!(out[0].1["sinceLastMainRequestMs"], 20_000, "时刻停在开关关掉之前那条");
    }

    #[test]
    fn off_emits_nothing() {
        let mut hb = Heartbeat::new(false, true);
        assert!(hb.fold(vec![(at(0), Trigger::MainRequest("r".into()))]).is_empty());
    }

    /// `-p`：只有 `main_request`（`interactive: false`）与退出那条 `session_end`；派子代理、输入这类
    /// 界面才有的触发一概不报。
    #[test]
    fn print_mode() {
        let mut hb = Heartbeat::new(true, false);
        let out = hb.fold(vec![
            (at(0), Trigger::MainRequest("r".into())),
            (at(500), Trigger::Run(Some(SUBAGENT_RUNNING))),
            (at(900), Trigger::UserInput),
            (at(1_000), Trigger::Run(None)),
            (at(1_293), Trigger::Reset("session_end")),
        ]);
        assert_eq!(kinds(&out), ["main_request", "session_end"]);
        assert_eq!(out[0].1["interactive"], false);
        assert_eq!(out[1].1["sinceLastEventMs"], 1_293);
    }
}
