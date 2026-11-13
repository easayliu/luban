//! 2.1.323 起官方 MCP 地址表（`GET /api/directory/server-urls`）取回的结果，给遥测那条 `tengu_mcp_registry_fetch` 用。
//!
//! 回包是 `{"urls": ["https://…/mcp", …]}`。官方报的 `url_count` 是**去掉查询串后去重**的条数，不是数组长度
//! （`cap/auto-2.1.323-20261113-full/00033`：1047 条、报 1046；`-bashfail/2.1.323/00028`：1048 条、报 1047——
//! `attentive-ui-mcp.attentivemobile.com/mcp` 出现两次，一次带 `?readOnly=false`）。这张表是服务端状态，一天里就会变
//! （上面两份相隔二十分钟，多了一条），所以不写死、按握手那条实际拿到的数。
//!
//! 取失败时官方报 `success: false`、`url_count: 0`，前面那条 `feature_ok{mcp_registry_fetch}` 换成
//! `feature_sad{…, error_code: "fetch_failed"}`（`cap/auto-2.1.323-20261113-full` 批次 `00286`，那次是连不上；非 200
//! 与回包解不出没有样本，按同一条路报）。

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::RwLock;

/// 还没有任何一次握手拿到过回包时报的条数：2026-11-13 的实际值（`cap/auto-2.1.323-20261113-{bashfail,model,resume,retry}`）。
const FALLBACK_COUNT: u32 = 1047;

/// 记着的会话数上限：满了整表清掉。清掉的会话落回「不知道」，按最近一次的条数报成功。
const MAX_SESSIONS: usize = 4096;

/// 一个会话那次握手的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fetch {
    /// 取到了，去重后的条数。
    Ok(u32),
    /// 没取到（连不上、非 200、回包解不出）。
    Failed,
}

/// 最近一次取到的条数（地址表无鉴权、不分账号，全局一份）；0 即还没取到过。
static LAST_COUNT: AtomicU32 = AtomicU32::new(0);

/// 各会话握手那一次的结果，键是（号, 会话 id）。
static RESULTS: LazyLock<RwLock<HashMap<(i64, String), Fetch>>> = LazyLock::new(Default::default);

/// 回包体里的条数：`urls` 各项去掉查询串后去重。解不出、没有 `urls` 数组是 `None`。
pub(crate) fn count(body: &[u8]) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let urls = v.get("urls")?.as_array()?;
    let distinct: std::collections::HashSet<&str> = urls
        .iter()
        .filter_map(|u| u.as_str())
        .map(|u| u.split_once('?').map_or(u, |(head, _)| head))
        .collect();
    u32::try_from(distinct.len()).ok()
}

/// 握手那条回来（成功失败都算）时记一笔。
pub(crate) fn note(cred_id: i64, session_id: &str, fetch: Fetch) {
    if let Fetch::Ok(n) = fetch {
        LAST_COUNT.store(n, Ordering::Relaxed);
    }
    remember(cred_id, session_id, fetch);
}

/// 只记这个会话的结果，不动全局那份最近条数（测试直接用它，免得改了别的测试看到的兜底值）。
pub(crate) fn remember(cred_id: i64, session_id: &str, fetch: Fetch) {
    let mut results = RESULTS.write();
    if results.len() >= MAX_SESSIONS {
        results.clear();
    }
    results.insert((cred_id, session_id.to_owned()), fetch);
}

/// 这个会话该报什么：握手记过就照它；没记过（握手还没回来、真实 CC 自己发的那条不经 luban、进程刚重启）按取到了报，
/// 条数用最近一次的，一次都没有就用 [`FALLBACK_COUNT`]。
pub(crate) fn reported(cred_id: i64, session_id: &str) -> Fetch {
    if let Some(f) = RESULTS.read().get(&(cred_id, session_id.to_owned())) {
        return *f;
    }
    match LAST_COUNT.load(Ordering::Relaxed) {
        0 => Fetch::Ok(FALLBACK_COUNT),
        n => Fetch::Ok(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_distinct_urls_without_the_query() {
        let body = br#"{"urls":["https://a.example/mcp","https://a.example/mcp?readOnly=false",
            "https://b.example/v1/mcp?toolsets=all,visualizations","https://c.example/mcp"]}"#;
        assert_eq!(count(body), Some(3));
        assert_eq!(count(br#"{"urls":[]}"#), Some(0));
        assert_eq!(count(br#"{"servers":[]}"#), None, "没有 urls 数组算没取到");
        assert_eq!(count(b"<html>"), None);
    }

    #[test]
    fn a_session_reports_its_own_handshake() {
        // 号用别处不会撞的 id；`LAST_COUNT` 是全局的，别的测试可能改它，所以只钉记过的那两种。
        let id = -7_323_001;
        remember(id, "s-ok", Fetch::Ok(1050));
        remember(id, "s-bad", Fetch::Failed);
        assert_eq!(reported(id, "s-ok"), Fetch::Ok(1050));
        assert_eq!(reported(id, "s-bad"), Fetch::Failed, "失败不落回兜底");
        assert!(matches!(reported(id, "s-unknown"), Fetch::Ok(n) if n > 0), "没记过按取到了报");
    }
}
