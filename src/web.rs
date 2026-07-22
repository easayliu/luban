//! 网页服务：授权登录 + 多凭证管理的 JSON 接口，其余路径由内嵌前端 SPA 兜底。

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};

use crate::admin_ui;
use crate::credentials::Credential;
use crate::oauth::{self, PkceChallenge};
use crate::store::CredentialStore;

/// 服务共享状态。
#[derive(Clone)]
struct AppState {
    http: reqwest::Client,
    /// 当前登录尝试的 PKCE 上下文。
    pkce: Arc<Mutex<Option<PkceChallenge>>>,
    /// 凭证存储。
    store: Arc<CredentialStore>,
}

type ApiError = (StatusCode, String);

/// 启动网页服务，绑定 `host:port`，可选自动打开浏览器。
pub async fn run(host: &str, port: u16, open_browser: bool, store: Arc<CredentialStore>) -> Result<()> {
    let state = AppState {
        http: reqwest::Client::new(),
        pkce: Arc::new(Mutex::new(None)),
        store,
    };

    let api = Router::new()
        .route("/authorize", get(authorize))
        .route("/exchange", post(exchange))
        .route("/credentials", get(list_credentials))
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_disabled))
        .route("/credentials/{id}/priority", post(set_priority))
        .route("/credentials/{id}/label", post(set_label))
        .route("/credentials/{id}/refresh", post(refresh_credential))
        .with_state(state);

    // `/api/*` 走 JSON 接口，其余路径由内嵌前端 SPA 兜底。
    let app = Router::new().nest("/api", api).fallback(admin_ui::fallback);

    let bind = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("绑定 {} 失败（端口可能被占用）", bind))?;

    let shown = if host == "0.0.0.0" || host == "::" { "127.0.0.1" } else { host };
    let url = format!("http://{shown}:{port}/");
    println!("luban 已启动：{}", url);
    if open_browser {
        open_in_browser(&url);
        println!("已尝试自动打开浏览器；若未弹出，请手动访问上面的地址。");
    }
    println!("按 Ctrl+C 结束。");

    axum::serve(listener, app).await.context("web 服务异常退出")?;
    Ok(())
}

// ---------- 授权 ----------

#[derive(Serialize)]
struct AuthorizeResp {
    url: String,
}

/// 生成新的 PKCE 挑战并返回授权 URL；PKCE 暂存于服务状态供后续交换使用。
async fn authorize(State(state): State<AppState>) -> Json<AuthorizeResp> {
    let pkce = PkceChallenge::generate();
    let url = pkce.authorize_url();
    *state.pkce.lock().unwrap() = Some(pkce);
    Json(AuthorizeResp { url })
}

#[derive(Deserialize)]
struct ExchangeReq {
    /// 用户从授权回调页粘贴的 `code#state`。
    code: String,
    /// 可选的显示名；留空则自动命名。
    #[serde(default)]
    label: Option<String>,
}

/// 用粘贴的 `code#state` 交换 token，并新增一条凭证。
async fn exchange(
    State(state): State<AppState>,
    Json(req): Json<ExchangeReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let pkce = state
        .pkce
        .lock()
        .unwrap()
        .clone()
        .ok_or(bad_request("尚未生成授权链接，请先点「添加账号」"))?;

    let tokens = oauth::exchange_code(&state.http, &pkce, &req.code)
        .await
        .map_err(|e| bad_request(e.to_string()))?;

    // 拉取账号 profile 拿邮箱/姓名/等级（失败不阻断，用兜底）。
    let profile = oauth::fetch_profile(&state.http, &tokens.access_token)
        .await
        .unwrap_or_default();

    // 显示名优先级：用户填写 > profile 邮箱 > profile 姓名 > 交换响应邮箱 > 「账号 N」。
    let label = match req.label.map(|s| s.trim().to_string()) {
        Some(s) if !s.is_empty() => s,
        _ => profile
            .email
            .clone()
            .or_else(|| profile.name.clone())
            .or_else(|| tokens.account.clone())
            .unwrap_or_else(|| {
                let n = state.store.list().map(|v| v.len()).unwrap_or(0) + 1;
                format!("账号 {}", n)
            }),
    };

    let cred = state
        .store
        .insert(
            &label,
            profile.tier.as_deref(),
            &tokens.access_token,
            &tokens.refresh_token,
            tokens.expires_at,
        )
        .map_err(internal)?;

    // 成功后清空 PKCE，避免重复使用。
    *state.pkce.lock().unwrap() = None;

    Ok(Json(CredentialView::from(&cred)))
}

// ---------- 凭证管理 ----------

/// 列出全部凭证（token 已脱敏）。
async fn list_credentials(State(state): State<AppState>) -> Result<Json<Vec<CredentialView>>, ApiError> {
    let list = state.store.list().map_err(internal)?;
    Ok(Json(list.iter().map(CredentialView::from).collect()))
}

/// 删除一条凭证。
async fn delete_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let removed = state.store.delete(id).map_err(internal)?;
    if !removed {
        return Err(not_found());
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SetDisabledReq {
    disabled: bool,
}

/// 启用/停用一条凭证。
async fn set_disabled(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetDisabledReq>,
) -> Result<Json<CredentialView>, ApiError> {
    if !state.store.set_disabled(id, req.disabled).map_err(internal)? {
        return Err(not_found());
    }
    view_of(&state, id)
}

#[derive(Deserialize)]
struct SetPriorityReq {
    priority: i64,
}

/// 设置优先级。
async fn set_priority(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetPriorityReq>,
) -> Result<Json<CredentialView>, ApiError> {
    if !state.store.set_priority(id, req.priority).map_err(internal)? {
        return Err(not_found());
    }
    view_of(&state, id)
}

#[derive(Deserialize)]
struct SetLabelReq {
    label: String,
}

/// 重命名。
async fn set_label(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetLabelReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let label = req.label.trim();
    if label.is_empty() {
        return Err(bad_request("名称不能为空"));
    }
    if !state.store.set_label(id, label).map_err(internal)? {
        return Err(not_found());
    }
    view_of(&state, id)
}

/// 手动刷新一条凭证的 token。
async fn refresh_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    let tokens = oauth::refresh(&state.http, &cred.refresh_token)
        .await
        .map_err(|e| bad_request(e.to_string()))?;
    state
        .store
        .update_tokens(id, &tokens.access_token, &tokens.refresh_token, tokens.expires_at)
        .map_err(internal)?;
    // 顺带刷新账号等级（失败忽略，不影响 token 刷新结果）。
    if let Ok(profile) = oauth::fetch_profile(&state.http, &tokens.access_token).await {
        if profile.tier.is_some() {
            let _ = state.store.set_tier(id, profile.tier.as_deref());
        }
    }
    view_of(&state, id)
}

/// 读取单条并转为脱敏视图。
fn view_of(state: &AppState, id: i64) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    Ok(Json(CredentialView::from(&cred)))
}

// ---------- 视图与错误 ----------

/// 对外暴露的凭证视图（不返回明文 token）。
#[derive(Serialize)]
struct CredentialView {
    id: i64,
    label: String,
    tier: Option<String>,
    priority: i64,
    disabled: bool,
    expires_in: u64,
    expired: bool,
    created_at: u64,
    updated_at: u64,
    /// 脱敏后的 refresh_token（前缀 + 尾 4 位），仅用于界面区分。
    token_hint: String,
}

impl From<&Credential> for CredentialView {
    fn from(c: &Credential) -> Self {
        let secs = c.expires_in_secs();
        Self {
            id: c.id,
            label: c.label.clone(),
            tier: c.tier.clone(),
            priority: c.priority,
            disabled: c.disabled,
            expires_in: secs,
            expired: secs == 0,
            created_at: c.created_at,
            updated_at: c.updated_at,
            token_hint: mask_token(&c.refresh_token),
        }
    }
}

/// 脱敏：保留前缀（到第三个 `-`）与尾 4 位，中间用 `…` 省略。
fn mask_token(token: &str) -> String {
    let tail: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let prefix: String = token.splitn(4, '-').take(3).collect::<Vec<_>>().join("-");
    if prefix.is_empty() {
        format!("…{}", tail)
    } else {
        format!("{}-…{}", prefix, tail)
    }
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}
fn not_found() -> ApiError {
    (StatusCode::NOT_FOUND, "凭证不存在".into())
}
fn internal(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// 尽力打开系统默认浏览器；失败静默忽略（页面地址已打印）。
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", url);
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = ("xdg-open", url);

    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new(cmd.0).arg(cmd.1).spawn();
    }
}
