//! 官方定价估算：按模型价目表，从 token 用量估算等价 API 费用（USD）。
//!
//! 订阅账号本身按订阅计费，此处仅用于「等价 API 费用」的参考统计。
//! 价目对齐官方 <https://platform.claude.com/docs/en/about-claude/pricing>
//! （每百万 token，MTok，美元）。相对基础输入价的倍率（官方所有模型通用）：
//! - 缓存写（5 分钟）：×1.25；缓存写（1 小时）：×2.0；缓存读：×0.10。
//!
//! 不做 >200K 长上下文加价——官方明确 Opus 5 / Sonnet 5 等含 1M 上下文按标准价计，
//! 故带 `[1m]` 后缀的模型名与裸 id 同价。
//!
//! Sonnet 5 有引导优惠：2026-08-31 前 $2/$10，9-01 起恢复 $3/$15，按当前时间自动切换。

use std::time::{SystemTime, UNIX_EPOCH};

/// Sonnet 5 引导优惠截止（2026-09-01T00:00:00Z，Unix 秒）。此刻起恢复标准价。
const SONNET5_INTRO_END: u64 = 1_788_220_800;

/// 每百万 token 的基础价（美元）。缓存价由基础输入价按倍率派生。
struct Rate {
    input: f64,
    output: f64,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 按模型名匹配价目（未知模型返回 None）。价格对齐官方定价表。
fn rate_for(model: &str) -> Option<Rate> {
    let m = model.to_ascii_lowercase();
    if m.contains("fable") || m.contains("mythos") {
        Some(Rate { input: 10.0, output: 50.0 })
    } else if m.contains("opus") {
        // 老 Opus（3 / 4.0 / 4.1）为 $15/$75；Opus 4.5 及以后（含 Opus 5）统一 $5/$25。
        // Opus 5 的 1M 上下文是默认档、不加价，故 `claude-opus-5[1m]` 与裸 id 同价。
        if m.contains("3-opus") || m.contains("opus-4-1") || m.contains("opus-4-2025") {
            Some(Rate { input: 15.0, output: 75.0 })
        } else {
            Some(Rate { input: 5.0, output: 25.0 })
        }
    } else if m.contains("haiku") {
        // Haiku 3.5 更便宜；Haiku 4.5 为 1/5。
        if m.contains("haiku-3") || m.contains("3-5-haiku") || m.contains("3.5") {
            Some(Rate { input: 0.80, output: 4.0 })
        } else {
            Some(Rate { input: 1.0, output: 5.0 })
        }
    } else if m.contains("sonnet") {
        // Sonnet 5 引导优惠期内 $2/$10，其余（含优惠到期、Sonnet 4.x）$3/$15。
        if m.contains("sonnet-5") && now_secs() < SONNET5_INTRO_END {
            Some(Rate { input: 2.0, output: 10.0 })
        } else {
            Some(Rate { input: 3.0, output: 15.0 })
        }
    } else {
        None
    }
}

/// 快速模式（请求体顶层 `speed: "fast"`）下的价目：同一模型跑得更快，但按溢价计费。
/// 目前仅 Opus 5 / Opus 4.8 支持，均为 $10/$50（标准价 $5/$25 的两倍）。
/// 其它模型即便请求里带了 `speed`，上游也不会按 fast 计费，故返回 None 走标准价。
fn fast_rate_for(m: &str) -> Option<Rate> {
    if m.contains("opus-5") || m.contains("opus-4-8") {
        Some(Rate { input: 10.0, output: 50.0 })
    } else {
        None
    }
}

/// `speed` 是否表示快速模式。上游只定义了 `"fast"`，其余（含 `"standard"`）按标准价。
fn is_fast(speed: Option<&str>) -> bool {
    speed.is_some_and(|s| s.trim().eq_ignore_ascii_case("fast"))
}

fn f(v: Option<i64>) -> f64 {
    v.unwrap_or(0).max(0) as f64
}

/// 计价所需的一次请求用量（各字段缺省视为 0）。用结构体而非一长串同类型位置参数，
/// 避免调用方把 `cache_5m` / `cache_1h` / `cache_read` 传串还能编译通过。
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage<'a> {
    /// 上游回报的模型名；为空则无从计价。
    pub model: Option<&'a str>,
    /// 实际速度档（`usage.speed`，请求体 `speed` 兜底）；`"fast"` 走溢价档。
    pub speed: Option<&'a str>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    /// 缓存写总量；仅在下面两档细分都缺失时按 5 分钟档折算。
    pub cache_creation_total: Option<i64>,
    pub cache_5m_tokens: Option<i64>,
    pub cache_1h_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
}

/// 估算单次请求的等价费用（USD）。
///
/// `speed` 为 `"fast"` 且模型支持快速模式时按溢价计，其余按标准价。缓存写区分 5 分钟 /
/// 1 小时两档；若上游未返回细分，则将 `cache_creation_total` 整体按 5 分钟档计。
/// 模型未知返回 None（不计入）。
pub fn estimate_usd(u: Usage<'_>) -> Option<f64> {
    let model = u.model?;
    // fast 档只对支持的模型生效；不支持时回落标准价，避免凭请求字段虚高。
    let rate = match is_fast(u.speed).then(|| fast_rate_for(&model.to_ascii_lowercase())).flatten()
    {
        Some(r) => r,
        None => rate_for(model)?,
    };
    let inp = f(u.input_tokens);
    let out = f(u.output_tokens);
    let cr = f(u.cache_read_tokens);

    // 缓存写细分；无细分时整体按 5 分钟档。
    let (c5, c1) = match (u.cache_5m_tokens, u.cache_1h_tokens) {
        (None, None) => (f(u.cache_creation_total), 0.0),
        (a, b) => (f(a), f(b)),
    };

    const PER: f64 = 1_000_000.0;
    let cost = inp * rate.input
        + out * rate.output
        + c5 * (rate.input * 1.25)
        + c1 * (rate.input * 2.0)
        + cr * (rate.input * 0.10);
    Some(cost / PER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(model: &str) -> (f64, f64) {
        let r = rate_for(model).unwrap_or_else(|| panic!("未匹配到价目: {model}"));
        (r.input, r.output)
    }

    /// 现役各档模型都能匹配到正确价目；Opus 5 及其 1M 变体按 $5/$25，不加长上下文溢价。
    #[test]
    fn current_models_priced() {
        assert_eq!(rate("claude-opus-5"), (5.0, 25.0));
        assert_eq!(rate("claude-opus-5[1m]"), (5.0, 25.0), "1M 上下文是默认档，不加价");
        assert_eq!(rate("claude-opus-4-8"), (5.0, 25.0));
        assert_eq!(rate("claude-haiku-4-5"), (1.0, 5.0));
        assert_eq!(rate("claude-fable-5"), (10.0, 50.0));
        assert_eq!(rate("claude-mythos-5"), (10.0, 50.0));
        // Sonnet 5 有引导优惠，价目随当前时间切换，两档取其一。
        let s5 = rate("claude-sonnet-5");
        assert!(s5 == (2.0, 10.0) || s5 == (3.0, 15.0), "Sonnet 5 价目异常: {s5:?}");
        assert_eq!(rate("claude-sonnet-4-6"), (3.0, 15.0));
    }

    /// 老 Opus（3 / 4.0 / 4.1）仍是 $15/$75，不能被新 Opus 的 $5/$25 覆盖。
    #[test]
    fn legacy_opus_keeps_old_price() {
        assert_eq!(rate("claude-3-opus-20240229"), (15.0, 75.0));
        assert_eq!(rate("claude-opus-4-20250514"), (15.0, 75.0));
        assert_eq!(rate("claude-opus-4-1-20250805"), (15.0, 75.0));
    }

    /// 便捷构造：只关心模型/速度档，token 用量按需覆盖。
    fn usage<'a>(model: &'a str, speed: Option<&'a str>) -> Usage<'a> {
        Usage { model: Some(model), speed, ..Default::default() }
    }

    /// 未知模型不计费（返回 None），避免用错价目污染统计。
    #[test]
    fn unknown_model_not_billed() {
        assert!(rate_for("gpt-4o").is_none());
        assert!(estimate_usd(Usage { input_tokens: Some(100), ..Default::default() }).is_none());
        assert!(
            estimate_usd(Usage { input_tokens: Some(100), ..usage("some-unknown", None) })
                .is_none()
        );
    }

    /// 缓存写细分缺失时整体按 5 分钟档计；各档倍率符合官方规则。
    #[test]
    fn cache_tiers_use_multipliers() {
        // Opus 5：输入 $5/MTok。1M 缓存写(5m) = 5 * 1.25 = $6.25；缓存读 = 5 * 0.10 = $0.5。
        let base = usage("claude-opus-5", None);
        assert_eq!(
            estimate_usd(Usage { cache_creation_total: Some(1_000_000), ..base }),
            Some(6.25),
            "无细分时缓存写整体按 5 分钟档"
        );
        assert_eq!(
            estimate_usd(Usage { cache_1h_tokens: Some(1_000_000), ..base }),
            Some(10.0),
            "1 小时缓存写为基础输入价 ×2"
        );
        assert_eq!(
            estimate_usd(Usage { cache_read_tokens: Some(1_000_000), ..base }),
            Some(0.5),
            "缓存读为基础输入价 ×0.1"
        );
        // 有细分时忽略 total，避免重复计费。
        assert_eq!(
            estimate_usd(Usage {
                cache_creation_total: Some(9_999_999),
                cache_5m_tokens: Some(1_000_000),
                ..base
            }),
            Some(6.25),
            "有细分时以细分为准"
        );
    }

    /// 快速模式：支持的模型按 $10/$50（标准价两倍），不支持的模型忽略 speed 走标准价。
    #[test]
    fn fast_mode_pricing() {
        // 1M 输入 + 1M 输出：标准 5+25=$30；fast 10+50=$60。
        fn io<'a>(u: Usage<'a>) -> Usage<'a> {
            Usage { input_tokens: Some(1_000_000), output_tokens: Some(1_000_000), ..u }
        }
        assert_eq!(estimate_usd(io(usage("claude-opus-5", None))), Some(30.0));
        assert_eq!(
            estimate_usd(io(usage("claude-opus-5", Some("fast")))),
            Some(60.0),
            "fast 档为标准价两倍"
        );

        let fast_48 = fast_rate_for("claude-opus-4-8").unwrap();
        assert_eq!((fast_48.input, fast_48.output), (10.0, 50.0));
        assert_eq!(rate("claude-opus-4-8"), (5.0, 25.0), "不带 speed 时仍是标准价");

        // 显式 standard、大小写变体、以及不支持 fast 的模型都走标准价。
        let inp = |m, s| estimate_usd(Usage { input_tokens: Some(1_000_000), ..usage(m, s) });
        assert_eq!(inp("claude-opus-5", Some("standard")), Some(5.0));
        assert_eq!(inp("claude-opus-5", Some("FAST")), Some(10.0), "speed 匹配应忽略大小写");
        assert_eq!(
            inp("claude-sonnet-4-6", Some("fast")),
            Some(3.0),
            "不支持 fast 的模型忽略 speed，按标准价"
        );
    }
}
