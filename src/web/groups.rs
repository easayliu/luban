//! 号池分组与接入 Key 的管理接口。
//!
//! 分组：所有人都能列出自己能用的（admin / 访客看全部，带号数与开放名单）；增删改与开放名单
//! 只有 admin。接入 Key：只有 admin（中间件按路由拦，访客连 GET 也不给——查看明文的接口
//! 也在这一组里）。

use super::*;

#[derive(Deserialize)]
pub(super) struct GroupReq {
    name: String,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
pub(super) struct GrantsReq {
    user_ids: Vec<i64>,
}

#[derive(Deserialize)]
pub(super) struct CreateKeyReq {
    #[serde(default)]
    label: String,
    /// 绑定的分组，按优先顺序；空 = 用全部号。
    #[serde(default)]
    group_ids: Vec<i64>,
}

#[derive(Deserialize)]
pub(super) struct UpdateKeyReq {
    #[serde(default)]
    label: String,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    group_ids: Vec<i64>,
}

#[derive(Serialize)]
pub(super) struct KeySecret {
    id: i64,
    key: String,
}

/// 分组操作被拒的原因映射成 HTTP 状态。
pub(super) fn group_error(e: store::GroupError) -> ApiError {
    let status = match e {
        store::GroupError::NotFound => StatusCode::NOT_FOUND,
        store::GroupError::NameTaken => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, e.to_string())
}

fn admin_only(actor: &Actor) -> Result<(), ApiError> {
    if actor.is_admin() {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "this action requires the admin account".into()))
    }
}

/// 分组名：1～32 个字符。
fn check_group_name(raw: &str) -> Result<&str, ApiError> {
    let name = raw.trim();
    let len = name.chars().count();
    if !(1..=32).contains(&len) {
        return Err(bad_request("the group name must be 1 to 32 characters"));
    }
    Ok(name)
}

/// `actor` 上号、改分组时能选哪些分组：admin 随便选（`None`），其余只能选开放给自己的。
pub(super) fn selectable_groups(
    state: &AppState,
    actor: &Actor,
) -> Result<Option<std::collections::HashSet<i64>>, ApiError> {
    if actor.is_admin() {
        return Ok(None);
    }
    let user = state.store.user_by_id(actor.id).map_err(internal)?.ok_or_else(not_found)?;
    Ok(Some(state.store.visible_group_ids(&user).map_err(internal)?))
}

/// 核对一组分组 id 是不是 `actor` 都能选的；不能选的按「分组不存在」回（不透露别的分组）。
pub(super) fn check_selectable(
    state: &AppState,
    actor: &Actor,
    group_ids: &[i64],
) -> Result<(), ApiError> {
    if let Some(allowed) = selectable_groups(state, actor)?
        && !group_ids.iter().all(|g| allowed.contains(g))
    {
        return Err(group_error(store::GroupError::UnknownGroup));
    }
    Ok(())
}

/// 列出分组：admin 与访客看全部（带号数与开放名单），代理和用户只看开放给自己的。
pub(super) async fn list_groups(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> Result<Json<Vec<store::PoolGroup>>, ApiError> {
    let groups = match actor.scope() {
        Scope::All => state.store.list_groups().map_err(internal)?,
        Scope::Owner(_) => {
            let user = state.store.user_by_id(actor.id).map_err(internal)?.ok_or_else(not_found)?;
            state.store.visible_groups(&user).map_err(internal)?
        }
    };
    Ok(Json(groups))
}

/// 新建分组（仅 admin）。
pub(super) async fn create_group(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<GroupReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    admin_only(&actor)?;
    let name = check_group_name(&req.name)?;
    let id =
        state.store.create_group(name, req.note.trim()).map_err(internal)?.map_err(group_error)?;
    tracing::info!(group_id = id, name, "pool group created");
    Ok(Json(serde_json::json!({ "id": id })))
}

/// 改分组的名称与说明（仅 admin）。
pub(super) async fn update_group(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<GroupReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    admin_only(&actor)?;
    let name = check_group_name(&req.name)?;
    state.store.update_group(id, name, req.note.trim()).map_err(internal)?.map_err(group_error)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 删分组（仅 admin，默认分组不能删）：只剩这一个分组的号挪进默认分组。
pub(super) async fn delete_group(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    admin_only(&actor)?;
    state.store.delete_group(id).map_err(internal)?.map_err(group_error)?;
    tracing::info!(group_id = id, "pool group deleted");
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 整体替换分组的开放名单（仅 admin）：代理（名下用户自动继承）或 admin 直属的用户。
pub(super) async fn set_group_grants(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<GrantsReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    admin_only(&actor)?;
    state.store.set_group_grants(id, &req.user_ids).map_err(internal)?.map_err(group_error)?;
    tracing::info!(group_id = id, users = ?req.user_ids, "pool group grants updated");
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------- 接入 Key ----------

/// 全部接入 Key（不含明文）。
pub(super) async fn list_api_keys(
    State(state): State<AppState>,
) -> Result<Json<Vec<store::ApiKey>>, ApiError> {
    Ok(Json(state.store.list_api_keys().map_err(internal)?))
}

/// 新建一把接入 Key，回它的明文（之后也能用「查看」再取）。
pub(super) async fn create_api_key(
    State(state): State<AppState>,
    Json(req): Json<CreateKeyReq>,
) -> Result<Json<KeySecret>, ApiError> {
    let key = store::generate_api_key();
    let label = req.label.trim();
    let id = state
        .store
        .create_api_key(label, &key, &req.group_ids)
        .map_err(internal)?
        .map_err(group_error)?;
    tracing::info!(key_id = id, label, groups = ?req.group_ids, "API key created");
    Ok(Json(KeySecret { id, key }))
}

/// 改一把 Key 的名称、停用状态与绑定的分组。
pub(super) async fn update_api_key(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateKeyReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .store
        .update_api_key(id, req.label.trim(), req.disabled, &req.group_ids)
        .map_err(internal)?
        .map_err(group_error)?;
    tracing::info!(key_id = id, disabled = req.disabled, groups = ?req.group_ids, "API key updated");
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 删一把 Key：拿它对接的系统随即 401。
pub(super) async fn delete_api_key(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.store.delete_api_key(id).map_err(internal)? {
        return Err((StatusCode::NOT_FOUND, "API key not found".into()));
    }
    tracing::info!(key_id = id, "API key deleted");
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 一把 Key 的明文（仅 admin）。
pub(super) async fn reveal_api_key(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<KeySecret>, ApiError> {
    let key = state
        .store
        .reveal_api_key(id)
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "API key not found".to_string()))?;
    Ok(Json(KeySecret { id, key }))
}
