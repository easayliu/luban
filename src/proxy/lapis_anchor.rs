//! 服务端下发的 `<total_tokens>` 提醒那一族开关：档位（`tengu_lapis_anchor`）、预算（`tengu_lapis_anchor_budget`）、
//! 用户输入之后跟不跟（`tengu_lapis_anchor_user_turn`）。
//!
//! 官方客户端每个进程各算一次（2.1.311 `snn()` / `RJ()` / `ann()`，2.1.310 同逻辑），都是先看 bootstrap 回包的
//! `client_data`，没有再看 eval 的同名特性，再没有取默认：
//! - 档位：`client_data` 里合法的模式串照用，`false` 记作 `off`；eval 那条（`rnn()`）只认合法的模式串，`false` 在那里
//!   不算 `off`；默认 `padded-countdown`。
//! - 预算：`client_data` 里的正整数；eval 那条要正的有限数（`k(…, 15000000)`）；默认 1500 万。
//! - 用户输入之后跟不跟：`client_data` 里的布尔；eval 那条原样按 JS 真假用（`k(…, true)`）；默认跟。
//!
//! 每一项取自 `client_data` 时，算它那一刻报一条 `tengu_lapis_anchor_from_client_data`，内容分别是 `{mode}`、
//! `{budget}`、`{userTurn}`（见 [`SessionMode::client_data_events`]）。
//!
//! 模式是 `off` 时 system 里那段、首轮环境说明里那段与每轮跟在用户消息后面的 `<total_tokens>` 一概不发。用户输入
//! 之后不跟时，只在工具结果之后的续轮跟，倒数也不在用户输入处重新锚定（官方 `twr` 里 `Vt` 为假就不调
//! `reanchorTaskBudget`），从会话起点一路往下减。
//!
//! 2026-10-27 起服务端在 opus-5-5 / sonnet-5-5 的 bootstrap（`?model=…`）里下发 `"off"`，haiku-5-5、haiku-4.5、
//! fable 的没有这一项（`cap/auto-2.1.311-20261027-full` 全部 bootstrap 回包；10-24 那份全都没有）。抓包里 opus /
//! sonnet 会话的 system 少了 `<total_tokens>…</total_tokens>` 那 51 字节，haiku / fable 会话照旧带。预算与用户轮
//! 两项至今没下发过，`{budget}` / `{userTurn}` 两条与它们在事件流里的先后按代码推（同一条附件回调里先算用户轮、
//! 再算预算），没有样本。
//!
//! bootstrap 按号、按请求的模型记（`client_data` 随 `model` 参数变，会话启动握手与首次保活拉 bootstrap 时顺带记，
//! 见 [`crate::oauth`]），eval 按号记，同 [`super::heron_brook`]。bootstrap 那份随整份 `client_data` 落库、启动时读回
//! （[`super::client_data`]，同官方把它存在本机配置里），重启后进行中的会话不会中途换档；eval 那份只在内存。
//! 两处都没见过的（号, 模型）按默认那档。
//! luban 只做得出 `padded-countdown` 与 `off` 两档，下发了别的档（`infinite` / `fixed` / `countdown`）时照
//! `padded-countdown` 发；eval 给的预算不是整数时按没下发算（倒数按整数记）。

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// 档位的特性名（`client_data` 里与 eval 里同名，下两个同）。
const FEATURE: &str = "tengu_lapis_anchor";
/// 预算。
const BUDGET_FEATURE: &str = "tengu_lapis_anchor_budget";
/// 用户输入之后跟不跟。
const USER_TURN_FEATURE: &str = "tengu_lapis_anchor_user_turn";

/// 默认预算（官方 `tnn`）：1500 万，同 [`super::session_link::TOTAL_TOKENS_BUDGET`]。
pub(crate) const DEFAULT_BUDGET: u64 = super::session_link::TOTAL_TOKENS_BUDGET;

/// 提醒模式（官方 `vtr` 那五档）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Infinite,
    Fixed,
    Countdown,
    PaddedCountdown,
}

impl Mode {
    /// bootstrap `client_data` 里那一项（官方 `snn()`）：合法的模式串照用，`false` 记作 `off`，别的当没下发。
    /// eval 那份只认合法的串，见 [`Delivered::of_eval`]。
    fn parse(v: Option<&Value>) -> Option<Self> {
        match v? {
            Value::Bool(false) => Some(Self::Off),
            Value::String(s) => match s.as_str() {
                "off" => Some(Self::Off),
                "infinite" => Some(Self::Infinite),
                "fixed" => Some(Self::Fixed),
                "countdown" => Some(Self::Countdown),
                "padded-countdown" => Some(Self::PaddedCountdown),
                _ => None,
            },
            _ => None,
        }
    }

    /// 遥测里报的模式名。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Infinite => "infinite",
            Self::Fixed => "fixed",
            Self::Countdown => "countdown",
            Self::PaddedCountdown => "padded-countdown",
        }
    }
}

/// 正整数（JSON 里写成 `1.5e7` 这种浮点的整数值也算）。
fn positive_integer(v: &Value) -> Option<u64> {
    v.as_u64().filter(|n| *n > 0).or_else(|| {
        v.as_f64()
            .filter(|f| {
                f.is_finite() && *f > 0.0 && f.fract() == 0.0 && *f < 9_007_199_254_740_992.0
            })
            .map(|f| f as u64)
    })
}

/// JS 的真假：`false`、`0`、空串、`null` 为假，别的都真。
fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 一处来源（bootstrap 的 `client_data`，或 eval）给出的这一族三项；没下发或不合法的为 `None`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Delivered {
    mode: Option<Mode>,
    budget: Option<u64>,
    user_turn: Option<bool>,
}

impl Delivered {
    /// `client_data` 那份（官方 `snn()` / `Utr()` / `jtr()` 读 `client_data` 的那一段）。
    fn of_client_data(data: &Value) -> Self {
        Self {
            mode: Mode::parse(data.get(FEATURE)),
            budget: data.get(BUDGET_FEATURE).and_then(positive_integer),
            user_turn: data.get(USER_TURN_FEATURE).and_then(Value::as_bool),
        }
    }

    /// eval 那份：一份 eval 是整份特性缓存，没有的项即没下发。取值照官方 `k(名, 默认)`：没有这一项、或 `value`
    /// 为 `null` 才落到默认。
    fn of_eval(features: &serde_json::Map<String, Value>) -> Self {
        let value = |k: &str| features.get(k).and_then(|f| f.get("value")).filter(|v| !v.is_null());
        Self {
            // `rnn()`：只认合法的模式串，`false` 在这里不算 `off`（只有 `client_data` 的 `false` 才是）。
            mode: Mode::parse(value(FEATURE).filter(|v| v.is_string())),
            budget: value(BUDGET_FEATURE).and_then(positive_integer),
            user_turn: value(USER_TURN_FEATURE).map(js_truthy),
        }
    }
}

/// 一个会话定下来的那一档：三项各自的取值，以及是不是 bootstrap 的 `client_data` 给的（是的话报那条事件）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionMode {
    pub(crate) mode: Mode,
    pub(crate) from_client_data: bool,
    /// `<total_tokens>` 的预算：system 那段与首轮环境说明写的就是它，每轮倒数从它往下减。
    pub(crate) budget: u64,
    pub(crate) budget_from_client_data: bool,
    /// 普通用户输入之后也跟一条提醒、并在那里重新锚定倒数。
    pub(crate) user_turn: bool,
    pub(crate) user_turn_from_client_data: bool,
}

/// 客户端自己的 system 与 messages 里实际有没有提醒、最大的那个数（遥测核真实 CC 透传、billing-only 用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Observed {
    pub(crate) reminders: bool,
    pub(crate) max: Option<u64>,
}

impl SessionMode {
    /// 什么都没下发时那一档（官方默认）。探测这类用不上提醒的请求也拿它占位。
    pub(crate) const DEFAULT: Self = Self {
        mode: Mode::PaddedCountdown,
        from_client_data: false,
        budget: DEFAULT_BUDGET,
        budget_from_client_data: false,
        user_turn: true,
        user_turn_from_client_data: false,
    };

    /// 发不发 `<total_tokens>`。
    pub(crate) fn reminders(self) -> bool {
        self.mode != Mode::Off
    }

    /// 这一轮末尾跟不跟一条提醒：工具结果之后的续轮都跟；普通用户输入之后看 [`Self::user_turn`]。
    pub(crate) fn reminds_after(self, regular_prompt: bool) -> bool {
        self.reminders() && (!regular_prompt || self.user_turn)
    }

    /// 会话首次输入时官方报的那几条 `tengu_lapis_anchor_from_client_data` 的内容，按官方算这几项的先后：档位、用户轮、
    /// 预算。只报 `client_data` 给的那几项；官方在档位是 `off` 时不再去算后两项，预算只在 `padded-countdown` 档才算。
    ///
    /// `observed` 是 system 与 messages 归客户端自己的会话（真实 CC 透传、billing-only）里实际看到的提醒：体与这一档
    /// 对不上（号下发 `off`、客户端却照默认发了提醒，或反过来）就当这个客户端没读到号的 `client_data`，一条都不报；
    /// 用户轮只在与客户端的默认（跟）一致时报，预算只在体里那个最大的数正好是它时报。完整模拟（体是 luban 按这一档
    /// 造的）传 `None`。
    pub(crate) fn client_data_events(self, observed: Option<Observed>) -> Vec<Value> {
        let mut out = Vec::new();
        if observed.is_some_and(|o| o.reminders != self.reminders()) {
            return out;
        }
        if self.from_client_data {
            out.push(json!({ "mode": self.mode.as_str() }));
        }
        if !self.reminders() {
            return out;
        }
        if self.user_turn_from_client_data && (observed.is_none() || self.user_turn) {
            out.push(json!({ "userTurn": self.user_turn }));
        }
        if self.mode == Mode::PaddedCountdown
            && self.budget_from_client_data
            && observed.is_none_or(|o| o.max == Some(self.budget))
        {
            out.push(json!({ "budget": self.budget }));
        }
        out
    }
}

/// `(号, 模型键)`。
type ClientDataKey = (i64, String);

/// `(号, 模型)` → bootstrap `client_data` 里这一族三项。
static CLIENT_DATA: LazyLock<parking_lot::RwLock<HashMap<ClientDataKey, Delivered>>> =
    LazyLock::new(Default::default);
/// 号 → eval 里这一族三项。
static EVAL: LazyLock<parking_lot::RwLock<HashMap<i64, Delivered>>> =
    LazyLock::new(Default::default);

/// 模型键：小写、去掉 `[1m]`（bootstrap 的 `model` 参数是规范名）。
fn model_key(model: &str) -> String {
    let m = model.trim().to_ascii_lowercase();
    m.strip_suffix("[1m]").unwrap_or(&m).to_string()
}

/// 记下一份 bootstrap 回包里的 `client_data`（[`super::client_data`] 拆好交过来，回包里没有这一项时是空对象）。
pub(crate) fn note_client_data(cred_id: i64, model: &str, data: &Value) {
    CLIENT_DATA.write().insert((cred_id, model_key(model)), Delivered::of_client_data(data));
}

/// 忘掉这个号这个模型的那份（[`super::client_data`] 按号限条数，挤出去的一并清掉）。
pub(crate) fn forget_client_data(cred_id: i64, model: &str) {
    CLIENT_DATA.write().remove(&(cred_id, model_key(model)));
}

/// 从一份 eval 的 `features` 里记下这个号的取值（见 [`Delivered::of_eval`]）。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, Value>) {
    EVAL.write().insert(cred_id, Delivered::of_eval(features));
}

/// 这个号用 `model` 当主模型时现在该是哪一档（不钉住，见 [`session_mode`]）。
fn current(cred_id: i64, model: &str) -> SessionMode {
    let client = CLIENT_DATA.read().get(&(cred_id, model_key(model))).copied().unwrap_or_default();
    let eval = EVAL.read().get(&cred_id).copied().unwrap_or_default();
    fn pick<T: Copy>(client: Option<T>, eval: Option<T>, default: T) -> (T, bool) {
        match client {
            Some(v) => (v, true),
            None => (eval.unwrap_or(default), false),
        }
    }
    let (mode, from_client_data) = pick(client.mode, eval.mode, Mode::PaddedCountdown);
    let (budget, budget_from_client_data) = pick(client.budget, eval.budget, DEFAULT_BUDGET);
    let (user_turn, user_turn_from_client_data) = pick(client.user_turn, eval.user_turn, true);
    SessionMode {
        mode,
        from_client_data,
        budget,
        budget_from_client_data,
        user_turn,
        user_turn_from_client_data,
    }
}

/// 钉住的那一档与上回问到的时刻。
type Pinned = (SessionMode, Instant);

/// 会话钉住的表上限：满了先清一天没见的，仍超过一半就只留最近见过的一半。
const PIN_CAP: usize = 4096;
/// 一个会话闲置这么久再来就按新进程重新定。
const PIN_IDLE: Duration = Duration::from_secs(24 * 60 * 60);

static PINS: LazyLock<parking_lot::Mutex<HashMap<(i64, String), Pinned>>> =
    LazyLock::new(Default::default);

/// 这个会话的那一档：同一个（号，会话 id）头一回问时按当时见过的 bootstrap / eval 定下来，之后不再变——
/// 官方一个进程只算一次（中途换模型、服务端改了下发都不跟），也免得中途改了 system 把缓存整段打断。
///
/// 只该由会话**主线程**的请求来钉：官方按进程启动时的主模型拉 bootstrap、读那个模型的 `client_data`，同一会话里的
/// 标题、侧查询用的是别的模型（haiku），让它们抢先钉住就把主线程定成了那个模型的档。别的请求用
/// [`peek_session_mode`]。
pub(crate) fn session_mode(cred_id: i64, session_id: &str, model: &str) -> SessionMode {
    session_mode_at(cred_id, session_id, model, true)
}

/// 同 [`session_mode`]，但会话还没钉时只看现在该是哪一档、不钉住（标题、侧查询这类非主线程请求用）。
pub(crate) fn peek_session_mode(cred_id: i64, session_id: &str, model: &str) -> SessionMode {
    session_mode_at(cred_id, session_id, model, false)
}

fn session_mode_at(cred_id: i64, session_id: &str, model: &str, pin: bool) -> SessionMode {
    let now = Instant::now();
    let mut map = PINS.lock();
    let key = (cred_id, session_id.to_string());
    if !pin {
        return match map.get_mut(&key) {
            Some(entry) if now.duration_since(entry.1) < PIN_IDLE => {
                entry.1 = now;
                entry.0
            }
            _ => current(cred_id, model),
        };
    }
    if map.len() >= PIN_CAP {
        map.retain(|_, (_, seen)| now.duration_since(*seen) < PIN_IDLE);
        if map.len() >= PIN_CAP / 2 {
            let mut seen: Vec<Instant> = map.values().map(|(_, s)| *s).collect();
            seen.sort_unstable();
            let cut = seen[seen.len() - PIN_CAP / 2];
            map.retain(|_, (_, s)| *s >= cut);
        }
    }
    let entry = map.entry(key).or_insert_with(|| (current(cred_id, model), now));
    if now.duration_since(entry.1) >= PIN_IDLE {
        entry.0 = current(cred_id, model);
    }
    entry.1 = now;
    entry.0
}

/// system 模板第四块（[`crate::proxy::render_system_rest`] 渲染出来的）末尾那段 `<total_tokens>`：模板里写的是默认预算。
/// 这一档不发提醒时连同它前面那个空行一起去掉（`cap/auto-2.1.311-20261027-full` 的 opus / sonnet 会话就是这样），
/// 预算不是默认值时换成这一档的预算，别的原样。
pub(crate) fn fit_system_rest(rest: String, m: SessionMode) -> String {
    let line = format!("<total_tokens>{DEFAULT_BUDGET} tokens left</total_tokens>");
    let Some(at) = rest.rfind(&line) else { return rest };
    let mut out = rest;
    if !m.reminders() {
        let from = if out[..at].ends_with("\n\n") { at - 2 } else { at };
        out.replace_range(from..at + line.len(), "");
    } else if m.budget != DEFAULT_BUDGET {
        out.replace_range(
            at..at + line.len(),
            &format!("<total_tokens>{} tokens left</total_tokens>", m.budget),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(m: SessionMode) -> (Mode, bool) {
        (m.mode, m.from_client_data)
    }

    #[test]
    fn follows_what_the_server_delivered() {
        let id = -93_311;
        let fresh = current(id, "claude-opus-5-5");
        assert_eq!(fresh, SessionMode::DEFAULT);
        assert!(fresh.reminders(), "没见过 bootstrap / eval 按默认那档发");

        // `cap/auto-2.1.311-20261027-full`：opus / sonnet 的 bootstrap 下发 "off"，haiku-5-5 的没有。
        let off = json!({ "cedar_basin": "2027-08-31", FEATURE: "off" });
        note_client_data(id, "claude-opus-5-5", &off);
        let plain = json!({ "cedar_basin": "2027-08-31" });
        note_client_data(id, "claude-haiku-5-5", &plain);
        let got = current(id, "claude-opus-5-5[1m]");
        assert_eq!(mode_of(got), (Mode::Off, true));
        assert!(!got.reminders());
        assert!(current(id, "claude-haiku-5-5").reminders(), "client_data 按模型记");

        // eval 只在 client_data 没给时起作用，且只认合法的模式串（官方 `rnn()`）：`false` 在这里不算 off。
        let eval = json!({ FEATURE: { "value": "off" } });
        note_eval(id, eval.as_object().unwrap());
        assert_eq!(mode_of(current(id, "claude-haiku-5-5")), (Mode::Off, false));
        let eval = json!({ FEATURE: { "value": false } });
        note_eval(id, eval.as_object().unwrap());
        assert_eq!(current(id, "claude-haiku-5-5").mode, Mode::PaddedCountdown);
        // client_data 那份的 `false` 才记作 off（官方 `snn()`）。
        note_client_data(id, "claude-haiku-5-5", &json!({ FEATURE: false }));
        assert_eq!(mode_of(current(id, "claude-haiku-5-5")), (Mode::Off, true));

        // 不认识的值当没下发。
        note_client_data(id, "claude-fable-5-1", &json!({ FEATURE: "sometimes" }));
        assert!(!current(id, "claude-fable-5-1").from_client_data);

        // 挤出去的那份忘掉，回到 eval / 默认。
        forget_client_data(id, "claude-opus-5-5");
        assert_eq!(mode_of(current(id, "claude-opus-5-5")), (Mode::PaddedCountdown, false));
    }

    /// 预算与用户轮两项：`client_data` 优先，eval 兜底，各自独立于档位；取值规则照官方 `Utr()` / `jtr()`。
    #[test]
    fn budget_and_user_turn_follow_client_data_then_eval() {
        let id = -93_313;
        let model = "claude-haiku-5-5";
        let eval = json!({
            BUDGET_FEATURE: { "value": 2_000_000 },
            USER_TURN_FEATURE: { "value": 0 },
        });
        note_eval(id, eval.as_object().unwrap());
        let m = current(id, model);
        assert_eq!((m.budget, m.budget_from_client_data), (2_000_000, false));
        assert_eq!(
            (m.user_turn, m.user_turn_from_client_data),
            (false, false),
            "eval 按 JS 真假用"
        );

        note_client_data(
            id,
            model,
            &json!({ BUDGET_FEATURE: 500_000.0, USER_TURN_FEATURE: true, FEATURE: "padded-countdown" }),
        );
        let m = current(id, model);
        assert_eq!((m.budget, m.budget_from_client_data), (500_000, true), "整数值的浮点也算");
        assert_eq!((m.user_turn, m.user_turn_from_client_data), (true, true));
        assert_eq!(mode_of(m), (Mode::PaddedCountdown, true));

        // client_data 里不合法的（非整数、非正、不是布尔）当没下发，落回 eval。
        note_client_data(id, model, &json!({ BUDGET_FEATURE: 1.5, USER_TURN_FEATURE: "no" }));
        let m = current(id, model);
        assert_eq!((m.budget, m.user_turn), (2_000_000, false));
        // eval 的 `null` 与没有这一项一样落到默认。
        let eval =
            json!({ BUDGET_FEATURE: { "value": null }, USER_TURN_FEATURE: { "value": null } });
        note_eval(id, eval.as_object().unwrap());
        let m = current(id, model);
        assert_eq!((m.budget, m.user_turn), (DEFAULT_BUDGET, true));
    }

    /// 首次输入那几条事件：只报 `client_data` 给的，先后是档位、用户轮、预算；`off` 不再算后两项；客户端自己的体与
    /// 这一档对不上就一条都不报。
    #[test]
    fn client_data_events_follow_the_official_order() {
        let all = SessionMode {
            mode: Mode::PaddedCountdown,
            from_client_data: true,
            budget: 500_000,
            budget_from_client_data: true,
            user_turn: false,
            user_turn_from_client_data: true,
        };
        assert_eq!(
            all.client_data_events(None),
            vec![
                json!({ "mode": "padded-countdown" }),
                json!({ "userTurn": false }),
                json!({ "budget": 500_000 })
            ]
        );
        let off = SessionMode { mode: Mode::Off, ..all };
        assert_eq!(off.client_data_events(None), vec![json!({ "mode": "off" })]);
        let fixed = SessionMode { mode: Mode::Fixed, ..all };
        assert_eq!(fixed.client_data_events(None).len(), 2, "预算只在 padded-countdown 档才算");
        assert!(SessionMode::DEFAULT.client_data_events(None).is_empty());

        // 客户端自己的体：号下发 off、体里照发提醒，对不上。
        let seen = |reminders, max| Some(Observed { reminders, max });
        assert!(off.client_data_events(seen(true, Some(DEFAULT_BUDGET))).is_empty());
        assert_eq!(off.client_data_events(seen(false, None)), vec![json!({ "mode": "off" })]);
        // 预算对得上体里的数才报；用户轮只在与客户端默认一致（跟）时报。
        let on = SessionMode { user_turn: true, ..all };
        assert_eq!(on.client_data_events(seen(true, Some(500_000))).len(), 3);
        assert_eq!(on.client_data_events(seen(true, Some(DEFAULT_BUDGET))).len(), 2);
        assert_eq!(all.client_data_events(seen(true, Some(500_000))).len(), 2);
    }

    #[test]
    fn a_session_keeps_the_mode_it_started_with() {
        let id = -93_312;
        let first = session_mode(id, "s-1", "claude-sonnet-5-5");
        assert!(first.reminders());
        note_client_data(id, "claude-sonnet-5-5", &json!({ FEATURE: "off" }));
        assert_eq!(session_mode(id, "s-1", "claude-sonnet-5-5"), first, "已开的会话不跟");
        assert!(!session_mode(id, "s-2", "claude-sonnet-5-5").reminders(), "新会话跟");
    }

    /// 非主线程的请求只看不钉：标题那条（haiku）抢在主线程之前到，主线程照样按自己的模型定档；钉住之后它们也拿钉住的那档。
    #[test]
    fn side_requests_do_not_pin_the_session() {
        let id = -93_314;
        note_client_data(id, "claude-opus-5-5", &json!({ FEATURE: "off" }));
        assert!(
            peek_session_mode(id, "s-1", "claude-haiku-4-5").reminders(),
            "没钉时按它自己的模型看"
        );
        assert!(!session_mode(id, "s-1", "claude-opus-5-5").reminders(), "主线程按 opus 那份定");
        assert!(
            !peek_session_mode(id, "s-1", "claude-haiku-4-5").reminders(),
            "钉住之后跟着钉住的那档"
        );
    }

    #[test]
    fn fits_the_trailing_reminder_of_the_system_rest() {
        let n = DEFAULT_BUDGET;
        let rest =
            || format!("…an exhaustive survey\n\n<total_tokens>{n} tokens left</total_tokens>");
        let off = SessionMode { mode: Mode::Off, ..SessionMode::DEFAULT };
        assert_eq!(fit_system_rest(rest(), off), "…an exhaustive survey");
        assert_eq!(fit_system_rest(rest(), SessionMode::DEFAULT), rest());
        let small = SessionMode { budget: 500_000, ..SessionMode::DEFAULT };
        assert_eq!(
            fit_system_rest(rest(), small),
            "…an exhaustive survey\n\n<total_tokens>500000 tokens left</total_tokens>"
        );
        assert_eq!(fit_system_rest("no reminder".into(), off), "no reminder");
    }
}
