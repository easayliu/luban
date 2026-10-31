//! 服务端开关 `tengu_curious_tower_stateless_models`：逗号分隔的模型清单，列在里面的模型 tether 引擎固定按无状态
//! 发送，遥测 `tengu_tether_decision` / `tengu_tether_live_outcome` 的 `modelHeldStateless` 报 true（2.1.308
//! `nJe()` = `k("tengu_curious_tower_stateless_models", "")`，默认空串即一个都不算）。
//!
//! 抓包里这个号拿到的值：`cap/2.1.280` 是 `fable-5-1, opus-5-`，`cap/2.1.285` 与 `cap/auto-2.1.285-20260930`
//! 是 `fable-5-1`，`cap/auto-2.1.291-20261006-full` 起直到 `cap/auto-2.1.308-20261022-full` 都是空串（关）。
//! 早先按抓包写死的「fable-5-1 与 1M 的 opus-5-5」其实就是那几天服务端给的清单。
//!
//! 按号从 eval 里记（保活与会话启动握手拉 eval 时顺带记）；eval 里没有这一项、值不是字符串的按空串记。没见过
//! eval 的号返回 `None`，由调用方兜底（`crate::telemetry` 按版本）。只存在内存里。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 开关名。
pub(super) const STATELESS_MODELS_FEATURE: &str = "tengu_curious_tower_stateless_models";

static LISTS: LazyLock<parking_lot::RwLock<HashMap<i64, String>>> = LazyLock::new(Default::default);

/// 从一份 eval 的 `features` 里记下这个号的清单：一份 eval 是整份特性缓存，没有这一项即按默认（空串）记。
pub(crate) fn note_eval(cred_id: i64, features: &serde_json::Map<String, serde_json::Value>) {
    let list = features
        .get(STATELESS_MODELS_FEATURE)
        .and_then(|f| f.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    LISTS.write().insert(cred_id, list.to_string());
}

/// 这个号 eval 里的清单原文；没见过它的 eval 是 `None`。
pub(crate) fn stateless_models(cred_id: i64) -> Option<String> {
    LISTS.read().get(&cred_id).cloned()
}

/// `model`（展示名，可带 `[1m]`）在不在清单里，照官方 `nJe` / `O8e`：清单按逗号拆开、各项去首尾空白与末尾的
/// `[1m]`（`Mt`），空项与单独一个 `*` 不算。模型名比两个：请求原名去掉其中的 `[1m]` / `[2m]`（`Fn(model)`），以及经
/// 模型目录映射后的规范名（`Fn(qe(model))`，[`canonical_model`]：`claude-sonnet-4-20250514` → `claude-sonnet-4-0`）。
/// 都转小写后，某项与其中任一个全等，或按 `*` 切开的各段依次出现在其中（`win`，没有 `*` 的一项即「是它的子串」：
/// `fable-5-1` 命中 `claude-fable-5-1`，`opus-5-` 连不带 1M 的 opus-5-5 也命中）就算。
pub(crate) fn lists_model(list: &str, model: &str) -> bool {
    let mut items = list
        .split(',')
        .map(|item| {
            let item = item.trim();
            item.len()
                .checked_sub(4)
                .filter(|&cut| {
                    item.is_char_boundary(cut) && item[cut..].eq_ignore_ascii_case("[1m]")
                })
                .map_or(item, |cut| &item[..cut])
                .trim()
                .to_lowercase()
        })
        .filter(|item| !item.is_empty() && item != "*")
        .peekable();
    // 清单里一项都没有（2.1.291 起线上都是空串）就不必算规范名：那要把目录里的名字逐个解析一遍，每条请求几微秒。
    if items.peek().is_none() {
        return false;
    }
    let names = [
        without_context_suffix(model).to_lowercase(),
        without_context_suffix(&canonical_model(model)).to_lowercase(),
    ];
    items.any(|item| names.iter().any(|name| item == *name || pieces_in_order(&item, name)))
}

/// 2.1.308 内置模型目录里的规范名（`PVr.models[].id`，按目录顺序），外加官方 `SE()` 那三个老模型。
const CATALOG_IDS: &[&str] = &[
    "claude-3-5-haiku",
    "claude-haiku-4-5",
    "claude-haiku-5-5",
    "claude-3-5-sonnet",
    "claude-3-7-sonnet",
    "claude-sonnet-4-0",
    "claude-sonnet-4-5",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "claude-sonnet-5-5",
    "claude-opus-4-0",
    "claude-opus-4-1",
    "claude-opus-4-5",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-opus-5-5",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-mythos-5",
    "claude-mythos-5-1",
    "claude-3-opus",
    "claude-3-sonnet",
    "claude-3-haiku",
];

/// 官方 `Ic` 认的 Bedrock 区域前缀（`$ue`）：`<区域>.anthropic.claude-…` 只认这几个。
const BEDROCK_REGIONS: &[&str] = &["us", "eu", "apac", "jp", "au", "us-gov", "global"];

/// 官方 `p0` 解析不了时按家族子串认的那串（按它的先后，先中先得）。规范名与子串不同的两项（`claude-opus-4`、
/// `claude-sonnet-4`）是正则 `claude-opus-4(?!-\d(?!\d))`：后面紧跟 `-<一位数>` 的不算。
const FAMILY_SUBSTRINGS: &[(&str, &str)] = &[
    ("claude-fable-5-1", "claude-fable-5-1"),
    ("claude-fable-5", "claude-fable-5"),
    ("claude-mythos-5-1", "claude-mythos-5-1"),
    ("claude-mythos-5", "claude-mythos-5"),
    ("claude-opus-5-5", "claude-opus-5-5"),
    ("claude-opus-5", "claude-opus-5"),
    ("claude-opus-4-8", "claude-opus-4-8"),
    ("claude-opus-4-7", "claude-opus-4-7"),
    ("claude-opus-4-6", "claude-opus-4-6"),
    ("claude-opus-4-5", "claude-opus-4-5"),
    ("claude-opus-4-1", "claude-opus-4-1"),
    ("claude-opus-4", "claude-opus-4-0"),
    ("claude-sonnet-5-5", "claude-sonnet-5-5"),
    ("claude-sonnet-5", "claude-sonnet-5"),
    ("claude-sonnet-4-6", "claude-sonnet-4-6"),
    ("claude-sonnet-4-5", "claude-sonnet-4-5"),
    ("claude-sonnet-4", "claude-sonnet-4-0"),
    ("claude-haiku-5-5", "claude-haiku-5-5"),
    ("claude-haiku-4-5", "claude-haiku-4-5"),
    ("claude-3-7-sonnet", "claude-3-7-sonnet"),
    ("claude-3-5-sonnet", "claude-3-5-sonnet"),
    ("claude-3-5-haiku", "claude-3-5-haiku"),
    ("claude-3-opus", "claude-3-opus"),
    ("claude-3-sonnet", "claude-3-sonnet"),
    ("claude-3-haiku", "claude-3-haiku"),
];

/// 官方 `qe(model)` 在第一方、没配 `modelOverrides`、服务端也没下发模型改名时给出的规范名（`Qv` → `_w` → `p0`）：
/// 1. 能按官方 `Ic` 解析成「家族 + 主版本 + 次版本」、尾巴只是日期 / `-fast` / `-latest` / `-v1:0` 这类（`qD`），就对回
///    内置目录里家族、写法、版本都相同的那个规范名（`g0`），对不上用解析出的主体（`Jv(base)`）；
/// 2. 解析不了的按家族子串认（[`FAMILY_SUBSTRINGS`]）；
/// 3. 都不是就去掉末尾的 `-YYYYMMDD`（`Jv`）。
///
/// 官方 `p0` 先查目录的 provider id 反查表（`c_e`）；目录里的 provider id 走第 1 步得到的是同一个规范名（测试拿官方
/// 原函数逐个对过），这里不另存那张表。结果是小写，可能还带着 `[1m]`，由调用方去掉。
pub(crate) fn canonical_model(model: &str) -> String {
    let e = model.to_lowercase();
    if let Some(id) = parse_model_id(&e).filter(|id| id.trailer_ok) {
        let known = CATALOG_IDS
            .iter()
            .find(|c| parse_model_id(c).is_some_and(|c| c.trailer_ok && c.same_version(&id)));
        return known.map_or_else(|| strip_date(&id.base).to_string(), |c| c.to_string());
    }
    let bare_major = |needle: &str| {
        e.match_indices(needle).any(|(at, _)| {
            let rest = &e.as_bytes()[at + needle.len()..];
            !(rest.first() == Some(&b'-')
                && rest.get(1).is_some_and(u8::is_ascii_digit)
                && !rest.get(2).is_some_and(u8::is_ascii_digit))
        })
    };
    FAMILY_SUBSTRINGS
        .iter()
        .find(
            |(needle, canon)| if needle == canon { e.contains(needle) } else { bare_major(needle) },
        )
        .map_or_else(|| strip_date(&e).to_string(), |(_, canon)| canon.to_string())
}

/// 官方 `Ic` 解析出的型号。
struct ModelId {
    family: String,
    major: u32,
    minor: Option<u32>,
    /// 版本号在家族前面的老写法（`claude-3-5-sonnet`）。
    legacy: bool,
    /// 解析到的主体（`claude-sonnet-4`），不含日期等尾巴。
    base: String,
    /// 尾巴只是日期 / `-fast` / `-latest` / `-v1:0` 这类（官方 `h3o`），`qD` 才认。
    trailer_ok: bool,
}

impl ModelId {
    /// 官方 `REt`：家族、写法、主次版本都相同（次版本缺省按 0）。
    fn same_version(&self, other: &ModelId) -> bool {
        self.family == other.family
            && self.legacy == other.legacy
            && self.major == other.major
            && self.minor.unwrap_or(0) == other.minor.unwrap_or(0)
    }
}

/// 官方 `Ic`：去首尾空白、转小写、去掉末尾 `[1m]` / `[2m]`、取最后一个 `/` 之后，认 `[<区域>.]anthropic.claude-…`
/// 这种 Bedrock 写法（区域不在 [`BEDROCK_REGIONS`] 里的不认），再按 `u` 的两种写法解析主体；主体后面要么没有、要么以
/// `-` / `@` 开头，否则不认。含空白的不认。
fn parse_model_id(e: &str) -> Option<ModelId> {
    let mut n = e.trim().to_lowercase();
    if n.is_empty() || n.chars().any(|c| c.is_whitespace() || c == '\u{feff}') {
        return None;
    }
    if n.ends_with("[1m]") || n.ends_with("[2m]") {
        n.truncate(n.len() - 4);
    }
    if let Some(at) = n.rfind('/') {
        n = n[at + 1..].to_string();
    }
    if let Some(rest) = n.strip_prefix("anthropic.claude-") {
        n = format!("claude-{rest}");
    } else if let Some(at) = n.find(".anthropic.claude-") {
        let region = &n[..at];
        if !region.is_empty() && region.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
            if !BEDROCK_REGIONS.contains(&region) {
                return None;
            }
            n = n[at + ".anthropic.".len()..].to_string();
        }
    }
    let (family, major, minor, legacy, used) = parse_model_core(&n)?;
    let tail = &n[used..];
    if !tail.is_empty() && !tail.starts_with(['-', '@']) {
        return None;
    }
    static TRAILER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"^(?:-fast|-latest)?(?:-v[0-9]{1,3}@[0-9]{8}|[-@][0-9]{8})?(?:-v[0-9]{1,3}(?::[0-9]{1,3})?)?$",
        )
        .expect("trailer pattern")
    });
    Some(ModelId {
        family,
        major,
        minor,
        legacy,
        base: n[..used].to_string(),
        trailer_ok: TRAILER.is_match(tail),
    })
}

/// 官方 `u` 的两种写法：`claude-<家族>-<主>[-<次>]`，或老写法 `claude-<主>[-<次>]-<家族>`；版本号各一到两位、后面
/// 不能紧跟数字。返回（家族、主、次、是否老写法、主体长度）。
fn parse_model_core(n: &str) -> Option<(String, u32, Option<u32>, bool, usize)> {
    const PREFIX: usize = "claude-".len();
    let s = n.strip_prefix("claude-")?;
    let digits = |t: &str| t.bytes().take_while(u8::is_ascii_digit).count();
    let letters = |t: &str| t.bytes().take_while(u8::is_ascii_lowercase).count();
    // 一到两位的版本号（后面紧跟数字的不算）：返回位数。
    let version = |t: &str| Some(digits(t)).filter(|d| (1..=2).contains(d));
    let fam = letters(s);
    if fam > 0
        && let Some(after) = s[fam..].strip_prefix('-')
        && let Some(d) = version(after)
    {
        let mut used = PREFIX + fam + 1 + d;
        let mut minor = None;
        if let Some(t) = after[d..].strip_prefix('-')
            && let Some(d2) = version(t)
        {
            minor = t[..d2].parse().ok();
            used += 1 + d2;
        }
        return Some((s[..fam].to_string(), after[..d].parse().ok()?, minor, false, used));
    }
    let d = version(s)?;
    let major = s[..d].parse().ok()?;
    let rest = &s[d..];
    if let Some(t) = rest.strip_prefix('-')
        && let Some(d2) = version(t)
        && let Some(f) = t[d2..].strip_prefix('-').map(letters).filter(|f| *f > 0)
    {
        let family = t[d2 + 1..d2 + 1 + f].to_string();
        return Some((family, major, t[..d2].parse().ok(), true, PREFIX + d + 1 + d2 + 1 + f));
    }
    let f = rest.strip_prefix('-').map(letters).filter(|f| *f > 0)?;
    Some((rest[1..1 + f].to_string(), major, None, true, PREFIX + d + 1 + f))
}

/// 官方 `Jv`：去掉末尾的 `-YYYYMMDD`。
fn strip_date(e: &str) -> &str {
    let b = e.as_bytes();
    if b.len() >= 9 && b[b.len() - 9] == b'-' && b[b.len() - 8..].iter().all(u8::is_ascii_digit) {
        &e[..e.len() - 9]
    } else {
        e
    }
}

/// 去掉模型名里每一处 `[1m]` / `[2m]`（不分大小写）。
fn without_context_suffix(model: &str) -> String {
    let mut out = String::with_capacity(model.len());
    let mut rest = model;
    while let Some(at) = rest.find('[') {
        let tail = &rest[at..];
        let hit = tail.get(..4).is_some_and(|t| {
            let t = t.to_ascii_lowercase();
            t == "[1m]" || t == "[2m]"
        });
        out.push_str(&rest[..at]);
        if hit {
            rest = &tail[4..];
        } else {
            out.push('[');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// 官方 `win`：`pattern` 按 `*` 切开（丢掉空段）后各段依次出现在 `name` 里；一段都没有的不算命中。
fn pieces_in_order(pattern: &str, name: &str) -> bool {
    let mut pieces = pattern.split('*').filter(|p| !p.is_empty()).peekable();
    if pieces.peek().is_none() {
        return false;
    }
    let mut at = 0;
    for piece in pieces {
        match name[at..].find(piece) {
            Some(i) => at += i + piece.len(),
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v["features"].as_object().unwrap().clone()
    }

    /// 抓包里出现过的三种清单，以及官方匹配的几种边角。
    #[test]
    fn lists_models_like_the_official_matcher() {
        // `cap/2.1.280`：fable-5-1 与 opus 系都在（`opus-5-` 是子串，1M 与否都命中）。
        let l280 = "fable-5-1, opus-5-";
        for m in ["claude-fable-5-1", "claude-opus-5-5[1m]", "claude-opus-5-5"] {
            assert!(lists_model(l280, m), "{m}");
        }
        // opus-5 不带后面那个连字符，`opus-5-` 不是它的子串。
        for m in ["claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5", "claude-opus-5"] {
            assert!(!lists_model(l280, m), "{m}");
        }
        // `cap/2.1.285`：只有 fable-5-1。
        assert!(lists_model("fable-5-1", "claude-fable-5-1[1m]"));
        assert!(!lists_model("fable-5-1", "claude-opus-5-5[1m]"));
        // 2.1.291 起：空串，谁都不在。
        assert!(!lists_model("", "claude-fable-5-1"));
        assert!(!lists_model("  ", "claude-fable-5-1"));
        // 单独一个 `*` 不算；带 `*` 的按段依次匹配；不分大小写；清单项末尾的 `[1m]` 去掉再比。
        assert!(!lists_model("*", "claude-fable-5-1"));
        assert!(lists_model("claude-*-5-1", "claude-fable-5-1"));
        assert!(!lists_model("claude-*-5-1", "claude-opus-5-5"));
        assert!(lists_model("FABLE-5-1", "claude-fable-5-1"));
        assert!(lists_model("claude-opus-5-5[1m]", "claude-opus-5-5"));
        assert!(lists_model(" , claude-opus-5-5 ,", "claude-opus-5-5[1M]"));
    }

    /// 规范名与清单匹配，逐条对官方原函数（从 2.1.308 可执行文件里原样抽出的 `Ic` / `u` / `h3o` / `p0` / `g0` / `Jv` /
    /// `O8e` / `win`，配上内置目录 `PVr`，在 node 里跑出来的结果）：自拟的各种写法之外，含目录里每个模型的全部
    /// provider id（官方先查 `c_e` 反查表，这里走解析也得到同一个规范名）。
    #[test]
    fn canonical_names_and_list_matches_follow_the_official_functions() {
        const CANONICAL: &[(&str, &str)] = &[
            ("CLAUDE-OPUS-5-5", "claude-opus-5-5"),
            ("anthropic.claude-fable-5", "claude-fable-5"),
            ("anthropic.claude-fable-5-1", "claude-fable-5-1"),
            ("anthropic.claude-haiku-4-5", "claude-haiku-4-5"),
            ("anthropic.claude-haiku-5-5", "claude-haiku-5-5"),
            ("anthropic.claude-opus-4-7", "claude-opus-4-7"),
            ("anthropic.claude-opus-4-8", "claude-opus-4-8"),
            ("anthropic.claude-opus-5", "claude-opus-5"),
            ("anthropic.claude-opus-5-5", "claude-opus-5-5"),
            ("anthropic.claude-sonnet-5", "claude-sonnet-5"),
            ("anthropic.claude-sonnet-5-5", "claude-sonnet-5-5"),
            ("anthropic/claude-opus-5-5", "claude-opus-5-5"),
            ("claude-3-5-haiku", "claude-3-5-haiku"),
            ("claude-3-5-haiku-20241022", "claude-3-5-haiku"),
            ("claude-3-5-haiku@20241022", "claude-3-5-haiku"),
            ("claude-3-5-sonnet", "claude-3-5-sonnet"),
            ("claude-3-5-sonnet-20241022", "claude-3-5-sonnet"),
            ("claude-3-5-sonnet-v2@20241022", "claude-3-5-sonnet"),
            ("claude-3-7-sonnet", "claude-3-7-sonnet"),
            ("claude-3-7-sonnet-20250219", "claude-3-7-sonnet"),
            ("claude-3-7-sonnet@20250219", "claude-3-7-sonnet"),
            ("claude-3-haiku-20240307", "claude-3-haiku"),
            ("claude-3-opus-20240229", "claude-3-opus"),
            ("claude-fable-5", "claude-fable-5"),
            ("claude-fable-5-1", "claude-fable-5-1"),
            ("claude-haiku-4-5", "claude-haiku-4-5"),
            ("claude-haiku-4-5-20251001", "claude-haiku-4-5"),
            ("claude-haiku-4-5@20251001", "claude-haiku-4-5"),
            ("claude-haiku-5-5", "claude-haiku-5-5"),
            ("claude-mythos-5", "claude-mythos-5"),
            ("claude-mythos-5-1", "claude-mythos-5-1"),
            ("claude-opus-4", "claude-opus-4-0"),
            ("claude-opus-4-0", "claude-opus-4-0"),
            ("claude-opus-4-1", "claude-opus-4-1"),
            ("claude-opus-4-1-20250805", "claude-opus-4-1"),
            ("claude-opus-4-1-latest", "claude-opus-4-1"),
            ("claude-opus-4-1@20250805", "claude-opus-4-1"),
            ("claude-opus-4-20250514", "claude-opus-4-0"),
            ("claude-opus-4-5", "claude-opus-4-5"),
            ("claude-opus-4-5-20251101", "claude-opus-4-5"),
            ("claude-opus-4-5@20251101", "claude-opus-4-5"),
            ("claude-opus-4-6", "claude-opus-4-6"),
            ("claude-opus-4-6-20260101", "claude-opus-4-6"),
            ("claude-opus-4-7", "claude-opus-4-7"),
            ("claude-opus-4-8", "claude-opus-4-8"),
            ("claude-opus-4@20250514", "claude-opus-4-0"),
            ("claude-opus-5", "claude-opus-5"),
            ("claude-opus-5-5", "claude-opus-5-5"),
            ("claude-opus-5-5-thinking", "claude-opus-5-5"),
            ("claude-opus-5-5[1m]", "claude-opus-5-5"),
            ("claude-sonnet-4", "claude-sonnet-4-0"),
            ("claude-sonnet-4-0", "claude-sonnet-4-0"),
            ("claude-sonnet-4-1", "claude-sonnet-4-1"),
            ("claude-sonnet-4-20250514", "claude-sonnet-4-0"),
            ("claude-sonnet-4-20250514-fast", "claude-sonnet-4-0"),
            ("claude-sonnet-4-5", "claude-sonnet-4-5"),
            ("claude-sonnet-4-5-20250929", "claude-sonnet-4-5"),
            ("claude-sonnet-4-5@20250929", "claude-sonnet-4-5"),
            ("claude-sonnet-4-6", "claude-sonnet-4-6"),
            ("claude-sonnet-4-99", "claude-sonnet-4-99"),
            ("claude-sonnet-4@20250514", "claude-sonnet-4-0"),
            ("claude-sonnet-5", "claude-sonnet-5"),
            ("claude-sonnet-5-5", "claude-sonnet-5-5"),
            ("claude-unknown-9-9", "claude-unknown-9-9"),
            ("eu.anthropic.claude-opus-4-1-20250805-v1:0", "claude-opus-4-1"),
            ("gpt-4o", "gpt-4o"),
            ("us.anthropic.claude-3-5-haiku-20241022-v1:0", "claude-3-5-haiku"),
            ("us.anthropic.claude-3-5-sonnet-20241022-v2:0", "claude-3-5-sonnet"),
            ("us.anthropic.claude-3-7-sonnet-20250219-v1:0", "claude-3-7-sonnet"),
            ("us.anthropic.claude-fable-5", "claude-fable-5"),
            ("us.anthropic.claude-fable-5-1", "claude-fable-5-1"),
            ("us.anthropic.claude-haiku-4-5-20251001-v1:0", "claude-haiku-4-5"),
            ("us.anthropic.claude-haiku-5-5", "claude-haiku-5-5"),
            ("us.anthropic.claude-opus-4-1-20250805-v1:0", "claude-opus-4-1"),
            ("us.anthropic.claude-opus-4-20250514-v1:0", "claude-opus-4-0"),
            ("us.anthropic.claude-opus-4-5-20251101-v1:0", "claude-opus-4-5"),
            ("us.anthropic.claude-opus-4-6-v1", "claude-opus-4-6"),
            ("us.anthropic.claude-opus-4-7", "claude-opus-4-7"),
            ("us.anthropic.claude-opus-4-8", "claude-opus-4-8"),
            ("us.anthropic.claude-opus-5", "claude-opus-5"),
            ("us.anthropic.claude-opus-5-5", "claude-opus-5-5"),
            ("us.anthropic.claude-sonnet-4-20250514-v1:0", "claude-sonnet-4-0"),
            ("us.anthropic.claude-sonnet-4-5-20250929-v1:0", "claude-sonnet-4-5"),
            ("us.anthropic.claude-sonnet-4-6", "claude-sonnet-4-6"),
            ("us.anthropic.claude-sonnet-5", "claude-sonnet-5"),
            ("us.anthropic.claude-sonnet-5-5", "claude-sonnet-5-5"),
        ];
        for (model, want) in CANONICAL {
            assert_eq!(canonical_model(model), *want, "{model}");
        }
        // 复审的例子：清单写的是规范名 `claude-sonnet-4-0`，请求带的是 provider id `claude-sonnet-4-20250514`。
        const LISTS: &[(&str, &str, bool)] = &[
            ("claude-sonnet-4-0", "claude-sonnet-4-20250514", true),
            ("claude-opus-4-0", "claude-opus-4-20250514", true),
            ("claude-haiku-4-5", "claude-haiku-4-5-20251001", true),
            ("fable-5-1, opus-5-", "claude-opus-5-5[1m]", true),
            ("fable-5-1", "claude-fable-5-1", true),
            ("claude-sonnet-4-0", "claude-sonnet-4-5-20250929", false),
            ("sonnet-4-0", "claude-sonnet-4-20250514", true),
            ("*-4-0", "claude-opus-4-20250514", true),
            ("claude-opus-4-1", "eu.anthropic.claude-opus-4-1-20250805-v1:0", true),
            ("", "claude-fable-5-1", false),
            ("*", "claude-fable-5-1", false),
        ];
        for (list, model, want) in LISTS {
            assert_eq!(lists_model(list, model), *want, "{list} / {model}");
        }
    }

    #[test]
    fn follows_the_accounts_eval() {
        let id = -93_201;
        assert_eq!(stateless_models(id), None, "没见过 eval");
        note_eval(
            id,
            &features(
                serde_json::json!({"features": {"tengu_curious_tower_stateless_models": {"value": "fable-5-1"}}}),
            ),
        );
        assert_eq!(stateless_models(id).as_deref(), Some("fable-5-1"));
        note_eval(id, &features(serde_json::json!({"features": {}})));
        assert_eq!(stateless_models(id).as_deref(), Some(""), "eval 里没有这一项按空串");
        note_eval(
            id,
            &features(
                serde_json::json!({"features": {"tengu_curious_tower_stateless_models": {"value": true}}}),
            ),
        );
        assert_eq!(stateless_models(id).as_deref(), Some(""), "不是字符串按空串");
    }
}
