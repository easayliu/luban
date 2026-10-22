//! 子代理进度摘要器的服务端配置（按会话主模型族各一个开关），收尾那条 `tengu_agent_summary_stopped`
//! 要报它。
//!
//! 可执行文件（2.1.303 `jn` / `Rd`）按会话主模型族挑开关：fable 读 `tengu_starry_wall`、opus 读
//! `tengu_groovy_ember`、其余读 `tengu_composed_plum`；值是 `{mode, refreshSeconds, maxInputTokens}`，
//! 逐项校验、不合法的项各自退回默认（整个不是对象则全取默认）：`mode` 只认 `full` / `short`（默认
//! `full`），`maxInputTokens` 夹到 1000~8000（默认 2000），`refreshSeconds` 每项夹到 30~240 秒、1~16 项
//! （默认 `[30]`）。2.1.307 多一项 `writer: {fable?, opus?}`（各是 1~100 字的模型名，不合法的那项丢掉，
//! 整个不是对象即 `{}`）：子代理是 opus / fable 时 short 摘要改由这个模型写，见 [`SummaryConfig::writer_for`]。
//! 没下发即 `source: fallback`、全取默认——`cap/auto-2.1.303-20261019-full` 那个号的 eval
//! 三个都没有，六条都报 `fallback`。下发了就报 eval 里那项的 `source`（`experiment` / `force` /
//! `defaultValue`）。只存在内存里，保活与会话握手拉 eval 时顺带记（[`note_eval`]）。
//!
//! **按时刻查。** 摘要器不是随时读配置：启动时读一次，之后每次计时器触发时重读（`z` 里 `G=jn(v)`），
//! 一条摘要按它发出那一刻的配置走，收尾报的是最后一次读到的那份。遥测是请求结束后才处理的，那时号的
//! eval 可能已经换过，所以每个号留一段带时刻的历史（[`note_eval_at`]），按「那一刻生效的那份」查
//! （[`config_at`]）。

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::SystemTime;

/// 三个模型族各自的开关名，按 [`flag_for_family`] 挑。
const FABLE_FLAG: &str = "tengu_starry_wall";
const OPUS_FLAG: &str = "tengu_groovy_ember";
const OTHER_FLAG: &str = "tengu_composed_plum";

/// 一个开关解析后的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SummaryConfig {
    pub(crate) short: bool,
    pub(crate) max_input_tokens: u64,
    /// 每次等多久（毫秒）：第 n 次取第 `min(n, 末项)` 项。
    pub(crate) waits_ms: Vec<u64>,
    /// eval 里那项的 `source`，没下发是 `fallback`。
    pub(crate) source: String,
    /// 2.1.307 的 `writer`：子代理是 fable / opus 时 short 摘要改由哪个模型写，没配是 `None`。
    pub(crate) writer_fable: Option<String>,
    pub(crate) writer_opus: Option<String>,
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self {
            short: false,
            max_input_tokens: 2_000,
            waits_ms: vec![30_000],
            source: "fallback".into(),
            writer_fable: None,
            writer_opus: None,
        }
    }
}

impl SummaryConfig {
    /// 子代理是这个族时配的 writer（官方 `he`：只有 fable / opus 读，其余没有）。
    pub(crate) fn writer_for(&self, agent_family: &str) -> Option<&str> {
        match agent_family {
            "fable" => self.writer_fable.as_deref(),
            "opus" => self.writer_opus.as_deref(),
            _ => None,
        }
    }

    /// 第 `calls` 次之后要等的毫秒数（官方 `se`）。
    pub(crate) fn wait_ms(&self, calls: u32) -> u64 {
        let i = (calls as usize).min(self.waits_ms.len().saturating_sub(1));
        self.waits_ms.get(i).copied().unwrap_or(30_000)
    }
}

/// 一个号的一份 eval 在什么时刻生效、三个开关各是什么。
type Snapshot = (SystemTime, HashMap<&'static str, SummaryConfig>);

/// 每个号最多留多少份历史：摘要器的子代理通常几分钟内跑完，留够近期的就行。
const HISTORY: usize = 32;

static CONFIGS: LazyLock<parking_lot::RwLock<HashMap<i64, Vec<Snapshot>>>> =
    LazyLock::new(Default::default);

/// 会话主模型族对应的开关名（官方 `Od`）。
pub(crate) fn flag_for_family(family: &str) -> &'static str {
    match family {
        "fable" => FABLE_FLAG,
        "opus" => OPUS_FLAG,
        _ => OTHER_FLAG,
    }
}

fn parse(feature: &serde_json::Value) -> SummaryConfig {
    let source = feature.get("source").and_then(|s| s.as_str()).unwrap_or("defaultValue");
    let mut cfg = SummaryConfig { source: source.to_string(), ..Default::default() };
    let Some(v) = feature.get("value").and_then(|v| v.as_object()) else { return cfg };
    cfg.short = v.get("mode").and_then(|m| m.as_str()) == Some("short");
    if let Some(n) = v.get("maxInputTokens").and_then(|n| n.as_f64()) {
        cfg.max_input_tokens = n.clamp(1_000.0, 8_000.0) as u64;
    }
    if let Some(a) = v.get("refreshSeconds").and_then(|a| a.as_array())
        && (1..=16).contains(&a.len())
        && let Some(secs) = a.iter().map(|s| s.as_i64()).collect::<Option<Vec<_>>>()
    {
        cfg.waits_ms = secs.into_iter().map(|s| s.clamp(30, 240) as u64 * 1_000).collect();
    }
    // 2.1.307：`string().min(1).max(100)`，按 JS 的长度（UTF-16 码元）数。
    if let Some(w) = v.get("writer").and_then(|w| w.as_object()) {
        let pick = |k: &str| {
            w.get(k)
                .and_then(|s| s.as_str())
                .filter(|s| (1..=100).contains(&s.encode_utf16().count()))
                .map(str::to_owned)
        };
        cfg.writer_fable = pick("fable");
        cfg.writer_opus = pick("opus");
    }
    cfg
}

/// 从一份 eval 的 `features` 里记下这个号的三个开关（整份替换：没有的就是没下发），从现在起生效。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    note_eval_at(cred_id, features, SystemTime::now());
}

/// 同 [`note_eval`]，指定生效时刻。
pub(crate) fn note_eval_at(
    cred_id: i64,
    features: &serde_json::Map<String, serde_json::Value>,
    at: SystemTime,
) {
    let map = [FABLE_FLAG, OPUS_FLAG, OTHER_FLAG]
        .into_iter()
        .filter_map(|k| features.get(k).map(|f| (k, parse(f))))
        .collect();
    let mut all = CONFIGS.write();
    let hist = all.entry(cred_id).or_default();
    // 按时刻有序插入（eval 到达一般是递增的，乱序的也放对位置）。
    let i = hist.partition_point(|(t, _)| *t <= at);
    hist.insert(i, (at, map));
    if hist.len() > HISTORY {
        hist.remove(0);
    }
}

/// 这个号在 `at` 那一刻、这个会话主模型族下生效的摘要器配置：取 `at` 之前（含）最近的一份 eval；
/// 那时还没见过 eval、或那份没下发这个开关都按默认（`fallback`）。
pub(crate) fn config_at(cred_id: i64, session_family: &str, at: SystemTime) -> SummaryConfig {
    let flag = flag_for_family(session_family);
    let all = CONFIGS.read();
    let Some(hist) = all.get(&cred_id) else { return SummaryConfig::default() };
    let i = hist.partition_point(|(t, _)| *t <= at);
    i.checked_sub(1).and_then(|i| hist[i].1.get(flag)).cloned().unwrap_or_default()
}

/// `(after, until]` 这段时间里这个号有没有新到的 eval：没有就说明这段时间摘要器重读到的配置与 `after`
/// 那一刻相同。
pub(crate) fn changed_between(cred_id: i64, after: SystemTime, until: SystemTime) -> bool {
    CONFIGS
        .read()
        .get(&cred_id)
        .is_some_and(|hist| hist.iter().any(|(t, _)| *t > after && *t <= until))
}

/// 摘要器收尾前最后一次读配置的时刻（官方 `z` / `ee`）。计时器每次触发都重读配置；一条摘要发出时
/// 等待计数 `j` 归零，`finally` 里（请求结束后）才排下一次，所以从最后一条摘要**结束**（`anchor`，一条
/// 都没发就是起跑那一刻）往后按官方的排法推：`Te = waits[min(calls, 末项)]`，每次等
/// `min(j < Te ? Te - j : Te, 30s)`、`j` 累加，每次触发按那一刻的配置取等待；之后没再发出摘要，说明那些
/// 触发都没到期或被跳过（跳过不归零 `j`）。落在 `stop` 之前（含）的最后一次触发就是答案；一次都没有时
/// 取 `last_read`（最后一条摘要发出那一刻，或起跑那一刻）。
pub(crate) fn last_read_at(
    cred_id: i64,
    session_family: &str,
    anchor: SystemTime,
    last_read: SystemTime,
    calls: u32,
    stop: SystemTime,
) -> SystemTime {
    use std::time::Duration;
    let mut read = last_read;
    let (mut t, mut j) = (anchor, 0u64);
    // 一个子代理跑不了几个小时；封顶防止异常时刻把循环拖长。
    for _ in 0..1_000 {
        let te = config_at(cred_id, session_family, t).wait_ms(calls);
        let wait = if j < te { te - j } else { te }.min(30_000);
        let next = t + Duration::from_millis(wait);
        if next > stop {
            break;
        }
        j += wait;
        t = next;
        read = t;
    }
    read
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_field_with_the_client_rules() {
        let features = serde_json::json!({
            "tengu_groovy_ember": {"value": {"mode": "short", "maxInputTokens": 99999,
                "refreshSeconds": [10, 60, 999]}, "source": "experiment"},
            "tengu_starry_wall": {"value": {"mode": "weird", "maxInputTokens": "x"}, "source": "force"},
        });
        note_eval(77_001, features.as_object().unwrap());
        let opus = config_at(77_001, "opus", SystemTime::now());
        assert!(opus.short);
        assert_eq!(opus.max_input_tokens, 8_000);
        assert_eq!(opus.waits_ms, [30_000, 60_000, 240_000]);
        assert_eq!((opus.wait_ms(0), opus.wait_ms(9)), (30_000, 240_000));
        assert_eq!(opus.source, "experiment");
        let fable = config_at(77_001, "fable", SystemTime::now());
        assert!(!fable.short, "认不出的 mode 退回 full");
        assert_eq!((fable.max_input_tokens, fable.source.as_str()), (2_000, "force"));
        assert_eq!(
            config_at(77_001, "haiku", SystemTime::now()),
            SummaryConfig::default(),
            "没下发是 fallback"
        );
        assert_eq!(
            config_at(77_002, "opus", SystemTime::now()),
            SummaryConfig::default(),
            "没见过 eval"
        );
    }

    /// 2.1.307 的 `writer`：按族各取一项，空串、超过 100 字、不是字符串的那项丢掉；整个不是对象即没配。
    #[test]
    fn parses_the_writer_per_family() {
        let long = "x".repeat(101);
        let features = serde_json::json!({
            "tengu_groovy_ember": {"value": {"mode": "short",
                "writer": {"opus": "claude-haiku-5-5", "fable": "", "sonnet": "claude-haiku-5-5"}}},
            "tengu_starry_wall": {"value": {"writer": {"fable": long, "opus": 3}}},
            "tengu_composed_plum": {"value": {"writer": "claude-haiku-5-5"}},
        });
        note_eval(77_006, features.as_object().unwrap());
        let opus = config_at(77_006, "opus", SystemTime::now());
        assert_eq!(opus.writer_for("opus"), Some("claude-haiku-5-5"));
        assert_eq!(opus.writer_for("fable"), None, "空串丢掉");
        assert_eq!(opus.writer_for("sonnet"), None, "只有 fable / opus 两项");
        let fable = config_at(77_006, "fable", SystemTime::now());
        assert_eq!((fable.writer_for("fable"), fable.writer_for("opus")), (None, None));
        let other = config_at(77_006, "haiku", SystemTime::now());
        assert_eq!(other.writer_for("opus"), None, "不是对象即 {{}}");
        assert_eq!(SummaryConfig::default().writer_for("opus"), None);
    }

    #[test]
    fn looks_up_the_config_in_force_at_a_moment() {
        use std::time::Duration;
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let at = |s: u64| t0 + Duration::from_secs(s);
        let short = serde_json::json!({"tengu_groovy_ember": {"value": {"mode": "short"}, "source": "force"}});
        let none = serde_json::json!({});
        note_eval_at(77_003, short.as_object().unwrap(), at(100));
        note_eval_at(77_003, none.as_object().unwrap(), at(200));
        assert!(!config_at(77_003, "opus", at(50)).short, "第一份 eval 之前按默认");
        assert!(config_at(77_003, "opus", at(100)).short);
        assert!(config_at(77_003, "opus", at(199)).short);
        assert!(!config_at(77_003, "opus", at(200)).short, "之后那份没下发");
    }

    #[test]
    fn replays_the_summary_timer_from_the_last_completion() {
        use std::time::Duration;
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let at = |s: u64| t0 + Duration::from_secs(s);
        // 没下发：等待 [30]。审查的例子：30 秒发、50 秒结束，下一次在 80 秒；65 秒收尾时最后一次读是 30 秒。
        assert_eq!(last_read_at(77_004, "opus", at(50), at(30), 1, at(65)), at(30));
        assert_eq!(last_read_at(77_004, "opus", at(50), at(30), 1, at(80)), at(80));
        assert_eq!(last_read_at(77_004, "opus", at(50), at(30), 1, at(139)), at(110));
        // 没发过摘要：从起跑起每 30 秒一次。
        assert_eq!(last_read_at(77_004, "opus", at(0), at(0), 0, at(29)), at(0));
        assert_eq!(last_read_at(77_004, "opus", at(0), at(0), 0, at(95)), at(90));
        // 等待 45 秒：30 秒那次没到期（j=30），再等 15 秒到 45 秒；之后每次 min(45, 30) = 30。
        let cfg = serde_json::json!({"tengu_groovy_ember": {"value": {"refreshSeconds": [45]}, "source": "force"}});
        note_eval_at(77_005, cfg.as_object().unwrap(), at(0));
        assert_eq!(last_read_at(77_005, "opus", at(0), at(0), 0, at(44)), at(30));
        assert_eq!(last_read_at(77_005, "opus", at(0), at(0), 0, at(74)), at(45));
        assert_eq!(last_read_at(77_005, "opus", at(0), at(0), 0, at(75)), at(75));
    }
}
