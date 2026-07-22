//! 官方定价估算：按模型价目表，从 token 用量估算等价 API 费用（USD）。
//!
//! 订阅账号本身按订阅计费，此处仅用于「等价 API 费用」的参考统计。
//! 价目对齐官方 <https://platform.claude.com/docs/en/about-claude/pricing>
//! （每百万 token，MTok，美元）。相对基础输入价的倍率（官方所有模型通用）：
//! - 缓存写（5 分钟）：×1.25；缓存写（1 小时）：×2.0；缓存读：×0.10。
//!
//! 不做 >200K 长上下文加价——官方明确 Sonnet 5 等含 1M 上下文按标准价计。
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
        // 已退役的 Opus 4 / 4.1 为 $15/$75；现役 Opus 4.5+ 为 $5/$25。
        if m.contains("opus-4-1") || m.contains("opus-4-2025") {
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

fn f(v: Option<i64>) -> f64 {
    v.unwrap_or(0).max(0) as f64
}

/// 估算单次请求的等价费用（USD）。
///
/// 缓存写区分 5 分钟 / 1 小时两档；若上游未返回细分（`cache_5m`/`cache_1h` 均为空），
/// 则将 `cache_creation_total` 整体按 5 分钟档计。模型未知返回 None（不计入）。
pub fn estimate_usd(
    model: Option<&str>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_creation_total: Option<i64>,
    cache_5m_tokens: Option<i64>,
    cache_1h_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
) -> Option<f64> {
    let rate = rate_for(model?)?;
    let inp = f(input_tokens);
    let out = f(output_tokens);
    let cr = f(cache_read_tokens);

    // 缓存写细分；无细分时整体按 5 分钟档。
    let (c5, c1) = match (cache_5m_tokens, cache_1h_tokens) {
        (None, None) => (f(cache_creation_total), 0.0),
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
