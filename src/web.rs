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
    /// 出站客户端池：不配代理的号共用直连那一份，配了代理的各有一份。
    /// 见 [`crate::clients::ClientPool`]。
    pub clients: std::sync::Arc<crate::clients::ClientPool>,
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
    /// 上游拒过的请求形态记忆表：进程内累积，用来在本地拦掉上游已经拒过一次的
    /// 「模型 + 取值」组合（`effort: 'xhigh'`、`role: 'system'` 之类），不再白发一次。
    /// 见 [`crate::proxy::ShapeMemory`]。
    pub shape_rejections: crate::proxy::ShapeMemory,
}

type ApiError = (StatusCode, String);

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
    let clients = std::sync::Arc::new(crate::clients::ClientPool::new()?);
    let state = AppState {
        clients,
        pkce: Arc::new(parking_lot::Mutex::new(Vec::new())),
        store,
        client_key: client_key.clone(),
        admin_env: admin_password.map(Arc::new),
        shape_rejections: Arc::default(),
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
                    Ok(Ok(n)) if n > 0 => tracing::info!(rows = n, "pruned expired usage logs"),
                    Ok(Err(e)) => tracing::warn!(error = %e, "failed to prune usage logs"),
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
        .route("/credentials/rpm-limit", post(set_rpm_limits))
        .route("/credentials/disabled", post(set_disabled_many))
        .route("/credentials/delete", post(delete_credentials))
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_disabled))
        .route("/credentials/{id}/priority", post(set_priority))
        .route("/credentials/{id}/label", post(set_label))
        .route("/credentials/{id}/proxy", post(set_proxy))
        .route("/credentials/{id}/device-limit", post(set_device_limit))
        .route("/credentials/{id}/rpm-limit", post(set_rpm_limit))
        .route("/credentials/{id}/devices", get(list_credential_devices))
        .route("/credentials/{id}/usage", get(list_credential_usage))
        .route("/credentials/{id}/devices/{device_id}", delete(unbind_credential_device))
        .route("/credentials/{id}/refresh", post(refresh_credential))
        .route("/credentials/{id}/test", post(test_credential))
        .route("/credentials/{id}/cooldown", delete(clear_cooldown))
        .route("/usage", get(list_usage))
        .route("/settings", get(get_settings))
        .route("/settings/api-key", post(set_api_key))
        .route("/settings/device-ttl", post(set_device_ttl))
        .route("/settings/device-retention", post(set_device_retention))
        .route("/settings/default-device-limit", post(set_default_device_limit))
        .route("/settings/default-rpm-limit", post(set_default_rpm_limit))
        .route("/settings/bare-rate-limit", post(set_bare_rate_limit))
        .route("/settings/rate-limit-retry-max", post(set_rate_limit_retry_max))
        .route("/settings/require-device-id", post(set_require_device_id))
        .route("/settings/min-client-version", post(set_min_client_version))
        .route("/settings/forwarding", post(set_forwarding))
        .route("/auth/password", post(auth::change_password))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_admin));

    // 失败的请求补一行「哪个方法打了哪条路径、回了几」。错误详情由 `internal`/`bad_request`
    // 各自记，方法与路径它们看不到，只能在这一层补——两行合起来才定位得到一次失败。
    let api = public.merge(protected).layer(middleware::from_fn(log_api_failures));

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
        .with_context(|| format!("failed to bind {} (the port may be in use)", bind))?;

    let shown = if host == "0.0.0.0" || host == "::" { "127.0.0.1" } else { host };
    let url = format!("http://{shown}:{port}/");
    let base = url.trim_end_matches('/');

    tracing::info!(addr = %bind, url = %url, "luban started");
    match &client_key {
        Some(_) => tracing::info!(
            "Claude Code setup: ANTHROPIC_BASE_URL={base}, ANTHROPIC_AUTH_TOKEN=<--api-key>"
        ),
        None => tracing::info!(
            "Claude Code setup: ANTHROPIC_BASE_URL={base} (no --api-key set, the proxy does not authenticate callers -- keep it local-only)"
        ),
    }
    if open_browser {
        open_in_browser(&url);
        tracing::info!(url = %url, "tried to open the browser; if nothing appeared, open the url manually");
    }

    // `into_make_service_with_connect_info` 而不是直接交 `app`：登录失败要记来源，
    // 而对端地址只有这里能拿到（见 [`auth::client_ip`]）。
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("the web server exited unexpectedly")?;
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
        signal::ctrl_c().await.expect("failed to install the Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received, shutting down gracefully ...");
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
        .ok_or_else(|| bad_request("this login attempt expired or was not found; click 'Add account' again to generate a new authorization link"))?;

    // exchange_code 内部会再比一次 state。冗余是有意的：这里是「按 state 找挑战」，那里是
    // 「确认挑战与粘贴内容配套」，万一将来查找逻辑改错了，那道校验还在。
    let tokens = oauth::exchange_code(state.clients.direct(), &pkce, &req.code)
        .await
        .map_err(|e| bad_request(e.to_string()))?;

    // 拉取账号 profile 拿邮箱/姓名/等级（失败不阻断，用兜底）。不阻断不等于不留痕：
    // 悄悄吞掉的话，账号加进来标签是「账号 N」、等级空着，看不出是 profile 没拉到。
    let profile = match oauth::fetch_profile(state.clients.direct(), &tokens.access_token).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "add credential: fetching the account profile failed, falling back for label and tier");
            oauth::Profile::default()
        }
    };

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
                format!("Account {}", n)
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
            profile.org_type.as_deref(),
        )
        .map_err(internal)?;

    // 用掉的挑战在取出时就已经从表里移除了，这里无需再清——其余进行中的登录不受影响。
    tracing::info!(
        cred_id = cred.id, cred = %cred.label,
        tier = ?cred.tier, org_type = ?cred.org_type,
        "credential added"
    );
    Ok(Json(CredentialView::new(&cred, 0, DefaultLimits::of(&state.store))))
}

// ---------- 用量日志 ----------

#[derive(Deserialize)]
struct UsageQuery {
    /// 返回条数上限（默认 100，最多 1000；按号查时默认 25、最多 200）。
    #[serde(default)]
    limit: Option<i64>,
    /// 跳过前多少条（页码 × 每页条数）。
    #[serde(default)]
    offset: Option<i64>,
    /// 翻页锚点：只取 id ≤ 它的记录。首次不传，之后把响应里的 `anchor` 原样带回来。
    /// 理由见 [`store::UsageLogQuery`]。
    #[serde(default)]
    until: Option<i64>,
}

/// 一页流水 + 整个集合的口径。前端要靠 `total` 算页数、靠 `anchor` 把整轮翻页钉在同一快照上。
#[derive(serde::Serialize)]
struct UsagePage {
    /// 满足筛选（含 `until` 上界）的总条数。
    total: i64,
    /// 同一集合的花费合计（USD）。
    total_cost: f64,
    /// 本轮翻页的锚点：请求里带了 `until` 就是它，否则是当前最大 id；空集为 null。
    anchor: Option<i64>,
    logs: Vec<store::UsageLog>,
}

/// 列出最近的用量日志（按时间倒序）。
async fn list_usage(
    State(state): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<UsagePage>, ApiError> {
    usage_page(&state, None, &q, 100, 1000)
}

/// 列出某凭证的请求流水（按时间倒序，页码翻页）。
///
/// 与卡片上那些聚合数的口径**不同**，这一点得记清楚：卡片的累计花费、设备请求数读的是
/// 终身账本（`credential_stats` / `device_costs`），而流水只保留近期（见
/// [`store::CredentialStore::prune_usage_logs`]）。于是「明细合计 < 卡片上的累计」是正常的，
/// 不是哪一边算错了。
async fn list_credential_usage(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<UsagePage>, ApiError> {
    // 与设备明细同口径：凭证不存在给 404，免得前端把「账号已被删」显示成「没有请求」。
    if state.store.get(id).map_err(internal)?.is_none() {
        return Err(not_found());
    }
    usage_page(&state, Some(id), &q, 25, 200)
}

/// 两条流水接口共用的取页逻辑：先按 `until`（没有就现取一个）钉住快照，再在同一条件下
/// 取统计与当页记录。
///
/// **统计与记录必须同锚点**：先算 total 再另取一次 max(id) 当锚点的话，两次之间新写入的
/// 请求会让 total 比锚点下真正翻得到的条数多，最后一页于是空着。
fn usage_page(
    state: &AppState,
    cred_id: Option<i64>,
    q: &UsageQuery,
    default_limit: i64,
    max_limit: i64,
) -> Result<Json<UsagePage>, ApiError> {
    let limit = q.limit.unwrap_or(default_limit).clamp(1, max_limit);
    let offset = q.offset.unwrap_or(0).max(0);
    let mut filter = store::UsageLogQuery { cred_id, until_id: q.until, offset, limit };
    let stats = state.store.usage_log_stats(filter).map_err(internal)?;
    // 首次请求没有锚点，就用这一刻的最大 id 当锚点——统计与记录都在它之下，两者自洽。
    filter.until_id = q.until.or(stats.max_id);
    let logs = state.store.query_usage_logs(filter).map_err(internal)?;
    Ok(Json(UsagePage {
        total: stats.total,
        total_cost: stats.cost_usd,
        anchor: filter.until_id,
        logs,
    }))
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
    let rpm = state.store.recent_rpm().map_err(internal)?;
    let defaults = DefaultLimits::of(&state.store);
    let views = list
        .iter()
        .map(|c| {
            CredentialView::new(c, counts.get(&c.id).copied().unwrap_or(0), defaults)
                .with_cooldown(
                    state.store.rate_limited_secs(c.id),
                    state.store.rate_limited_models(c.id),
                )
                .with_stats(
                    quotas.get(&c.id).cloned(),
                    last_used.get(&c.id).copied(),
                    costs.get(&c.id).copied().unwrap_or(0.0),
                    // 窗口内一条流水都没有的账号不在 map 里，就是 0 RPM。
                    rpm.get(&c.id).copied().unwrap_or(0),
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
        return Err((
            StatusCode::NOT_FOUND,
            "device binding not found (it may have expired or moved to another credential)".into(),
        ));
    }
    tracing::info!(cred_id = id, device_id = %device_id, "device binding removed manually");
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
    tracing::info!(cred_id = id, "credential deleted");
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
    tracing::info!(count = n, priority = req.priority, "priority set in bulk");
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
        return Err(bad_request("select at least one credential"));
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
    tracing::info!(count = n, device_limit = limit, "device limit set in bulk");
    list_credentials(State(state)).await
}

#[derive(Deserialize)]
struct SetRpmLimitsReq {
    ids: Vec<i64>,
    /// 三态同单账号接口：`> 0` 独立上限；`0` 跟随全局默认；`< 0` 明确不限。
    rpm_limit: i64,
}

/// 批量设置账号 RPM 上限，返回更新后的整份列表。
async fn set_rpm_limits(
    State(state): State<AppState>,
    Json(req): Json<SetRpmLimitsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let limit = if req.rpm_limit < 0 { -1 } else { req.rpm_limit };
    let n = state.store.set_rpm_limits(&req.ids, limit).map_err(internal)?;
    tracing::info!(count = n, rpm_limit = limit, "rpm limit set in bulk");
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
    tracing::info!(count = n, disabled = req.disabled, "enabled/disabled in bulk");
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
    tracing::info!(count = n, "credentials deleted in bulk");
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
        return Err(bad_request("the name must not be empty"));
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

#[derive(Deserialize)]
struct SetRpmLimitReq {
    /// RPM 上限三态：`> 0` 本账号独立上限；`0` 跟随全局默认；`< 0` 本账号明确不限。
    rpm_limit: i64,
}

/// 设置该账号每分钟最多转发多少条请求。
///
/// 计数在进程内存里，改完即时生效；已经记在窗口里的那些不会因为调高上限而消失，
/// 也不会因为调低而被追认——只影响之后的判定。
async fn set_rpm_limit(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetRpmLimitReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let limit = if req.rpm_limit < 0 { -1 } else { req.rpm_limit };
    if !state.store.set_rpm_limit(id, limit).map_err(internal)? {
        return Err(not_found());
    }
    tracing::info!(cred_id = id, rpm_limit = limit, "rpm limit set");
    view_of(&state, id)
}

#[derive(Deserialize)]
struct SetProxyReq {
    /// 代理 URL；`null` 或空串表示清除（改回直连）。
    proxy: Option<String>,
}

/// 设置/清除某个账号专用的出站代理。
///
/// 配好之后这个号的**全部**出站流量都走它：转发、token 刷新、profile、连通性测试。
/// 校验放在入库之前——存进去一条建不出客户端的代理，故障要等到下次真有请求选中这个号
/// 才暴露，那时现场只剩一条「这个号所有请求都失败」。
async fn set_proxy(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetProxyReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    let proxy = match req.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            Some(crate::clients::validate_proxy(raw).map_err(|e| bad_request(format!("{e:#}")))?)
        }
        None => None,
    };
    if !state.store.set_proxy(id, proxy.as_deref()).map_err(internal)? {
        return Err(not_found());
    }
    // 丢掉旧代理那份缓存客户端，否则它的连接池还会继续把请求送去老代理——改完之后
    // 「看着已经换了、实际还在走旧的」是这类缓存最典型的坑。
    if let Some(old) = cred.proxy.as_deref() {
        state.clients.forget(old);
    }
    tracing::info!(
        cred_id = id, cred = %cred.label,
        proxy = %proxy.as_deref().unwrap_or("<direct>"),
        "credential proxy updated"
    );
    view_of(&state, id)
}

/// 手动刷新一条凭证的 token。
async fn refresh_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    // 手动刷新同样走这个号自己的代理；代理坏掉时如实报错，不退回直连（见 ClientPool）。
    let http = state.clients.for_credential(&cred).map_err(|e| bad_request(format!("{e:#}")))?;
    let tokens =
        oauth::refresh(&http, &cred.refresh_token).await.map_err(|e| bad_request(e.to_string()))?;
    state
        .store
        .update_tokens(id, &tokens.access_token, &tokens.refresh_token, tokens.expires_at)
        .map_err(internal)?;
    // 顺带刷新账号等级、回填账号 UUID（失败忽略，不影响 token 刷新结果）。忽略归忽略，
    // 三处失败都留一行——否则「刷新成功了但等级还是旧的」在日志里毫无痕迹。
    match oauth::fetch_profile(&http, &tokens.access_token).await {
        Ok(profile) => {
            if profile.tier.is_some()
                && let Err(e) = state.store.set_tier(id, profile.tier.as_deref())
            {
                tracing::warn!(cred_id = id, error = %e, "failed to write back the account tier (the refresh itself succeeded)");
            }
            if let Some(uuid) = profile.account_uuid.as_deref()
                && let Err(e) = state.store.set_account_uuid(id, uuid)
            {
                tracing::warn!(cred_id = id, error = %e, "failed to backfill the account uuid (the refresh itself succeeded)");
            }
            // 组织类型同样回填：旧库里的号是在这一列存在之前加的，只有刷新一次才补得上。
            if profile.org_type.is_some()
                && let Err(e) = state.store.set_org_type(id, profile.org_type.as_deref())
            {
                tracing::warn!(cred_id = id, error = %e, "failed to write back the organization type (the refresh itself succeeded)");
            }
        }
        Err(e) => {
            tracing::warn!(cred_id = id, error = %e, "fetching the profile after refresh failed, tier and uuid left unchanged");
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
        return Err(bad_request("specify the model name to test"));
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
    let rpm = state.store.recent_rpm_of(id).map_err(internal)?;
    Ok(Json(
        CredentialView::new(&cred, count, DefaultLimits::of(&state.store))
            .with_cooldown(
                state.store.rate_limited_secs(cred.id),
                state.store.rate_limited_models(cred.id),
            )
            .with_stats(quota, last_used, cost_total, rpm),
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
    /// 软绑定保留期（秒）：超过有效期的绑定不再占名额，但这段时间内设备回来仍优先回原号。
    /// 0 表示永久保留。
    device_binding_retention_secs: i64,
    /// 全局默认设备数上限；0 表示默认不限。账号未单独配置时套用它。
    default_device_limit: i64,
    /// 全局默认账号 RPM 上限（最近 60 秒最多转发多少条）；0 表示默认不限。
    /// 账号未单独配置时套用它。
    default_rpm_limit: i64,
    /// 是否要求请求携带有效设备身份（`metadata.user_id`）；关闭后放行裸客户端。
    require_device_id: bool,
    /// 允许接入的最低 Claude Code 客户端版本；空串表示不限。只卡 UA 自报 `claude-cli/<版本>`
    /// 的请求，见 [`crate::store::MIN_CLIENT_VERSION`]。
    min_client_version: String,
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
    /// 来访自带 `device_id` 时要不要换成派生值（[`Self::spoof_identity`] 的子项）。
    spoof_device_id: bool,
    /// 给 `x-anthropic-billing-header` 补 `cch`。
    billing_cch: bool,
    /// 补齐客户端未携带的 `accept-encoding`/`anthropic-version`/`x-client-request-id`。
    fill_client_headers: bool,
    /// 合并并按官方顺序重排 `anthropic-beta`（含塞入 oauth beta）。
    merge_beta: bool,
    /// 把 `system` 对齐成官方订阅客户端的 4 块形态（拆/并块 + 块数封顶 4）。
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
    /// 官方基座那块的缓存断点带不带 `scope:"global"`（跨账号共享基座缓存）。
    cache_scope_global: bool,
    /// 缓存断点写不写 `ttl:"1h"`（对齐官方；关掉即沿用客户端自己传的时长）。
    cache_ttl_1h: bool,
    /// 非流式 `/v1/messages` 改成流式发给上游，再把 SSE 聚合回整段 JSON 给客户端。
    nonstream_as_sse: bool,
    /// 剥掉官方客户端从不发送的顶层字段（缺省语义的 `tool_choice`、`thinking.display`）。
    strip_extra_fields: bool,
    /// 把会被上游判成第三方应用的工具名换成假名转发，回程再还原。
    tool_name_mimic: bool,
}

impl From<crate::store::ForwardFlags> for ForwardingResp {
    fn from(f: crate::store::ForwardFlags) -> Self {
        Self {
            spoof_identity: f.spoof_identity,
            spoof_device_id: f.spoof_device_id,
            billing_cch: f.billing_cch,
            fill_client_headers: f.fill_client_headers,
            merge_beta: f.merge_beta,
            system_shape: f.system_shape,
            orig_header_case: f.orig_header_case,
            thinking_signature_retry: f.thinking_signature_retry,
            simulate_cc: f.simulate_cc,
            fill_metadata: f.fill_metadata,
            rate_limit_retry: f.rate_limit_retry,
            cache_scope_global: f.cache_scope_global,
            cache_ttl_1h: f.cache_ttl_1h,
            nonstream_as_sse: f.nonstream_as_sse,
            strip_extra_fields: f.strip_extra_fields,
            tool_name_mimic: f.tool_name_mimic,
        }
    }
}

fn settings_resp(state: &AppState) -> SettingsResp {
    let device_binding_ttl_secs = state.store.device_binding_ttl();
    let device_binding_retention_secs = state.store.device_binding_retention();
    let default_device_limit = state.store.default_device_limit();
    let default_rpm_limit = state.store.default_rpm_limit();
    let require_device_id = state.store.require_device_id();
    let min_client_version = state.store.min_client_version().unwrap_or_default();
    let bare_rate_limit = state.store.bare_rate_limit();
    let bare_rate_window_secs = state.store.bare_rate_window_secs();
    let rate_limit_retry_max = state.store.rate_limit_retry_max() as i64;
    let forwarding = state.store.forward_flags().into();
    if let Some(k) = &state.client_key {
        return SettingsResp {
            api_key: Some(k.to_string()),
            env_managed: true,
            device_binding_ttl_secs,
            device_binding_retention_secs,
            default_device_limit,
            default_rpm_limit,
            require_device_id,
            min_client_version,
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
        device_binding_retention_secs,
        default_device_limit,
        default_rpm_limit,
        require_device_id,
        min_client_version,
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
        return Err(bad_request(
            "the inbound key is managed by the LUBAN_API_KEY environment variable and cannot be changed from the web UI",
        ));
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
struct SetDeviceRetentionReq {
    /// 软绑定保留期（秒）；0（或负数）表示永久保留。
    device_binding_retention_secs: i64,
}

/// 设置软绑定保留期（秒）。
///
/// 不在这里校验「必须 >= 有效期」：两个设置各存各的，比较放在选路时做
/// （见 [`crate::store::effective_retention`]），免得改动顺序还得先改大的那个。
async fn set_device_retention(
    State(state): State<AppState>,
    Json(req): Json<SetDeviceRetentionReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let secs = req.device_binding_retention_secs.max(0);
    state
        .store
        .set_setting(crate::store::DEVICE_BINDING_RETENTION, &secs.to_string())
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
struct SetDefaultRpmLimitReq {
    /// 全局默认账号 RPM 上限；0（或负数）表示默认不限。
    default_rpm_limit: i64,
}

/// 设置全局默认账号 RPM 上限（账号自身未单独配置时生效）。
async fn set_default_rpm_limit(
    State(state): State<AppState>,
    Json(req): Json<SetDefaultRpmLimitReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let limit = req.default_rpm_limit.max(0);
    state
        .store
        .set_setting(crate::store::DEFAULT_RPM_LIMIT, &limit.to_string())
        .map_err(internal)?;
    tracing::info!(limit, "default rpm limit changed");
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
    tracing::info!(limit, window = ?req.bare_rate_window_secs, "bare-request rate limit changed");
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
    tracing::info!(retry_max = n, "upstream-429 credential-swap retry cap changed");
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
    tracing::info!(required = req.required, "device identity check toggled");
    Ok(Json(settings_resp(&state)))
}

#[derive(Deserialize)]
struct SetMinClientVersionReq {
    /// 最低 Claude Code 客户端版本（`2.1.220`、`2.1`、`2` 都收）；空串表示不限。
    min_client_version: String,
}

/// 设置最低客户端版本闸：UA 自报 `claude-cli/<版本>` 且低于此值的请求直接 403。
///
/// 只收能解析的版本串——写错一个字（`v2.1`、`最新版`）在代理侧会被当成「没配」而静默放行，
/// 那时网页上明明写着一个值、闸却没开，是最难查的一种。故在入口处直接回 400。
async fn set_min_client_version(
    State(state): State<AppState>,
    Json(req): Json<SetMinClientVersionReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    let version = req.min_client_version.trim();
    if version.is_empty() {
        state.store.delete_setting(crate::store::MIN_CLIENT_VERSION).map_err(internal)?;
        tracing::info!("minimum client version cleared");
        return Ok(Json(settings_resp(&state)));
    }
    if crate::proxy::parse_version(version).is_none() {
        return Err(bad_request(
            "the minimum client version must look like 2.1.220 (2 and 2.1 are accepted too)",
        ));
    }
    state.store.set_setting(crate::store::MIN_CLIENT_VERSION, version).map_err(internal)?;
    tracing::info!(version, "minimum client version changed");
    Ok(Json(settings_resp(&state)))
}

/// 转发形态开关的改动请求：**只有出现的字段会被写入**，其余保持原值。
/// 前端每次拨一个开关就只带那一个字段，不必回传全量、也不会互相覆盖。
#[derive(Deserialize)]
struct SetForwardingReq {
    spoof_identity: Option<bool>,
    spoof_device_id: Option<bool>,
    billing_cch: Option<bool>,
    fill_client_headers: Option<bool>,
    merge_beta: Option<bool>,
    system_shape: Option<bool>,
    orig_header_case: Option<bool>,
    thinking_signature_retry: Option<bool>,
    simulate_cc: Option<bool>,
    fill_metadata: Option<bool>,
    rate_limit_retry: Option<bool>,
    cache_scope_global: Option<bool>,
    cache_ttl_1h: Option<bool>,
    nonstream_as_sse: Option<bool>,
    strip_extra_fields: Option<bool>,
    tool_name_mimic: Option<bool>,
}

/// 逐项开关转发形态改动。全关即「零改写直接转发」——实测上游唯一必需的是注入
/// `Authorization`，这些开关都只影响与官方客户端的形态贴合度，见
/// [`crate::store::ForwardFlags`]。
async fn set_forwarding(
    State(state): State<AppState>,
    Json(req): Json<SetForwardingReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    use crate::store::{
        FILL_CLIENT_HEADERS, FILL_METADATA, MERGE_BETA, NONSTREAM_AS_SSE, ORIG_HEADER_CASE,
        RATE_LIMIT_RETRY, SIMULATE_CC, SPOOF_BILLING_CCH, SPOOF_DEVICE_ID, SPOOF_IDENTITY_ENABLED,
        STRIP_EXTRA_FIELDS, SYSTEM_CACHE_SCOPE, SYSTEM_CACHE_TTL, SYSTEM_SHAPE,
        THINKING_SIGNATURE_RETRY, TOOL_NAME_MIMIC,
    };
    let items = [
        (SPOOF_IDENTITY_ENABLED, req.spoof_identity),
        (SPOOF_DEVICE_ID, req.spoof_device_id),
        (SPOOF_BILLING_CCH, req.billing_cch),
        (FILL_CLIENT_HEADERS, req.fill_client_headers),
        (MERGE_BETA, req.merge_beta),
        (SYSTEM_SHAPE, req.system_shape),
        (ORIG_HEADER_CASE, req.orig_header_case),
        (THINKING_SIGNATURE_RETRY, req.thinking_signature_retry),
        (SIMULATE_CC, req.simulate_cc),
        (FILL_METADATA, req.fill_metadata),
        (RATE_LIMIT_RETRY, req.rate_limit_retry),
        (SYSTEM_CACHE_SCOPE, req.cache_scope_global),
        (SYSTEM_CACHE_TTL, req.cache_ttl_1h),
        (NONSTREAM_AS_SSE, req.nonstream_as_sse),
        (STRIP_EXTRA_FIELDS, req.strip_extra_fields),
        (TOOL_NAME_MIMIC, req.tool_name_mimic),
    ];
    for (key, value) in items.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))) {
        state.store.set_setting(key, if value { "true" } else { "false" }).map_err(internal)?;
        tracing::info!(key, enabled = value, "forwarding shape toggle changed");
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

/// 构造凭证视图时要用到的两个全局默认上限。
///
/// 包成结构体而不是并列两个 `i64` 参数：位置写反了照样编译得过，而那是一个「把设备上限当成
/// RPM 上限算」的静默错误——同 [`store::Select`] 的理由。
#[derive(Clone, Copy)]
struct DefaultLimits {
    /// 全局默认设备数上限，见 [`store::CredentialStore::default_device_limit`]。
    device: i64,
    /// 全局默认账号 RPM 上限，见 [`store::CredentialStore::default_rpm_limit`]。
    rpm: i64,
}

impl DefaultLimits {
    fn of(store: &store::CredentialStore) -> Self {
        Self { device: store.default_device_limit(), rpm: store.default_rpm_limit() }
    }
}

/// 对外暴露的凭证视图（不返回明文 token）。
#[derive(Serialize)]
struct CredentialView {
    id: i64,
    label: String,
    tier: Option<String>,
    /// 组织类型原值（`claude_team`/`claude_enterprise`/…）。前端据此给团队号单独打标——
    /// 团队额度是整个组织共享的，跟同名档位的个人号不是一回事。
    org_type: Option<String>,
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
    /// 账号自身的 RPM 上限设置：`> 0` 独立上限；`0` 跟随全局默认；`< 0` 明确不限。
    rpm_limit: i64,
    /// 实际生效的 RPM 上限（已套用全局默认）；0 表示不限。前端拿它和 `rpm` 一起显示成
    /// 「12 / 30」，两个数同一个窗口（最近 60 秒），可以直接比。
    rpm_limit_effective: i64,
    /// 自动检测到的上游账号级错误原因（如封号）；`None` 表示未被自动停用。
    ban_reason: Option<String>,
    /// 该账号专用的出站代理；`None` 表示直连。**原样返回、不脱敏**：代理串里可能带账号密码，
    /// 但这是个已经过管理鉴权的接口，而把它打码会让人没法确认自己配的到底是哪一条。
    proxy: Option<String>,
    /// 脱敏后的 refresh_token（前缀 + 尾 4 位），仅用于界面区分。
    token_hint: String,
    /// 最新一次的订阅额度快照（无请求记录时为 None）。
    quota: Option<store::QuotaSnapshot>,
    /// 最近一次被使用（转发请求）的时间戳（Unix 秒）；从未使用为 None。
    last_used: Option<i64>,
    /// 累计等价 API 费用（USD）。
    cost_total: f64,
    /// 当前 RPM：最近 60 秒经该账号转发的请求数（见 [`store::CredentialStore::recent_rpm`]）。
    ///
    /// 与 `quota.requests_5h/7d` 不同，它不依赖上游限流头——那两个要等一条带头的响应才刷新，
    /// 且窗口起点由 `reset` 反推，看不出「此刻压了多少」。
    rpm: i64,
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
    /// 由凭证 + 已绑定设备数 + 全局默认上限构造视图。
    fn new(c: &Credential, device_count: i64, defaults: DefaultLimits) -> Self {
        let secs = c.expires_in_secs();
        Self {
            id: c.id,
            label: c.label.clone(),
            tier: c.tier.clone(),
            org_type: c.org_type.clone(),
            priority: c.priority,
            disabled: c.disabled,
            expires_in: secs,
            expires_at: c.expires_at,
            expired: secs == 0,
            created_at: c.created_at,
            updated_at: c.updated_at,
            device_limit: c.device_limit,
            device_limit_effective: store::effective_device_limit(c.device_limit, defaults.device),
            device_count,
            rpm_limit: c.rpm_limit,
            rpm_limit_effective: store::effective_rpm_limit(c.rpm_limit, defaults.rpm),
            ban_reason: c.ban_reason.clone(),
            proxy: c.proxy.clone(),
            token_hint: mask_token(&c.refresh_token),
            quota: None,
            last_used: None,
            cost_total: 0.0,
            rpm: 0,
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

    /// 链式附加额度快照、最近使用时间、累计费用与当前 RPM。
    fn with_stats(
        mut self,
        quota: Option<store::QuotaSnapshot>,
        last_used: Option<i64>,
        cost_total: f64,
        rpm: i64,
    ) -> Self {
        self.quota = quota;
        self.last_used = last_used;
        self.cost_total = cost_total;
        self.rpm = rpm;
        self
    }
}

/// 脱敏：保留前缀（到第三个 `-`）与尾 4 位，中间用 `…` 省略。
fn mask_token(token: &str) -> String {
    let tail: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let prefix: String = token.splitn(4, '-').take(3).collect::<Vec<_>>().join("-");
    if prefix.is_empty() { format!("…{}", tail) } else { format!("{}-…{}", prefix, tail) }
}

// 错误详情在这两个构造器里记，而不是让每个 handler 自己写一行：管理接口的失败此前**只**
// 回给客户端、服务端一行不留，出了 500 在日志里根本查不到。方法与路径由
// [`log_api_failures`] 那层补，两边合起来才是完整的一次失败。
//
// 没用 `#[track_caller]` 带出调用位置：这两个函数大量以 `.map_err(internal)` 的函数指针形式
// 传递，reify 出的 shim 不透传 caller location，记出来的位置是错的，不如不记。
fn bad_request(msg: impl Into<String>) -> ApiError {
    let msg = msg.into();
    tracing::warn!(reason = %msg, "admin api rejected the request");
    (StatusCode::BAD_REQUEST, msg)
}
fn not_found() -> ApiError {
    (StatusCode::NOT_FOUND, "credential not found".into())
}

/// `/api/*` 失败响应的兜底日志：方法、路径、状态码。
///
/// 挂在鉴权中间件**外面**，所以 `require_admin` 直接回的 401 也会被记下——那是唯一能看出
/// 有人在猜管理密码的地方。成功的请求不记：管理接口的成功变更各自已有 `info!`，全记只是噪音。
///
/// 路径取 `OriginalUri` 而非 `uri()`：这一层在 `nest("/api", ..)` **里面**，`uri()` 已被剥掉
/// `/api` 前缀，直接记会得到 `/auth/login` 这种对不上真实请求的路径。
async fn log_api_failures(
    req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map(|u| u.path().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());
    let resp = next.run(req).await;
    let status = resp.status();
    if status.is_server_error() {
        tracing::error!(%method, %path, status = status.as_u16(), "admin api failed");
    } else if status.is_client_error() {
        tracing::warn!(%method, %path, status = status.as_u16(), "admin api failed");
    }
    resp
}
fn internal(e: impl std::fmt::Display) -> ApiError {
    let msg = e.to_string();
    tracing::error!(error = %msg, "admin api internal error");
    (StatusCode::INTERNAL_SERVER_ERROR, msg)
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
