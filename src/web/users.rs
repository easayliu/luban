//! 控制台账号管理：admin 管全部代理和用户，代理只管挂在自己名下的用户。
//!
//! 中间件只放 admin 与代理进来（见 `auth::access_of` 的 `Manager`），这里再按「这个账号是不是
//! 你能管的」收窄：管不着的一律回 404，不回 403，免得试出别人名下有谁。

use super::*;

#[derive(Deserialize)]
pub(super) struct CreateUserReq {
    username: String,
    password: String,
    /// `agent` 或 `user`，缺省 `user`。代理开的只能是 `user`。
    #[serde(default)]
    role: Option<UserRole>,
    /// 用户挂在谁名下：admin 或某个代理，缺省 admin 自己。只有 admin 开用户时能指定。
    #[serde(default)]
    parent_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct SetUserPasswordReq {
    password: String,
}

#[derive(Deserialize)]
pub(super) struct SetUserDisabledReq {
    disabled: bool,
}

#[derive(Deserialize)]
pub(super) struct SetUserParentReq {
    parent_id: i64,
}

/// 用户名：2～32 个字符，只允许字母、数字和 `_ - . @`。
fn check_username(raw: &str) -> Result<&str, ApiError> {
    let name = raw.trim();
    let len = name.chars().count();
    if !(2..=32).contains(&len) {
        return Err(bad_request("the username must be 2 to 32 characters"));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '@')) {
        return Err(bad_request("the username may only contain letters, digits and _ - . @"));
    }
    // 访客的用户名固定是 viewer，还没设访客密码时这一行可能不存在，先占住。
    if name.eq_ignore_ascii_case("viewer") {
        return Err((StatusCode::CONFLICT, "the username is already taken".into()));
    }
    Ok(name)
}

fn user_not_found() -> ApiError {
    (StatusCode::NOT_FOUND, "user not found".into())
}

/// 取一个 `actor` 管得着的账号：admin 管全部代理和用户，代理只管自己名下的用户。
async fn manageable(state: &AppState, actor: &Actor, id: i64) -> Result<store::User, ApiError> {
    let user = state.store.user_by_id(id).await.map_err(internal)?.ok_or_else(user_not_found)?;
    let ok = match actor.role {
        UserRole::Admin => matches!(user.role, UserRole::Agent | UserRole::User),
        UserRole::Agent => user.role == UserRole::User && user.parent_id == Some(actor.id),
        UserRole::Viewer | UserRole::User => false,
    };
    if ok { Ok(user) } else { Err(user_not_found()) }
}

/// 写库时核对管理关系用的 `manager`（见 `store::MANAGED_BY`）：admin 为 None，代理为自己的 id。
///
/// [`manageable`] 只是先查一遍好回 404、记日志；真正挡越权的是写语句里的这道条件——两者之间
/// 可能等了一次密码哈希，期间用户被转走的话，前一道已经过时。
fn manager_of(actor: &Actor) -> Option<i64> {
    (!actor.is_admin()).then_some(actor.id)
}

/// 存储层报的「上级不成立」（不存在或角色不对，比如等哈希期间上级被删了）回 400，其余 500。
fn parent_error(e: anyhow::Error) -> ApiError {
    if e.downcast_ref::<store::InvalidParent>().is_some() {
        bad_request("the parent account no longer exists or cannot hold this kind of account")
    } else {
        internal(e)
    }
}

/// 列出账号：admin 看全部代理和用户，代理只看自己名下的用户，都带名下号数（代理只读看得到
/// 下属的号，见 [`Actor::team_lead`]）。
pub(super) async fn list_users(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> Result<Json<Vec<store::UserListItem>>, ApiError> {
    let list = match actor.role {
        UserRole::Admin => state.store.list_users(None),
        _ => state.store.list_users(Some(actor.id)),
    }
    .await
    .map_err(internal)?;
    Ok(Json(list))
}

/// 开账号。admin 可开代理或用户（用户可指定挂在哪个代理名下）；代理只能开挂在自己名下的用户。
pub(super) async fn create_user(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<CreateUserReq>,
) -> Result<Json<store::User>, ApiError> {
    let username = check_username(&req.username)?;
    let password = auth::check_new_password(&req.password)?;
    let (role, parent_id) = match actor.role {
        UserRole::Admin => match req.role.unwrap_or(UserRole::User) {
            UserRole::Agent => (UserRole::Agent, actor.id),
            UserRole::User => {
                let parent = req.parent_id.unwrap_or(actor.id);
                if parent != actor.id {
                    let p = state.store.user_by_id(parent).await.map_err(internal)?;
                    if !p.is_some_and(|p| p.role == UserRole::Agent) {
                        return Err(bad_request("a user can only belong to the admin or an agent"));
                    }
                }
                (UserRole::User, parent)
            }
            UserRole::Admin | UserRole::Viewer => {
                return Err(bad_request("only agents and users can be created"));
            }
        },
        _ => {
            if req.role.is_some_and(|r| r != UserRole::User) {
                return Err((StatusCode::FORBIDDEN, "agents can only create users".into()));
            }
            (UserRole::User, actor.id)
        }
    };
    let hash = auth::hash_password(password).await?;
    let user = state
        .store
        .create_user(username, &hash, role, parent_id)
        .await
        .map_err(parent_error)?
        .ok_or_else(|| (StatusCode::CONFLICT, "the username is already taken".to_string()))?;
    tracing::info!(
        by = %actor.username, user_id = user.id, username = %user.username,
        role = ?user.role, parent_id, "console account created"
    );
    Ok(Json(user))
}

/// 重置一个账号的密码，并让它的全部会话下线。
pub(super) async fn set_user_password(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<SetUserPasswordReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = manageable(&state, &actor, id).await?;
    let hash = auth::hash_password(auth::check_new_password(&req.password)?).await?;
    // 写哈希、作废会话与核对管理关系在同一个事务里：算哈希这几十毫秒里用户被转走了的话，
    // 这里写不进去，回 404。
    if !state
        .store
        .reset_managed_user_password(id, &hash, manager_of(&actor))
        .await
        .map_err(internal)?
    {
        return Err(user_not_found());
    }
    tracing::info!(by = %actor.username, user_id = id, username = %user.username, "console password reset");
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 停用 / 启用一个账号。停用的人登录不了、名下的号不接流量；停代理连带它名下的用户。
pub(super) async fn set_user_disabled(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<SetUserDisabledReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = manageable(&state, &actor, id).await?;
    if !state
        .store
        .set_user_disabled(id, req.disabled, manager_of(&actor))
        .await
        .map_err(internal)?
    {
        return Err(user_not_found());
    }
    tracing::info!(
        by = %actor.username, user_id = id, username = %user.username,
        disabled = req.disabled, "console account disabled state changed"
    );
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 把用户转到另一个代理（或 admin）名下，仅 admin。
pub(super) async fn set_user_parent(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<SetUserParentReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !actor.is_admin() {
        return Err((StatusCode::FORBIDDEN, "only the admin can move users".into()));
    }
    let user = manageable(&state, &actor, id).await?;
    if user.role != UserRole::User {
        return Err(bad_request("only users can be moved"));
    }
    if req.parent_id != actor.id {
        let p = state.store.user_by_id(req.parent_id).await.map_err(internal)?;
        if !p.is_some_and(|p| p.role == UserRole::Agent) {
            return Err(bad_request("a user can only belong to the admin or an agent"));
        }
    }
    state.store.set_user_parent(id, req.parent_id).await.map_err(parent_error)?;
    tracing::info!(
        by = %actor.username, user_id = id, username = %user.username,
        parent_id = req.parent_id, "console user moved"
    );
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 删除账号：代理名下还有用户、或名下还有号时拒绝（先转走 / 删掉）。
pub(super) async fn delete_user(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = manageable(&state, &actor, id).await?;
    match state.store.delete_user(id, manager_of(&actor)).await.map_err(internal)? {
        Ok(()) => {}
        Err(store::DeleteUserError::NotFound | store::DeleteUserError::Protected) => {
            return Err(user_not_found());
        }
        Err(store::DeleteUserError::HasChildren(n)) => {
            return Err((
                StatusCode::CONFLICT,
                format!("this agent still has {n} users; move or delete them first"),
            ));
        }
        Err(store::DeleteUserError::HasCredentials(n)) => {
            return Err((
                StatusCode::CONFLICT,
                format!("this account still owns {n} credentials; delete them first"),
            ));
        }
    }
    tracing::info!(by = %actor.username, user_id = id, username = %user.username, "console account deleted");
    Ok(Json(serde_json::json!({ "ok": true })))
}
