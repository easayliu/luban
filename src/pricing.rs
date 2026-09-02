//! 官方定价估算：按模型价目表，从 token 用量估算等价 API 费用（USD）。
//!
//! 订阅账号本身按订阅计费，此处仅用于「等价 API 费用」的参考统计。
//! 价目对齐官方 <https://platform.claude.com/docs/en/about-claude/pricing>
//! （每百万 token，MTok，美元；最后核对 2026-09-02）。相对基础输入价的倍率：
//! - 缓存写（5 分钟）：×1.25；缓存写（1 小时）：×2.0——所有模型通用。
//! - 缓存读：×0.10 通用；**Fable 5.1 / Mythos 5.1 例外为 ×0.025**（$0.25/MTok）。
//!
//! 不做 >200K 长上下文加价——官方明确 4.6 及以后模型含 1M 上下文按标准价计，
//! 故带 `[1m]` 后缀的模型名与裸 id 同价。
//!
//! Sonnet 5 的 $2/$10 原为 2026-08-31 截止的引导价，官方已宣布转为永久标准价，
//! 9-01 不再涨到 $3/$15，故此处不再按时间切换。

/// 缓存写倍率（相对基础输入价），所有模型通用：5 分钟档与 1 小时档。
const CACHE_WRITE_5M_MULT: f64 = 1.25;
const CACHE_WRITE_1H_MULT: f64 = 2.0;
/// 通用缓存读倍率（相对基础输入价）。
const CACHE_READ_MULT: f64 = 0.10;
/// Fable 5.1 / Mythos 5.1 的缓存读倍率——官方特例。
const CACHE_READ_MULT_FABLE_5_1: f64 = 0.025;

/// 每百万 token 的基础价（美元）。缓存写价由基础输入价按通用倍率派生，
/// 缓存读倍率按模型带在 `cache_read_mult` 里（绝大多数为 0.10）。
struct Rate {
    input: f64,
    output: f64,
    cache_read_mult: f64,
}

impl Rate {
    const fn new(input: f64, output: f64) -> Self {
        Rate { input, output, cache_read_mult: CACHE_READ_MULT }
    }
}

/// 按模型名匹配价目（未知模型返回 None）。价格对齐官方定价表。
fn rate_for(model: &str) -> Option<Rate> {
    let m = model.to_ascii_lowercase();
    if m.contains("fable") || m.contains("mythos") {
        // Fable / Mythos 系列统一 $10/$50；5.1 代的缓存读是 0.025×（$0.25/MTok），
        // 5.0 代仍是通用的 0.10×（$1/MTok）。
        let is_5_1 = m.contains("-5-1") || m.contains("-5.1");
        Some(Rate {
            cache_read_mult: if is_5_1 { CACHE_READ_MULT_FABLE_5_1 } else { CACHE_READ_MULT },
            ..Rate::new(10.0, 50.0)
        })
    } else if m.contains("opus") {
        // 老 Opus（3 / 4.0 / 4.1）为 $15/$75；Opus 4.5 及以后（含 Opus 5）统一 $5/$25。
        // Opus 5 的 1M 上下文是默认档、不加价，故 `claude-opus-5[1m]` 与裸 id 同价。
        if m.contains("3-opus") || m.contains("opus-4-1") || m.contains("opus-4-2025") {
            Some(Rate::new(15.0, 75.0))
        } else {
            Some(Rate::new(5.0, 25.0))
        }
    } else if m.contains("haiku") {
        // Haiku 3.5 更便宜；Haiku 4.5 为 1/5。
        if m.contains("haiku-3") || m.contains("3-5-haiku") || m.contains("3.5") {
            Some(Rate::new(0.80, 4.0))
        } else {
            Some(Rate::new(1.0, 5.0))
        }
    } else if m.contains("sonnet") {
        // Sonnet 5 为 $2/$10（原引导价已转永久）；Sonnet 4.x 为 $3/$15。
        if m.contains("sonnet-5") { Some(Rate::new(2.0, 10.0)) } else { Some(Rate::new(3.0, 15.0)) }
    } else {
        None
    }
}

/// 快速模式（请求体顶层 `speed: "fast"`）下的价目：同一模型跑得更快，但按溢价计费。
/// 目前仅 Opus 5 / Opus 4.8 支持，均为 $10/$50（标准价 $5/$25 的两倍）。
/// 其它模型即便请求里带了 `speed`，上游也不会按 fast 计费，故返回 None 走标准价。
fn fast_rate_for(m: &str) -> Option<Rate> {
    if m.contains("opus-5") || m.contains("opus-4-8") { Some(Rate::new(10.0, 50.0)) } else { None }
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
/// 缓存读倍率按模型取（通用 0.10，Fable 5.1 / Mythos 5.1 为 0.025）。
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
        + c5 * (rate.input * CACHE_WRITE_5M_MULT)
        + c1 * (rate.input * CACHE_WRITE_1H_MULT)
        + cr * (rate.input * rate.cache_read_mult);
    Some(cost / PER)
}

/// 对外公开价目（`GET /api/pricing`）时列出的模型 id：官方定价页有价、且第一方 API 仍在
/// 服务的现役模型（已退役的 Opus 4.1 / Sonnet 4 / Haiku 3.5 等走 OAuth 打不通，不列）。
///
/// 带 `[1m]` 后缀的是 Claude Code 请求 1M 上下文时的写法，与裸 id 同价；New API 按模型名
/// 精确匹配倍率，所以要作为独立条目列出，否则带后缀的请求在它那边就没价。Haiku 4.5 只有
/// 200K 上下文，没有 `[1m]` 变体。
pub const LISTED_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-fable-5-1[1m]",
    "claude-mythos-5-1",
    "claude-mythos-5-1[1m]",
    "claude-fable-5",
    "claude-fable-5[1m]",
    "claude-mythos-5",
    "claude-mythos-5[1m]",
    "claude-opus-5",
    "claude-opus-5[1m]",
    "claude-opus-4-8",
    "claude-opus-4-8[1m]",
    "claude-opus-4-7",
    "claude-opus-4-7[1m]",
    "claude-opus-4-6",
    "claude-opus-4-6[1m]",
    "claude-opus-4-5",
    "claude-sonnet-5",
    "claude-sonnet-5[1m]",
    "claude-sonnet-4-6",
    "claude-sonnet-4-6[1m]",
    "claude-sonnet-4-5",
    "claude-haiku-4-5",
];

/// 一个模型的公开价目：基础价按 USD / MTok，缓存三档按相对基础输入价的倍率给出——
/// 下游（New API 等）自己的计费模型就是「基础价 × 倍率」，直接给倍率省得它反推。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub model: &'static str,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_write_5m_mult: f64,
    pub cache_write_1h_mult: f64,
    pub cache_read_mult: f64,
}

/// [`LISTED_MODELS`] 逐个套价目表得到的公开价目。列表里的模型都保证能匹配到价目
/// （有测试护栏），所以这里静默跳过 `None` 只是形式上的兜底。
pub fn price_table() -> Vec<ModelPrice> {
    LISTED_MODELS
        .iter()
        .filter_map(|&model| {
            let r = rate_for(model)?;
            Some(ModelPrice {
                model,
                input_per_mtok: r.input,
                output_per_mtok: r.output,
                cache_write_5m_mult: CACHE_WRITE_5M_MULT,
                cache_write_1h_mult: CACHE_WRITE_1H_MULT,
                cache_read_mult: r.cache_read_mult,
            })
        })
        .collect()
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
        assert_eq!(rate("claude-fable-5-1"), (10.0, 50.0));
        assert_eq!(rate("claude-mythos-5"), (10.0, 50.0));
        assert_eq!(rate("claude-mythos-5-1"), (10.0, 50.0));
        // Sonnet 5 的 $2/$10 已是永久标准价，不再随时间涨回 $3/$15。
        assert_eq!(rate("claude-sonnet-5"), (2.0, 10.0));
        assert_eq!(rate("claude-sonnet-5[1m]"), (2.0, 10.0));
        assert_eq!(rate("claude-sonnet-4-6"), (3.0, 15.0));
    }

    /// 公开价目列表里的每个模型都必须能匹配到价目——列表是手写的，防止加了新 id 却忘了
    /// 给 `rate_for` 加分支，导致对外静默少一行。顺带核对几个代表值。
    #[test]
    fn every_listed_model_has_a_price() {
        let table = price_table();
        assert_eq!(table.len(), LISTED_MODELS.len(), "有模型没匹配到价目");
        let find = |m: &str| *table.iter().find(|p| p.model == m).expect(m);

        let opus = find("claude-opus-5[1m]");
        assert_eq!((opus.input_per_mtok, opus.output_per_mtok), (5.0, 25.0));
        assert_eq!(
            (opus.cache_write_5m_mult, opus.cache_write_1h_mult, opus.cache_read_mult),
            (1.25, 2.0, 0.10)
        );
        assert_eq!(find("claude-fable-5-1").cache_read_mult, 0.025);
        assert_eq!(find("claude-fable-5").cache_read_mult, 0.10);
        let s5 = find("claude-sonnet-5");
        assert_eq!((s5.input_per_mtok, s5.output_per_mtok), (2.0, 10.0));
        // 列表本身不该有重复条目。
        let mut names: Vec<_> = LISTED_MODELS.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), LISTED_MODELS.len(), "LISTED_MODELS 有重复");
    }

    /// 缓存读倍率：Fable 5.1 / Mythos 5.1 为 0.025×（$0.25/MTok），其余模型（含 Fable 5）0.10×。
    #[test]
    fn fable_5_1_cache_read_discount() {
        let read =
            |m: &str| estimate_usd(Usage { cache_read_tokens: Some(1_000_000), ..usage(m, None) });
        assert_eq!(read("claude-fable-5-1"), Some(0.25), "Fable 5.1 缓存读 $0.25/MTok");
        assert_eq!(read("claude-fable-5-1[1m]"), Some(0.25));
        assert_eq!(read("claude-mythos-5-1"), Some(0.25), "Mythos 5.1 同价");
        assert_eq!(read("claude-fable-5"), Some(1.0), "Fable 5 仍是通用 0.10×");
        assert_eq!(read("claude-mythos-5"), Some(1.0));
        assert_eq!(read("claude-opus-5"), Some(0.5));
        assert_eq!(read("claude-sonnet-5"), Some(0.2));
        // 缓存写不受特例影响：Fable 5.1 的 5m 写仍是 10 × 1.25 = $12.5。
        assert_eq!(
            estimate_usd(Usage {
                cache_5m_tokens: Some(1_000_000),
                ..usage("claude-fable-5-1", None)
            }),
            Some(12.5)
        );
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
