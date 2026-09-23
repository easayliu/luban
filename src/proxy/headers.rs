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
pub(super) fn merge_beta(
    incoming: Option<&str>,
    model: Option<&str>,
    version: Option<(u64, u64, u64)>,
) -> String {
    let mut parts: Vec<String> = incoming
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let has = |parts: &[String], beta: &str| has_beta(parts, beta);
    let pos = |parts: &[String], beta: &str| find_beta(parts, beta);

    // 官方非主线程 profile：只保证 `oauth` 在，别的一项都不补。
    if is_official_non_main_beta(&parts) {
        if !has(&parts, config::OAUTH_BETA_HEADER) {
            let at = usize::from(parts.first().is_some_and(|p| p == config::CC_BETA_CLAUDE_CODE));
            parts.insert(at, config::OAUTH_BETA_HEADER.to_string());
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
            if !has(&parts, config::CC_BETA_THINKING_DISPLAY_UPDATES) {
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
    if !has(&parts, config::CC_BETA_EXTENDED_CACHE_TTL) {
        let at = pos(&parts, config::CC_BETA_CACHE_DIAGNOSIS).unwrap_or(parts.len());
        parts.insert(at, config::CC_BETA_EXTENDED_CACHE_TTL.to_string());
    }
    parts.join(",")
}

/// 出站体带 `fallbacks` 时头上必须有 `server-side-fallback`：串里没有（按名字比、不比
/// 日期）就补 2.1.260 那个日期（[`config::CC_BETA_SERVER_SIDE_FALLBACK_JUN`]，数组形态的
/// `fallbacks` 只认它），位置照 [`merge_beta`]：`effort` 之后，没有则 `advanced-tool-use`
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
/// 日期，不是两项。逐字相等地判会让 [`merge_beta`] 给一个已经带新日期的来访再插一条旧的。
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
/// 标题生成、安全分类、额度探测）。命中即 [`merge_beta`] 只补 `oauth`、别的一项都不动。
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
/// 走到 [`merge_beta`] 的只有 CC 形态的来访（非 CC 形态的走模拟路径），所以「没有
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
/// 不会产生的排列——和 [`merge_beta`] 要解决的问题同类，只是从「值内顺序」变成「头之间顺序」。
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
    build_forward_headers_for(headers, token, flags, sim, session_id, None, false)
}

/// [`build_forward_headers`] 带模型名的版本：`model` 只喂给 [`merge_beta`] 做族相关的两条
/// 规则（fable 的 `thinking-display-updates` / `redact-thinking`）。转发路径与探测都知道模型，
/// 走这个；不带模型的那个留给测试与无 body 的场景。
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
    // - 模拟路径：整串由 profile 直接给出（[`simulated_beta`]），**不过** [`merge_beta`]。
    //   那套增量规则是拿来补 API-key 端残缺串的，对一份已经完整的官方串只会往里插上一版
    //   才发的项。
    // - CC 形态来访：仍走 [`merge_beta`]，按经验规则把订阅端多出来的几项补回官方位置。
    let incoming = headers.get("anthropic-beta").and_then(|v| v.to_str().ok());
    let beta = match sim {
        Some(sim) => Some(simulated_beta(sim.profile.beta, incoming)),
        None if flags.merge_beta => {
            // 来访自报的版本决定按哪一版的官方形态补，见 [`merge_beta`]。
            let version = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
            Some(merge_beta(incoming, model, version.and_then(trusted_cc_version)))
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
    if let Ok(v) = HeaderValue::from_str(&uuid_v4()) {
        out.insert("x-client-request-id", v);
    }
    out
}

/// 模拟模式的 `anthropic-beta`：profile 那串（[`config::CcProfile::beta`]，逐字取自抓包）
/// 打底，补上 `oauth` 的官方位置，来访客户端自己带的项去重后**追加在后面**。
///
/// **不再过 [`merge_beta`]。** 那套增量落位规则的输入是 API-key 端的**残缺**串，作用是把
/// 订阅端多出来的几项补回官方位置；而 profile 里那串本来就是完整的订阅端官方串，再过一遍
/// 只会按 2.1.258 的规则往里插 `server-side-fallback` 之类 2.1.260 已经不发的项——补出一个
/// 两个版本的混合体。
///
/// 追加而非插空：客户端带的多半是官方不发的项（`output-128k` 之类），本来就没有「官方位置」
/// 可言，硬塞进官方串中间反而造出一个官方不产生的排列。丢掉它们更不行——那是客户端明确要的
/// 能力，丢了它的请求就直接变了语义。
///
/// `oauth` 的落位：官方串以 `claude-code-20250219` 开头时紧随其后（opus / fable / sonnet），
/// 否则排在最前（haiku 三个 profile 的 `claude-code` 在串中间，`oauth` 在队首）。这两种
/// 排列在 2.1.260 的六份抓包上都成立，也正是 [`merge_beta`] 用的同一条规则。
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
mod tests {
    use crate::proxy::test_support::{PLAIN_BODY, all_on, names, sim_for};
    use crate::proxy::{
        HeaderValue, build_forward_headers, config, header, merge_beta, store, uuid_v4,
    };

    /// 三个模型族的 `anthropic-beta`，逐字取自 `cap/raw` 的原始报文头
    /// （claude-cli/2.1.220，每对都是同机、经 luban 与直连相隔几十秒）：
    /// `(模型, 客户端自己发的, 官方订阅客户端发的)`。
    ///
    /// haiku 那对是关键反例：它的客户端把 `claude-code-20250219` 排在**队尾**、`oauth` 在
    /// 队首，与 opus/sonnet 正好相反，任何单一顺序表都同时对不上两边。
    const BETA_PAIRS: &[(&str, &str, &str)] = &[
        (
            "opus-5 (00002/00006)",
            "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,effort-2025-11-24,fallback-credit-2026-06-01",
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
             interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,fallback-credit-2026-06-01,\
             extended-cache-ttl-2025-04-11",
        ),
        (
            "sonnet-5 (00012/00009)",
            "claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             effort-2025-11-24",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,\
             effort-2025-11-24,extended-cache-ttl-2025-04-11",
        ),
        (
            "haiku-4.5 (00026/00031)",
            "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,claude-code-20250219",
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,claude-code-20250219,\
             advanced-tool-use-2025-11-20,extended-cache-ttl-2025-04-11",
        ),
    ];

    /// 2.1.258：API-key 端的**原始请求头**（`cap/2.1.258-api` 00025 haiku / 00017 sonnet /
    /// 00006 opus / 00013 fable，CC 经 luban 的入站原文）喂进去，必须逐字节还原订阅端直连
    /// 抓包的串（`cap/2.1.258` 00031 / 00026 / 00012 / 00013）。
    ///
    /// API-key 端缺的是 oauth / advanced-tool-use / server-side-fallback / extended-cache-ttl /
    /// cache-diagnosis；`fallback-credit` 与 `afk-mode` 四族都自带；fable 缺
    /// thinking-display-updates 却多一个 redact-thinking。API-key 端 fable 样本带 `context-1m`
    /// （[1m] 会话）而订阅端样本不带，期望值按 opus 的位置把它放在 `oauth` 之后。
    #[test]
    fn merged_beta_matches_2_1_258_official_order() {
        const CASES: &[(&str, &str, &str)] = &[
            (
                "claude-haiku-4-5-20251001",
                "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
                 fallback-credit-2026-06-01,afk-mode-2026-01-31",
                "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,server-side-fallback-2026-07-01,\
                 fallback-credit-2026-06-01,afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,\
                 cache-diagnosis-2026-04-07",
            ),
            (
                "claude-sonnet-5",
                "claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 advisor-tool-2026-03-01,effort-2025-11-24,fallback-credit-2026-06-01,\
                 afk-mode-2026-01-31",
                "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,effort-2025-11-24,\
                 server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
                 afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
            ),
            (
                "claude-opus-5",
                "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
                 redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,effort-2025-11-24,\
                 fallback-credit-2026-06-01,afk-mode-2026-01-31",
                "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
                 interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
                 server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
                 afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
            ),
            (
                "claude-fable-5-1",
                "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
                 redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,effort-2025-11-24,\
                 fallback-credit-2026-06-01,afk-mode-2026-01-31",
                "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
                 interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,effort-2025-11-24,\
                 server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
                 thinking-display-updates-2026-08-18,afk-mode-2026-01-31,\
                 extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
            ),
        ];
        // 来访自报 2.1.258，故参照的是 [`config::CC_PROFILES_2_1_258`] 那一版的官方形态。
        let v258 = Some((2u64, 1, 258));
        for (model, api_key_client, official) in CASES {
            assert_eq!(
                &merge_beta(Some(api_key_client), Some(model), v258),
                official,
                "{model} 的 beta 串没对齐"
            );
            // 订阅端客户端经 luban：本来就是官方串，一个字都不该动（幂等）。
            assert_eq!(&merge_beta(Some(official), Some(model), v258), official, "{model} 不幂等");
        }
        // 不知道模型时，族相关的两条不做：fable 的 API-key 串只补通用几项。
        let fable_api = CASES[3].1;
        let no_model = merge_beta(Some(fable_api), None, v258);
        assert!(no_model.contains("redact-thinking-2026-02-12"), "不知道族就不删: {no_model}");
        assert!(!no_model.contains("thinking-display-updates"), "不知道族就不补: {no_model}");
    }

    /// 2.1.270 的**完整订阅端串**经 [`merge_beta`] 必须一个字不动。
    ///
    /// `cap/2.1.270/00017` / `00025`（sonnet-5 直连，同一会话首轮与续轮，beta 逐字相同）：
    /// 这一版 sonnet 主线程**不发** `server-side-fallback` 与 `fallback-credit`，队尾多了
    /// `message-threads`。v0.3.114 之前 [`config::cc_profile_at`] 把所有 ≥2.1.260 的来访都套
    /// 2.1.260 那张表，那张表的 sonnet 行（外推的）两项都有，于是一条完整的官方请求会被塞回
    /// `server-side-fallback-2026-06-01,fallback-credit-2026-06-01`——一个官方 2.1.270 不产生的串。
    ///
    /// 同一批抓包里的标题生成 haiku（`00024`）与额度探测（`00005`）一并钉住：前者走非主线程
    /// 豁免、后者没有 `claude-code`，本来就只补 `oauth`，这里防回归。
    ///
    /// 其余三族**没有 2.1.270 样本、不外推**：仍按 2.1.260 表处理。opus 的完整 2.1.260 串在
    /// 自报 2.1.270 时同样不该动——那张表的 opus 行本来就不发 `server-side-fallback`。
    #[test]
    fn merged_beta_is_idempotent_on_2_1_270_sonnet() {
        const SONNET_00017: &str = "claude-code-20250219,oauth-2025-04-20,\
             interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,\
             thinking-display-updates-2026-08-18,afk-mode-2026-01-31,\
             extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
        const TITLE_00024: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             advisor-tool-2026-03-01,structured-outputs-2025-12-15,cache-diagnosis-2026-04-07,\
             message-threads-2026-08-12";
        const QUOTA_00005: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05";
        let v270 = Some((2u64, 1, 270));
        let haiku = Some("claude-haiku-4-5-20251001");
        assert_eq!(merge_beta(Some(SONNET_00017), Some("claude-sonnet-5"), v270), SONNET_00017);
        assert_eq!(merge_beta(Some(TITLE_00024), haiku, v270), TITLE_00024);
        assert_eq!(merge_beta(Some(QUOTA_00005), haiku, v270), QUOTA_00005);
        // 2.1.270 与 2.1.277 之间没抓包的小版本按最近一份已证形态（2.1.270 表）处理，不退回
        // 2.1.260 表；2.1.277 起另有自己的表（`merged_beta_is_idempotent_on_2_1_277_main_threads`）。
        assert_eq!(
            merge_beta(Some(SONNET_00017), Some("claude-sonnet-5"), Some((2, 1, 276))),
            SONNET_00017
        );
        // 2.1.270 sonnet 的 profile 行就是这条串去掉 `oauth` 与 `afk-mode`，模拟路径的拼法
        // （[`simulated_beta`]）要能把 `oauth` 放回原位。
        let seed = config::cc_profile_at(config::CcProfileKind::MainSonnet, v270);
        assert_eq!(seed.version, "2.1.270");
        assert_eq!(
            crate::proxy::simulated_beta(seed.beta, None),
            SONNET_00017.replace(",afk-mode-2026-01-31", "")
        );
        // 其余三族不外推：自报 2.1.270 的 opus 仍拿 2.1.260 的行，完整的 2.1.260 opus 串不动。
        let opus_260 = config::cc_profile_at(config::CcProfileKind::MainOpus, Some((2, 1, 260)));
        assert_eq!(opus_260.version, "2.1.260");
        assert_eq!(
            config::cc_profile_at(config::CcProfileKind::MainOpus, v270).beta,
            opus_260.beta
        );
        let opus_official = crate::proxy::simulated_beta(opus_260.beta, None);
        assert_eq!(merge_beta(Some(&opus_official), Some("claude-opus-5"), v270), opus_official);
    }

    /// 2.1.270 sonnet 的**API-key 端没有抓包**，这里只钉落位规则，不宣称差分：把 2.1.258 那套
    /// 「API-key 端缺 oauth / advanced-tool-use / extended-cache-ttl / cache-diagnosis」套到
    /// `00017` 上（`server-side-fallback` 这一版本来就没有），补回去要落在官方位置——尤其
    /// `cache-diagnosis` 不再是队尾，得插在 `message-threads` 之前。真样本到了若差分不同，
    /// 改的是这条测试的输入，不是落位规则。
    #[test]
    fn merged_beta_places_cache_diagnosis_before_message_threads() {
        const OFFICIAL: &str = "claude-code-20250219,oauth-2025-04-20,\
             interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,\
             thinking-display-updates-2026-08-18,afk-mode-2026-01-31,\
             extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
        let stripped: Vec<&str> = OFFICIAL
            .split(',')
            .filter(|p| {
                ![
                    config::OAUTH_BETA_HEADER,
                    config::CC_BETA_ADVANCED_TOOL_USE,
                    config::CC_BETA_EXTENDED_CACHE_TTL,
                    config::CC_BETA_CACHE_DIAGNOSIS,
                ]
                .contains(p)
            })
            .collect();
        assert_eq!(
            merge_beta(Some(&stripped.join(",")), Some("claude-sonnet-5"), Some((2, 1, 270))),
            OFFICIAL
        );
    }

    /// 2.1.270 sonnet 官方串**只缺** `thinking-display-updates` 时，补回去要落在 `effort` 之后、
    /// `afk-mode` 之前（`cap/2.1.270/00017`）。这一版没有 `fallback-credit`，此前只认它做锚点，
    /// 于是这一项会被追加到队尾、排在 `message-threads` 后面。
    ///
    /// 「只缺这一项」现实里对应 API-key 端的 2.1.270 sonnet（2.1.258 时 API-key 端的 fable 就是
    /// 缺它），没有抓包，故只钉落位，不宣称差分。
    #[test]
    fn merged_beta_places_thinking_display_after_effort_without_fallback_credit() {
        const OFFICIAL: &str = "claude-code-20250219,oauth-2025-04-20,\
             interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,\
             thinking-display-updates-2026-08-18,afk-mode-2026-01-31,\
             extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
        let without = OFFICIAL.replace(",thinking-display-updates-2026-08-18", "");
        assert_eq!(
            merge_beta(Some(&without), Some("claude-sonnet-5"), Some((2, 1, 270))),
            OFFICIAL
        );
        // 连 `afk-mode` 也没带（它是动态项）：仍紧跟 `effort`。
        let without_afk = without.replace(",afk-mode-2026-01-31", "");
        assert_eq!(
            merge_beta(Some(&without_afk), Some("claude-sonnet-5"), Some((2, 1, 270))),
            OFFICIAL.replace(",afk-mode-2026-01-31", "")
        );
    }

    /// 补齐 + 落位后应与官方客户端的 beta 串**逐字节一致**，三个模型族都要过。
    #[test]
    fn merged_beta_matches_official_order() {
        for (model, client, official) in BETA_PAIRS {
            let v = HeaderValue::from_str(client).unwrap();
            assert_eq!(
                &merge_beta(Some(v.to_str().unwrap()), None, Some((2, 1, 220))),
                official,
                "{model} 的 beta 串没对齐"
            );
        }
    }

    /// 客户端自有的那串**一字不动**：这是三对抓包里唯一稳定的不变量，重排它就等于自造判据。
    #[test]
    fn merged_beta_preserves_client_order() {
        for (model, client, _) in BETA_PAIRS {
            let v = HeaderValue::from_str(client).unwrap();
            let out = merge_beta(Some(v.to_str().unwrap()), None, Some((2, 1, 220)));
            let kept: Vec<&str> =
                out.split(',').filter(|b| client.split(',').any(|c| c.trim() == *b)).collect();
            let sent: Vec<&str> = client.split(',').map(str::trim).collect();
            assert_eq!(kept, sent, "{model} 的客户端自有串被重排了: {out}");
        }
    }

    /// 未知 beta 不被丢弃，也不被挪位——它在客户端串里什么位置就还在什么位置。
    #[test]
    fn merged_beta_keeps_unknown_betas_in_place() {
        let (_, client, official) = BETA_PAIRS[1];
        let v = HeaderValue::from_str(&format!("{client},some-future-beta-2027-01-01")).unwrap();
        let out = merge_beta(Some(v.to_str().unwrap()), None, Some((2, 1, 220)));
        // 客户端把它放在自有串末尾，官方串里它就该在 effort 之后、extended-cache-ttl 之前。
        assert_eq!(
            out,
            official.replace(
                ",extended-cache-ttl-2025-04-11",
                ",some-future-beta-2027-01-01,extended-cache-ttl-2025-04-11"
            )
        );
    }

    /// 无来访 beta 是退化情形（真实客户端必带），仍要给出确定输出：四个注入项按落位规则排。
    #[test]
    fn merged_beta_from_empty_is_deterministic() {
        assert_eq!(
            merge_beta(None, None, None),
            "oauth-2025-04-20,advanced-tool-use-2025-11-20,prompt-caching-scope-2026-01-05,\
             extended-cache-ttl-2025-04-11"
        );
    }

    /// 来访客户端的头（API-key 模式的 CC，取自抓包 041）。构造成 `HeaderMap` 时保持插入序，
    /// 与 axum 从线上解析出来的顺序一致。
    fn incoming_headers() -> crate::proxy::HeaderMap {
        let mut h = crate::proxy::HeaderMap::new();
        for (k, v) in [
            ("accept", "application/json"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
            ("authorization", "Bearer luban-CLIENT-KEY"),
            ("connection", "keep-alive"),
            ("content-type", "application/json"),
            ("proxy-connection", "Keep-Alive"),
            ("x-claude-code-session-id", "6eb83bfc-fdf8-4c43-ba4a-6ff95c60a0de"),
            ("x-stainless-arch", "arm64"),
            ("x-stainless-os", "MacOS"),
            ("anthropic-beta", "claude-code-20250219,effort-2025-11-24"),
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("anthropic-version", "2023-06-01"),
            ("x-app", "cli"),
        ] {
            h.insert(crate::proxy::HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        h
    }

    /// 需要 luban 改写取值的头必须**留在来访客户端给它们的位置上**。
    ///
    /// 剥离后再 `insert` 会把它们追加到队尾，得到官方客户端不会产生的头序——和 `merge_beta`
    /// 处理的是同一类问题（那个管值内顺序，这个管头之间的顺序）。
    #[test]
    fn forward_headers_keep_client_order() {
        let out =
            build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", all_on(), None, None);

        assert_eq!(
            names(&out),
            vec![
                "accept",
                "accept-encoding",
                "authorization", // 原位，值被换成 OAuth token
                "connection",    // keep-alive 保留转发
                "content-type",
                // proxy-connection 被剥离
                "x-claude-code-session-id",
                "x-stainless-arch",
                "x-stainless-os",
                "anthropic-beta", // 原位，值被合并重排
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version", // 原位
                "x-app",
                "x-client-request-id", // 客户端没带，无原位可循，追加末尾
            ],
            "转发头序被打乱"
        );

        // 值确实被覆盖了，不是原样透传。
        assert_eq!(out["authorization"], "Bearer sk-ant-oat01-REAL");
        assert!(
            out["anthropic-beta"].to_str().unwrap().contains(config::OAUTH_BETA_HEADER),
            "anthropic-beta 未合并 oauth"
        );
    }

    /// 形态开关全关 = 只注入鉴权，其余头逐项原样：beta 不重排、不塞 oauth，
    /// 也不补任何客户端没带的头。实测上游只强制 `Authorization`，故这条路径必须真能走通。
    #[test]
    fn all_flags_off_only_injects_auth() {
        let flags = store::ForwardFlags {
            spoof_identity: false,
            spoof_device_id: false,
            normalize_device_fp: false,
            billing_cch: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
            thinking_signature_retry: false,
            thinking_modified_retry: false,
            redacted_thinking_retry: false,
            simulate_cc: false,
            simulate_full_system: false,
            fill_absent_tools: false,
            fill_metadata: false,
            rate_limit_retry: false,
            cache_scope_global: false,
            cache_ttl_1h: false,
            eager_tool_streaming: false,
            nonstream_as_sse: false,
            strip_extra_fields: false,
            tool_name_mimic: false,
            inject_thinking: false,
            flatten_tool_schemas: true,
            strip_empty_text: true,
            hoist_system_role: false,
            reject_openai_shape: false,
            reject_session_conflict: false,
            reject_probes: false,
            reject_probes_strict: false,
            reject_refusals: false,
            reject_empty_replies: false,
            api_telemetry: false,
            keepalive_telemetry: false,
            fable_refusal_fallback: false,
            opus_refusal_fallback: false,
        };
        let out =
            build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", flags, None, None);

        // 头序与来访一致，且末尾不再追加 x-client-request-id。
        assert_eq!(
            names(&out),
            vec![
                "accept",
                "accept-encoding",
                "authorization",
                "connection",
                "content-type",
                "x-claude-code-session-id",
                "x-stainless-arch",
                "x-stainless-os",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version",
                "x-app",
            ],
            "全关时不应补头"
        );
        // 唯一必需的改动仍然生效。
        assert_eq!(out["authorization"], "Bearer sk-ant-oat01-REAL");
        // beta 原样转发：既不重排也不塞 oauth。
        assert_eq!(out["anthropic-beta"], "claude-code-20250219,effort-2025-11-24");
        assert!(
            !out["anthropic-beta"].to_str().unwrap().contains(config::OAUTH_BETA_HEADER),
            "merge_beta 关闭后不应塞 oauth beta"
        );
    }

    /// 客户端什么都没带时，`fill_client_headers` 决定补不补——关掉就真的一个都不补，
    /// 只留鉴权（`accept-encoding` 由上游 client 的 default_headers 兜底，不在这一层）。
    #[test]
    fn fill_off_adds_nothing_for_bare_client() {
        let mut bare = crate::proxy::HeaderMap::new();
        bare.insert(
            crate::proxy::HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );

        let on = build_forward_headers(&bare, "tok", all_on(), None, None);
        assert_eq!(
            names(&on),
            vec![
                "content-type",
                "anthropic-version",
                "anthropic-beta",
                "accept-encoding",
                "x-client-request-id",
                "authorization"
            ],
            "开启时应补齐这四个头"
        );

        let flags = store::ForwardFlags { fill_client_headers: false, ..all_on() };
        let off = build_forward_headers(&bare, "tok", flags, None, None);
        assert_eq!(
            names(&off),
            vec!["content-type", "anthropic-beta", "authorization"],
            "关闭后只该有客户端原有的头 + beta + 鉴权"
        );
        assert!(!off.contains_key("x-client-request-id"));
        assert!(!off.contains_key(header::ACCEPT_ENCODING));
        assert!(!off.contains_key("anthropic-version"));
    }

    /// 覆盖失败时必须把 `authorization` 摘掉，不能把来访者的接入 key 漏给上游。
    /// （这条只有在「照常转发再覆盖」的写法下才存在，剥离式写法天然没有这个洞。）
    #[test]
    fn never_leaks_client_key_upstream() {
        // token 里塞进换行——`HeaderValue::from_str` 会拒绝，走到移除分支。
        let out = build_forward_headers(&incoming_headers(), "bad\ntoken", all_on(), None, None);
        assert!(!out.contains_key("authorization"), "构造失败时应移除该头: {out:?}");
        // 任何路径下都不得把接入 key 转发出去。
        for (_, v) in out.iter() {
            assert!(
                !v.to_str().unwrap_or("").contains("luban-CLIENT-KEY"),
                "接入 key 泄漏到上游: {out:?}"
            );
        }
        assert!(!out.contains_key("x-api-key"), "x-api-key 不应转发");
    }

    /// 起个裸 TCP「上游」，用 [`crate::clients::upstream_client`] 那份**真配置**打一发，
    /// 返回请求的原始字节（不做大小写归一化——这里正是要看拼写的）。
    async fn capture_wire(orig_case: bool) -> String {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut r = BufReader::new(&stream);
            let mut raw = String::new();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let end = line == "\r\n";
                raw.push_str(&line);
                if end {
                    break;
                }
            }
            (&stream).write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").unwrap();
            raw
        });

        let req = crate::clients::upstream_client(None)
            .unwrap()
            .post(format!("http://{addr}/v1/messages?beta=true"))
            .headers(build_forward_headers(
                &incoming_headers(),
                "sk-ant-oat01-REAL",
                all_on(),
                None,
                None,
            ))
            .body(r#"{"model":"claude-sonnet-5"}"#);
        let req = if orig_case { req.orig_headers(crate::proxy::orig_header_case()) } else { req };
        let _ = req.send().await;

        server.join().unwrap()
    }

    /// 取出线上的头名，保留原始拼写与顺序。
    fn wire_names(raw: &str) -> Vec<&str> {
        raw.lines()
            .skip(1) // 请求行
            .filter(|l| !l.is_empty())
            .map(|l| l.split(':').next().unwrap())
            .collect()
    }

    /// 线上字节的看门狗：头名的**拼写与顺序**都要与官方客户端一致
    /// （基准是 `cap/raw/00006` 的原始报文头，claude-cli/2.1.220 直连）。
    ///
    /// 这条同时钉住三件事，任一退化都会失败：
    /// 1. 分裂大小写——标准头首字母大写、`anthropic-*`/`x-app`/`x-client-request-id` 全小写、
    ///    `X-Stainless-OS` 的 `OS` 全大写（机械 title-case 会写成 `X-Stainless-Os`）。
    /// 2. `User-Agent` 落在 `Content-Type` 与 `X-Claude-Code-Session-Id` 之间，而
    ///    `Connection`/`Host`/`Accept-Encoding`/`Content-Length` 是队尾四个——这是官方线序，
    ///    不是字母序（曾照 `cap/040.json` 的字母序排过，那是抓包工具重排的产物）。
    /// 3. 显式的 `Connection: keep-alive` 确实发出（客户端库默认认为 HTTP/1.1 隐含、不发）。
    #[tokio::test]
    async fn wire_bytes_match_official_header_form() {
        let raw = capture_wire(true).await;
        assert_eq!(
            wire_names(&raw),
            &[
                "Accept",
                "Authorization",
                "Content-Type",
                "User-Agent", // 来访没带，由 upstream_client 兜底，靠 CC_HEADER_ORDER 归位
                "X-Claude-Code-Session-Id",
                "X-Stainless-Arch",
                "X-Stainless-OS",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version",
                "x-app",
                "x-client-request-id",
                // 队尾四个：客户端库自己追加的那些，官方也在这个位置。
                "Connection",
                "Host",
                "Accept-Encoding",
                "Content-Length",
            ],
            "线上头名的拼写或顺序与官方形态不符:\n{raw}"
        );
        assert!(raw.contains("Connection: keep-alive"), "显式的 Connection 头被吞掉了:\n{raw}");
    }

    /// `orig_header_case` 关掉后的形态。**注意它并不等于换 wreq 之前那份**：
    ///
    /// - 头名退回全小写（这点与 reqwest 时代一致）；
    /// - 但 `default_headers` 里的 `user-agent`/`accept-encoding` 被**前置到队首**，
    ///   来访客户端的头序因此被打散（reqwest 是把它们并进原位/队尾的）；
    /// - `host`/`content-length` 仍在队尾。
    ///
    /// 也就是说这条 off 路径比开着**更不像**官方客户端，它的用途只是出问题时能二分
    /// （「是 OrigHeaderMap 引入的问题，还是别处」），不是一个可用的形态选择。
    #[tokio::test]
    async fn wire_bytes_fall_back_to_lowercase_when_off() {
        let raw = capture_wire(false).await;
        let wire = wire_names(&raw);

        let tail = wire.len() - 2;
        assert_eq!(
            &wire[..tail],
            &[
                // default_headers 被前置，不在来访客户端给它们的位置上
                "user-agent",
                "accept-encoding",
                // 以下按来访头序
                "accept",
                "authorization",
                "connection",
                "content-type",
                "x-claude-code-session-id",
                "x-stainless-arch",
                "x-stainless-os",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version",
                "x-app",
                "x-client-request-id",
            ],
            "关掉开关后的形态与实测不符:\n{raw}"
        );
        // 位置不可控，只断言这两个确实在末尾。
        let mut appended = wire[tail..].to_vec();
        appended.sort_unstable();
        assert_eq!(appended, ["content-length", "host"], "\n{raw}");
    }

    /// 补齐的 x-client-request-id 是标准 uuid v4 形态。
    #[test]
    fn generates_uuid_v4() {
        let u = uuid_v4();
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "非法字符: {u}");
        assert_eq!(parts[2].as_bytes()[0], b'4', "version 位应为 4: {u}");
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'), "variant 位不对: {u}");
        assert_ne!(u, uuid_v4(), "每请求应不同");
    }

    /// 头侧 `ensure_fallback_beta`：补 06-01 那条在 `effort` 之后；已有（不论日期）不动；
    /// `build_forward_headers_for` 传 `fallback_beta` 时 opus 的官方串（本不发）也带上。
    #[test]
    fn fallback_beta_is_added_once_after_effort() {
        let out = crate::proxy::ensure_fallback_beta(
            "claude-code-20250219,effort-2025-11-24,fallback-credit-2026-06-01".into(),
        );
        assert_eq!(
            out,
            "claude-code-20250219,effort-2025-11-24,server-side-fallback-2026-06-01,fallback-credit-2026-06-01"
        );
        let had = "a,server-side-fallback-2026-07-01,b".to_string();
        assert_eq!(crate::proxy::ensure_fallback_beta(had.clone()), had, "同名不同日期算已有");
        assert_eq!(
            crate::proxy::ensure_fallback_beta(String::new()),
            "server-side-fallback-2026-06-01"
        );
        assert_eq!(
            crate::proxy::ensure_fallback_beta("x,y".into()),
            "x,y,server-side-fallback-2026-06-01"
        );

        let sim = sim_for(PLAIN_BODY);
        let with = crate::proxy::build_forward_headers_for(
            &crate::proxy::HeaderMap::new(),
            "tok",
            all_on(),
            Some(&sim),
            None,
            Some("claude-opus-5"),
            true,
        );
        let beta = with.get("anthropic-beta").unwrap().to_str().unwrap().to_string();
        assert_eq!(beta.matches("server-side-fallback-").count(), 1, "{beta}");
        let idx = |b: &str| beta.split(',').position(|p| p.starts_with(b)).unwrap();
        assert!(idx("effort-") < idx("server-side-fallback-"), "{beta}");
        // 2.1.280 的 opus 不发 `fallback-credit` 了，`effort` 下一格是 `dangerous-tool-use`。
        assert!(idx("server-side-fallback-") < idx("dangerous-tool-use-"), "{beta}");
        let without = crate::proxy::build_forward_headers_for(
            &crate::proxy::HeaderMap::new(),
            "tok",
            all_on(),
            Some(&sim),
            None,
            Some("claude-opus-5"),
            false,
        );
        assert!(
            !without
                .get("anthropic-beta")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("server-side-fallback")
        );
    }

    /// 模拟路径产出的 `anthropic-beta` 必须**逐字节**等于官方那串——这是
    /// [`config::CC_PROFILES`] 里几串 beta 唯一的正确性依据。官方串取自 `cap/2.1.280`
    /// 非 auto 模式那段的主线程（haiku 只有 auto 段样本，它在两种模式下形态本就一致）。
    ///
    /// 四族分开验：haiku 不发 `mid-conversation-*`/`effort` 且 `claude-code-20250219` 在**串
    /// 中间**；四族队尾都是 `message-threads`；opus 多 `context-1m`；opus / fable 多 `per-turn-control`
    /// 与 `mid-conversation-tool-changes`。共用一份种子串就会给某一族发出真实客户端不产生的排列。
    #[test]
    fn simulated_beta_matches_official() {
        // cap/2.1.280/00065（opus-5-5 直连，非 auto 模式）。
        const OFFICIAL_OPUS: &str = "claude-code-20250219,oauth-2025-04-20,\
             context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
             advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
             mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
             dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
             thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
             cache-diagnosis-2026-04-07,message-threads-2026-08-12";
        // cap/2.1.280/00068（fable-5-1 直连，非 auto 模式）。
        const OFFICIAL_FABLE: &str = "claude-code-20250219,oauth-2025-04-20,\
             interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,per-turn-control-2026-07-01,\
             mid-conversation-tool-changes-2026-07-01,advisor-tool-2026-03-01,\
             advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
             effort-2025-11-24,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
             thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
             cache-diagnosis-2026-04-07,message-threads-2026-08-12";
        // cap/2.1.280/00073（sonnet-5 直连，非 auto 模式，`thread: create`）。
        const OFFICIAL_SONNET: &str = "claude-code-20250219,oauth-2025-04-20,\
             interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
             advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
             effort-2025-11-24,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
             thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
             cache-diagnosis-2026-04-07,message-threads-2026-08-12";
        // cap/2.1.280/00038（haiku-4.5 直连，`thread: create`）。
        const OFFICIAL_HAIKU: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
             advanced-tool-use-2025-11-20,dangerous-tool-use-2026-09-03,\
             thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
             extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
             message-threads-2026-08-12";

        for (model, official) in [
            ("claude-sonnet-5", OFFICIAL_SONNET),
            ("claude-opus-5", OFFICIAL_OPUS),
            ("claude-fable-5-1", OFFICIAL_FABLE),
            ("claude-fable-5", OFFICIAL_FABLE), // 没有 fable-5 样本，按族归 fable
            ("gpt-4o", OFFICIAL_SONNET),        // 认不出的模型退回 sonnet 主串
            ("claude-haiku-4-5-20251001", OFFICIAL_HAIKU),
        ] {
            let profile = crate::proxy::cc_profile_for(model);
            assert_eq!(crate::proxy::simulated_beta(profile.beta, None), official, "{model}");
        }

        // 客户端自己要的 beta 不丢，去重后追加在官方串之后。
        let with_client = crate::proxy::simulated_beta(
            crate::proxy::cc_profile_for("claude-sonnet-5").beta,
            Some("output-128k-2025-02-19, effort-2025-11-24"),
        );
        assert!(
            with_client.contains("output-128k-2025-02-19"),
            "客户端的 beta 被丢了: {with_client}"
        );
        assert_eq!(with_client.matches("effort-2025-11-24").count(), 1, "重复项: {with_client}");
    }

    /// 2.1.277 三个辅助 profile 的 beta 串逐字对上抓包（去掉 `oauth` 之后；四族主线程由
    /// [`simulated_beta_matches_official`] 钉住）。
    #[test]
    fn profile_betas_match_the_2_1_277_captures() {
        use config::CcProfileKind::*;
        let cases: &[(config::CcProfileKind, &str, &str)] = &[
            (
                SdkSubagentHaiku,
                "cap/2.1.277/00049",
                "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
                 thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
                 cache-diagnosis-2026-04-07,message-threads-2026-08-12",
            ),
            (
                SessionTitleHaiku,
                "cap/2.1.277/00022",
                "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
                 structured-outputs-2025-12-15,cache-diagnosis-2026-04-07",
            ),
            (
                QuotaProbe,
                "cap/2.1.277/00005",
                "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05",
            ),
        ];
        for (kind, cap, official) in cases {
            let p = config::cc_profile_at(*kind, Some((2, 1, 277)));
            assert_eq!(p.version, "2.1.277", "{kind:?}");
            assert_eq!(p.beta, *official, "{kind:?}（{cap}）");
        }
        // 2.1.280 的额度探测（`cap/2.1.280/00008`）与 2.1.277 逐字相同。
        let quota = config::cc_profile(QuotaProbe);
        assert_eq!((quota.version, quota.beta), ("2.1.280", cases[2].2));
        // 每个 2.1.280 主线程 profile 的 `oauth` 都由 simulated_beta 落到官方位置：以 claude-code
        // 开头的紧随其后，haiku 排在最前（cap/2.1.280/00038 头两项 `oauth,interleaved`）。
        assert!(
            crate::proxy::simulated_beta(config::cc_profile(MainHaiku).beta, None)
                .starts_with("oauth-2025-04-20,interleaved-thinking-2025-05-14,")
        );
        assert!(
            crate::proxy::simulated_beta(config::cc_profile(MainOpus).beta, None)
                .starts_with("claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,")
        );
    }

    /// 六个已观察的 2.1.260 profile 的 beta 串逐字对上抓包（去掉 `oauth` 与动态的 `afk-mode`
    /// 之后）。这张表现在只给 2.1.260 ~ 2.1.276 来访的 [`merge_beta`] 做参照，与 2.1.277 表没
    /// 编的两个 kind 兜底，验收照旧。
    #[test]
    fn profile_betas_match_the_2_1_260_captures() {
        use config::CcProfileKind::*;
        // 每项：profile、抓包出处、官方串（去掉 oauth 与 afk-mode）。
        let cases: &[(config::CcProfileKind, &str, &str)] = &[
            (
                SdkSubagentHaiku,
                "cap/2.1.260/00020",
                "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 claude-code-20250219,thinking-display-updates-2026-08-18,\
                 cache-diagnosis-2026-04-07",
            ),
            (
                HelperSubagentHaiku,
                "cap/2.1.260/00024",
                "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,server-side-fallback-2026-06-01,\
                 fallback-credit-2026-06-01,cache-diagnosis-2026-04-07",
            ),
            (
                SessionTitleHaiku,
                "cap/2.1.260-2/00058",
                "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
                 structured-outputs-2025-12-15,fallback-credit-2026-06-01,\
                 cache-diagnosis-2026-04-07",
            ),
            (
                SecurityClassifierSonnet,
                "cap/2.1.260/00019",
                "claude-code-20250219,context-1m-2025-08-07,\
                 interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 mid-conversation-system-2026-04-07,auto-mode-classifier-2026-07-16,\
                 extended-cache-ttl-2025-04-11",
            ),
            (
                QuotaProbe,
                "cap/2.1.260-2/00004",
                "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05",
            ),
        ];
        for (kind, cap, official) in cases {
            let p = config::cc_profile_at(*kind, Some((2, 1, 260)));
            assert_eq!(p.version, "2.1.260", "{kind:?}");
            assert_eq!(p.beta, *official, "{kind:?}（{cap}）");
        }
    }

    /// [`merge_beta`] 对一条**完整的** 2.1.277 / 2.1.280 订阅端串必须幂等：参照串按来访版本
    /// 取那一版的表，`server-side-fallback` 四族都不在参照里、不补；2.1.277 的 `fallback-credit`
    /// 已在位，2.1.280 的参照里没有它、也不补。参照选错一版就会把上一版才有的项塞回来——
    /// 2.1.280 的串拿 2.1.277 表去补，就会在 `effort` 后面多出一个 `fallback-credit`。
    #[test]
    fn merged_beta_is_idempotent_on_2_1_277_and_2_1_280_main_threads() {
        use config::CcProfileKind::*;
        for version in [(2, 1, 277), (2, 1, 280)] {
            for (kind, model) in [
                (MainOpus, "claude-opus-5"),
                (MainFable, "claude-fable-5-1"),
                (MainSonnet, "claude-sonnet-5"),
                (MainHaiku, "claude-haiku-4-5-20251001"),
            ] {
                let official = crate::proxy::simulated_beta(
                    config::cc_profile_at(kind, Some(version)).beta,
                    None,
                );
                assert_eq!(
                    merge_beta(Some(&official), Some(model), Some(version)),
                    official,
                    "{kind:?} {version:?}: 完整的官方串过 merge_beta 不该多一项"
                );
                assert!(!official.contains("server-side-fallback"), "{kind:?}");
            }
        }
    }

    /// 官方辅助请求经 [`merge_beta`] 之后**一项都不多**：它们各有一套更短的 beta 集合，
    /// 主线程那几项（`advanced-tool-use`/`server-side-fallback`/`extended-cache-ttl`…）
    /// 补进去就是一个官方从不产生的串。API-key 端不发 `oauth`，那一项要补。
    #[test]
    fn merge_beta_leaves_official_helper_requests_alone() {
        for (name, kind) in [
            ("标题生成", config::CcProfileKind::SessionTitleHaiku),
            ("安全分类", config::CcProfileKind::SecurityClassifierSonnet),
            ("无工具 helper", config::CcProfileKind::HelperSubagentHaiku),
            ("额度探测", config::CcProfileKind::QuotaProbe),
        ] {
            let official = crate::proxy::simulated_beta(config::cc_profile(kind).beta, None);
            // API-key 端那份就是官方串去掉 oauth；merge_beta 应当只把它补回来。
            let api_key_side: Vec<&str> =
                official.split(',').filter(|p| *p != config::OAUTH_BETA_HEADER).collect();
            assert_eq!(
                merge_beta(Some(&api_key_side.join(",")), None, Some((2, 1, 260))),
                official,
                "{name}: merge_beta 不该给辅助请求补主线程那几项"
            );
        }
    }

    /// **SDK 子代理不是主线程**：它有 `claude-code`，前几条判据都放它过去，于是会被补上
    /// `advanced-tool-use` 与 `extended-cache-ttl`——官方那条（`cap/2.1.260/00020`）两样都没有。
    ///
    /// 判据是「有 `thinking-display-updates`，却一个 `effort`/`advisor-tool`/`per-turn-control`
    /// 都没有」。这一条不能误伤 2.1.220 那代的 haiku（[`BETA_PAIRS`] 第三对）——它压根没有
    /// `thinking-display-updates`，照旧要补那两项。
    #[test]
    fn merge_beta_leaves_the_sdk_subagent_alone() {
        // 2.1.260 的子代理串。2.1.277 的子代理带上了 advisor-tool / advanced-tool-use，这条
        // 判据认不出它——见 config::cc_2_1_277_missing_samples 第 3 条，没有 API-key 端样本前不改。
        let official = crate::proxy::simulated_beta(
            config::cc_profile_at(config::CcProfileKind::SdkSubagentHaiku, Some((2, 1, 260))).beta,
            None,
        );
        let api_key_side: Vec<&str> =
            official.split(',').filter(|p| *p != config::OAUTH_BETA_HEADER).collect();
        let out = merge_beta(
            Some(&api_key_side.join(",")),
            Some("claude-haiku-4-5-20251001"),
            Some((2, 1, 260)),
        );
        assert_eq!(out, official, "SDK 子代理只该补 oauth");
        assert!(!out.contains("advanced-tool-use"), "官方那条没有这一项: {out}");
        assert!(!out.contains("extended-cache-ttl"), "也没有这一项: {out}");

        // 反例：2.1.220 的 haiku 没有 thinking-display-updates，照旧按主线程补齐。
        let (_, client_220, official_220) = BETA_PAIRS[2];
        assert_eq!(
            &merge_beta(Some(client_220), None, Some((2, 1, 220))),
            official_220,
            "老世代的 haiku 不该被当成 SDK 子代理"
        );
    }

    /// 模拟路径追加客户端 beta 时，**同名不同日期**算已经有了，**互斥项**直接丢。
    ///
    /// 两种都是官方从不产生的组合：两条 `server-side-fallback`、或
    /// `redact-thinking` 与 `thinking-display-updates` 并存。
    #[test]
    fn simulated_beta_rejects_conflicting_client_betas() {
        // 2.1.260 的官方 fable 串里是 06-01；客户端带 07-01，不该拼出两条。（2.1.277 四族都不发
        // 这一项了，故用 2.1.260 那行做同名不同日期的样本。）
        let fable_260 =
            config::cc_profile_at(config::CcProfileKind::MainFable, Some((2, 1, 260))).beta;
        let out = crate::proxy::simulated_beta(fable_260, Some("server-side-fallback-2026-07-01"));
        assert_eq!(out.matches("server-side-fallback-").count(), 1, "只该有一条: {out}");
        assert!(out.contains("server-side-fallback-2026-06-01"), "留官方那条日期: {out}");
        // 2.1.277 的 fable 串里没有它：客户端带来的那条照发（不是同名、不是互斥，上游判）。
        let fable = crate::proxy::cc_profile_for("claude-fable-5-1").beta;
        let out = crate::proxy::simulated_beta(fable, Some("server-side-fallback-2026-07-01"));
        assert_eq!(out.matches("server-side-fallback-").count(), 1, "{out}");

        // opus 官方串有 thinking-display-updates，客户端带 redact-thinking → 丢。
        let opus = crate::proxy::cc_profile_for("claude-opus-5").beta;
        let out = crate::proxy::simulated_beta(opus, Some(config::CC_BETA_REDACT_THINKING));
        assert!(!out.contains("redact-thinking"), "互斥项不能并存: {out}");
        assert!(out.contains(config::CC_BETA_THINKING_DISPLAY_UPDATES), "{out}");

        // 反向也拦：官方串有 redact-thinking（额度探测）时，客户端的 display-updates 丢掉。
        let probe = config::cc_profile(config::CcProfileKind::QuotaProbe).beta;
        let out =
            crate::proxy::simulated_beta(probe, Some(config::CC_BETA_THINKING_DISPLAY_UPDATES));
        assert!(!out.contains("thinking-display-updates"), "反向同样互斥: {out}");

        // 不冲突的照旧追加，一个都不能少。
        let out = crate::proxy::simulated_beta(opus, Some("output-128k-2025-02-19"));
        assert!(out.contains("output-128k-2025-02-19"), "{out}");
    }

    /// 2.1.260 的 fable 缺了 `server-side-fallback-2026-06-01` 时必须补得回来。
    ///
    /// 回归：`seed_has` 原先是逐字比对常量 `…-2026-07-01`，而 fable 的官方串里是 06-01，
    /// 于是「官方发这一项吗」永远答否，整个补齐分支成了死代码。
    #[test]
    fn merge_beta_backfills_the_fable_server_side_fallback() {
        // 2.1.260 fable 的 API-key 端形态：缺 oauth / advanced-tool-use /
        // server-side-fallback / extended-cache-ttl / cache-diagnosis。
        let api_key = "claude-code-20250219,interleaved-thinking-2025-05-14,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             per-turn-control-2026-07-01,effort-2025-11-24,fallback-credit-2026-06-01,\
             thinking-display-updates-2026-08-18";
        let out = merge_beta(Some(api_key), Some("claude-fable-5-1"), Some((2, 1, 260)));
        assert!(
            out.contains(config::CC_BETA_SERVER_SIDE_FALLBACK_JUN),
            "fable 该补上 06-01 那条: {out}"
        );
        assert!(
            !out.contains(config::CC_BETA_SERVER_SIDE_FALLBACK),
            "不能补成 2.1.258 那个日期: {out}"
        );
        assert_eq!(out.matches("server-side-fallback-").count(), 1, "{out}");
        // 补完就是官方那串（去掉动态的 afk-mode）——这才是这条测试真正要钉的。
        assert_eq!(
            out,
            crate::proxy::simulated_beta(
                config::cc_profile_at(config::CcProfileKind::MainFable, Some((2, 1, 260))).beta,
                None
            )
        );
        // 位置：`effort` 之后、`fallback-credit` 之前（官方序）。
        let idx = |b: &str| out.split(',').position(|p| p.starts_with(b)).unwrap();
        assert!(idx("effort-") < idx("server-side-fallback-"));
        assert!(idx("server-side-fallback-") < idx("fallback-credit-"));

        // 2.1.260 的 opus 整项不发，别给它补。
        let out = merge_beta(Some(api_key), Some("claude-opus-5"), Some((2, 1, 260)));
        assert!(!out.contains("server-side-fallback-"), "opus 2.1.260 不发这一项: {out}");
    }

    /// 带日期的 beta 按**名字**判在不在：一个已经带 `server-side-fallback-2026-06-01` 的
    /// 2.1.260 来访，不该再被插一条 `-2026-07-01`。
    #[test]
    fn merge_beta_does_not_duplicate_a_redated_beta() {
        // fable 2.1.260 的 API-key 端形态：有 per-turn-control 与 06-01 那条。
        let incoming = "claude-code-20250219,interleaved-thinking-2025-05-14,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             per-turn-control-2026-07-01,effort-2025-11-24,\
             server-side-fallback-2026-06-01,fallback-credit-2026-06-01";
        let out = merge_beta(Some(incoming), Some("claude-fable-5-1"), Some((2, 1, 260)));
        assert_eq!(out.matches("server-side-fallback-").count(), 1, "只该有一条: {out}");
        assert!(out.contains("server-side-fallback-2026-06-01"), "保留客户端那条日期: {out}");
        assert!(out.contains(config::OAUTH_BETA_HEADER), "oauth 要补上: {out}");
    }

    /// 模拟模式下来访那套头一个不留：UA/x-app/x-stainless-* 全是官方取值，
    /// 客户端自带的非官方头（`x-my-tool`）不转发，`anthropic-beta` 取并集。
    #[test]
    fn simulated_headers_replace_client_headers() {
        let mut client = crate::proxy::HeaderMap::new();
        for (k, v) in [
            ("user-agent", "python-httpx/0.27.0"),
            ("accept", "text/event-stream"),
            ("x-my-tool", "cherry-studio"),
            ("anthropic-beta", "output-128k-2025-02-19"),
        ] {
            client.insert(crate::proxy::HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        let sim = sim_for(PLAIN_BODY);
        let out = build_forward_headers(&client, "sk-ant-oat01-REAL", all_on(), Some(&sim), None);
        let get = |k: &str| out.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default();

        assert_eq!(get("user-agent"), config::CC_USER_AGENT);
        assert_eq!(get("accept"), "application/json", "官方即便流式也发 application/json");
        assert_eq!(get("x-app"), "cli");
        assert_eq!(get("x-stainless-os"), "MacOS");
        assert_eq!(get("anthropic-version"), "2023-06-01");
        assert_eq!(get("accept-encoding"), config::CC_ACCEPT_ENCODING);
        assert_eq!(get("x-claude-code-session-id"), sim.session_id, "会话 id 与 metadata 同值");
        assert!(!get("x-client-request-id").is_empty(), "每请求一个 uuid");
        assert_eq!(get("authorization"), "Bearer sk-ant-oat01-REAL");
        assert!(out.get("x-my-tool").is_none(), "客户端的非官方头不该带到上游");
        assert!(get("anthropic-beta").contains("output-128k-2025-02-19"), "客户端 beta 不该丢");
        assert!(get("anthropic-beta").contains(config::OAUTH_BETA_HEADER));

        // 表里的头全在，且没有多出表外的头（除四个由 HTTP 客户端自己追加的）。
        for (name, _) in config::CC_SIM_HEADERS {
            assert!(out.contains_key(*name), "缺头 {name}");
        }
    }
}
