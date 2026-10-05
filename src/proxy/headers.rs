//! 出站请求头构造：合并 `anthropic-beta`、组装转发头（含模拟模式整套重建）、
//! 头名大小写/顺序、随机 uuid、可转发头判据。

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use rand::RngExt;

use crate::config;
use crate::store;

use super::body::trusted_cc_version;
use super::simulation::{Simulation, cc_profile_kind_for};
use super::uuid_from_bytes;

/// 合并来访的 `anthropic-beta`：**客户端自有的那串顺序不动**，只把 API-key 模式的客户端不会
/// 自带的那几项补进去，各自插到官方位置上（fable 族另剥一项，见下）。这里是「注入哪几项」的
/// 唯一真源。
///
/// 只追加不落位会得到官方客户端不会产生的排列（缺失项全堆在末尾），集合对了顺序错，一次精确
/// 字符串匹配即可判定中间有代理。但**落位不能靠一张全局顺序表**——haiku 的客户端把
/// `claude-code-20250219` 排在队尾，opus/sonnet 排在队首，任何单一总序都同时满足不了
/// （见 [`config::cc_beta_order_is_not_a_table`]）。四对 raw 抓包里唯一稳定的是「客户端自有串
/// 的相对顺序在订阅模式下逐字不变」，故这里保留原串，按经验规则插入：
///
/// - [`config::OAUTH_BETA_HEADER`]：OAuth 鉴权必需。客户端串以
///   [`config::CC_BETA_CLAUDE_CODE`] 开头就插它后面，否则插最前（haiku 即后者）。
/// - [`config::CC_BETA_ADVANCED_TOOL_USE`]：有 [`config::CC_BETA_EFFORT`] 就插它前面；没有
///   （haiku）就插在 [`config::CC_BETA_ADVISOR_TOOL`] 之后（2.1.251 起 haiku 的官方串是
///   `…claude-code,advisor-tool,advanced-tool-use,server-side-fallback…`）；两个都没有
///   （2.1.220 的 haiku）才跟在客户端自有串之后。
/// - [`config::CC_BETA_PROMPT_CACHING_SCOPE`]：四份抓包里客户端都自带，真缺时补在末尾。
/// - [`config::CC_BETA_EXTENDED_CACHE_TTL`]：2.1.220 时官方恒为最后一项；2.1.251 起队尾是
///   [`config::CC_BETA_CACHE_DIAGNOSIS`]，它排在其前。故有 `cache-diagnosis` 就插它前面，
///   没有才追加队尾。
///
/// **2.1.251+ 世代还差几项**（判据：客户端串里有 [`config::CC_BETA_ADVISOR_TOOL`]，2.1.220
/// 的客户端没有这项，老规则对它们保持原样）。依据 `cap/2.1.258-api` 的 API-key 端原始请求头
/// （00006 opus / 00013 fable / 00017 sonnet / 00025 haiku），对照 `cap/2.1.258` 订阅端直连抓包：
/// API-key 端缺 oauth / advanced-tool-use / server-side-fallback / extended-cache-ttl /
/// cache-diagnosis，`fallback-credit` 与动态的 `afk-mode` 四族都自带；fable 另缺
/// `thinking-display-updates`、多一个 `redact-thinking`。
///
/// - [`config::CC_BETA_SERVER_SIDE_FALLBACK`]：`effort` 之后；没有 `effort`（haiku）就在
///   `advanced-tool-use` 之后。
/// - [`config::CC_BETA_FALLBACK_CREDIT`]：`server-side-fallback` 之后（API-key 端四族都自带，
///   这条只是兜底）。与 `server-side-fallback` 同一道闸：**参照串里有这一项的族才补**——
///   2.1.270 的 sonnet 主线程两项都不发了（`cap/2.1.270/00017`）。
/// - [`config::CC_BETA_CACHE_DIAGNOSIS`]：队尾（在 `extended-cache-ttl` 之前先补，后者再插到
///   它前面）。2.1.270 起队尾是 [`config::CC_BETA_MESSAGE_THREADS`]，有它就插它前面。
/// - [`config::CC_BETA_MESSAGE_THREADS`]：**不补**。2.1.270 没有 API-key 端样本，不知道那一侧
///   发不发；且它与 body 顶层的 `thread` 成对。来访带了就原位保留。
/// - fable 族（由 [`cc_profile_for`] 判：该族官方串里有 [`config::CC_BETA_THINKING_DISPLAY_UPDATES`]）：
///   补 `thinking-display-updates`（`fallback-credit` 之后；没有它——2.1.270 的 sonnet——就沿
///   `server-side-fallback` → `effort` → `advanced-tool-use` 这条链找上一格），并**剥掉**
///   [`config::CC_BETA_REDACT_THINKING`]——订阅端 fable 不发它，API-key 端发；这是本函数
///   唯一会删客户端项的地方，fable 上原始思维链本来就不返回，删了没有语义损失。与之配套的
///   body 侧 `thinking.display:"updates"` 由 [`fill_thinking_display`] 补。
///
/// `model` 为 `None` 时族相关的几条一条都不做（`server-side-fallback` / `fallback-credit` /
/// `thinking-display-updates` 与剥 `redact-thinking`）——不知道是哪族就不猜。
///
/// **参照串按来访自报的版本取**（[`config::cc_profile_at`]）：2.1.258 / 2.1.260 / 2.1.270
/// 三张表，2.1.270 只有 sonnet 一行。对一条**完整的**订阅端串本函数必须幂等——参照串选错
/// 一版，就会把上一版才有的项塞回一条官方请求里（2.1.270 sonnet 曾被补回
/// `server-side-fallback` 与 `fallback-credit`，见 [`tests::merged_beta_is_idempotent_on_2_1_270_sonnet`]）。
///
/// **非主线程 profile 整条豁免**（[`is_official_non_main_beta`]）：2.1.260 的 SDK 子代理、
/// 标题生成、安全分类、无工具 helper 与额度探测各有一套**更短**的官方 beta 集合，把主线程那几项补进去只会
/// 拼出一个官方从不产生的串。它们只补 `oauth`（OAuth 端硬性要求），其余一律不动。
///
/// **按前缀判在不在**（[`has_beta`]）：`server-side-fallback` 这类带日期的项在 2.1.258 与
/// 2.1.260 之间换过日期，逐字相等地判会给一个已经带 `-2026-06-01` 的来访再插一条
/// `-2026-07-01`，拼出两条同名 beta。
///
/// 2.1.220 三对抓包（opus-5 / sonnet-5 / haiku-4.5）用这套规则都能**逐字节**还原官方串；
/// 2.1.258 四族以 API-key 端原始请求头为输入，同样逐字节还原订阅端官方串。回归测试见 [`tests::merged_beta_matches_official_order`] 与
/// [`tests::merged_beta_matches_2_1_258_official_order`]。
///
/// **SDK 子代理由调用方告知**（[`BetaCtx::only_oauth`]）：子代理光看 beta 串认不出来：2.1.277 起它带 `advisor-tool`，[`is_official_non_main_beta`]
/// 那条「有 display-updates 却没有主线程标记」的判据放它过去，于是被当主线程补上
/// `advanced-tool-use` 与 `extended-cache-ttl`——`cap/2.1.285` 的子代理（claude-code-guide，
/// `00120` 首轮、`00127` 等五条续轮）两样都不发（[`config::cc_2_1_277_missing_samples`] 第 3 条
/// 记的正是这个缺口）。调用方从请求体判得出来（billing header 里的 `cc_is_subagent=true`），
/// 判出来就与其余非主线程 profile 一样只补 `oauth`。`/model` 预热与 `count_tokens` 同理
/// （[`super::session_link::CcRequestKind::beta_only_oauth`]）。
///
/// **`extended-cache-ttl` 只在出站体真写了 `ttl:"1h"` 时补**（[`BetaCtx::ttl_1h`]）：
/// `cap/auto-2.1.285-20260930` 的 137 条 `/v1/messages` 里，这项 beta 在与不在和体里有没有
/// `"ttl":"1h"` 一一对应——`/compact`、`/btw` 那两条分叉体里一个 1h 断点都没有，头上也就没有它。
///
/// **`thinking.display:"omitted"` 的不补 `thinking-display-updates`**（[`BetaCtx::display_omitted`]）：
/// `-p` 打印模式四族都写 `omitted`（`00441`、`00554` 等），官方串里就没有这一项。
pub(super) fn merge_beta_for(
    incoming: Option<&str>,
    model: Option<&str>,
    version: Option<(u64, u64, u64)>,
    ctx: BetaCtx,
) -> String {
    let mut parts: Vec<String> = incoming
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let has = |parts: &[String], beta: &str| has_beta(parts, beta);
    let pos = |parts: &[String], beta: &str| find_beta(parts, beta);

    // 官方非主线程 profile：只保证 `oauth` 在，别的一项都不补。
    if ctx.only_oauth || is_official_non_main_beta(&parts) {
        if !has(&parts, config::OAUTH_BETA_HEADER) {
            let at = usize::from(parts.first().is_some_and(|p| p == config::CC_BETA_CLAUDE_CODE));
            parts.insert(at, config::OAUTH_BETA_HEADER.to_string());
        }
        // SDK 子代理订阅端比 API-key 端多的不只 `oauth`：2.1.251 起的子代理带
        // `cache-diagnosis`（`cap/2.1.285/00120`、`00127`，排在 `message-threads` 之前），API-key
        // 端不发。`advanced-tool-use` / `extended-cache-ttl` 子代理确实不发，照旧不补。
        if ctx.subagent
            && has(&parts, config::CC_BETA_ADVISOR_TOOL)
            && !has(&parts, config::CC_BETA_CACHE_DIAGNOSIS)
        {
            let at = pos(&parts, config::CC_BETA_MESSAGE_THREADS).unwrap_or(parts.len());
            parts.insert(at, config::CC_BETA_CACHE_DIAGNOSIS.to_string());
        }
        return parts.join(",");
    }

    // 2.1.251+ 世代的判据；老客户端（2.1.220）不补下面那四项。
    //
    // 判据不能只看 `advisor-tool`：2.1.260 的 fable 主线程把它换成了
    // `per-turn-control`（`cap/2.1.260/00018`），只认前者就会把一条 2.1.260 的请求当成
    // 2.1.220 处理，`cache-diagnosis` 之类一项都不补。
    //
    // **不能把 `fallback-credit` 也算进判据**：2.1.220 的客户端自己就发它
    // （`cap/raw/00002`），认了它就会把老客户端一起当成 2.1.251+。
    let modern =
        has(&parts, config::CC_BETA_ADVISOR_TOOL) || has(&parts, config::CC_BETA_PER_TURN_CONTROL);
    // 2.1.260 一代：`server-side-fallback` 的日期换成了 06-01。判据有三个来源——来访自报的
    // 版本、只有这一代才发的 `per-turn-control`、以及它自己已经带的那条的日期。
    let gen_260 = version.is_some_and(|v| v >= (2, 1, 260))
        || has(&parts, config::CC_BETA_PER_TURN_CONTROL)
        || parts.iter().any(|p| p == config::CC_BETA_SERVER_SIDE_FALLBACK_JUN);
    let server_side_fallback = if gen_260 {
        config::CC_BETA_SERVER_SIDE_FALLBACK_JUN
    } else {
        config::CC_BETA_SERVER_SIDE_FALLBACK
    };
    // 参照串按**来访自报的版本**取：拿 2.1.260 那张表去处理一个 2.1.258 的客户端，会给它
    // 补上 `thinking-display-updates` 并剥掉 `redact-thinking`，拼出一条混了两个版本的请求。
    let seed = model.map(|m| config::cc_profile_at(cc_profile_kind_for(m), version).beta);
    // **按名字比，不比日期**：2.1.260 的 fable 官方串里是 `server-side-fallback-2026-06-01`，
    // 而这里问的常量是 07-01 那个。逐字相等地判，fable 那一族就永远问不出「官方发这一项」，
    // 于是缺了它也补不上——正是这条日期差把整个补齐分支变成了死代码。
    let seed_has = |beta: &str| {
        seed.is_some_and(|s: &str| s.split(',').any(|p| beta_name(p.trim()) == beta_name(beta)))
    };

    if !has(&parts, config::OAUTH_BETA_HEADER) {
        let at = usize::from(parts.first().is_some_and(|p| p == config::CC_BETA_CLAUDE_CODE));
        parts.insert(at, config::OAUTH_BETA_HEADER.to_string());
    }
    if !has(&parts, config::CC_BETA_ADVANCED_TOOL_USE) {
        let at = pos(&parts, config::CC_BETA_EFFORT)
            .or_else(|| pos(&parts, config::CC_BETA_ADVISOR_TOOL).map(|i| i + 1))
            .or_else(|| pos(&parts, config::CC_BETA_PER_TURN_CONTROL).map(|i| i + 1))
            .unwrap_or(parts.len());
        parts.insert(at, config::CC_BETA_ADVANCED_TOOL_USE.to_string());
    }
    if !has(&parts, config::CC_BETA_PROMPT_CACHING_SCOPE) {
        parts.push(config::CC_BETA_PROMPT_CACHING_SCOPE.to_string());
    }
    if modern {
        // 官方串里有这一项的族才补。2.1.260 的 opus 主线程整项不发了
        // （`cap/2.1.260-2/00025`），照旧补就是把 2.1.258 的形态发给一个 2.1.260 的来访。
        if seed_has(config::CC_BETA_SERVER_SIDE_FALLBACK)
            && !has(&parts, config::CC_BETA_SERVER_SIDE_FALLBACK)
        {
            let at = pos(&parts, config::CC_BETA_EFFORT)
                .or_else(|| pos(&parts, config::CC_BETA_ADVANCED_TOOL_USE))
                .map_or(parts.len(), |i| i + 1);
            parts.insert(at, server_side_fallback.to_string());
        }
        // 同上一项：官方串里有它的族才补。2.1.270 的 sonnet 主线程把 `server-side-fallback` 与
        // `fallback-credit` 一起去掉了（`cap/2.1.270/00017`），照旧补就是给一条完整的订阅端请求
        // 塞回两个上一版的 beta。API-key 端四族本来都自带它，这条在 2.1.258 / 2.1.260 上只是兜底。
        if seed_has(config::CC_BETA_FALLBACK_CREDIT)
            && !has(&parts, config::CC_BETA_FALLBACK_CREDIT)
        {
            let at = pos(&parts, config::CC_BETA_SERVER_SIDE_FALLBACK)
                .or_else(|| pos(&parts, config::CC_BETA_EFFORT))
                .or_else(|| pos(&parts, config::CC_BETA_ADVANCED_TOOL_USE))
                .map_or(parts.len(), |i| i + 1);
            parts.insert(at, config::CC_BETA_FALLBACK_CREDIT.to_string());
        }
        if seed_has(config::CC_BETA_THINKING_DISPLAY_UPDATES) {
            if !ctx.display_omitted && !has(&parts, config::CC_BETA_THINKING_DISPLAY_UPDATES) {
                // 官方位置紧跟 `fallback-credit`（2.1.258 fable、2.1.260 四族）。2.1.270 的
                // sonnet 不发 `fallback-credit` 了，它就紧跟 `effort`（`cap/2.1.270/00017`：
                // `…effort,thinking-display-updates,afk-mode…`）。锚点链与上面补
                // `fallback-credit` 的那条相同——它本来就落在这条链的下一格。只认
                // `fallback-credit` 会把它掉到队尾、排在 `message-threads` 后面。
                let at = pos(&parts, config::CC_BETA_FALLBACK_CREDIT)
                    .or_else(|| pos(&parts, config::CC_BETA_SERVER_SIDE_FALLBACK))
                    .or_else(|| pos(&parts, config::CC_BETA_EFFORT))
                    .or_else(|| pos(&parts, config::CC_BETA_ADVANCED_TOOL_USE))
                    .map_or(parts.len(), |i| i + 1);
                parts.insert(at, config::CC_BETA_THINKING_DISPLAY_UPDATES.to_string());
            }
            // `thinking-display-updates` 与 `redact-thinking` 在 2.1.260 的六份抓包上恒为
            // 互斥，2.1.258 时也只有 fable 一族如此。补了前者就得剥后者。
            if !seed_has(config::CC_BETA_REDACT_THINKING) {
                parts.retain(|p| p != config::CC_BETA_REDACT_THINKING);
            }
        }
        if !has(&parts, config::CC_BETA_CACHE_DIAGNOSIS) {
            // 2.1.258 / 2.1.260 时它是队尾；2.1.270 起队尾是 `message-threads`
            // （`cap/2.1.270/00017`、`00024`），来访带了就插它前面。
            let at = pos(&parts, config::CC_BETA_MESSAGE_THREADS).unwrap_or(parts.len());
            parts.insert(at, config::CC_BETA_CACHE_DIAGNOSIS.to_string());
        }
    }
    if ctx.ttl_1h {
        insert_extended_cache_ttl(&mut parts);
    }
    parts.join(",")
}

/// `extended-cache-ttl` 不在就补到官方位置：有 `cache-diagnosis` 插它前面，没有追加队尾。
fn insert_extended_cache_ttl(parts: &mut Vec<String>) {
    if !has_beta(parts, config::CC_BETA_EXTENDED_CACHE_TTL) {
        let at = find_beta(parts, config::CC_BETA_CACHE_DIAGNOSIS).unwrap_or(parts.len());
        parts.insert(at, config::CC_BETA_EXTENDED_CACHE_TTL.to_string());
    }
}

/// 出站体改写之后再对一次 `extended-cache-ttl`：头是在改写**之前**建的（改写要看头上有哪些
/// beta），[`merge_beta_for`] 那时只能按来访体判 [`BetaCtx::ttl_1h`]。API-key 端的三块形态被
/// 整形时断点会被补成 `ttl:"1h"`（[`super::rewrite_body`] 里的 `fill_cache_ttl`），来访体没有、
/// 出站体有——这时头上得跟着补，否则上游按 5 分钟算、还是「体里写了字段头上没声明」。
/// 反过来不删：luban 从不去掉体里已有的 1h，来访有、出站就有。
pub(super) fn ensure_cache_ttl_beta(beta: &str) -> String {
    let mut parts: Vec<String> =
        beta.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    insert_extended_cache_ttl(&mut parts);
    parts.join(",")
}

/// [`merge_beta_for`] 要的那几项**请求事实**：它自己只看得到 beta 串，这几样得从请求体与请求
/// 分类里来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BetaCtx {
    /// 只补 `oauth`、别的一项不动（[`super::session_link::CcRequestKind::beta_only_oauth`]）。
    pub(super) only_oauth: bool,
    /// 出站体里有 `ttl:"1h"` 的断点：只有这时才补 `extended-cache-ttl`。
    pub(super) ttl_1h: bool,
    /// `thinking.display` 是 `"omitted"`：不补 `thinking-display-updates`。
    pub(super) display_omitted: bool,
    /// SDK 子代理：只补 `oauth` 之外还要补 `cache-diagnosis`，见 [`merge_beta_for`]。
    pub(super) subagent: bool,
}

impl BetaCtx {
    /// 主线程、体里带 1h 断点、不压思考——订阅端主线程最常见的那一种，也是测试里按 API-key
    /// 端残缺串还原官方串时默认的前提。
    pub(super) const MAIN: Self =
        Self { only_oauth: false, ttl_1h: true, display_omitted: false, subagent: false };

    /// 按请求分类与来访体取：`raw` 是来访体原文，`body` 是解析好的那份。1h 断点先按字节粗筛
    /// （[`super::body::body_has_pair`]），命中再按解析好的体逐个断点核（[`super::body::has_cache_ttl_1h`]）
    /// ——工具入参里的 `ttl:"1h"` 不算。转发循环外算一次，换号重试沿用。
    pub(super) fn of(
        kind: super::session_link::CcRequestKind,
        raw: &[u8],
        body: Option<&serde_json::Value>,
    ) -> Self {
        Self {
            only_oauth: kind.beta_only_oauth(),
            ttl_1h: super::body::body_has_pair(raw, b"\"ttl\"", b"\"1h\"")
                && body.is_some_and(super::body::has_cache_ttl_1h),
            display_omitted: body
                .and_then(|v| v.get("thinking"))
                .and_then(|t| t.get("display"))
                .and_then(|d| d.as_str())
                == Some("omitted"),
            subagent: kind == super::session_link::CcRequestKind::Subagent,
        }
    }
}

/// [`merge_beta_for`] 的测试简写：主线程（[`BetaCtx::MAIN`]）。
#[cfg(test)]
pub(super) fn merge_beta(
    incoming: Option<&str>,
    model: Option<&str>,
    version: Option<(u64, u64, u64)>,
) -> String {
    merge_beta_for(incoming, model, version, BetaCtx::MAIN)
}

/// 出站体带 `fallbacks` 时头上必须有 `server-side-fallback`：串里没有（按名字比、不比
/// 日期）就补 2.1.260 那个日期（[`config::CC_BETA_SERVER_SIDE_FALLBACK_JUN`]，数组形态的
/// `fallbacks` 只认它），位置照 [`merge_beta_for`]：`effort` 之后，没有则 `advanced-tool-use`
/// 之后，都没有追加在末尾。已经带了（fable 的官方串、2.1.258 的客户端）一个字不动。
pub(super) fn ensure_fallback_beta(beta: String) -> String {
    let mut parts: Vec<String> =
        beta.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    if has_beta(&parts, config::CC_BETA_SERVER_SIDE_FALLBACK_JUN) {
        return beta;
    }
    let at = find_beta(&parts, config::CC_BETA_EFFORT)
        .or_else(|| find_beta(&parts, config::CC_BETA_ADVANCED_TOOL_USE))
        .map_or(parts.len(), |i| i + 1);
    parts.insert(at, config::CC_BETA_SERVER_SIDE_FALLBACK_JUN.to_string());
    parts.join(",")
}

/// 一项 beta 在不在串里，**按名字比、忽略日期后缀**。
///
/// `beta` 传的是含日期的全名（常量都是那个形态），比较时只取到最后一个 `-` 之前的名字段：
/// `server-side-fallback-2026-07-01` 与 `server-side-fallback-2026-06-01` 是同一项换了
/// 日期，不是两项。逐字相等地判会让 [`merge_beta_for`] 给一个已经带新日期的来访再插一条旧的。
pub(super) fn has_beta(parts: &[String], beta: &str) -> bool {
    find_beta(parts, beta).is_some()
}

/// [`has_beta`] 的位置版。
fn find_beta(parts: &[String], beta: &str) -> Option<usize> {
    let name = beta_name(beta);
    parts.iter().position(|p| beta_name(p) == name)
}

/// 去掉 beta 名末尾的日期段（`-YYYY-MM-DD`）。没有日期段的原样返回。
fn beta_name(beta: &str) -> &str {
    // 日期段恒为 11 个字符：`-` + `2026-04-07`。短于它、或去掉之后剩下空串的，都不算带日期。
    let Some(head) = beta.len().checked_sub(11).map(|i| &beta[..i]) else { return beta };
    let tail = &beta[beta.len() - 11..];
    let looks_like_date = tail.starts_with('-')
        && tail[1..]
            .chars()
            .enumerate()
            .all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() });
    if looks_like_date && !head.is_empty() { head } else { beta }
}

/// 这串 beta 是不是官方**非主线程** profile 那几套之一（SDK 子代理、无工具 helper、
/// 标题生成、安全分类、额度探测）。命中即 [`merge_beta_for`] 只补 `oauth`、别的一项都不动。
///
/// 它们各自的 beta 集合都比主线程短得多，把主线程那几项（`advanced-tool-use` /
/// `server-side-fallback` / `extended-cache-ttl` / `cache-diagnosis`）补进去，拼出来的是
/// 官方从不产生的串。
///
/// 四条判据，任一命中即算（依据 `cap/2.1.260` 与 `cap/2.1.260-2` 的六个 profile）：
///
/// 1. 有 `auto-mode-classifier-`：只有安全分类那条发（`00019`、`00030`）；
/// 2. 有 `structured-outputs-`：只有标题生成那条发（`2.1.260-2/00058`）；
/// 3. **没有** `claude-code-`：无工具 helper（`00024`）与额度探测（`2.1.260-2/00004`）都不发它；
/// 4. **SDK 子代理**（`00020`、`00025`）：有 `claude-code` 也有
///    `thinking-display-updates`，却**一个** `effort` / `advisor-tool` / `per-turn-control`
///    都没有。前三条判据都放它过去，于是它会被当成主线程补上 `advanced-tool-use` 与
///    `extended-cache-ttl`——官方那条两样都没有。
///
///    这一条不会误伤谁：主线程四族要么有 `effort`（opus/fable/sonnet），要么有
///    `advisor-tool`（2.1.258/2.1.260 的 haiku 主线程）；2.1.220 那代的 haiku
///    （[`tests::BETA_PAIRS`]）压根没有 `thinking-display-updates`。
///
/// 走到 [`merge_beta_for`] 的只有 CC 形态的来访（非 CC 形态的走模拟路径），所以「没有
/// claude-code beta」在这里是个可用的信号，而不是「随便哪个第三方客户端」。
pub(super) fn is_official_non_main_beta(parts: &[String]) -> bool {
    // 一项都没带不算：官方每个 profile 至少五项。压根没有 `anthropic-beta` 头的来访要走
    // 正常的补齐规则，不然它只会拿到一个孤零零的 `oauth`。
    if parts.is_empty() {
        return false;
    }
    let main_thread_marker =
        [config::CC_BETA_EFFORT, config::CC_BETA_ADVISOR_TOOL, config::CC_BETA_PER_TURN_CONTROL]
            .iter()
            .any(|b| has_beta(parts, b));
    let sdk_subagent =
        has_beta(parts, config::CC_BETA_THINKING_DISPLAY_UPDATES) && !main_thread_marker;

    has_beta(parts, config::CC_BETA_AUTO_MODE_CLASSIFIER)
        || has_beta(parts, config::CC_BETA_STRUCTURED_OUTPUTS)
        || !has_beta(parts, config::CC_BETA_CLAUDE_CODE)
        || sdk_subagent
}

/// 组装发往上游的请求头：原样转发可转发头，再对需要 luban 决定取值的头**原位覆盖**。
///
/// **头序**：`HeaderMap` 按插入序迭代，hyper 也按这个顺序写到线上，所以来访客户端的头序
/// 默认是保住的。但「先在 [`is_forwardable`] 里剥离、之后再 `insert`」会把那些头从原位摘走、
/// 追加到队尾（`anthropic-beta`/`anthropic-version`/`authorization` 都是），得到官方客户端
/// 不会产生的排列——和 [`merge_beta_for`] 要解决的问题同类，只是从「值内顺序」变成「头之间顺序」。
/// 故这里让它们照常转发，再用 `insert` 覆盖：`insert` 命中已有 key 时原位替换值，位置不动。
///
/// 只在客户端没带时才补的头（`accept-encoding`、`x-client-request-id`）没有原位可循，
/// 追加在末尾；官方客户端这两个头都带，走的是原位覆盖那条路。
///
/// **开关**（[`store::ForwardFlags`]，默认全开）：`merge_beta` 关掉即原样转发客户端那串
/// `anthropic-beta`（含不再塞 `oauth-2025-04-20`）；`fill_client_headers` 关掉即不补任何
/// 客户端没带的头。唯一无条件执行的是注入 `Authorization`——实测那是上游唯一必需的改动。
///
/// 注意 `fill_client_headers` 关掉后，若客户端自己也没带 `accept-encoding`，兜底会落到
/// [`crate::clients::upstream_client`] 的 `default_headers`（同为官方取值），不会退化成
/// tower-http 那个非官方的 `zstd,gzip,deflate,br`。
///
/// 无法对齐的部分（头名大小写、hyper 自己追加的 `user-agent`/`host`/`content-length`）
/// 见 [`crate::config::known_fingerprint_gaps`]。
#[cfg(test)]
pub(super) fn build_forward_headers(
    headers: &HeaderMap,
    token: &str,
    flags: store::ForwardFlags,
    sim: Option<&Simulation>,
    session_id: Option<&str>,
) -> HeaderMap {
    build_forward_headers_for(headers, token, flags, sim, session_id, None, false, BetaCtx::MAIN)
}

/// [`build_forward_headers`] 带模型名的版本：`model` 只喂给 [`merge_beta_for`] 做族相关的两条
/// 规则（fable 的 `thinking-display-updates` / `redact-thinking`）。转发路径与探测都知道模型，
/// 走这个；不带模型的那个留给测试与无 body 的场景。
#[allow(clippy::too_many_arguments)]
pub(super) fn build_forward_headers_for(
    headers: &HeaderMap,
    token: &str,
    flags: store::ForwardFlags,
    sim: Option<&Simulation>,
    session_id: Option<&str>,
    model: Option<&str>,
    // 出站体会带 `fallbacks`（[`refusal_fallbacks_for`]）：头上必须有 `server-side-fallback`
    // beta，否则是「体里写了字段、头上没声明」的自相矛盾，见 [`ensure_fallback_beta`]。
    fallback_beta: bool,
    // 喂给 [`merge_beta_for`] 的请求事实（只补 `oauth` 的几类、体里有没有 1h 断点……）。
    beta_ctx: BetaCtx,
) -> HeaderMap {
    let mut out = match sim {
        // 模拟模式：来访那套头一个不留，整体换成官方的（见 [`official_headers`]）。
        Some(sim) => official_headers(sim),
        None => {
            let mut out = HeaderMap::new();
            // `append` 而非 `insert`：同名多值头要全部保留，`insert` 会只剩最后一个。
            for (k, v) in headers.iter() {
                if is_forwardable(k, v) {
                    out.append(k.clone(), v.clone());
                }
            }
            // 出站两处必须**同值**：头上这个与 `metadata.user_id` 里那个，官方逐字相同。
            //
            // `session_id` 是 [`outbound_session_id`] 选定的那一个，故这里不是「没有才
            // 补」而是「以它为准」——只补缺会漏掉两种情形，两种都能让出站自相矛盾：
            //
            // - 来访头是 `sess-42` 这类非法值、体里却有合法 uuid：选出来的是体里那个，
            //   而非法的头原样转发了出去；
            // - 来访没带头、体里有合法 uuid：`bare_session` 只在「体里没有 metadata」时
            //   才有值，于是这条路上头一直是缺的。
            //
            // 值相同时一个字节都不动（绝大多数请求走的就是这条），故正常形态的转发不受
            // 影响。`insert` 命中已有 key 时原位替换、位置不动；客户端重复带了这个头时
            // 一并收敛成一个——官方不发重复头。
            if let Some(sid) = session_id
                && out.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()) != Some(sid)
                && let Ok(v) = HeaderValue::from_str(sid)
            {
                out.insert("x-claude-code-session-id", v);
            }
            out
        }
    };
    // anthropic-version 缺省补齐。
    if flags.fill_client_headers && !out.contains_key("anthropic-version") {
        out.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    // anthropic-beta：两条路各走各的。
    //
    // - 模拟路径：整串由 profile 直接给出（[`simulated_beta`]），**不过** [`merge_beta_for`]。
    //   那套增量规则是拿来补 API-key 端残缺串的，对一份已经完整的官方串只会往里插上一版
    //   才发的项。
    // - CC 形态来访：仍走 [`merge_beta_for`]，按经验规则把订阅端多出来的几项补回官方位置。
    let incoming = headers.get("anthropic-beta").and_then(|v| v.to_str().ok());
    let beta = match sim {
        Some(sim) => Some(simulated_beta(&sim.beta, incoming)),
        None if flags.merge_beta => {
            // 来访自报的版本决定按哪一版的官方形态补，见 [`merge_beta_for`]。
            let version = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
            Some(merge_beta_for(incoming, model, version.and_then(trusted_cc_version), beta_ctx))
        }
        None => None,
    };
    let beta = if fallback_beta {
        Some(ensure_fallback_beta(
            beta.or_else(|| incoming.map(str::to_string)).unwrap_or_default(),
        ))
    } else {
        beta
    };
    if let Some(beta) = beta {
        match HeaderValue::from_str(&beta) {
            Ok(v) => {
                out.insert("anthropic-beta", v);
            }
            // 两条路都只产出 ASCII，理论上不可达；真发生时保留来访原值，别把这个头发空。
            Err(e) => {
                tracing::warn!(error = %e, "building anthropic-beta failed, keeping the inbound value")
            }
        }
    }
    if flags.fill_client_headers {
        // accept-encoding：客户端没带时补上官方客户端的取值（缺失本身就是特征）。
        if !out.contains_key(header::ACCEPT_ENCODING) {
            out.insert(
                header::ACCEPT_ENCODING,
                HeaderValue::from_static(config::CC_ACCEPT_ENCODING),
            );
        }
        // x-client-request-id：官方客户端每请求一个 uuid v4；API-key 模式的 CC 不发，补齐。
        if !out.contains_key("x-client-request-id")
            && let Ok(v) = HeaderValue::from_str(&uuid_v4())
        {
            out.insert("x-client-request-id", v);
        }
    }
    // 注入 OAuth 鉴权，原位覆盖来访的任何鉴权头。
    match HeaderValue::from_str(&format!("Bearer {token}")) {
        Ok(v) => {
            out.insert(header::AUTHORIZATION, v);
        }
        // 这个头现在是**照常转发再覆盖**的，覆盖失败就必须摘掉：
        // 留在原地等于把来访者的接入 key 漏给上游。
        Err(e) => {
            tracing::error!(error = %e, "building Authorization failed, dropping the header so the inbound key cannot leak");
            out.remove(header::AUTHORIZATION);
        }
    }
    out
}

/// 模拟模式下整套重建的转发头：[`config::CC_SIM_HEADERS`] 那张固定表 + 两个随请求变的值
/// （会话 id、请求 id）。`Authorization` 与 `anthropic-beta` 由 [`build_forward_headers`]
/// 随后覆盖上去。
///
/// **来访客户端自己的头一个都不带过去**：一个 UA 是 `python-httpx/0.27`、没有 `x-app`、
/// 却发着 CC 系统提示词和 OAuth token 的请求，本身就是个比缺任何单项都强的判据；留着任何
/// 一个非官方头都等于白伪装。唯一的例外是 `anthropic-beta`——客户端可能真的需要某个 beta，
/// 那串在 [`simulated_beta`] 里与官方自有串取并集，不丢。
///
/// 插入序即 [`config::CC_SIM_HEADERS`] 的表序（官方线序），末尾两个动态头除外——线上的
/// 拼写与顺序另由 `orig_header_case` 按 [`config::CC_HEADER_ORDER`] 归位，那张表里
/// `X-Claude-Code-Session-Id` 与 `x-client-request-id` 都在各自的官方位置上。
fn official_headers(sim: &Simulation) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in config::CC_SIM_HEADERS {
        match (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            (Ok(n), Ok(v)) => {
                out.insert(n, v);
            }
            // 常量表，理论上不可达；真写错了也只是少一个头，不该因此拒掉整条请求。
            _ => tracing::error!(
                header = name,
                "building a simulated header failed (bad constant table), skipping it"
            ),
        }
    }
    // 与 `metadata.user_id` 里的 session_id 同值——官方两处逐字相同。
    if let Ok(v) = HeaderValue::from_str(&sim.session_id) {
        out.insert("x-claude-code-session-id", v);
    }
    // 2.1.277 起每条都带的请求类别（`main` / `subagent` / `auxiliary`），按 profile 取
    // （[`config::CcProfile::request_class`]）；线上位置由 [`config::CC_HEADER_ORDER`] 归到
    // `x-app` 之后。
    out.insert("x-claude-code-request-class", HeaderValue::from_static(sim.profile.request_class));
    // 2.1.285 起每条 messages 都带（[`config::CC_DISPATCH_ID`]），额度探测那条不带
    // （`cap/2.1.285/00017`）。线上位置由 [`config::CC_HEADER_ORDER`] 归到
    // `anthropic-dangerous-direct-browser-access` 之后。
    if sim.profile.kind != config::CcProfileKind::QuotaProbe {
        out.insert("anthropic-dispatch-id", HeaderValue::from_static(config::CC_DISPATCH_ID));
    }
    // 2.1.285 起与 billing header 里的 `cc_prompt_id` 同值同现（见 [`config::CC_HEADER_ORDER`]
    // 里那一项的出处）。
    if super::parse_version(sim.profile.version).is_some_and(|v| v >= (2, 1, 285))
        && let Some(pid) = sim.link.prompt_id.as_deref()
        && let Ok(v) = HeaderValue::from_str(pid)
    {
        out.insert("x-claude-code-prompt-id", v);
    }
    if let Ok(v) = HeaderValue::from_str(&uuid_v4()) {
        out.insert("x-client-request-id", v);
    }
    out
}

/// 模拟模式的 `anthropic-beta`：profile 那串（[`config::CcProfile::beta`]，逐字取自抓包）
/// 打底，补上 `oauth` 的官方位置，来访客户端自己带的项去重后**追加在后面**。
///
/// **不再过 [`merge_beta_for`]。** 那套增量落位规则的输入是 API-key 端的**残缺**串，作用是把
/// 订阅端多出来的几项补回官方位置；而 profile 里那串本来就是完整的订阅端官方串，再过一遍
/// 只会按 2.1.258 的规则往里插 `server-side-fallback` 之类 2.1.260 已经不发的项——补出一个
/// 两个版本的混合体。
///
/// 追加而非插空：客户端带的多半是官方不发的项（`output-128k` 之类），本来就没有「官方位置」
/// 可言，硬塞进官方串中间反而造出一个官方不产生的排列。丢掉它们更不行——那是客户端明确要的
/// 能力，丢了它的请求就直接变了语义。例外是 [`config::CC_BETA_CONTEXT_1M`]：官方 1M 会话也发，
/// 位置有抓包可依——紧跟开头的 `claude-code` / `oauth`（`cap/auto-2.1.285-20260930/00235`）。
/// 来访不带就不注入，普通 200K 请求照旧没有它。
///
/// `oauth` 的落位：官方串以 `claude-code-20250219` 开头时紧随其后（opus / fable / sonnet），
/// 否则排在最前（haiku 三个 profile 的 `claude-code` 在串中间，`oauth` 在队首）。这两种
/// 排列在 2.1.260 的六份抓包上都成立，也正是 [`merge_beta_for`] 用的同一条规则。
pub(super) fn simulated_beta(seed: &str, incoming: Option<&str>) -> String {
    let mut parts: Vec<&str> = seed.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    let at = usize::from(parts.first() == Some(&config::CC_BETA_CLAUDE_CODE));
    parts.insert(at, config::OAUTH_BETA_HEADER);
    // 客户端自己带的 beta **一项不丢**，去重后追加在官方串之后。
    for p in incoming.unwrap_or("").split(',').map(str::trim).filter(|p| !p.is_empty()) {
        // **同名不同日期算已经有了**：客户端带 `server-side-fallback-2026-07-01`、官方串里
        // 是 06-01，逐字比不出重复，追加进去就是两条同名 beta——官方从不产生的形态。
        if parts.iter().any(|q| beta_name(q) == beta_name(p)) {
            continue;
        }
        // **互斥项不能并存**：`redact-thinking` 与 `thinking-display-updates` 在 2.1.260 的
        // 六份抓包上从不同时出现（前者是「原始思维链打码」，后者是「思维链按 updates 显示」，
        // 语义本就冲突）。官方串已经选了一边，客户端带来的另一边只能丢——留着就是一条
        // 官方不产生的组合，而 body 侧那个 `thinking.display` 也只跟着官方串走。
        if beta_conflicts_with_seed(&parts, p) {
            tracing::warn!(
                beta = p,
                "dropping a client beta that contradicts the official set for this profile"
            );
            continue;
        }
        // **认不出来的照发，交给上游判**。这里曾按一张白名单把不认识的项丢掉，理由是
        // 「追加一个上游不认的 beta 是一发稳定 400」。那个理由站不住：
        //
        // - 丢掉之后**客户端并不知道**自己要的能力没了。它 body 里配套的字段还在，于是
        //   照样 400，只是错误文案变成了「体里写了、头上没声明」——比上游直说
        //   「不认识这个 beta」难查得多；
        // - 白名单天生落后。上游每发一个新 beta，所有真的在用它的客户端都会被 luban
        //   静默降级，而这条路上没人看得出来。
        //
        // 判「这个 beta 上游认不认」是上游的事，不是代理的事。互斥与同名那两条仍然拦
        // （上面两个 `continue`）——那不是「上游认不认」，是**同一条请求里自相矛盾**，
        // 拦掉是在修形态，不是在替客户端做决定。
        if !is_known_beta(p) {
            tracing::debug!(
                beta = p,
                "forwarding a client beta luban does not recognize; upstream decides"
            );
        }
        if beta_name(p) == beta_name(config::CC_BETA_CONTEXT_1M) {
            let head = [config::CC_BETA_CLAUDE_CODE, config::OAUTH_BETA_HEADER];
            let at = parts
                .iter()
                .take_while(|q| head.iter().any(|h| beta_name(q) == beta_name(h)))
                .count();
            parts.insert(at, p);
            continue;
        }
        parts.push(p);
    }
    parts.join(",")
}

/// 客户端这一项与已经定下来的官方串是不是互斥。
///
/// 目前只有一对：[`config::CC_BETA_REDACT_THINKING`] ↔
/// [`config::CC_BETA_THINKING_DISPLAY_UPDATES`]。2.1.260 的六个 profile 里，有前者的
/// （helper / 标题 / 分类 / 额度探测）必无后者，有后者的（主线程四族、SDK 子代理）必无前者。
fn beta_conflicts_with_seed(seed: &[&str], candidate: &str) -> bool {
    const EXCLUSIVE: &[(&str, &str)] =
        &[(config::CC_BETA_REDACT_THINKING, config::CC_BETA_THINKING_DISPLAY_UPDATES)];
    let has = |b: &str| seed.iter().any(|q| beta_name(q) == beta_name(b));
    EXCLUSIVE.iter().any(|(a, b)| {
        (beta_name(candidate) == beta_name(a) && has(b))
            || (beta_name(candidate) == beta_name(b) && has(a))
    })
}

/// 上游已知接受的 `anthropic-beta` 前缀。
///
/// **这不再是一张准入白名单**——不在此列的客户端 beta 照样转发（见 [`simulated_beta`]，
/// 判「上游认不认」是上游的事）。它现在只用来决定要不要打一行「luban 不认识这一项」的
/// debug 日志：上游发了新 beta 时，那行日志是唯一的提示。
fn is_known_beta(beta: &str) -> bool {
    const KNOWN_PREFIXES: &[&str] = &[
        "claude-code-",
        "oauth-",
        "interleaved-thinking-",
        "redact-thinking-",
        "thinking-token-count-",
        "context-management-",
        "prompt-caching-scope-",
        "advanced-tool-use-",
        "effort-",
        "extended-cache-ttl-",
        "output-128k-",
        "context-1m-",
        "mid-conversation-system-",
        "advisor-tool-",
        "afk-mode-",
        "cache-diagnosis-",
        "fast-mode-",
        // 2.1.258 起官方随 `thinking.display:"updates"` 一并发出（cap/2.1.258/00013）。
        "thinking-display-updates-",
        // 以下四项自 2.1.260 起在官方串里出现，见 [`config::CC_PROFILES`]。少了它们，
        // 一个真发这些 beta 的 2.1.260 客户端经模拟路径过来时会被**静默丢掉**能力声明
        // ——body 里配套的字段还在，于是变成「体里写了、头上没声明」的稳定 400。
        "per-turn-control-",
        "structured-outputs-",
        "auto-mode-classifier-",
        "server-side-fallback-",
        "fallback-credit-",
    ];
    KNOWN_PREFIXES.iter().any(|prefix| beta.starts_with(prefix))
}

/// 按官方拼写与顺序构造 `OrigHeaderMap`（`wreq` 据它决定线上头名的大小写**与顺序**）。
///
/// 整张 [`config::CC_HEADER_ORDER`] 无条件塞进去，不按本次请求裁剪——预检实测：表里有、
/// 本次没带的头**不会**凭空发出。反之表外的头照发，但一律小写且排在所有表内头之后，
/// 所以自定义头不会因缺表项而丢失，只是拿不到官方位置。
///
/// 这也是 `Host`/`User-Agent`/`Content-Length` 唯一的归位途径：它们由 HTTP 客户端自己追加，
/// 不在我们的 `HeaderMap` 里，但只要列进这张表就会落到官方位置，而不是被钉在队尾。
pub(crate) fn orig_header_case() -> wreq::header::OrigHeaderMap {
    let mut orig = wreq::header::OrigHeaderMap::new();
    for name in config::CC_HEADER_ORDER {
        // 返回值是 `HeaderMap::append` 的语义（false = 新键），不是成功与否，别拿来判错。
        orig.insert(*name);
    }
    orig
}

/// 生成一个随机 uuid v4（小写带连字符），用于补齐 `x-client-request-id`。
pub(super) fn uuid_v4() -> String {
    uuid_from_bytes(rand::rng().random())
}

/// 请求头是否可转发：跳过接入 key、Host、逐跳头。
///
/// `accept-encoding` 刻意**保留转发**：官方客户端必带 `gzip, deflate, br, zstd`，剥掉它等于
/// 发出一个「自称 claude-cli 却不声明压缩支持」的请求。上游会照单压缩（连 140 字节的错误体
/// 都压，SSE 也不例外），故上游客户端开了对应的解压 feature，wreq 收到时已解码；
/// 回给客户端的是未压缩内容（见 [`is_resp_forwardable`]）。
///
/// `authorization`/`anthropic-beta`/`anthropic-version` 也**保留转发**——它们的值随后会被
/// [`build_forward_headers`] 原位覆盖。在这里剥离会让它们被追加到头列表末尾，破坏来访头序。
///
/// `connection` 只在值为 `keep-alive` 时转发：官方客户端显式发这个头（抓包 040 可见），
/// 而 hyper 认为 HTTP/1.1 隐含 keep-alive、默认不发，剥掉就是个稳定差异。其余取值
/// （`close` 会打掉连接池、`upgrade` 更不能转）照旧当逐跳头丢弃。
fn is_forwardable(name: &HeaderName, value: &HeaderValue) -> bool {
    // HeaderName 构造时即归一化为小写，无需再转。
    let n = name.as_str();
    if n == "connection" {
        return value.as_bytes().eq_ignore_ascii_case(b"keep-alive");
    }
    !matches!(
        n,
        "host"
            | "x-api-key"
            | "content-length"
            | "expect"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// 响应头是否可回传：跳过由框架管理的分帧类头。
///
/// `content-encoding` 保留转发，但正常情况下它**根本不会出现**——wreq 解码后会把它连同
/// `content-length` 一起摘掉，我们回给客户端的是未压缩内容。只有上游用了我们没开的编码时
/// 它才会残留，那时压缩体确实是原样透传的，这个头必须跟着走，否则客户端会把压缩字节当明文解析。
///
/// 客户端向 luban 声明了 `accept-encoding` 却收到未压缩内容，这在 HTTP 里完全合法
/// （accept-encoding 是偏好不是要求）。代价是 luban→客户端这一腿不再压缩。
pub(super) fn is_resp_forwardable(name: &HeaderName) -> bool {
    let n = name.as_str().to_ascii_lowercase();
    !matches!(n.as_str(), "content-length" | "transfer-encoding" | "connection")
}

#[cfg(test)]
mod tests;
