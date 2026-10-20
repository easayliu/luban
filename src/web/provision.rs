//! 控制台 Key（上号 Key `/provision-keys`、MCP Key `/mcp-keys`）的管理接口：各人管理自己名下的，
//! admin 管理全部；访客不开放（由中间件拦下）。两种 Key 共用一套 handler，只差 [`KeyKind`]。
//! Key 本身怎么用见 [`store::ProvisionKey`] 与 `auth` 里的鉴权分支。所属账号改过密码的 Key
//! 作废：认证时与列表时比对密码指纹，对不上就删掉。

use super::*;
use crate::store::KeyKind;

#[derive(Deserialize)]
pub(super) struct CreateProvisionKeyReq {
    #[serde(default)]
    label: String,
}

#[derive(Deserialize)]
pub(super) struct UpdateProvisionKeyReq {
    #[serde(default)]
    label: String,
    #[serde(default)]
    disabled: bool,
}

#[derive(Serialize)]
pub(super) struct ProvisionKeySecret {
    id: i64,
    key: String,
}

/// 名称最长多少个字符。
const MAX_LABEL_CHARS: usize = 64;

/// admin 管全部，其余人只管自己名下的。
fn owner_filter(actor: &Actor) -> Option<i64> {
    (!actor.is_admin()).then_some(actor.id)
}

fn check_label(label: &str) -> Result<&str, ApiError> {
    let label = label.trim();
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(bad_request(format!("name must be at most {MAX_LABEL_CHARS} characters")));
    }
    Ok(label)
}

/// Key 列表（不含明文）。所属账号改过密码、已作废的 Key 顺手删掉，不列出来。
async fn list_keys(
    state: &AppState,
    actor: &Actor,
    kind: KeyKind,
) -> Result<Json<Vec<store::ProvisionKey>>, ApiError> {
    let mut keys =
        state.store.list_provision_keys(kind, owner_filter(actor)).await.map_err(internal)?;
    let mut revoked = Vec::new();
    keys.retain(|k| {
        let current = k
            .owner
            .as_ref()
            .is_some_and(|(role, hash)| auth::provision_key_current(state, *role, hash, &k.pw_tag));
        if !current {
            revoked.push(k.id);
        }
        current
    });
    for id in revoked {
        if let Err(e) = state.store.delete_provision_key(None, id, None).await {
            tracing::warn!(error = %e, key_id = id, "failed to delete a revoked key");
        }
    }
    Ok(Json(keys))
}

/// 给自己新建一把 Key，回它的明文——只回这一次，库里不存明文。
async fn create_key(
    state: &AppState,
    actor: &Actor,
    kind: KeyKind,
    req: CreateProvisionKeyReq,
) -> Result<Json<ProvisionKeySecret>, ApiError> {
    let label = check_label(&req.label)?;
    let hash =
        state.store.user_password_hash(actor.id).await.map_err(internal)?.unwrap_or_default();
    let pw_tag = auth::password_tag(state, actor.role, &hash);
    if pw_tag.is_empty() {
        return Err(bad_request("set a password for this account before creating a key"));
    }
    let key = store::generate_key(kind);
    let id = state
        .store
        .create_provision_key(kind, actor.id, label, &key, &pw_tag)
        .await
        .map_err(internal)?;
    tracing::info!(key_id = id, kind = kind.as_str(), label, owner = %actor.username, "console key created");
    Ok(Json(ProvisionKeySecret { id, key }))
}

/// 改名称与停用状态。
async fn update_key(
    state: &AppState,
    actor: &Actor,
    kind: KeyKind,
    id: i64,
    req: UpdateProvisionKeyReq,
) -> Result<Json<serde_json::Value>, ApiError> {
    let label = check_label(&req.label)?;
    if !state
        .store
        .update_provision_key(kind, id, owner_filter(actor), label, req.disabled)
        .await
        .map_err(internal)?
    {
        return Err((StatusCode::NOT_FOUND, "key not found".into()));
    }
    tracing::info!(
        key_id = id,
        kind = kind.as_str(),
        disabled = req.disabled,
        "console key updated"
    );
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 删一把 Key：持有它的脚本或工具随即 401。
async fn delete_key(
    state: &AppState,
    actor: &Actor,
    kind: KeyKind,
    id: i64,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state
        .store
        .delete_provision_key(Some(kind), id, owner_filter(actor))
        .await
        .map_err(internal)?
    {
        return Err((StatusCode::NOT_FOUND, "key not found".into()));
    }
    tracing::info!(key_id = id, kind = kind.as_str(), "console key deleted");
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(super) async fn list_provision_keys(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> Result<Json<Vec<store::ProvisionKey>>, ApiError> {
    list_keys(&state, &actor, KeyKind::Provision).await
}

pub(super) async fn create_provision_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<CreateProvisionKeyReq>,
) -> Result<Json<ProvisionKeySecret>, ApiError> {
    create_key(&state, &actor, KeyKind::Provision, req).await
}

pub(super) async fn update_provision_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateProvisionKeyReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    update_key(&state, &actor, KeyKind::Provision, id, req).await
}

pub(super) async fn delete_provision_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    delete_key(&state, &actor, KeyKind::Provision, id).await
}

pub(super) async fn list_mcp_keys(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> Result<Json<Vec<store::ProvisionKey>>, ApiError> {
    list_keys(&state, &actor, KeyKind::Mcp).await
}

pub(super) async fn create_mcp_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<CreateProvisionKeyReq>,
) -> Result<Json<ProvisionKeySecret>, ApiError> {
    create_key(&state, &actor, KeyKind::Mcp, req).await
}

pub(super) async fn update_mcp_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateProvisionKeyReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    update_key(&state, &actor, KeyKind::Mcp, id, req).await
}

pub(super) async fn delete_mcp_key(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    delete_key(&state, &actor, KeyKind::Mcp, id).await
}
