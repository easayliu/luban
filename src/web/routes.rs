//! 路由表：公开接口、需鉴权的管理接口、`/v1/*` 转发与前端 SPA 兜底。

use super::*;

/// 组装整张路由表；`state` 在这里 move 进去。
pub(super) fn router(state: AppState) -> Router {
    // 公开接口（无需登录）：鉴权状态机 + 给 New API 等下游拉取的价目表。
    let public = Router::new()
        .route("/auth/state", get(auth::state))
        .route("/auth/login", post(auth::login))
        .route("/auth/setup", post(auth::setup))
        .route("/pricing", get(get_pricing))
        .route("/ratio_config", get(get_ratio_config));

    // 需登录的接口（未设管理密码时一律 401）。谁能打哪条由中间件按路由判，默认只给 admin，
    // 见 [`auth::require_login`]。
    let protected = Router::new()
        .route("/authorize", get(authorize))
        .route("/exchange", post(exchange))
        .route("/credentials", get(list_credentials))
        .route("/credentials/priority", post(set_priorities))
        .route("/credentials/device-limit", post(set_device_limits))
        .route("/credentials/session-limit", post(set_session_limits))
        .route("/credentials/rpm-limit", post(set_rpm_limits))
        .route("/credentials/quota-pause-pct", post(set_quota_pause_pcts_many))
        .route("/credentials/disabled", post(set_disabled_many))
        .route("/credentials/delete", post(delete_credentials))
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_disabled))
        .route("/credentials/{id}/priority", post(set_priority))
        .route("/credentials/{id}/label", post(set_label))
        .route("/credentials/{id}/proxy", post(set_proxy))
        .route("/credentials/{id}/device-limit", post(set_device_limit))
        .route("/credentials/{id}/session-limit", post(set_session_limit))
        .route("/credentials/{id}/rpm-limit", post(set_rpm_limit))
        .route("/credentials/{id}/quota-pause-pct", post(set_credential_quota_pause_pct))
        .route("/credentials/{id}/devices", get(list_credential_devices))
        .route("/credentials/{id}/usage", get(list_credential_usage))
        .route("/credentials/{id}/stats", get(get_credential_stats))
        .route("/credentials/{id}/devices/{device_id}", delete(unbind_credential_device))
        .route(
            "/credentials/{id}/sessions",
            get(list_credential_sessions).delete(clear_credential_sessions),
        )
        .route("/credentials/{id}/sessions/{session_key}", delete(unbind_credential_session))
        .route("/credentials/{id}/sessions/{session_key}/events", get(list_session_events))
        .route("/credentials/{id}/slots/{slot}/events", get(list_slot_events))
        .route("/credentials/{id}/refresh", post(refresh_credential))
        .route("/credentials/{id}/reauthorize", post(reauthorize_credential))
        .route("/credentials/{id}/test", post(test_credential))
        .route("/credentials/{id}/cooldown", delete(clear_cooldown))
        .route("/models", get(list_models))
        .route("/learned-rejections", get(list_learned_rejections).delete(clear_learned_rejections))
        .route("/learned-rejections/delete", post(forget_learned_rejection))
        .route("/learned-rejections/delete-group", post(forget_learned_group))
        .route("/credentials/proxy", post(set_proxies))
        .route("/proxies", get(list_saved_proxies).post(add_saved_proxy))
        .route("/proxies/test", post(test_proxy))
        .route("/proxies/delete", post(delete_saved_proxies))
        .route("/proxies/batch", post(add_saved_proxies))
        .route("/proxies/{id}", post(update_saved_proxy).delete(delete_saved_proxy))
        .route("/usage", get(list_usage))
        .route("/ban-events", get(list_ban_events))
        .route("/ban-events/{id}/logs", get(list_ban_event_logs))
        .route("/metrics", get(get_metrics))
        .route("/metrics/cache-series", get(get_cache_series))
        .route("/metrics/ttft-series", get(get_ttft_series))
        .route("/metrics/breakdown", get(get_usage_breakdown))
        .route("/metrics/rejections", get(get_rejections))
        .route("/settings", get(get_settings))
        .route("/settings/device-ttl", post(set_device_ttl))
        .route("/settings/device-retention", post(set_device_retention))
        .route("/settings/session-ttl", post(set_session_ttl))
        .route("/settings/session-retention", post(set_session_retention))
        .route("/settings/default-device-limit", post(set_default_device_limit))
        .route("/settings/default-session-limit", post(set_default_session_limit))
        .route("/settings/default-rpm-limit", post(set_default_rpm_limit))
        .route("/settings/device-rpm-limit", post(set_device_rpm_limit))
        .route("/settings/session-rpm-limit", post(set_session_rpm_limit))
        .route("/settings/session-concurrency-limit", post(set_session_concurrency_limit))
        .route("/settings/bare-rate-limit", post(set_bare_rate_limit))
        .route("/settings/rate-limit-retry-max", post(set_rate_limit_retry_max))
        .route("/settings/quota-pause-pct", post(set_quota_pause_pct))
        .route("/settings/require-device-id", post(set_require_device_id))
        .route("/settings/min-client-version", post(set_min_client_version))
        .route("/settings/latest-cc-release", post(set_latest_cc_release))
        .route("/settings/oauth-scopes", post(set_oauth_scopes))
        .route("/settings/forwarding", post(set_forwarding))
        .route("/export", get(export))
        .route("/import", post(import))
        .route("/auth/password", post(auth::change_password))
        .route("/auth/me", get(auth::me))
        .route("/auth/viewer-password", post(auth::set_viewer_password))
        .route("/auth/logout", post(auth::logout))
        .route("/billing", get(get_billing))
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/{id}", post(update_group).delete(delete_group))
        .route("/groups/{id}/grants", post(set_group_grants))
        .route("/credentials/groups", post(set_credentials_groups))
        .route("/credentials/{id}/groups", post(set_credential_groups))
        .route("/api-keys", get(list_api_keys).post(create_api_key))
        .route("/api-keys/{id}", post(update_api_key).delete(delete_api_key))
        .route("/api-keys/{id}/reveal", get(reveal_api_key))
        .route("/provision-keys", get(list_provision_keys).post(create_provision_key))
        .route("/provision-keys/{id}", post(update_provision_key).delete(delete_provision_key))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}", delete(delete_user))
        .route("/users/{id}/password", post(set_user_password))
        .route("/users/{id}/disabled", post(set_user_disabled))
        .route("/users/{id}/parent", post(set_user_parent))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_login));

    // 失败的请求补一行「哪个方法打了哪条路径、回了几」。错误详情由 `internal`/`bad_request`
    // 各自记，方法与路径它们看不到，只能在这一层补——两行合起来才定位得到一次失败。
    let api = public.merge(protected).layer(middleware::from_fn(log_api_failures));

    // `/api/*` 管理接口；`/v1/*` 转发到官方 API；其余由内嵌前端 SPA 兜底。
    Router::new()
        .nest("/api", api)
        // axum 对 `Bytes` 提取器默认限 2MB，超过的请求进不了 handler 就被 413 拦掉——
        // 而上游官方 /v1/messages 的上限是 32MB，长对话/带附件的合法请求很容易超 2MB。
        // 这里放到 64MB 留出余量，真正的大小判决交给上游；管理接口维持默认即可。
        .route("/v1/{*path}", any(proxy::handle).layer(DefaultBodyLimit::max(64 * 1024 * 1024)))
        // 个别移动端/前置层会以 POST 打开首页；用 PRG 把最终文档历史落成 GET。
        .route(
            "/",
            get(admin_ui::fallback)
                .post(admin_ui::redirect_root_post)
                .layer(admin_ui::compression()),
        )
        // SPA 只允许由 GET/HEAD 打开。若把 POST 也兜底成 index.html，浏览器会把页面
        // 记作表单提交结果，之后在移动端刷新便弹出“确认重新提交表单”。
        .fallback_service(get(admin_ui::fallback).layer(admin_ui::compression()))
        .with_state(state)
}
