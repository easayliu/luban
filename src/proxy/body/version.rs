//! 来访 UA 与 Claude Code 客户端版本的解析与可信度判定。

use super::*;

/// 从一组头里取 `User-Agent` 供日志与落库用：没有该头或不是可打印 ASCII 时为 `-`。
///
/// 来访头与出站头两侧都用它——[`ReqLog`] 两份 UA 各存各的，取值规则必须是同一套，
/// 否则「入站 == 出站」这个判断会因为两边截断/回退方式不同而失真。
///
/// 截断到 120 字符：官方 CC 那串（`claude-cli/2.1.220 (external, cli)`）只有 35 字符，
/// 浏览器与各路 SDK 拼出来的能有几百，整条打出来会把日志行撑得没法看。截断只影响日志与
/// 落库，转发出去的那份头一个字节都不动。
///
/// 取值恒为可见 ASCII：`to_str()` 对非 ASCII 头值直接失败，那类一律落 `-`。按 `char` 截而不是
/// `&s[..120]` 只是不给未来留坑——真按字节切，哪天换个不做此保证的取值方式就会切出 panic。
pub(in crate::proxy) fn ua_of(headers: &HeaderMap) -> String {
    const MAX: usize = 120;
    match headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()) {
        Some(ua) if !ua.trim().is_empty() => ua.chars().take(MAX).collect(),
        _ => "-".into(),
    }
}

/// 把版本串解析成可比较的三元组：`2` → `(2,0,0)`、`2.1` → `(2,1,0)`、`2.1.220` → `(2,1,220)`。
///
/// 三段以后的（`1.2.3.4`）忽略尾巴，预发布后缀（`2.1.220-beta.1`）按主版本 `2.1.220` 算——
/// 这道闸只用来卡「太旧」，把 beta 判成比正式版旧会误伤真正在用新版的人。任何一段不是数字、
/// 或压根没有第一段时返回 `None`（调用方据此当成「读不出版本」，一律放行）。
pub(crate) fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    // 先截掉预发布/构建后缀，只留 `数字.数字…` 那一截。
    let head: &str = s.trim().split(['-', '+']).next().unwrap_or("");
    let mut parts = head.split('.').map(|p| p.trim().parse::<u64>().ok());
    let major = parts.next().flatten()?;
    // 缺失的段按 0 补（`2` == `2.0.0`）；写了但不是数字的段则整串作废。
    let mut seg = || match parts.next() {
        None => Some(0),
        Some(v) => v,
    };
    Some((major, seg()?, seg()?))
}

/// 从 `User-Agent` 里抠出 Claude Code 自报的版本：`claude-cli/2.1.220 (external, cli)`
/// → `(2, 1, 220)`。UA 里没有 `claude-cli/`、或后面那串不是版本号时返回 `None`。
pub(in crate::proxy) fn cc_cli_version(ua: &str) -> Option<(u64, u64, u64)> {
    let rest = ua.split_once("claude-cli/")?.1;
    // 版本串到第一个非「数字/点」字符为止（官方那串后面跟的是空格 + `(external, cli)`）。
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(rest.len());
    parse_version(&rest[..end])
}

/// 官方已发布的最新 Claude Code 版本：从 `downloads.claude.ai/claude-code-releases/latest`
/// 学来的（[`crate::oauth::latest_release`]）与写死的 [`config::CC_LATEST_KNOWN_RELEASE`]
/// 取大者。
///
/// 取大者是为了两头兜底：进程刚起还没拉到 `latest` 时有个不至于太旧的下限——下限是**抓包
/// 证实过的**最新版，不是模拟路径那个更旧的 [`config::CC_VERSION_BASE`]，否则启动窗口里真实
/// 新版的来访会被判成冒充；反过来那个端点若哪天回了个更旧的数（缓存、回滚），也不能把已经
/// 证实存在的版本判成「不存在」。写死的那个不低于模拟版本，有测试钉着，故 luban 自己发出去
/// 的版本也在上限之内。
pub(crate) fn known_latest_release() -> (u64, u64, u64) {
    let base = parse_version(config::CC_LATEST_KNOWN_RELEASE).unwrap_or((0, 0, 0));
    crate::oauth::latest_release().map_or(base, |l| l.max(base))
}

/// 来访 UA 自报的 CC 版本，**且这个版本说得通**——不高于 [`known_latest_release`]。
///
/// 高于官方最新版的自报版本按「读不出版本」处理（`None`）：这不是官方客户端，跳过模拟、
/// 沿用它的版本去补 billing header / 跑握手 / 发额度探测，都是在替一个不存在的版本背书。
/// 低于最新版的一律认——用户不升级是常态，下限另有 [`below_min_client_version`] 管。
pub(in crate::proxy) fn trusted_cc_version(ua: &str) -> Option<(u64, u64, u64)> {
    trusted_cc_version_against(ua, known_latest_release())
}

/// [`trusted_cc_version`] 的纯函数形态：`latest` 由调用方给，供测试不碰全局缓存。
pub(in crate::proxy) fn trusted_cc_version_against(
    ua: &str,
    latest: (u64, u64, u64),
) -> Option<(u64, u64, u64)> {
    let v = cc_cli_version(ua)?;
    (v <= latest).then_some(v)
}

/// 最低客户端版本闸：来访 UA 自报的 CC 版本低于 `min` 时，返回 `(自报版本, 要求版本)` 供
/// 日志与错误消息使用；放行时返回 `None`。
///
/// 三种情况一律放行，都是刻意的：
/// - `min` 没配（`None`/空串）或不是版本号 —— 闸没开；
/// - UA 里没有 `claude-cli/` —— 非 CC 客户端（SDK、浏览器、自写脚本），无版本可比；
/// - `claude-cli/` 后面读不出版本号 —— 宁可放过，也不为一个解析不了的串把人挡在门外。
///
/// 注意这只是一道**引导升级**的闸，不是安全边界：UA 是客户端自报的，随手改一个头就能绕过。
pub(in crate::proxy) fn below_min_client_version(
    ua: &str,
    min: Option<&str>,
) -> Option<(String, String)> {
    let min = min?;
    let want = parse_version(min)?;
    let got = cc_cli_version(ua)?;
    (got < want).then(|| (format!("{}.{}.{}", got.0, got.1, got.2), min.trim().to_string()))
}
