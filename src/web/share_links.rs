//! 分享链接的管理接口（`/share-links`）：只给 admin（中间件按路由默认拒绝其他人，访客也在
//! 拒绝之列）。链接本身怎么用见 [`store::ShareLink`] 与 `auth` 里的鉴权分支。

use super::*;

#[derive(Deserialize)]
pub(super) struct CreateShareLinkReq {
    #[serde(default)]
    label: String,
    /// 有效期（秒），须在 [`store::SHARE_MIN_TTL_SECS`] 与 [`store::SHARE_MAX_TTL_SECS`] 之间。
    ttl_secs: u64,
}

#[derive(Serialize)]
pub(super) struct ShareLinkSecret {
    #[serde(flatten)]
    link: store::ShareLink,
    /// 明文凭据，只回这一次。前端拼成 `#/share/<key>` 的链接。
    key: String,
}

/// 名称最长多少个字符。
const MAX_LABEL_CHARS: usize = 64;

pub(super) async fn list_share_links(
    State(state): State<AppState>,
) -> Result<Json<Vec<store::ShareLink>>, ApiError> {
    Ok(Json(state.store.list_share_links().await.map_err(internal)?))
}

/// 新建一条，回它的明文——只回这一次，库里不存明文。
pub(super) async fn create_share_link(
    State(state): State<AppState>,
    Json(req): Json<CreateShareLinkReq>,
) -> Result<Json<ShareLinkSecret>, ApiError> {
    let label = req.label.trim();
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(bad_request(format!("name must be at most {MAX_LABEL_CHARS} characters")));
    }
    if !(store::SHARE_MIN_TTL_SECS..=store::SHARE_MAX_TTL_SECS).contains(&req.ttl_secs) {
        return Err(bad_request(format!(
            "ttl_secs must be between {} and {}",
            store::SHARE_MIN_TTL_SECS,
            store::SHARE_MAX_TTL_SECS
        )));
    }
    let key = store::generate_prefixed_key(store::SHARE_KEY_PREFIX);
    let link = state.store.create_share_link(label, &key, req.ttl_secs).await.map_err(internal)?;
    tracing::info!(link_id = link.id, label, ttl_secs = req.ttl_secs, "share link created");
    Ok(Json(ShareLinkSecret { link, key }))
}

/// 删一条：持这条链接的人下一次请求即 401。
pub(super) async fn delete_share_link(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.store.delete_share_link(id).await.map_err(internal)? {
        return Err((StatusCode::NOT_FOUND, "share link not found".into()));
    }
    tracing::info!(link_id = id, "share link deleted");
    Ok(Json(serde_json::json!({ "ok": true })))
}
