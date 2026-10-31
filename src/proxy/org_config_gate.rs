//! 服务端开关 `tengu_org_config_required`：组织配置那道闸（2.1.310 起的可执行文件里有）。官方启动时按它判这台机器
//! 要不要等组织的托管设置与策略拉回来再放行，并报两条遥测：
//!
//! - `tengu_org_config_gate_shadow`：影子判定，每个进程都报。`candidate_now` 按当前开关、`candidate_if_armed` 按
//!   「开关全开、两档都收、100%」判这台机器在不在闸内，不在就各带一个排除原因（`exclusion_now` / `exclusion_if_armed`）。
//! - `tengu_org_config_gate`：真判定，只有开关开着、这台机器又在闸内（[`Candidacy::Candidate`]）才报，紧跟一条
//!   `feature_ok` / `feature_sad{org_config_gate}`。
//!
//! 在不在闸内（2.1.314 `aye`）：订阅档得是 team / enterprise（取不到档位的已登录号按开关的第一档算），开关开着，档位在
//! `plans` 里，机器分桶（[`bucket`]：`sha256("org_config_gate:" + userID)` 前 8 个十六进制位模 100）小于 `percent`。
//! `userID` 就是 `metadata.user_id` 里的 `device_id`，所以同一个号上不同机器有进有出。
//!
//! 开关按号从 eval 里记（保活与会话启动握手拉 eval 时顺带记）。`cap/auto-2.1.314-20261030-full/00009` 起下发
//! `{enabled: true, plans: [team, enterprise], percent: 25, graceTtlHours: 24, budgetMs: 10000, …}`；10-28 之前是关、
//! 0%、96h（`cap/auto-2.1.309-*`）。只存在内存里，没见过 eval 的号不改模板。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 开关名。
const FEATURE: &str = "tengu_org_config_required";

/// 能进闸的两档（官方 `cPt`）。
const PLANS: [&str; 2] = ["team", "enterprise"];

/// `exemptReasons` 认得的原因名（官方 `X1` 那张表）。
const REASONS: &[&str] = &[
    "ok",
    "none_configured",
    "refused_credential",
    "refused_not_entitled",
    "refused_other",
    "blocked_by_intermediary",
    "unavailable_timeout",
    "unavailable_connect",
    "unavailable_server",
    "unavailable_rate_limited",
    "unavailable_credential_refresh",
    "unavailable_pending",
    "unavailable_settings_unusable",
    "client_no_credentials",
    "client_error",
    "not_attempted",
    "not_required",
];

/// 解析好的开关（官方 `vto` 的结果）。下发的不合法（不是对象、缺 `enabled`、比例越界……）整份按关（[`Flag::OFF`]）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Flag {
    pub(crate) enabled: bool,
    /// 收哪几档，只会是 [`PLANS`] 里的，按官方的先后。
    pub(crate) plans: Vec<&'static str>,
    /// 0 ~ 100。
    pub(crate) percent: f64,
    pub(crate) grace_ttl_hours: f64,
    pub(crate) budget_ms: f64,
}

impl Flag {
    /// 官方的默认值（`tJ`）：关、不收任何档、0%、宽限 96 小时、等 6 秒。
    pub(crate) const OFF: Self = Self {
        enabled: false,
        plans: Vec::new(),
        percent: 0.0,
        grace_ttl_hours: 96.0,
        budget_ms: 6000.0,
    };

    /// 照官方 `vto` 解析下发的值：可以是对象，也可以是 JSON 字串；`false` / `null` / 没下发即关。
    fn parse(v: Option<&serde_json::Value>) -> Self {
        Self::try_parse(v).unwrap_or(Self::OFF)
    }

    fn try_parse(v: Option<&serde_json::Value>) -> Option<Self> {
        let parsed;
        let v = match v {
            Some(serde_json::Value::String(s)) => {
                parsed = serde_json::from_str::<serde_json::Value>(s).ok()?;
                &parsed
            }
            Some(v) => v,
            None => return Some(Self::OFF),
        };
        if matches!(v, serde_json::Value::Null | serde_json::Value::Bool(false)) {
            return Some(Self::OFF);
        }
        let o = v.as_object()?;
        // 可选项官方都是 `r.x ?? 默认`：`null` 与没给一样取默认，只有 `enabled` 必须是布尔。
        let opt = |k: &str| o.get(k).filter(|x| !x.is_null());
        let enabled = o.get("enabled")?.as_bool()?;
        let plans = match opt("plans") {
            None => Vec::new(),
            Some(p) => p
                .as_array()?
                .iter()
                .map(|x| PLANS.iter().copied().find(|p| x.as_str() == Some(p)))
                .collect::<Option<Vec<_>>>()?,
        };
        let number = |k: &str, default: Option<f64>, lo: f64, hi: f64| match opt(k) {
            None => default.filter(|d| (lo..=hi).contains(d)),
            Some(x) => x.as_f64().filter(|x| x.is_finite() && (lo..=hi).contains(x)),
        };
        let percent = number("percent", (!enabled).then_some(0.0), 0.0, 100.0)?;
        let grace_ttl_hours = number("graceTtlHours", Some(96.0), 1.0, 720.0)?;
        let budget_ms = number("budgetMs", Some(6000.0), 1000.0, 30000.0)?;
        if opt("hostTokens").is_some_and(|x| !x.is_boolean()) {
            return None;
        }
        if let Some(r) = opt("exemptReasons")
            && !r.as_array()?.iter().all(|x| x.as_str().is_some_and(|x| REASONS.contains(&x)))
        {
            return None;
        }
        match opt("refuseNotEntitled") {
            Some(x) if !x.is_boolean() => return None,
            None if enabled => return None,
            _ => {}
        }
        Some(Self { enabled, plans, percent, grace_ttl_hours, budget_ms })
    }
}

/// 号 → 最近一份 eval 里的开关。
static FLAGS: LazyLock<parking_lot::RwLock<HashMap<i64, Flag>>> = LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的开关：一份 eval 是整份特性缓存，没有这一项就是关。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let flag = Flag::parse(features.get(FEATURE).and_then(|f| f.get("value")));
    FLAGS.write().insert(cred_id, flag);
}

/// 这个号最近一份 eval 里的开关；没见过 eval 的是 `None`。
pub(crate) fn flag(cred_id: i64) -> Option<Flag> {
    FLAGS.read().get(&cred_id).cloned()
}

/// 机器分桶（官方 `fbo`）：`sha256("org_config_gate:" + userID)` 十六进制前 8 位当整数，模 100。
pub(crate) fn bucket(device_id: &str) -> u32 {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(format!("org_config_gate:{device_id}").as_bytes());
    u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 100
}

/// 订阅档（`organization.organization_type`，`claude_team` 之类）→ 官方的 `subscriptionType`；认不出的是 `None`。
pub(crate) fn tier_of(org_type: Option<&str>) -> Option<&'static str> {
    let t = org_type?.trim().to_ascii_lowercase();
    let t = t.strip_prefix("claude_").unwrap_or(&t);
    ["max", "pro", "team", "enterprise"].into_iter().find(|x| *x == t)
}

/// 这台机器在不在闸内（官方 `aye` 的结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Candidacy {
    /// 在，按这一档算。
    Candidate(&'static str),
    /// 不在，排除原因。
    Excluded(&'static str),
}

/// 官方 `aye`，只走已登录（`claude.ai` 登录、第一方地址）这一支：订阅档取不到的按 `untiered_login`、算开关的第一档
/// （没有就 team）；不是 team / enterprise 的 `tier_unknown`；再看开关、档位、分桶。
pub(crate) fn candidacy(flag: &Flag, tier: Option<&'static str>, bucket: u32) -> Candidacy {
    let tier = match tier {
        Some(t) => t,
        None => flag.plans.first().copied().unwrap_or("team"),
    };
    if !PLANS.contains(&tier) {
        return Candidacy::Excluded("tier_unknown");
    }
    if !flag.enabled {
        return Candidacy::Excluded("flag_off");
    }
    if !flag.plans.contains(&tier) {
        return Candidacy::Excluded("tier_not_in_plans");
    }
    if f64::from(bucket) >= flag.percent {
        return Candidacy::Excluded("rollout_percent");
    }
    Candidacy::Candidate(tier)
}

/// 「开关全开」那一份（官方影子判定的 `candidate_if_armed`：`enabled`、两档、100%，其余照下发）。
pub(crate) fn armed(flag: &Flag) -> Flag {
    Flag { enabled: true, plans: PLANS.to_vec(), percent: 100.0, ..flag.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `cap/auto-2.1.314-20261030-full/00009` 的那份。
    fn served() -> serde_json::Value {
        json!({"enabled": true, "plans": ["team", "enterprise"], "percent": 25, "graceTtlHours": 24,
               "budgetMs": 10000, "exemptReasons": ["not_attempted"], "refuseNotEntitled": false,
               "hostTokens": false})
    }

    #[test]
    fn parses_the_served_flag_like_the_official_reader() {
        let f = Flag::parse(Some(&served()));
        assert_eq!(
            f,
            Flag {
                enabled: true,
                plans: vec!["team", "enterprise"],
                percent: 25.0,
                grace_ttl_hours: 24.0,
                budget_ms: 10000.0
            }
        );
        assert_eq!(Flag::parse(Some(&json!(served().to_string()))), f, "JSON 字串同样认");
        assert_eq!(Flag::parse(None), Flag::OFF);
        assert_eq!(Flag::parse(Some(&json!(false))), Flag::OFF);
        assert_eq!(Flag::parse(Some(&json!({"enabled": false}))), Flag::OFF, "关着可以不给比例");
        let bad = |patch: serde_json::Value| {
            let mut v = served();
            for (k, x) in patch.as_object().unwrap() {
                if x.is_null() {
                    v.as_object_mut().unwrap().remove(k);
                } else {
                    v[k] = x.clone();
                }
            }
            Flag::parse(Some(&v))
        };
        assert_eq!(bad(json!({"percent": null})), Flag::OFF, "开着却没给比例");
        assert_eq!(bad(json!({"percent": 101})), Flag::OFF);
        assert_eq!(bad(json!({"plans": ["team", "max"]})), Flag::OFF);
        assert_eq!(bad(json!({"graceTtlHours": 0})), Flag::OFF);
        assert_eq!(bad(json!({"budgetMs": 31000})), Flag::OFF);
        assert_eq!(bad(json!({"exemptReasons": ["whatever"]})), Flag::OFF);
        assert_eq!(
            bad(json!({"refuseNotEntitled": null})),
            Flag::OFF,
            "开着却没给 refuseNotEntitled"
        );
        assert_eq!(bad(json!({"graceTtlHours": null})).grace_ttl_hours, 96.0, "缺了取默认");
        // 显式 `null` 与没给一样（官方 `??`）：可选项取默认，开着照旧开着；只有 `enabled` 不能是 null。
        let with_null = |k: &str| {
            let mut v = served();
            v[k] = serde_json::Value::Null;
            Flag::parse(Some(&v))
        };
        let on = Flag::parse(Some(&served()));
        assert_eq!(with_null("graceTtlHours"), Flag { grace_ttl_hours: 96.0, ..on.clone() });
        assert_eq!(with_null("budgetMs"), Flag { budget_ms: 6000.0, ..on.clone() });
        assert_eq!(with_null("plans"), Flag { plans: Vec::new(), ..on.clone() });
        assert_eq!(with_null("hostTokens"), on);
        assert_eq!(with_null("exemptReasons"), on);
        assert_eq!(with_null("percent"), Flag::OFF, "开着时比例 null 同没给：不合法");
        assert_eq!(with_null("refuseNotEntitled"), Flag::OFF, "开着时 null 同没给：不合法");
        assert_eq!(with_null("enabled"), Flag::OFF);
        let off_nulls = json!({"enabled": false, "percent": null, "refuseNotEntitled": null,
                               "graceTtlHours": null, "budgetMs": null, "plans": null});
        assert_eq!(Flag::parse(Some(&off_nulls)), Flag::OFF, "关着时 null 都取默认");
    }

    #[test]
    fn candidacy_follows_tier_flag_plans_and_bucket() {
        let f = Flag::parse(Some(&served()));
        assert_eq!(candidacy(&f, Some("team"), 24), Candidacy::Candidate("team"));
        assert_eq!(candidacy(&f, Some("team"), 25), Candidacy::Excluded("rollout_percent"));
        assert_eq!(candidacy(&f, Some("max"), 0), Candidacy::Excluded("tier_unknown"));
        assert_eq!(candidacy(&f, None, 0), Candidacy::Candidate("team"), "取不到档位按第一档");
        let only_enterprise = Flag { plans: vec!["enterprise"], ..f.clone() };
        assert_eq!(
            candidacy(&only_enterprise, Some("team"), 0),
            Candidacy::Excluded("tier_not_in_plans")
        );
        assert_eq!(candidacy(&Flag::OFF, Some("team"), 0), Candidacy::Excluded("flag_off"));
        assert_eq!(candidacy(&armed(&Flag::OFF), Some("team"), 99), Candidacy::Candidate("team"));
        assert_eq!(
            candidacy(&armed(&Flag::OFF), Some("pro"), 0),
            Candidacy::Excluded("tier_unknown")
        );
    }

    #[test]
    fn bucket_matches_the_official_hash() {
        // node：`crypto.createHash("sha256").update("org_config_gate:" + id).digest("hex").slice(0, 8)` 取整模 100。
        // 抓包那台机器（`cap/auto-2.1.314-20261030-full` 的 `device_id`）是 21，比 25 小，所以那 24 条影子判定都在闸内。
        assert_eq!(bucket("b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d"), 21);
        assert_eq!(bucket(&"a".repeat(64)), 73);
        assert_eq!(tier_of(Some("claude_team")), Some("team"));
        assert_eq!(tier_of(Some("claude_max")), Some("max"));
        assert_eq!(tier_of(Some("claude_free")), None);
        assert_eq!(tier_of(None), None);
    }

    #[test]
    fn remembers_each_accounts_eval() {
        let id = -93_801;
        assert_eq!(flag(id), None);
        let features = json!({"tengu_org_config_required": {"value": served()}});
        note_eval(id, features.as_object().unwrap());
        assert_eq!(flag(id).unwrap().percent, 25.0);
        note_eval(id, json!({}).as_object().unwrap());
        assert_eq!(flag(id), Some(Flag::OFF), "整份特性缓存里没有这一项就是关");
    }
}
