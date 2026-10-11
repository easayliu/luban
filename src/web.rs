//! 网页服务：授权登录 + 多凭证管理的 JSON 接口，其余路径由内嵌前端 SPA 兜底。

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{any, delete, get, post},
};
use serde::{Deserialize, Serialize};

use crate::admin_ui;
use crate::auth::{self, Actor};
use crate::credentials::{
    Credential, PRIORITY_DEFAULT, PRIORITY_MAX, PRIORITY_MIN, priority_tiers_by_rank,
};
use crate::oauth::{self, PkceChallenge};
use crate::proxy;
use crate::proxy::AccountRejection;
use crate::store::{self, CredentialStore, Scope, UserRole};

mod billing;
mod credentials;
mod groups;
mod keepalive;
mod learned;
mod login;
mod metrics;
mod migrate;
mod pricing;
mod provision;
mod proxies;
mod reauth;
mod routes;
mod settings;
mod settings_update;
mod tasks;
mod usage;
mod users;
mod views;

use billing::*;
pub(crate) use credentials::MemberCaps;
use credentials::*;
use groups::*;
use keepalive::*;
use learned::*;
use login::*;
use metrics::*;
use migrate::*;
use pricing::*;
use provision::*;
use proxies::*;
use reauth::*;
use routes::*;
use settings::*;
use settings_update::*;
use tasks::*;
use usage::*;
use users::*;
use views::*;

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
    pkce: Arc<parking_lot::Mutex<PendingPkce>>,
    /// 凭证存储。
    pub store: Arc<CredentialStore>,
    /// `--api-key` / `LUBAN_API_KEY` 设的接入 Key；None 时只认库里的 Key，两边都没有就全部拒绝。
    pub client_key: Option<Arc<String>>,
    /// 管理密码（环境接管，明文；None 表示未由环境设置）。
    pub admin_env: Option<Arc<String>>,
    /// 只读访客密码（环境接管，明文；None 表示未由环境设置）。见 [`auth::Role::Viewer`]。
    pub viewer_env: Option<Arc<String>>,
    /// 首次设置管理密码的初始化口令：每次启动随机生成、重启前不变，未设密码时打进日志。
    /// 设密码必须带上它，本机也一样，见 [`auth::setup`]。
    pub setup_token: Arc<String>,
    /// 上游拒过的请求形态记忆表，用来在本地拦掉上游已经拒过一次的「模型 + 取值」组合
    /// （`effort: 'xhigh'`、`role: 'system'` 之类），不再白发一次。写穿落库、启动回填、
    /// 7 天保鲜，见 [`crate::proxy::ShapeMemory`]。
    pub shape_rejections: crate::proxy::ShapeMemory,
    /// 上游以 `deprecated` 拒过的「模型 + 字段」记忆表：学过之后转发前自动剥掉该字段，
    /// 客户端无需改动即可正常使用。持久化同上，见 [`crate::proxy::DeprecatedFieldMemory`]。
    pub deprecated_fields: crate::proxy::DeprecatedFieldMemory,
    /// 上游回过 **200 却零输出**的「模型 + 无 tools 单条消息 + `max_tokens`」，与上游**拒答**
    /// 过的「模型 + 提示词哈希」两格记忆表：学过之后同类 / 同一条提示词本地 403，不再白发一次、
    /// 也不再在上游留一条「问一句什么都没得到」或「又发了一遍被拒的内容」的记录。
    /// 持久化同上（`kind = "empty_reply"` / `"refusal"`），见 [`crate::proxy::EmptyReplyMemory`]。
    pub empty_replies: crate::proxy::EmptyReplyMemory,
    /// 拒绝日志的抑制表：撞上限的客户端往往每几十毫秒重试一次，一条不落地记会把日志刷没。
    /// 见 [`crate::proxy::RejectionLog`]。
    pub rejection_log: crate::proxy::RejectionLog,
    /// 瞬时限流的退避记忆表：同一条「账号 + 模型」路线连撞几次，交回客户端的 `retry-after`
    /// 就翻几倍。见 [`crate::proxy::TransientBackoff`]。
    pub transient_backoff: crate::proxy::TransientBackoff,
    /// 上游负载表：每条「账号 + 模型」路线此刻的在飞数，与每个账号最近一分钟的发送记录。
    /// 只为给裸 429（一个限流头都不带的那一档）留下能对上游限额的读数，见
    /// [`crate::proxy::UpstreamLoad`]。
    pub upstream_load: crate::proxy::UpstreamLoad,
    /// 每会话并发在途表：限制单个 session 同时在飞的请求数，防止 Claude Desktop 的
    /// cache 预热脉冲（20+ 条 `max_tokens=1`）瞬间打爆上游。见 [`crate::proxy::SessionConcurrency`]。
    pub session_concurrency: crate::proxy::SessionConcurrency,
    /// **在途请求数**：已进入转发入口、响应尚未走完的那些。
    ///
    /// 由 [`crate::proxy::InFlightGuard`] 增减，随响应流一起存活——流式回复要几十秒才走完，
    /// 只在 `handle` 返回时减一会把这类请求算成「瞬间就结束了」，并发数永远显示成 0～1。
    pub in_flight: Arc<std::sync::atomic::AtomicI64>,
    /// 逐请求遥测的汇聚点：转发路径在响应流结束时把每条 `/v1/messages` 的形态与用量交给它，
    /// 由后台任务按官方节奏攒批发出（见 [`crate::telemetry`]）。保活也从它取该凭证最近一次
    /// 见到的 `anthropic-organization-id`。
    pub telemetry: crate::telemetry::Telemetry,
    /// 转发 `/v1/*` 的上游地址（不带末尾 `/`）。线上恒为 [`crate::config::UPSTREAM_BASE_URL`]；
    /// 做成字段只为端到端测试能把它指到本地起的模拟上游，见 `proxy::handler` 的测试。
    pub upstream_base: Arc<str>,
}

impl AppState {
    /// [`Self::for_test`] 配的环境接入 Key。
    #[cfg(test)]
    pub(crate) const TEST_CLIENT_KEY: &'static str = "test-client-key";

    /// 测试用的最小状态：给定（内存）库，其余字段全取默认。
    ///
    /// `pkce` 是私有字段，crate 内别处的测试自己拼不出 [`AppState`]，而转发路径
    /// （[`crate::proxy::handle`]）的端到端用例要的正是一份能跑的状态。不出网——
    /// [`crate::clients::ClientPool::new`] 只是把出站客户端建起来。
    ///
    /// 带一把环境接入 Key（[`Self::TEST_CLIENT_KEY`]）：转发不带 Key 一律拒绝，端到端用例得带上它。
    #[cfg(test)]
    pub(crate) fn for_test(store: Arc<CredentialStore>) -> Self {
        Self {
            clients: std::sync::Arc::new(
                crate::clients::ClientPool::new().expect("测试用的出站客户端池建不起来"),
            ),
            pkce: Arc::new(parking_lot::Mutex::new(Vec::new())),
            store,
            client_key: Some(Arc::new(Self::TEST_CLIENT_KEY.into())),
            admin_env: None,
            viewer_env: None,
            setup_token: Arc::new("test-setup-token".into()),
            shape_rejections: Arc::default(),
            deprecated_fields: Arc::default(),
            empty_replies: Arc::default(),
            rejection_log: Arc::default(),
            transient_backoff: Arc::default(),
            upstream_load: Arc::default(),
            session_concurrency: Arc::default(),
            in_flight: Arc::default(),
            telemetry: Default::default(),
            upstream_base: crate::config::UPSTREAM_BASE_URL.into(),
        }
    }
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
    viewer_password: Option<String>,
) -> Result<()> {
    let client_key = api_key.map(Arc::new);
    let clients = std::sync::Arc::new(crate::clients::ClientPool::new()?);
    let state = AppState {
        clients,
        pkce: Arc::new(parking_lot::Mutex::new(Vec::new())),
        store: store.clone(),
        client_key: client_key.clone(),
        admin_env: admin_password.map(Arc::new),
        viewer_env: viewer_password.map(Arc::new),
        setup_token: Arc::new(auth::new_setup_token()),
        shape_rejections: Arc::default(),
        deprecated_fields: Arc::default(),
        empty_replies: Arc::default(),
        rejection_log: Arc::default(),
        transient_backoff: Arc::default(),
        upstream_load: Arc::default(),
        session_concurrency: Arc::default(),
        in_flight: Arc::default(),
        telemetry: Default::default(),
        upstream_base: crate::config::UPSTREAM_BASE_URL.into(),
    };

    spawn_background_tasks(&state).await;

    auth::sync_env_accounts(&state).await;

    // 未设管理密码时，启动日志里要给出初始化口令（`state` 下面会被 move 进路由）。
    let setup_token = (!auth::admin_configured(&state).await).then(|| state.setup_token.clone());

    let app = router(state);

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
            "Claude Code setup: ANTHROPIC_BASE_URL={base}, ANTHROPIC_AUTH_TOKEN=<an access key created in the console> (forwarding rejects every request until one exists)"
        ),
    }
    if let Some(token) = &setup_token {
        auth::log_setup_token(token);
        tracing::info!("or open {url}#setup_token={token} to have it filled in");
    }
    if open_browser {
        // 还没设密码：口令带在 `#` 后面，初始化页自动填好，本机开箱不用去翻日志。片段不会
        // 发给服务端，进不了访问日志；页面读完就把它从地址栏抹掉。
        let open_url = match &setup_token {
            Some(token) => format!("{url}#setup_token={token}"),
            None => url.clone(),
        };
        open_in_browser(&open_url);
        tracing::info!(url = %url, "tried to open the browser; if nothing appeared, open the url manually");
    }

    // `into_make_service_with_connect_info` 而不是直接交 `app`：登录失败要记来源，
    // 而对端地址只有这里能拿到（见 [`auth::client_ip`]）。
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("the web server exited unexpectedly")?;
    // 在途请求都结束了，它们的流水还在后台往库里写，等写完再退出。
    store.drain_writes(std::time::Duration::from_secs(5)).await;
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
mod tests;
