//! 网页服务：授权登录 + 多凭证管理的 JSON 接口，其余路径由内嵌前端 SPA 兜底。

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    routing::{any, delete, get, post},
};
use serde::{Deserialize, Serialize};

use crate::admin_ui;
use crate::auth;
use crate::config;
use crate::credentials::Credential;
use crate::oauth::{self, PkceChallenge};
use crate::proxy;
use crate::store::{self, CredentialStore};

/// 服务共享状态。
#[derive(Clone)]
pub struct AppState {
    pub http: wreq::Client,
    /// 当前登录尝试的 PKCE 上下文。
    pkce: Arc<Mutex<Option<PkceChallenge>>>,
    /// 凭证存储。
    pub store: Arc<CredentialStore>,
    /// 接入用的 API Key（None 表示不校验来访身份）。
    pub client_key: Option<Arc<String>>,
    /// 管理密码（环境接管，明文；None 表示未由环境设置）。
    pub admin_env: Option<Arc<String>>,
}

type ApiError = (StatusCode, String);

/// 构造发往上游的 HTTP 客户端，刻意贴近官方客户端的传输形态：
/// - `http1_only`：官方客户端（Bun 自带的 HTTP 客户端）走 HTTP/1.1（抓包里有
///   `Connection`/`Host`，h2 不会有这两个头）。默认会经 ALPN 协商 h2，留下 h2 的
///   SETTINGS/伪头指纹；h2 还强制头名小写，逐头大小写也就无从谈起。
/// - `user_agent`：给 luban 自身发起的账号级请求（token 刷新、profile）兜底；转发 `/v1/*`
///   时来访客户端自己的 UA 会覆盖它。
/// - `default_headers` 里的 `accept-encoding`：**必须显式钉住**。开了解压 feature 后，
///   tower-http 的解压中间件会给「没带这个头」的请求补一个它自己的取值
///   `zstd,gzip,deflate,br`（顺序与写法都不是官方客户端会产生的；换到 wreq 后这个行为照旧，
///   预检里复现过）。转发 `/v1/*` 时 [`proxy::build_forward_headers`] 通常已保证该头存在，
///   但 `fill_client_headers` 开关关掉、且来访客户端自己也没带时就轮到这里兜底；luban 自身
///   发起的刷新/profile 请求同理。钉成官方值即可堵死：wreq 与 tower-http 都只填「缺失」的头。
///
/// 抽成函数是为了让 [`crate::proxy`] 的线上字节回归测试用到的是**这一份真配置**，
/// 而不是测试里另抄一份。无法对齐的部分见 [`config::known_fingerprint_gaps`]。
pub fn upstream_client() -> Result<wreq::Client> {
    use axum::http::{HeaderMap, HeaderValue, header::ACCEPT_ENCODING};

    let mut defaults = HeaderMap::new();
    defaults.insert(ACCEPT_ENCODING, HeaderValue::from_static(config::CC_ACCEPT_ENCODING));

    wreq::Client::builder()
        .http1_only()
        .user_agent(config::CC_USER_AGENT)
        .default_headers(defaults)
        .build()
        .context("构造上游 HTTP 客户端失败")
}

/// 启动网页服务 + 转发代理，绑定 `host:port`，可选自动打开浏览器。
pub async fn run(
    host: &str,
    port: u16,
    open_browser: bool,
    store: Arc<CredentialStore>,
    api_key: Option<String>,
    admin_password: Option<String>,
) -> Result<()> {
    let client_key = api_key.map(Arc::new);
    let http = upstream_client()?;
    let state = AppState {
        http,
        pkce: Arc::new(Mutex::new(None)),
        store,
        client_key: client_key.clone(),
        admin_env: admin_password.map(Arc::new),
    };

    // 公开鉴权接口（无需登录）。
    let public = Router::new()
        .route("/auth/state", get(auth::state))
        .route("/auth/login", post(auth::login))
        .route("/auth/setup", post(auth::setup));

    // 需管理鉴权的接口（未设密码时中间件放行）。
    let protected = Router::new()
        .route("/authorize", get(authorize))
        .route("/exchange", post(exchange))
        .route("/credentials", get(list_credentials))
        .route("/credentials/priority", post(set_priorities))
        .route("/credentials/device-limit", post(set_device_limits))
        .route("/credentials/disabled", post(set_disabled_many))
        .route("/credentials/delete", post(delete_credentials))
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_disabled))
        .route("/credentials/{id}/priority", post(set_priority))
        .route("/credentials/{id}/label", post(set_label))
        .route("/credentials/{id}/device-limit", post(set_device_limit))
        .route("/credentials/{id}/devices", get(list_credential_devices))
        .route("/credentials/{id}/devices/{device_id}", delete(unbind_credential_device))
        .route("/credentials/{id}/refresh", post(refresh_credential))
        .route("/usage", get(list_usage))
        .route("/settings", get(get_settings))
        .route("/settings/api-key", post(set_api_key))
        .route("/settings/device-ttl", post(set_device_ttl))
        .route("/settings/default-device-limit", post(set_default_device_limit))
        .route("/settings/require-device-id", post(set_require_device_id))
        .route("/settings/forwarding", post(set_forwarding))
        .route("/auth/password", post(auth::change_password))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_admin));

    let api = public.merge(protected);

    // `/api/*` 管理接口；`/v1/*` 转发到官方 API；其余由内嵌前端 SPA 兜底。
    let app = Router::new()
        .nest("/api", api)
        .route("/v1/{*path}", any(proxy::handle))
        .fallback(admin_ui::fallback)
        .with_state(state);

    let bind = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("绑定 {} 失败（端口可能被占用）", bind))?;

    let shown = if host == "0.0.0.0" || host == "::" { "127.0.0.1" } else { host };
    let url = format!("http://{shown}:{port}/");
    let base = url.trim_end_matches('/');

    tracing::info!(addr = %bind, url = %url, "luban 已启动");
    match &client_key {
        Some(_) => tracing::info!(
            "Claude Code 接入：ANTHROPIC_BASE_URL={base}，ANTHROPIC_AUTH_TOKEN=<--api-key>"
        ),
        None => tracing::info!(
            "Claude Code 接入：ANTHROPIC_BASE_URL={base}（未设 --api-key，代理不校验来访，请仅本机使用）"
        ),
    }
    if open_browser {
        open_in_browser(&url);
        tracing::info!("已尝试打开浏览器；若未弹出请手动访问 {url}");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("web 服务异常退出")?;
    Ok(())
}

/// 等待关闭信号：Ctrl-C 或（Unix 下）SIGTERM，收到后让 axum 排空在途请求再退出。
///
/// 容器内 luban 常以 PID 1 运行，内核对 PID 1 不套用信号默认动作——若不显式
/// 处理 SIGTERM，`docker stop`/`restart` 会因信号被忽略而空等 10 秒宽限期才 SIGKILL
/// 强杀，表现为「重启很久」。这里注册处理器即可让重启秒停，且不切断流式响应。
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("安装 Ctrl-C 处理器失败");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("安装 SIGTERM 处理器失败")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("收到关闭信号，正在优雅关闭 ...");
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
    let profile = oauth::fetch_profile(&state.http, &tokens.access_token).await.unwrap_or_default();

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
            profile.account_uuid.as_deref(),
        )
        .map_err(internal)?;

    // 成功后清空 PKCE，避免重复使用。
    *state.pkce.lock().unwrap() = None;

    tracing::info!(id = cred.id, label = %cred.label, tier = ?cred.tier, "新增凭证");
    Ok(Json(CredentialView::new(&cred, 0, state.store.default_device_limit())))
}

// ---------- 用量日志 ----------

#[derive(Deserialize)]
struct UsageQuery {
    /// 返回条数上限（默认 100，最多 1000）。
    #[serde(default)]
    limit: Option<i64>,
}

/// 列出最近的用量日志（按时间倒序）。
async fn list_usage(
    State(state): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<Vec<store::UsageLog>>, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let logs = state.store.list_usage_logs(limit).map_err(internal)?;
    Ok(Json(logs))
}

// ---------- 凭证管理 ----------

/// 列出全部凭证（token 已脱敏）。
async fn list_credentials(
    State(state): State<AppState>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    let list = state.store.list().map_err(internal)?;
    let counts = state.store.device_counts().map_err(internal)?;
    let quotas = state.store.latest_quotas().map_err(internal)?;
    let last_used = state.store.last_used().map_err(internal)?;
    let costs = state.store.cost_by_cred().map_err(internal)?;
    let default_limit = state.store.default_device_limit();
    let views = list
        .iter()
        .map(|c| {
            CredentialView::new(c, counts.get(&c.id).copied().unwrap_or(0), default_limit)
                .with_stats(
                    quotas.get(&c.id).cloned(),
                    last_used.get(&c.id).copied(),
                    costs.get(&c.id).copied().unwrap_or(0.0),
                )
        })
        .collect();
    Ok(Json(views))
}

/// 列出某凭证当前绑定的设备明细（按最近活跃倒序）。
///
/// 口径与卡片上的「设备 x/y」一致：只含 TTL 内仍活跃的绑定，所以条数必然等于 x。
async fn list_credential_devices(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<store::DeviceBinding>>, ApiError> {
    // 凭证不存在时给 404：否则前端会把「账号已被删掉」显示成「该账号没有设备」。
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    Ok(Json(state.store.list_devices(id).map_err(internal)?))
}

/// 手动解除某设备与该凭证的绑定，立即腾出一个设备名额。
///
/// 「解绑」不等于「拉黑」：该设备的下一次请求会重新走选号，名额没满时完全可能又落回同一个
/// 账号。要把设备挡在外面得靠设备上限，不是这个接口。
///
/// 绑定不存在同样给 404（而非静默 ok）：明细是前端缓存的，设备可能已被 TTL 回收或已换到别的
/// 账号，静默成功会让人以为解绑生效、实际点了个空。
async fn unbind_credential_device(
    State(state): State<AppState>,
    Path((id, device_id)): Path<(i64, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    if !state.store.unbind_device(id, &device_id).map_err(internal)? {
        return Err((StatusCode::NOT_FOUND, "设备绑定不存在（可能已过期或已换到其它账号）".into()));
    }
    tracing::info!(cred_id = id, device_id = %device_id, "手动解除设备绑定");
    Ok(Json(serde_json::json!({ "ok": true })))
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
    tracing::info!(id, "删除凭证");
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
struct SetPrioritiesReq {
    /// 待调整的账号 id 列表。
    ids: Vec<i64>,
    /// 统一设置的优先级（数值小者优先）。
    priority: i64,
}

/// 批量设置优先级：把选中的账号统一调到同一档，返回更新后的整份列表。
async fn set_priorities(
    State(state): State<AppState>,
    Json(req): Json<SetPrioritiesReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let n = state.store.set_priorities(&req.ids, req.priority).map_err(internal)?;
    tracing::info!(count = n, priority = req.priority, "批量设置优先级");
    list_credentials(State(state)).await
}

/// 批量操作的公共入参：待处理的账号 id 列表。
#[derive(Deserialize)]
struct IdsReq {
    ids: Vec<i64>,
}

/// 校验批量入参并返回 id 列表；空列表视为客户端错误而非静默 no-op。
fn check_ids(ids: &[i64]) -> Result<(), ApiError> {
    if ids.is_empty() {
        return Err(bad_request("请至少选择一个账号"));
    }
    Ok(())
}

#[derive(Deserialize)]
struct SetDeviceLimitsReq {
    ids: Vec<i64>,
    /// 三态同单账号接口：`> 0` 独立上限；`0` 跟随全局默认；`< 0` 明确不限。
    device_limit: i64,
}

/// 批量设置设备数上限，返回更新后的整份列表。
async fn set_device_limits(
    State(state): State<AppState>,
    Json(req): Json<SetDeviceLimitsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    // 负值统一收敛为 -1，与单账号接口保持一致。
    let limit = if req.device_limit < 0 { -1 } else { req.device_limit };
    let n = state.store.set_device_limits(&req.ids, limit).map_err(internal)?;
    tracing::info!(count = n, device_limit = limit, "批量设置设备上限");
    list_credentials(State(state)).await
}

#[derive(Deserialize)]
struct SetDisabledManyReq {
    ids: Vec<i64>,
    disabled: bool,
}

/// 批量启用/停用，返回更新后的整份列表。
async fn set_disabled_many(
    State(state): State<AppState>,
    Json(req): Json<SetDisabledManyReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let n = state.store.set_disabled_many(&req.ids, req.disabled).map_err(internal)?;
    tracing::info!(count = n, disabled = req.disabled, "批量启停");
    list_credentials(State(state)).await
}

/// 批量删除（连带清历史用量与设备绑定），返回删除后的整份列表。
///
/// 用 POST 而非 DELETE：带请求体的 DELETE 在部分代理/客户端上会被丢掉 body。
async fn delete_credentials(
    State(state): State<AppState>,
    Json(req): Json<IdsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let n = state.store.delete_many(&req.ids).map_err(internal)?;
    tracing::info!(count = n, "批量删除凭证");
    list_credentials(State(state)).await
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

#[derive(Deserialize)]
struct SetDeviceLimitReq {
    /// 设备数上限三态：`> 0` 本账号独立上限；`0` 跟随全局默认；`< 0` 本账号明确不限。
    device_limit: i64,
}

/// 设置设备数上限。
async fn set_device_limit(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetDeviceLimitReq>,
) -> Result<Json<CredentialView>, ApiError> {
    // 负值统一收敛为 -1，避免库里出现各式各样的“不限”取值。
    let limit = if req.device_limit < 0 { -1 } else { req.device_limit };
    if !state.store.set_device_limit(id, limit).map_err(internal)? {
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
    // 顺带刷新账号等级、回填账号 UUID（失败忽略，不影响 token 刷新结果）。
    if let Ok(profile) = oauth::fetch_profile(&state.http, &tokens.access_token).await {
        if profile.tier.is_some() {
            let _ = state.store.set_tier(id, profile.tier.as_deref());
        }
        if let Some(uuid) = profile.account_uuid.as_deref() {
            let _ = state.store.set_account_uuid(id, uuid);
        }
    }
    view_of(&state, id)
}

/// 读取单条并转为脱敏视图（含已绑定设备数）。
fn view_of(state: &AppState, id: i64) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    let count = state.store.device_count(id).map_err(internal)?;
    // 单账号视图只查这一个 id：此前调的是三个「全库聚合」再 remove 一条，改一次开关就要把
    // usage_logs 整表聚合三遍。
    let quota = state.store.latest_quota(id).map_err(internal)?;
    let last_used = state.store.last_used_at(id).map_err(internal)?;
    let cost_total = state.store.cost_of(id).map_err(internal)?;
    let default_limit = state.store.default_device_limit();
    Ok(Json(
        CredentialView::new(&cred, count, default_limit).with_stats(quota, last_used, cost_total),
    ))
}

// ---------- 接入设置 ----------

#[derive(Serialize)]
struct SettingsResp {
    /// 当前接入 key（可能为空 = 不校验）。
    api_key: Option<String>,
    /// 是否由环境变量/启动参数接管（true 时网页只读）。
    env_managed: bool,
    /// 设备绑定有效期（秒）；0 表示永不过期。
    device_binding_ttl_secs: i64,
    /// 全局默认设备数上限；0 表示默认不限。账号未单独配置时套用它。
    default_device_limit: i64,
    /// 是否要求请求携带有效设备身份（`metadata.user_id`）；关闭后放行裸客户端。
    require_device_id: bool,
    /// 转发形态开关（默认全开）。
    #[serde(flatten)]
    forwarding: ForwardingResp,
}

/// 转发形态开关的对外形态；字段名与 [`crate::store::ForwardFlags`] 一一对应。
#[derive(Serialize)]
struct ForwardingResp {
    /// 改写 `metadata.user_id` 的 account_uuid/device_id。
    spoof_identity: bool,
    /// 给 `x-anthropic-billing-header` 补 `cch`。
    billing_cch: bool,
    /// 补齐客户端未携带的 `accept-encoding`/`anthropic-version`/`x-client-request-id`。
    fill_client_headers: bool,
    /// 合并并按官方顺序重排 `anthropic-beta`（含塞入 oauth beta）。
    merge_beta: bool,
    /// 把 `system` 对齐成官方订阅客户端的 4 块形态（拆块 + 全 1h + 基座 global）。
    system_shape: bool,
    /// 按官方拼写与顺序发出头名。
    orig_header_case: bool,
    /// 上游拒绝 thinking 块签名时，降级历史 thinking 后重试一次。
    thinking_signature_retry: bool,
}

impl From<crate::store::ForwardFlags> for ForwardingResp {
    fn from(f: crate::store::ForwardFlags) -> Self {
        Self {
            spoof_identity: f.spoof_identity,
            billing_cch: f.billing_cch,
            fill_client_headers: f.fill_client_headers,
            merge_beta: f.merge_beta,
            system_shape: f.system_shape,
            orig_header_case: f.orig_header_case,
            thinking_signature_retry: f.thinking_signature_retry,
        }
    }
}

fn settings_resp(state: &AppState) -> SettingsResp {
    let device_binding_ttl_secs = state.store.device_binding_ttl();
    let default_device_limit = state.store.default_device_limit();
    let require_device_id = state.store.require_device_id();
    let forwarding = state.store.forward_flags().into();
    if let Some(k) = &state.client_key {
        return SettingsResp {
            api_key: Some(k.to_string()),
            env_managed: true,
            device_binding_ttl_secs,
            default_device_limit,
            require_device_id,
            forwarding,
        };
    }
    let api_key = state
        .store
        .get_setting(crate::store::CLIENT_API_KEY)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    SettingsResp {
        api_key,
        env_managed: false,
        device_binding_ttl_secs,
        default_device_limit,
        require_device_id,
        forwarding,
    }
}

/// 读取接入设置。
async fn get_settings(State(state): State<AppState>) -> Json<SettingsResp> {
    Json(settings_resp(&state))
}

#[derive(Deserialize)]
struct SetApiKeyReq {
    /// 新 key；空串表示清除（关闭鉴权）。
    api_key: String,
}

/// 设置/清除接入 key（环境接管时禁止）。
async fn set_api_key(
    State(state): State<AppState>,
    Json(req): Json<SetApiKeyReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    if state.client_key.is_some() {
        return Err(bad_request("接入 Key 已由环境变量 LUBAN_API_KEY 接管，无法在网页修改"));
    }
    let key = req.api_key.trim();
    if key.is_empty() {
        state.store.delete_setting(crate::store::CLIENT_API_KEY).map_err(internal)?;
    } else {
        state.store.set_setting(crate::store::CLIENT_API_KEY, key).map_err(internal)?;
    }
    Ok(Json(settings_resp(&state)))
}

#[derive(Deserialize)]
struct SetDeviceTtlReq {
    /// 设备绑定有效期（秒）；0（或负数）表示永不过期。
    device_binding_ttl_secs: i64,
}

/// 设置设备绑定有效期（秒）。
async fn set_device_ttl(
    State(state): State<AppState>,
    Json(req): Json<SetDeviceTtlReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let ttl = req.device_binding_ttl_secs.max(0);
    state
        .store
        .set_setting(crate::store::DEVICE_BINDING_TTL, &ttl.to_string())
        .map_err(internal)?;
    Ok(Json(settings_resp(&state)))
}

#[derive(Deserialize)]
struct SetDefaultDeviceLimitReq {
    /// 全局默认设备数上限；0（或负数）表示默认不限。
    default_device_limit: i64,
}

/// 设置全局默认设备数上限（账号自身未单独配置时生效）。
async fn set_default_device_limit(
    State(state): State<AppState>,
    Json(req): Json<SetDefaultDeviceLimitReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let limit = req.default_device_limit.max(0);
    state
        .store
        .set_setting(crate::store::DEFAULT_DEVICE_LIMIT, &limit.to_string())
        .map_err(internal)?;
    Ok(Json(settings_resp(&state)))
}

#[derive(Deserialize)]
struct SetRequireDeviceIdReq {
    /// 是否要求请求携带有效设备身份。
    required: bool,
}

/// 开关设备身份校验：关闭后，无 `metadata.user_id` 的请求不再 403，而是以
/// 「不绑定、不占设备名额」的方式转发（也无法被身份伪装）。
async fn set_require_device_id(
    State(state): State<AppState>,
    Json(req): Json<SetRequireDeviceIdReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let value = if req.required { "true" } else { "false" };
    state.store.set_setting(crate::store::REQUIRE_DEVICE_ID, value).map_err(internal)?;
    tracing::info!(required = req.required, "设备身份校验开关变更");
    Ok(Json(settings_resp(&state)))
}

/// 转发形态开关的改动请求：**只有出现的字段会被写入**，其余保持原值。
/// 前端每次拨一个开关就只带那一个字段，不必回传全量、也不会互相覆盖。
#[derive(Deserialize)]
struct SetForwardingReq {
    spoof_identity: Option<bool>,
    billing_cch: Option<bool>,
    fill_client_headers: Option<bool>,
    merge_beta: Option<bool>,
    system_shape: Option<bool>,
    orig_header_case: Option<bool>,
    thinking_signature_retry: Option<bool>,
}

/// 逐项开关转发形态改动。全关即「零改写直接转发」——实测上游唯一必需的是注入
/// `Authorization`，这些开关都只影响与官方客户端的形态贴合度，见
/// [`crate::store::ForwardFlags`]。
async fn set_forwarding(
    State(state): State<AppState>,
    Json(req): Json<SetForwardingReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    use crate::store::{
        FILL_CLIENT_HEADERS, MERGE_BETA, ORIG_HEADER_CASE, SPOOF_BILLING_CCH,
        SPOOF_IDENTITY_ENABLED, SYSTEM_SHAPE, THINKING_SIGNATURE_RETRY,
    };
    let items = [
        (SPOOF_IDENTITY_ENABLED, req.spoof_identity),
        (SPOOF_BILLING_CCH, req.billing_cch),
        (FILL_CLIENT_HEADERS, req.fill_client_headers),
        (MERGE_BETA, req.merge_beta),
        (SYSTEM_SHAPE, req.system_shape),
        (ORIG_HEADER_CASE, req.orig_header_case),
        (THINKING_SIGNATURE_RETRY, req.thinking_signature_retry),
    ];
    for (key, value) in items.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))) {
        state.store.set_setting(key, if value { "true" } else { "false" }).map_err(internal)?;
        tracing::info!(key, enabled = value, "转发形态开关变更");
    }
    Ok(Json(settings_resp(&state)))
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
    /// 过期时刻（Unix 秒）。前端展示用它而非 `expires_in`：倒计时要么静止要么得自己走，
    /// 而绝对时刻渲染多少次都是同一个值，也不受浏览器时钟偏差影响。
    expires_at: u64,
    expired: bool,
    created_at: u64,
    updated_at: u64,
    /// 账号自身的设备上限设置：`> 0` 独立上限；`0` 跟随全局默认；`< 0` 明确不限。
    device_limit: i64,
    /// 实际生效的设备上限（已套用全局默认）；0 表示不限。
    device_limit_effective: i64,
    /// 当前已绑定的设备数。
    device_count: i64,
    /// 自动检测到的上游账号级错误原因（如封号）；`None` 表示未被自动停用。
    ban_reason: Option<String>,
    /// 脱敏后的 refresh_token（前缀 + 尾 4 位），仅用于界面区分。
    token_hint: String,
    /// 最新一次的订阅额度快照（无请求记录时为 None）。
    quota: Option<store::QuotaSnapshot>,
    /// 最近一次被使用（转发请求）的时间戳（Unix 秒）；从未使用为 None。
    last_used: Option<i64>,
    /// 累计等价 API 费用（USD）。
    cost_total: f64,
}

impl CredentialView {
    /// 由凭证 + 已绑定设备数 + 全局默认设备上限构造视图。
    fn new(c: &Credential, device_count: i64, default_device_limit: i64) -> Self {
        let secs = c.expires_in_secs();
        Self {
            id: c.id,
            label: c.label.clone(),
            tier: c.tier.clone(),
            priority: c.priority,
            disabled: c.disabled,
            expires_in: secs,
            expires_at: c.expires_at,
            expired: secs == 0,
            created_at: c.created_at,
            updated_at: c.updated_at,
            device_limit: c.device_limit,
            device_limit_effective: store::effective_device_limit(
                c.device_limit,
                default_device_limit,
            ),
            device_count,
            ban_reason: c.ban_reason.clone(),
            token_hint: mask_token(&c.refresh_token),
            quota: None,
            last_used: None,
            cost_total: 0.0,
        }
    }

    /// 链式附加额度快照、最近使用时间与累计费用。
    fn with_stats(
        mut self,
        quota: Option<store::QuotaSnapshot>,
        last_used: Option<i64>,
        cost_total: f64,
    ) -> Self {
        self.quota = quota;
        self.last_used = last_used;
        self.cost_total = cost_total;
        self
    }
}

/// 脱敏：保留前缀（到第三个 `-`）与尾 4 位，中间用 `…` 省略。
fn mask_token(token: &str) -> String {
    let tail: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let prefix: String = token.splitn(4, '-').take(3).collect::<Vec<_>>().join("-");
    if prefix.is_empty() { format!("…{}", tail) } else { format!("{}-…{}", prefix, tail) }
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
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new(cmd.0).arg(cmd.1).spawn();
    }
}
