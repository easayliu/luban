//! 网页服务：授权登录 + 多凭证管理的 JSON 接口，其余路径由内嵌前端 SPA 兜底。

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
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

/// 一次登录尝试还没换 token 之前，PKCE 上下文最多留多久。
///
/// 用户要在浏览器里完成授权再把 `code#state` 粘回来，几分钟足够；留太久只是让过期的挑战
/// 一直占着位置。到点后那次登录会被判成「尚未生成授权链接」，重新点一次即可。
const PKCE_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// 同时最多保留几个待完成的登录尝试。纯属防御——正常同时开几个标签页也就个位数，
/// 上限只是不让反复点「添加账号」把内存撑起来。超出时丢掉最旧的那个。
const PKCE_MAX_PENDING: usize = 32;

/// 服务共享状态。
#[derive(Clone)]
pub struct AppState {
    pub http: wreq::Client,
    /// 进行中的登录尝试：`state` → (PKCE 上下文, 创建时刻)。
    ///
    /// **按 state 索引而不是只留一份**：原先是个全局单槽，两个标签页（或两个人）同时点
    /// 「添加账号」时，后一次 `authorize` 会把前一次的 verifier/state 直接覆盖掉，前一个人
    /// 粘贴回来就撞上「state 不匹配，可能存在 CSRF 或粘贴错误」——一句会把人引去查 CSRF 的
    /// 误导性报错，实际上只是两次登录互相踩了。
    ///
    /// 用 `parking_lot::Mutex` 而非 `std::sync::Mutex`：后者要 `.unwrap()` 解毒化，
    /// 而这里每条临界区都只是查表/插表，毒化本就无从谈起。
    pkce: Arc<parking_lot::Mutex<Vec<(String, PkceChallenge, std::time::Instant)>>>,
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
        pkce: Arc::new(parking_lot::Mutex::new(Vec::new())),
        store,
        client_key: client_key.clone(),
        admin_env: admin_password.map(Arc::new),
    };

    // 每天裁剪一次用量日志流水：终身统计在账本里（见 store 的 credential_stats/device_costs），
    // 流水只需保留近期。interval 的首个 tick 立即触发，兼作启动清理；删除是分批短事务，
    // 走 spawn_blocking 避免拿着 SQLite 锁占住异步线程。
    {
        let store = state.store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(24 * 3600));
            loop {
                tick.tick().await;
                let store = store.clone();
                match tokio::task::spawn_blocking(move || store.prune_usage_logs()).await {
                    Ok(Ok(n)) if n > 0 => tracing::info!(rows = n, "已裁剪过期用量日志"),
                    Ok(Err(e)) => tracing::warn!(error = %e, "裁剪用量日志失败"),
                    _ => {}
                }
            }
        });
    }

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
        .route("/credentials/{id}/test", post(test_credential))
        .route("/credentials/{id}/cooldown", delete(clear_cooldown))
        .route("/usage", get(list_usage))
        .route("/settings", get(get_settings))
        .route("/settings/api-key", post(set_api_key))
        .route("/settings/device-ttl", post(set_device_ttl))
        .route("/settings/default-device-limit", post(set_default_device_limit))
        .route("/settings/bare-rate-limit", post(set_bare_rate_limit))
        .route("/settings/rate-limit-retry-max", post(set_rate_limit_retry_max))
        .route("/settings/require-device-id", post(set_require_device_id))
        .route("/settings/forwarding", post(set_forwarding))
        .route("/auth/password", post(auth::change_password))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_admin));

    let api = public.merge(protected);

    // `/api/*` 管理接口；`/v1/*` 转发到官方 API；其余由内嵌前端 SPA 兜底。
    let app = Router::new()
        .nest("/api", api)
        // axum 对 `Bytes` 提取器默认限 2MB，超过的请求进不了 handler 就被 413 拦掉——
        // 而上游官方 /v1/messages 的上限是 32MB，长对话/带附件的合法请求很容易超 2MB。
        // 这里放到 64MB 留出余量，真正的大小判决交给上游；管理接口维持默认即可。
        .route("/v1/{*path}", any(proxy::handle).layer(DefaultBodyLimit::max(64 * 1024 * 1024)))
        // 个别移动端/前置层会以 POST 打开首页；用 PRG 把最终文档历史落成 GET。
        .route("/", get(admin_ui::fallback).post(admin_ui::redirect_root_post))
        // SPA 只允许由 GET/HEAD 打开。若把 POST 也兜底成 index.html，浏览器会把页面
        // 记作表单提交结果，之后在移动端刷新便弹出“确认重新提交表单”。
        .fallback_service(get(admin_ui::fallback))
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

/// 生成新的 PKCE 挑战并返回授权 URL；挑战按其 `state` 暂存，供后续交换时取回。
///
/// 并发的多次登录互不干扰——每次各占一格，见 [`AppState::pkce`]。顺手清掉过期与超量的格子。
async fn authorize(State(state): State<AppState>) -> Json<AuthorizeResp> {
    let pkce = PkceChallenge::generate();
    let url = pkce.authorize_url();
    remember_pkce(&mut state.pkce.lock(), pkce, std::time::Instant::now());
    Json(AuthorizeResp { url })
}

/// 进行中的登录尝试表，见 [`AppState::pkce`]。
type PendingPkce = Vec<(String, PkceChallenge, std::time::Instant)>;

/// 记下一次新的登录尝试，顺手清掉过期与超量的格子。
///
/// 抽成自由函数是为了能直接测——它修的正是一个簿记 bug（并发登录互相顶掉），
/// 而这类 bug 只在「同时两个人操作」时才现形，靠手点几乎复现不出来。
fn remember_pkce(pending: &mut PendingPkce, pkce: PkceChallenge, now: std::time::Instant) {
    pending.retain(|(_, _, at)| now.duration_since(*at) < PKCE_TTL);
    pending.push((pkce.state.clone(), pkce, now));
    // 超量时丢最旧的（尾插，故最旧在头部）。
    let overflow = pending.len().saturating_sub(PKCE_MAX_PENDING);
    pending.drain(..overflow);
}

/// 取出 `state` 对应的那次登录并从表中移除（一次挑战只能用一次）；过期的顺手清掉。
fn take_pkce(
    pending: &mut PendingPkce,
    state: &str,
    now: std::time::Instant,
) -> Option<PkceChallenge> {
    pending.retain(|(_, _, at)| now.duration_since(*at) < PKCE_TTL);
    let i = pending.iter().position(|(s, _, _)| s == state)?;
    Some(pending.remove(i).1)
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
    // 先从粘贴内容里取出 state，据此找到**它自己那次**登录的挑战——不能拿「最后一次生成的
    // 那个」，否则并发登录会互相顶掉（见 [`AppState::pkce`]）。取出即移除：一次挑战只能用一次。
    let returned_state = oauth::state_of(&req.code).map_err(|e| bad_request(e.to_string()))?;
    let pkce = take_pkce(&mut state.pkce.lock(), &returned_state, std::time::Instant::now())
        .ok_or_else(|| bad_request("这次登录已过期或未找到，请重新点「添加账号」生成授权链接"))?;

    // exchange_code 内部会再比一次 state。冗余是有意的：这里是「按 state 找挑战」，那里是
    // 「确认挑战与粘贴内容配套」，万一将来查找逻辑改错了，那道校验还在。
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

    // 用掉的挑战在取出时就已经从表里移除了，这里无需再清——其余进行中的登录不受影响。
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
                .with_cooldown(
                    state.store.rate_limited_secs(c.id),
                    state.store.rate_limited_models(c.id),
                )
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

#[derive(Deserialize)]
struct TestReq {
    /// 要测的模型名（如 `claude-opus-5`）。原样发给上游，不做白名单校验——模型名会随官方
    /// 上新变化，写死一份清单只会在下次上新时把新模型挡在外面，而「模型名不对」上游本来就
    /// 会回一条清清楚楚的 404/400，那正是这个功能要展示的东西。
    model: String,
}

/// 连通性测试：用**指定**账号向上游发一条最小请求，看这个号能不能用这个模型。
///
/// 停用/封禁的号也允许测——「它是不是已经恢复了」正是要问的问题，所以这里只校验凭证存在。
/// 测试的副作用与代价见 [`proxy::probe`]：不选号，但账号状态按真实流量的口径更新（429 打
/// 冷却、命中封号特征自动停用），会写一条用量日志（卡片上的额度与花费据此更新），也真的会
/// 消耗一点点订阅额度。上游拒绝（4xx/5xx）不是本接口的错误，照样 200 返回一份结果，
/// 由前端展示状态码与原因；只有「凭证不存在」「模型名没填」才是 4xx。
async fn test_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<TestReq>,
) -> Result<Json<proxy::ProbeReport>, ApiError> {
    let model = req.model.trim();
    if model.is_empty() {
        return Err(bad_request("请填写要测试的模型名"));
    }
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    Ok(Json(proxy::probe(&state, &cred, model).await))
}

/// 手动解除该凭证的限流状态：进程内的模型级冷却全清，且若它是被账号级限流**自动停用**的，
/// 一并重新启用（等价于手动打开启用开关，只是不会误碰人工停用/封号的号）。
///
/// 解除错了，下一条请求撞上 429 会重新打上，最坏多一次往返——所以这里不做任何「确认上游真的
/// 恢复了」的前置校验，想稳妥的话入口旁边就是连通性测试（它通过时也会自动恢复调度）。
async fn clear_cooldown(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<CredentialView>, ApiError> {
    state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    state.store.clear_rate_limited(id, None);
    state.store.resume_if_rate_limited(id).map_err(internal)?;
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
        CredentialView::new(&cred, count, default_limit)
            .with_cooldown(
                state.store.rate_limited_secs(cred.id),
                state.store.rate_limited_models(cred.id),
            )
            .with_stats(quota, last_used, cost_total),
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
    /// 单凭证裸请求速率上限（窗口内条数）；0 表示不限。
    bare_rate_limit: i64,
    /// 裸请求速率窗口（秒），默认 60。
    bare_rate_window_secs: i64,
    /// 上游 429 时最多换几个号重试；0 表示不重试。
    rate_limit_retry_max: i64,
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
    /// 非 Claude Code 客户端的请求，按官方抓包形态模拟成 CC 请求。
    simulate_cc: bool,
    /// 已是 CC 形态但不带 `metadata.user_id` 的请求，补一份官方形态的身份。
    fill_metadata: bool,
    /// 上游回 429 时给该号打冷却并换号重试。
    rate_limit_retry: bool,
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
            simulate_cc: f.simulate_cc,
            fill_metadata: f.fill_metadata,
            rate_limit_retry: f.rate_limit_retry,
        }
    }
}

fn settings_resp(state: &AppState) -> SettingsResp {
    let device_binding_ttl_secs = state.store.device_binding_ttl();
    let default_device_limit = state.store.default_device_limit();
    let require_device_id = state.store.require_device_id();
    let bare_rate_limit = state.store.bare_rate_limit();
    let bare_rate_window_secs = state.store.bare_rate_window_secs();
    let rate_limit_retry_max = state.store.rate_limit_retry_max() as i64;
    let forwarding = state.store.forward_flags().into();
    if let Some(k) = &state.client_key {
        return SettingsResp {
            api_key: Some(k.to_string()),
            env_managed: true,
            device_binding_ttl_secs,
            default_device_limit,
            require_device_id,
            bare_rate_limit,
            bare_rate_window_secs,
            rate_limit_retry_max,
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
        bare_rate_limit,
        bare_rate_window_secs,
        rate_limit_retry_max,
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
struct SetBareRateLimitReq {
    /// 单凭证在窗口内允许的裸请求条数；0（或负数）表示不限。
    bare_rate_limit: i64,
    /// 窗口秒数；缺省或 `<= 0` 时保持现值不动（不因为改上限就把窗口重置成默认值）。
    bare_rate_window_secs: Option<i64>,
}

/// 设置裸请求速率上限（每个凭证各算各的，只统计无 `metadata.user_id` 的请求）。
///
/// 计数在进程内存里，改上限即时生效；窗口只在显式给出正数时才写，避免前端只想调上限却把
/// 窗口顺手清成默认值。
async fn set_bare_rate_limit(
    State(state): State<AppState>,
    Json(req): Json<SetBareRateLimitReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let limit = req.bare_rate_limit.max(0);
    state.store.set_setting(crate::store::BARE_RATE_LIMIT, &limit.to_string()).map_err(internal)?;
    if let Some(window) = req.bare_rate_window_secs.filter(|w| *w > 0) {
        state
            .store
            .set_setting(crate::store::BARE_RATE_WINDOW_SECS, &window.to_string())
            .map_err(internal)?;
    }
    tracing::info!(limit, window = ?req.bare_rate_window_secs, "裸请求速率上限变更");
    Ok(Json(settings_resp(&state)))
}

#[derive(Deserialize)]
struct SetRateLimitRetryMaxReq {
    /// 上游 429 时最多换几个号重试；0 表示不重试，上限由后端夹到 10。
    rate_limit_retry_max: i64,
}

/// 设置上游 429 的换号重试次数（开关另见转发形态里的 `rate_limit_retry`）。
async fn set_rate_limit_retry_max(
    State(state): State<AppState>,
    Json(req): Json<SetRateLimitRetryMaxReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let n = req.rate_limit_retry_max.clamp(0, 10);
    state
        .store
        .set_setting(crate::store::RATE_LIMIT_RETRY_MAX, &n.to_string())
        .map_err(internal)?;
    tracing::info!(retry_max = n, "上游 429 换号重试次数变更");
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
    simulate_cc: Option<bool>,
    fill_metadata: Option<bool>,
    rate_limit_retry: Option<bool>,
}

/// 逐项开关转发形态改动。全关即「零改写直接转发」——实测上游唯一必需的是注入
/// `Authorization`，这些开关都只影响与官方客户端的形态贴合度，见
/// [`crate::store::ForwardFlags`]。
async fn set_forwarding(
    State(state): State<AppState>,
    Json(req): Json<SetForwardingReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    use crate::store::{
        FILL_CLIENT_HEADERS, FILL_METADATA, MERGE_BETA, ORIG_HEADER_CASE, RATE_LIMIT_RETRY,
        SIMULATE_CC, SPOOF_BILLING_CCH, SPOOF_IDENTITY_ENABLED, SYSTEM_SHAPE,
        THINKING_SIGNATURE_RETRY,
    };
    let items = [
        (SPOOF_IDENTITY_ENABLED, req.spoof_identity),
        (SPOOF_BILLING_CCH, req.billing_cch),
        (FILL_CLIENT_HEADERS, req.fill_client_headers),
        (MERGE_BETA, req.merge_beta),
        (SYSTEM_SHAPE, req.system_shape),
        (ORIG_HEADER_CASE, req.orig_header_case),
        (THINKING_SIGNATURE_RETRY, req.thinking_signature_retry),
        (SIMULATE_CC, req.simulate_cc),
        (FILL_METADATA, req.fill_metadata),
        (RATE_LIMIT_RETRY, req.rate_limit_retry),
    ];
    for (key, value) in items.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))) {
        state.store.set_setting(key, if value { "true" } else { "false" }).map_err(internal)?;
        tracing::info!(key, enabled = value, "转发形态开关变更");
    }
    Ok(Json(settings_resp(&state)))
}

// ---------- 视图与错误 ----------

/// 一个模型当前的冷却剩余时间，见 [`CredentialView::rate_limited_models`]。
#[derive(Serialize)]
struct ModelCooldown {
    model: String,
    secs: i64,
}

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
    /// **账号级**进程内冷却的剩余秒数；`0` 表示不在冷却中。
    ///
    /// 正常路径上这一项几乎恒为 0：账号级 429 走的是落库的 `resume_at`（见下），只有落库
    /// 失败的兜底分支才会退回进程内冷却。留着它是为了让那个兜底状态在后台也能看见。
    /// 模型级冷却在 `rate_limited_models` 里，两者不可混用——见 `crate::store::RateLimitCooldown`。
    rate_limited_secs: i64,
    /// **模型级**冷却明细（容量限制/超额池满那种，默认 30 秒，记在进程内）。
    ///
    /// 这一档**不代表账号有问题**：只有列出的这些模型在选号时让位，该号的其余模型照常服务，
    /// 所以前端不能拿它把账号显示成「不可调度」。此前它压根没被透出来，于是 fable 撞超额池
    /// 被冷却时后台一片正常，选号侧却已经跳过它了。
    rate_limited_models: Vec<ModelCooldown>,
    /// 被上游账号级限流而**自动停用**时，到点自动恢复调度的时刻（Unix 秒）；`None` 表示
    /// 不自动恢复（正常在用、人工停用、或封号）。
    ///
    /// 前端据此把「被限流暂停」和「已停用/已封号」分开显示：两者 `disabled` 都是 `true`，
    /// 区别只在这一项。展示绝对时刻而非倒计时的理由同 `expires_at`。恢复有三条路：到点自动、
    /// 连通性测试通过、手动打开启用开关。
    resume_at: Option<u64>,
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
            rate_limited_secs: 0,
            rate_limited_models: Vec::new(),
            resume_at: c.resume_at,
        }
    }

    /// 附加冷却状态：账号级剩余秒数 + 模型级明细（都在内存里，没有就是 0 / 空）。
    fn with_cooldown(mut self, secs: i64, models: Vec<(String, i64)>) -> Self {
        self.rate_limited_secs = secs;
        self.rate_limited_models =
            models.into_iter().map(|(model, secs)| ModelCooldown { model, secs }).collect();
        self
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::PkceChallenge;

    /// **并发的多次登录不得互相顶掉。**
    ///
    /// 这是一条真实 bug 的护栏：原先 PKCE 只有一个全局槽位，两个标签页（或两个人）同时点
    /// 「添加账号」，后一次生成就把前一次的 verifier/state 覆盖了，前一个人粘贴回来撞上的是
    /// 「state 不匹配，可能存在 CSRF 或粘贴错误」——一句会把人引去查 CSRF 的误导性报错。
    #[test]
    fn concurrent_logins_do_not_clobber_each_other() {
        let now = std::time::Instant::now();
        let mut pending = PendingPkce::new();

        let a = PkceChallenge::generate();
        let b = PkceChallenge::generate();
        let (sa, sb) = (a.state.clone(), b.state.clone());
        let (va, vb) = (a.verifier.clone(), b.verifier.clone());
        assert_ne!(sa, sb, "两次生成的 state 必须不同");

        remember_pkce(&mut pending, a, now);
        remember_pkce(&mut pending, b, now);

        // 先发起的那次照样能换回**自己**的 verifier，而不是被后一次顶掉。
        let got_a = take_pkce(&mut pending, &sa, now).expect("先发起的那次登录不该被顶掉");
        assert_eq!(got_a.verifier, va);
        let got_b = take_pkce(&mut pending, &sb, now).expect("后发起的那次也要在");
        assert_eq!(got_b.verifier, vb);

        // 取出即移除：一次挑战只能用一次，重放拿不到东西。
        assert!(take_pkce(&mut pending, &sa, now).is_none(), "挑战不得被重复使用");
        assert!(pending.is_empty());
    }

    /// 过期的登录尝试会被清掉，不认识的 state 一律取不到。
    #[test]
    fn pkce_entries_expire_and_unknown_state_misses() {
        let now = std::time::Instant::now();
        let mut pending = PendingPkce::new();
        let p = PkceChallenge::generate();
        let s = p.state.clone();
        remember_pkce(&mut pending, p, now);

        assert!(take_pkce(&mut pending, "someone-elses-state", now).is_none());
        // 刚好到 TTL 就算过期（条件是严格小于）。
        remember_pkce(&mut pending, PkceChallenge::generate(), now);
        let expired_at = now + PKCE_TTL;
        assert!(take_pkce(&mut pending, &s, expired_at).is_none(), "过期的应被清掉");
        assert!(pending.is_empty(), "过期项不该留在表里");
    }

    /// 反复点「添加账号」不能把内存撑起来：超量时丢最旧的，最新的那次必须留下。
    #[test]
    fn pkce_table_is_bounded_and_drops_the_oldest() {
        let now = std::time::Instant::now();
        let mut pending = PendingPkce::new();
        let mut states = Vec::new();
        for _ in 0..(PKCE_MAX_PENDING + 5) {
            let p = PkceChallenge::generate();
            states.push(p.state.clone());
            remember_pkce(&mut pending, p, now);
        }
        assert_eq!(pending.len(), PKCE_MAX_PENDING);
        assert!(take_pkce(&mut pending, &states[0], now).is_none(), "最旧的应被丢弃");
        let newest = states.last().unwrap();
        assert!(take_pkce(&mut pending, newest, now).is_some(), "最新的一次必须还在");
    }
}
