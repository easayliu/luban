use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};

use crate::config;
use crate::store;
use crate::web::AppState;

use super::ban::{
    AccountRejection, classify_account_rejection, is_third_party_rejection,
    park_org_oauth_disallowed, parse_upstream_error,
};
use super::body::{
    OutboundIdentity, THINKING_MIN_MAX_TOKENS, ensure_beta_query, outbound_identity,
    rewrite_body_out, sim_device_fingerprint, ua_of, with_outbound_identity,
};
use super::headers::{build_forward_headers_for, orig_header_case};
use super::learned_rules::is_max_plan;
use super::logging::{ban_context, shape_summary, spawn_usage_log};
use super::rate_limit::{
    LimitScope, RateLimitInfo, park_if_quota_nearly_exhausted, park_rate_limited,
    rate_limit_scope_for,
};
use super::session_id::session_id_for;
use super::session_link::{CcRequestKind, CcSessionLink};
use super::simulation::{Simulation, SimulationReason, cc_profile_for, cc_system_base};
use super::upstream::{
    Aggregated, SseAggregator, Upstream, error_chain, resp_shape, upstream_error_kind,
};
use super::{QUOTA_PROBE_MODEL, UsageSniffer, header_opt, new_request_id};

// ---------- 连通性测试 ----------

/// 一次连通性测试最多等多久。上游客户端本身没设超时（流式响应可以跑很久），但测试是人在
/// 网页上等着的：它只发一条 `max_tokens=1` 的请求，正常几百毫秒就该回来，超过这个数就是
/// 上游不通或被中间设备吞了，报出来比让页面一直转圈有用。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 一次连通性测试的结果（[`crate::web`] 原样 JSON 回给前端）。
#[derive(serde::Serialize)]
pub struct ProbeReport {
    /// 测试是否通过：上游 2xx **且**拿到了一条完整有效的 Message（见 [`probe_report`]）。
    /// 2xx 之后流里带错误事件、流断在半路、响应体不是 Message，都算不通过。
    pub ok: bool,
    /// 上游 HTTP 状态码；**`0` 表示请求根本没到上游**（取 token 失败、连不上、超时），
    /// 此时原因在 `error` 里。
    pub status: u16,
    /// 从「开始发」到「响应体读完」的耗时（毫秒）。请求没发出去时是失败前的耗时。
    pub latency_ms: u128,
    /// 上游实际回报的模型名（成功时才有）。可能与请求的不同——别名会在上游解析成具体版本，
    /// 这正是「这个模型名到底指向什么」的答案。
    pub model: Option<String>,
    /// 上游错误类型（`error.type`，如 `rate_limit_error`/`permission_error`）。
    pub error_type: Option<String>,
    /// 失败原因原文（上游 `error.message`，解析不出就是整段响应体 / luban 侧的错误链）。
    pub error: Option<String>,
    /// 本次响应的限流头快照；请求没到上游、或响应压根没带这些头时为 `None`。
    pub quota: Option<ProbeQuota>,
}

/// 一次测试从上游限流头读到的额度快照。
///
/// 字段名与 [`store::QuotaSnapshot`] 对齐（前端两处共用同一套读法），但**少了窗口花费和
/// 请求数**：这些值要由后端按 `usage_logs` 聚合，单次响应的限流头里没有。
///
/// 这份读数同时会随用量日志落库（见 [`log_probe_usage`]），所以卡片上的额度也跟着更新——
/// 弹窗与卡片显示的是同一次读数，不会一个新一个旧。
#[derive(serde::Serialize)]
pub struct ProbeQuota {
    /// `anthropic-ratelimit-unified-status`（如 `allowed`/`allowed_warning`/`rejected`）。
    pub unified_status: Option<String>,
    pub rl_5h_utilization: Option<f64>,
    pub rl_5h_reset: Option<i64>,
    pub rl_7d_utilization: Option<f64>,
    pub rl_7d_reset: Option<i64>,
    /// `…-representative-claim`：上游认为「当前是哪个窗口在管事」。
    pub rl_representative: Option<String>,
    /// `retry-after`（秒）。只有 429 才有，且它是**这次拒绝**给出的等待时间，比各窗口的
    /// reset 更直接（实测给过 63 小时，直指 7 天窗口的重置时刻）。
    pub retry_after_secs: Option<i64>,
    /// 本次请求是否动用了 **usage credits**（`…-overage-in-use`）：套餐额度满了但照样 200，
    /// 烧的是按量计费的钱。
    pub overage_in_use: Option<bool>,
}

impl ProbeQuota {
    /// 从已解析的限流头构造；一个字段都没有时返回 `None`。
    ///
    /// CDN 拦截页、网关错误那类响应压根不带这些头，给前端一坨全 `null` 的对象，它就得自己
    /// 再判一遍「这些是不是全空」——不如在这里说清楚「没有」。
    pub(super) fn from_info(info: &RateLimitInfo) -> Option<Self> {
        let q = Self {
            unified_status: info.unified_status.clone(),
            rl_5h_utilization: info.five_h_utilization,
            rl_5h_reset: info.five_h_reset,
            rl_7d_utilization: info.seven_d_utilization,
            rl_7d_reset: info.seven_d_reset,
            rl_representative: info.representative.clone(),
            retry_after_secs: info.retry_after,
            overage_in_use: info.overage_in_use,
        };
        let empty = q.unified_status.is_none()
            && q.rl_5h_utilization.is_none()
            && q.rl_5h_reset.is_none()
            && q.rl_7d_utilization.is_none()
            && q.rl_7d_reset.is_none()
            && q.rl_representative.is_none()
            && q.retry_after_secs.is_none()
            && q.overage_in_use.is_none();
        (!empty).then_some(q)
    }
}

impl ProbeReport {
    /// 请求没到上游（或没读到响应）时的结果：状态码留 0，原因写进 `error`。
    fn failed(latency_ms: u128, error: String) -> Self {
        Self {
            ok: false,
            status: 0,
            latency_ms,
            model: None,
            error_type: None,
            error: Some(error),
            quota: None,
        }
    }
}

/// 用**指定**凭证向上游发一条最小请求，测这个账号能不能用这个模型。
///
/// 与转发路径的两处刻意不同：
///
/// 1. **不选号**：走 [`store::access_token_of`] 直接取这一个凭证的 token
///    （[`store::valid_access_token_for_device`] 会按负载均衡挑号，那测出来的就不是它了），
///    也因此不写设备绑定、不占 `device_limit` 名额、不计裸请求限流。停用/封禁的号照样能测——
///    「它是不是已经恢复了」正是要问的问题。
/// 2. **形态开关一律按默认全开**（[`store::ForwardFlags::default`]），不读库里那份配置：
///    测试要回答的是「这个账号 + 这个模型通不通」，掺进用户自己拨过的开关，失败时就分不清
///    是账号的问题还是配置的问题了。于是这里恒定发一条**官方形态**的请求，作为基准。
///
/// 而**账号状态照真实流量的口径更新**：这条请求是真实的——真花额度、拿到的也是上游此刻
/// 的真实判决，429 就该打冷却（同一套 [`rate_limit_scope`] 分格）、命中封号特征就该
/// [`store::CredentialStore::record_ban`]、刷新时发现 `refresh_token` 被作废亦然（见
/// [`store::access_token_of`]）。否则测试报了「已封禁」而卡片上一切如常，两边各说各话，
/// 用户还得自己动手把号停掉。唯一仍与转发不同的是**不换号重试**——测的就是这一个，
/// 换了号结论就不是它的了。冷却与转发共用 `rate_limit_retry` 那个开关（关掉即两边都
/// 退回「只透传不冷却」）；注意这里读的是**库里真实配置**而非上面那份全开的形态开关——
/// 形态按基准发、状态按真实规则记，两件事各归各。
///
/// 但它**照常写一条用量日志**（[`log_probe_usage`]）：卡片上的额度快照与累计花费都出自
/// `usage_logs`，不写就等于「测出来的额度只在弹窗里存在」，而这条请求真的花了钱、也真的
/// 拿到了此刻最新的限流头。日志里那条以 `device_id = "probe"` 标出，与真实流量可区分。
///
/// 代价是它**真的会消耗一点订阅额度**：请求带官方 `system` 基座（opus 族约 300 token、
/// sonnet 族约 2700），与真实流量共用同一份 1h 全局缓存前缀，稳定后走缓存读价。
pub async fn probe(
    state: &AppState,
    cred: &crate::credentials::Credential,
    model: &str,
) -> ProbeReport {
    let started = std::time::Instant::now();
    // 一个 deadline 覆盖取/刷新 token、发送请求和读完响应体。只给 send() 套 timeout 不够：
    // 上游若只回响应头却不结束 body，或 token 刷新卡住，前端 mutation 会永远 pending。
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    let token = if cred.needs_refresh() {
        // 刷新会轮换 refresh_token，不能把 refresh future 直接放进 timeout：上游若已经轮换、
        // 本地却在 update_tokens 前被取消，旧 token 就作废且新 token 永久丢失。独立任务的
        // JoinHandle 即使因等待超时被丢弃，任务仍会继续跑完并落库；页面只是不再一直等它。
        let refresh_store = state.store.clone();
        let refresh_clients = state.clients.clone();
        let refresh_cred = cred.clone();
        let refresh = tokio::spawn(async move {
            store::access_token_of(&refresh_store, &refresh_clients, &refresh_cred).await
        });
        match tokio::time::timeout_at(deadline, refresh).await {
            Ok(Ok(Ok(t))) => t,
            Ok(Ok(Err(e))) => {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    model,
                    error = %e,
                    "connectivity test: getting an access_token failed"
                );
                return ProbeReport::failed(
                    started.elapsed().as_millis(),
                    format!("failed to get a token: {e}"),
                );
            }
            Ok(Err(e)) => {
                tracing::error!(
                    cred_id = cred.id, cred = %cred.label,
                    model,
                    error = %e,
                    "connectivity test: the token refresh task died"
                );
                return ProbeReport::failed(
                    started.elapsed().as_millis(),
                    format!("the token refresh task died: {e}"),
                );
            }
            Err(_) => {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    model,
                    timeout_secs = PROBE_TIMEOUT.as_secs(),
                    "connectivity test: getting an access_token timed out, the refresh task continues in the background"
                );
                return ProbeReport::failed(
                    started.elapsed().as_millis(),
                    format!(
                        "connectivity test timed out (overall cap {}s): the token refresh continues in the background",
                        PROBE_TIMEOUT.as_secs()
                    ),
                );
            }
        }
    } else {
        cred.access_token.clone()
    };

    // 复用「裸客户端」那份设备指纹，不另造一个：指纹只用于派生伪装 device_id，每加一份就
    // 等于给这个账号在上游多一台设备，而测试并不需要一个自己的身份。取模拟路径那份
    // （[`sim_device_fingerprint`]）：这条测试走模拟路径，平台段与 UA 段都是它发出去的那套。
    let device_fp = sim_device_fingerprint(None);
    let flags = store::ForwardFlags::default();
    // 直接构造 `Simulation` 而不走 `Simulation::detect`：这条请求本来就是 luban 自己发的裸
    // 请求（body 里没有那句身份声明），detect 只会在开关关掉时返回 None，那样发出去必被上游拒。
    let sim = probe_simulation(cred, model);
    let headers = build_forward_headers_for(
        &HeaderMap::new(),
        &token,
        flags,
        Some(&sim),
        None,
        Some(model),
        false,
        super::headers::BetaCtx::MAIN,
    );
    // 出站 UA 要随日志落库（入站那份没有——测试不来自任何客户端）。在 headers 被 move 进
    // Upstream 之前取，取值规则与转发路径同一套。
    let out_ua = ua_of(&headers);
    let probe_request_id = new_request_id();
    let upstream = Upstream {
        _state: std::marker::PhantomData,
        // 测试也走这个号自己的代理——不然「测通了」测的是直连那条路，与真实转发不是一回事。
        // 代理建不出来同样标记禁用：与转发/刷新/保活口径一致，坏代理不留在池里。
        client: match state.clients.for_credential(cred) {
            Ok(c) => c,
            Err(e) => {
                let reason = format!("[proxy] {e:#}");
                let _ = state.store.record_ban(
                    cred.id,
                    &store::BanContext {
                        reason: reason.clone(),
                        source: "proxy",
                        error_message: Some(format!("{e:#}")),
                        request_id: Some(probe_request_id.clone()),
                        ..Default::default()
                    },
                );
                return ProbeReport::failed(started.elapsed().as_millis(), format!("{e:#}"));
            }
        },
        method: Method::POST,
        // 这条请求整条都是照官方形态造的，URL 上那个 `?beta=true` 一并带上。
        url: ensure_beta_query(&format!("{}/v1/messages", config::UPSTREAM_BASE_URL)),
        headers,
        flags,
        billable: true,
        sim: Some(sim),
        // 走的是模拟那条路（sim 恒为 Some），会话 id 在 Simulation 里，出站两处也都取它。
        bare_session: None,
        session_out: None,
        // 连通性测试保持非流式：它下面那套读法（`up.bytes()` 一把梭 + [`probe_report`] 按
        // 整段 Message 解析出 model/error_type）是照非流式响应写的，改成 SSE 就全得跟着改，
        // 而这条请求本来就不是客户端流量（`max_tokens:1` 的 ping），形态对齐的收益也不在这。
        force_stream: false,
        // 探测体不带 `tools`（见 [`probe_body`]），没有可混淆的名字。
        tool_names: None,
        // 同上：模拟路径的链在 `sim` 里。
        client_link: None,
        // 连通性测试自己造 body，形态由 profile 定，不必再判一次。
        cc_kind: CcRequestKind::Main,
        // 探测不补 fallbacks：它要测的是这个号在**这个模型**上通不通，换模型作答等于没测。
        refusal_fallbacks: None,
    };

    let body = probe_body(model);
    let shaped = upstream.shape(&body, cred, &device_fp);
    let plog = ProbeLog {
        store: &state.store,
        cred,
        req_model: model,
        started: &started,
        out_ua: (out_ua != "-").then_some(out_ua),
        request_id: probe_request_id.clone(),
        sent: shaped.clone(),
    };
    let sent = upstream.send(shaped);
    // 全部匹配到的限流头原文，只进日志不进 JSON：结构化的那几项已经够前端展示，而排查时
    // 「上游到底回了哪些头」得看原样的一整串。请求没到上游时留空。
    let mut ratelimit_raw = String::new();
    let report = match tokio::time::timeout_at(deadline, sent).await {
        Err(_) => ProbeReport::failed(
            started.elapsed().as_millis(),
            format!(
                "connectivity test timed out (overall cap {}s): still waiting on the upstream response",
                PROBE_TIMEOUT.as_secs()
            ),
        ),
        Ok(Err(e)) => ProbeReport::failed(
            started.elapsed().as_millis(),
            format!("upstream request failed [{}]: {}", upstream_error_kind(&e), error_chain(&e)),
        ),
        Ok(Ok(up)) => {
            let status = up.status();
            // 限流头必须在 `bytes()` 之前读——它会把整个响应消费掉，之后就没有头可看了。
            // 200 与 429 都带这组头，后者尤其有用：能直接看出是哪个窗口满了、要等多久。
            let info = RateLimitInfo::from_headers(up.headers());
            ratelimit_raw = info.raw.clone();
            let quota = ProbeQuota::from_info(&info);
            // `content-encoding` 同样得在消费响应前看；解不开的编码下 body 是乱码字节，
            // 封号判定必须跳过（与转发路径同一条宁漏勿误的规则）。
            let (is_sse, content_encoding) = resp_shape(&up);
            let compressed = content_encoding.is_some();
            // `up.bytes()` 会吃掉 `up`，上游的 request-id 先取走给流水用。
            let upstream_request_id = header_opt(up.headers(), "request-id");
            // 429 照真实流量打冷却。开关读库里真实配置（形态那份 flags 是恒定全开的基准，
            // 与「要不要管 429」无关）；与转发一样，重试次数配成 0 也视同关闭。
            let probe_scope = (status == StatusCode::TOO_MANY_REQUESTS)
                .then(|| rate_limit_scope_for(&info, Some(model), is_max_plan(cred)));
            // 测试通过时要不要照真实判决恢复限流那几档，见下面 `report.ok` 那段。
            let mut resume_rate_limited = false;
            let rate_limit_handling = state.store.forward_flags().rate_limit_retry
                && state.store.rate_limit_retry_max() > 0;
            if let Some(LimitScope::Unsupported(_)) = &probe_scope
                && rate_limit_handling
            {
                let reason = info.plan_denial_reason();
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    model,
                    ratelimit = %info.raw,
                    "connectivity test: this account's plan does not include the model, remembering that"
                );
                if let Err(e) = state.store.deny_model(cred.id, model, &reason, info.unified_reset)
                {
                    tracing::error!(cred_id = cred.id, error = %e, "persisting the model denial failed");
                }
            } else if let Some(scope) = probe_scope
                && !matches!(scope, LimitScope::Unsupported(_))
                && rate_limit_handling
            {
                let cooldown = info.cooldown_for(&scope);
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    model,
                    scope = scope.label(),
                    cooldown_secs = cooldown.as_secs(),
                    ratelimit = %info.raw,
                    "connectivity test hit an upstream 429, taking the credential out of the pool"
                );
                // 连通性测试是人在网页上点出来的**单发**探活，不参与连撞计数：一次手动
                // 探活撞上一阵拥堵，不该把这个号判成「这条路线走不通」。
                park_rate_limited(&state.store, cred, &scope, cooldown, false);
            // 200 也可能是「就差最后一点额度」：阈值机制在这里先过一道（见
            // [`park_if_quota_nearly_exhausted`]）。它把号停下时整条恢复分支**都不走**
            // ——否则一次手动探活会把刚按阈值停掉的号放回池子，下一条真实请求再停一次。
            } else if status.is_success()
                && !park_if_quota_nearly_exhausted(&state.store, cred, &info)
            {
                // 恢复留到整条回复读完、报告确实 ok 之后，见下面 `report.ok` 那段。
                resume_rate_limited = true;
            }
            match tokio::time::timeout_at(deadline, up.bytes()).await {
                // 已拿到真实状态码与限流头，只是 body 没有结束；保留这些信息并照样落一条日志。
                Err(_) => {
                    plog.record(status, &Bytes::new(), &info, upstream_request_id.as_deref());
                    ProbeReport {
                        ok: false,
                        status: status.as_u16(),
                        latency_ms: started.elapsed().as_millis(),
                        model: None,
                        error_type: None,
                        error: Some(format!(
                            "reading the upstream response body timed out (overall cap {}s)",
                            PROBE_TIMEOUT.as_secs()
                        )),
                        quota,
                    }
                }
                // 响应体读到一半断了：状态码与限流头都是真的，只是内容不完整，如实报出来。
                // 这一条同样落日志——额度快照来自头，不依赖 body。
                Ok(Err(e)) => {
                    plog.record(status, &Bytes::new(), &info, upstream_request_id.as_deref());
                    ProbeReport {
                        ok: false,
                        status: status.as_u16(),
                        latency_ms: started.elapsed().as_millis(),
                        model: None,
                        error_type: None,
                        error: Some(format!("failed to read the upstream response body: {e}")),
                        quota,
                    }
                }
                Ok(Ok(bytes)) => {
                    // 主线程形态的探活是流式的（官方主线程恒为 `stream:true`），回来的是
                    // SSE。把它攒回一条整段 Message，后面那套读法（封号判定、
                    // [`probe_report`] 解 model/error_type）就不必分两种。攒不出来时退回
                    // 原始字节——错误响应本来就是整段 JSON，不走 SSE。
                    let (bytes, stream_end) = if compressed {
                        (bytes, ProbeStreamEnd::Undecodable)
                    } else if is_sse {
                        aggregate_probe_sse(&bytes)
                    } else {
                        (bytes, ProbeStreamEnd::NotStreamed)
                    };
                    plog.record(status, &bytes, &info, upstream_request_id.as_deref());
                    // 命中封号特征照真实流量停用：判定器与转发共用同一个（含 401 裸响应、
                    // 「端点不支持」豁免那些规则），测试报出「已封禁」的同时卡片也变红，
                    // 而不是弹窗里一个结论、列表里另一个。
                    let verdict = if compressed {
                        AccountRejection::Other
                    } else {
                        classify_account_rejection(status, &bytes)
                    };
                    if let AccountRejection::Ban(reason) = verdict {
                        tracing::warn!(
                            cred_id = cred.id, cred = %cred.label,
                            status = status.as_u16(),
                            reason = %reason,
                            "connectivity test detected an account-level error, auto-disabling the credential"
                        );
                        let ctx = ban_context(
                            &reason,
                            "probe",
                            status,
                            &bytes,
                            &plog.request_id,
                            upstream_request_id.as_deref(),
                        );
                        if let Err(e) = state.store.record_ban(cred.id, &ctx) {
                            tracing::warn!(error = %e, "failed to auto-disable the credential");
                        }
                    } else if verdict == AccountRejection::SubscriptionInactive {
                        // 与转发同一口径：订阅还没生效，暂停调度（已暂停的不重写）。
                        park_org_oauth_disallowed(&state.store, cred, status.as_u16(), "probe");
                    }
                    let report = probe_report(
                        status,
                        &bytes,
                        stream_end,
                        started.elapsed().as_millis(),
                        quota,
                    );
                    if report.ok {
                        settle_passing_probe(&state.store, cred, model, &info, resume_rate_limited);
                    }
                    report
                }
            }
        }
    };

    tracing::info!(
        cred_id = cred.id, cred = %cred.label,
        model,
        ok = report.ok,
        status = report.status,
        latency_ms = report.latency_ms,
        error = %report.error.as_deref().unwrap_or("-"),
        ratelimit = %ratelimit_raw,
        request_id = %probe_request_id,
        "connectivity test"
    );
    report
}

/// 一个会话在本进程里的起点，供 [`spawn_session_handshake`] 用。
///
/// 模拟路径与真实 CC 路径都能给出这三样，只是来源不同：前者取 [`Simulation`]，
/// 后者取 [`client_session_link`] 那条链 + 客户端自报的版本。
pub(super) struct SessionStart<'a> {
    pub(super) session_id: &'a str,
    /// 写进遥测身份的客户端版本。模拟路径取 profile 的，真实 CC 取它自报的那个。
    pub(super) version: String,
    pub(super) prompt_id: &'a str,
    /// 这条来访走的是模拟路径吗。决定要不要补无鉴权的公共请求，
    /// 见 [`crate::oauth::HandshakeRunner::public_traffic`]。
    pub(super) simulated: bool,
}

/// 从两条路里取会话起点；不是新会话（或这条请求不在链上）时 `None`。
pub(super) fn session_start<'a>(
    sim: Option<&'a Simulation>,
    client_link: Option<&'a (String, CcSessionLink)>,
    client_version: &str,
) -> Option<SessionStart<'a>> {
    if let Some(s) = sim {
        return s.link.first_seen.then(|| SessionStart {
            session_id: &s.session_id,
            version: s.profile.version.to_string(),
            prompt_id: s.link.prompt_id.as_deref().unwrap_or_default(),
            simulated: true,
        });
    }
    let (sid, link) = client_link?;
    link.first_seen.then(|| SessionStart {
        session_id: sid,
        // 真实 CC 用**它自报的**版本：这个会话在上游那边就是那个版本的客户端在跑，
        // 握手事件里报 luban 自己那个版本等于把两个版本混进同一个会话。
        version: client_version.to_string(),
        prompt_id: link.prompt_id.as_deref().unwrap_or_default(),
        simulated: false,
    })
}

/// 替一个**新会话**在后台跑一遍官方启动握手（policy_limits / settings / eval /
/// penguin / MCP registry / bootstrap …，见 [`crate::oauth::HandshakeRunner`]）。
///
/// **模拟与真实 CC 两条路都发。** 曾经只给模拟路径发，理由是「真实 CC 自己会打这一串」
/// ——那是错的。要对齐形态的是 **luban 这张凭证**在上游眼里的样子；客户端自己那串（如果
/// 它真的登录过）走的是**它自己的账号**，luban 这张凭证一条都收不到。
/// `cap/2.1.258-api` 正好证到：那台机器的 bootstrap/penguin 带的是它自己的
/// `sk-ant-oat01-…`，而经 luban 转发的那一侧**没有** policy_limits / settings / eval /
/// mcp_servers / code_triggers / metrics / 额度探测。于是「一个只发 messages、从不拉策略
/// 配置的 OAuth 账号」本身就是判据。
///
/// **只在会话的第一条请求上发一次**（[`CcSessionLink::first_seen`]，键含 cred_id，
/// 见 [`CC_SESSIONS`]）。同一会话后续请求、以及 30 分钟一跳的保活循环，都不会再走到
/// 这里——保活那边发的是它自己那份周期端点，两者的触发条件不重叠：保活是「按时间」，
/// 这里是「按新会话」，撞在一起时由 `oauth::handshake_recent` 拦一道。
pub(super) async fn spawn_session_handshake(
    state: &crate::web::AppState,
    cred: &crate::credentials::Credential,
    token: &str,
    sent: &Bytes,
    start: SessionStart<'_>,
    out: &HeaderMap,
    model: Option<&str>,
) {
    let Ok(client) = state.clients.for_credential(cred) else { return };
    let display_model = model.unwrap_or("claude-sonnet-5").to_string();
    // 身份取**这条主请求实际发出去的那份**，见 [`outbound_identity`]。
    let ident = outbound_identity(sent, cred);
    let snapshot = crate::telemetry::SessionSnapshot {
        session_id: start.session_id.to_string(),
        device_id: ident.device_id.clone(),
        account_uuid: ident.account_uuid.clone(),
        version: start.version.clone(),
        model: display_model.clone(),
        // 会话级 beta 从**实际发出的**头上筛，与遥测那边同一套口径。
        betas: crate::telemetry::session_betas(
            out.get("anthropic-beta").and_then(|v| v.to_str().ok()).unwrap_or_default(),
        ),
        prompt_id: start.prompt_id.to_string(),
        started_wall: std::time::SystemTime::now(),
    };
    let h = crate::telemetry::Handshake {
        snapshot,
        // bootstrap 的 `model=` 是规范名，去掉展示名里的 `[1m]`。
        model: display_model.split('[').next().unwrap_or(&display_model).to_string(),
    };
    let org = state.telemetry.org_uuid(cred.id);
    // 无鉴权的公共请求（mcp-registry / downloads）只给模拟客户端补：真实 CC 自己就在发
    // 那几条（`cap/2.1.258-api` 里它们是客户端直连打的，不经 luban），再补一遍就是同一台
    // 机器把同一批公共端点打了两遍。见 [`crate::oauth::HandshakeRunner::public_traffic`]。
    let simulated = start.simulated;
    let runner = crate::oauth::HandshakeRunner::new(cred, h, org, simulated);

    // 整串握手跑在**一个**后台任务里，严格按抓包的先后来：
    //
    // ```text
    // policy_limits + settings（并发） → eval → 额度探测 → 收尾那批
    // ```
    //
    // **必须是同一个任务。** 拆成「lead 一个任务 + 限时等它 + 再 spawn rest」看着等价，
    // 其实不是：等超时之后 rest 就发了，而 lead 可能还卡在 policy/settings 上，于是线上
    // 顺序变成 `policy/settings → rest → eval → quota`——抓包里 eval 与额度探测是排在
    // penguin/mcp/bootstrap 那批**之前**的。串在一个任务里，这个交错构造不出来。
    //
    // 调用方只**限时等一个信号**（额度探测发完），拿到或超时都继续发主请求：
    //
    // - 拿到 → 与抓包一致（那四条都在首条 messages 之前，+0/+1/+1442/+1648ms，
    //   主请求 +2492ms）；
    // - 超时 → 只是不再等，任务照跑到底。**不能用 `timeout` 直接套 future**：那会把它
    //   整个 drop 掉、正在飞的请求当场取消，而 `note_handshake` 已经把这张凭证标成
    //   「刚握过手」、保活也会跳过自己那份——两头都不发，这个会话的启动流量就整个没了。
    let (probe_done, wait_probe) = tokio::sync::oneshot::channel::<()>();
    let client_for_task = client.clone();
    let token_for_task = token.to_string();
    let cred_for_task = cred.clone();
    let session_id = start.session_id.to_string();
    let probe_version = start.version.clone();
    tokio::spawn(async move {
        handshake_sequence(
            runner.lead(&client_for_task, &token_for_task),
            send_quota_probe(
                &client_for_task,
                &token_for_task,
                &cred_for_task,
                &ident,
                &probe_version,
                &session_id,
            ),
            runner.rest(&client_for_task, &token_for_task),
            probe_done,
        )
        .await;
        // downloads 那两条离会话起点很远（+9.7s / +125s），且**无鉴权**——跟 rest 挤在
        // 同一秒发完就是把两条本该稀稀拉拉的后台请求塞进了启动风暴。只给模拟客户端补：
        // 真实 CC 自己就在发。见 [`crate::oauth::HandshakeRunner::downloads`]。
        if simulated {
            runner.downloads(&client_for_task).await;
        }
    });

    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(config::HANDSHAKE_LEAD_TIMEOUT_MS),
        wait_probe,
    )
    .await;
    if waited.is_err() {
        tracing::debug!(
            cred_id = cred.id,
            "session handshake lead is still running past the cap; sending the first request \
             anyway (the handshake keeps going in order in the background)"
        );
    }
}

/// 串行跑完握手三段，并在**额度探测发完之后**发出放行信号。
///
/// 抽成一个函数是为了让「顺序」本身可测（[`tests::handshake_runs_in_capture_order`]）——
/// 这段代码唯一的职责就是顺序，而顺序错了不会有任何编译期或运行期症状，只会在上游那边
/// 看到一个真实客户端不产生的时序。
///
/// 放行信号发在 `quota` 之后、`rest` 之前：抓包里 eval 与额度探测排在 penguin/mcp/
/// bootstrap 那批之前，而那批与首条 messages 是重叠的。
pub(super) async fn handshake_sequence(
    lead: impl std::future::Future<Output = ()>,
    quota: impl std::future::Future<Output = ()>,
    rest: impl std::future::Future<Output = ()>,
    probe_done: tokio::sync::oneshot::Sender<()>,
) {
    lead.await;
    quota.await;
    // 接收端可能已经等超时走了，`send` 失败是正常情形。
    let _ = probe_done.send(());
    rest.await;
}

/// 官方启动串里那条额度探测：`POST /v1/messages?beta=true`，haiku、`max_tokens:1`、
/// 正文就一个词 `quota`（`cap/2.1.260-2/00004`）。
///
/// **只在模拟路径的新会话上补**（调用点已经判过）：真实 CC 自己每次启动都会发一条，经
/// luban 转发过去，再补一条就是同一个会话发了两遍。
///
/// **代价是实打实的**：每个新的模拟会话多一次上游调用。输出只有 1 个 token，钱可以忽略，
/// 但它**占一次请求数**——5h 窗口按请求数也算一笔。不想要就关掉 `api_telemetry`，那条开关
/// 连这条一起管。
///
/// 结果只记 debug：这条既不是客户端流量，也不该影响任何转发判定，失败了就当没发过。
async fn send_quota_probe(
    client: &wreq::Client,
    token: &str,
    cred: &crate::credentials::Credential,
    ident: &OutboundIdentity,
    version: &str,
    session_id: &str,
) {
    let model = QUOTA_PROBE_MODEL;
    let flags = store::ForwardFlags::default();
    let sim = Simulation {
        base: None,
        profile: config::cc_profile(config::CcProfileKind::QuotaProbe),
        beta: config::cc_profile(config::CcProfileKind::QuotaProbe).beta.into(),
        session_id: session_id.to_string(),
        // 额度探测不在会话链上：官方那条既没有 billing header，也没有 `diagnostics`。
        link: CcSessionLink::default(),
        reason: SimulationReason::Probe,
        // 官方那条没有 `system`，自然也没有第四块。
        rest: None,
        env: None,
        context_1m: false,
        fill_absent_tools: false,
        trim_tools: false,
        usage_limit: false,
        thread: Default::default(),
    };
    let mut headers = build_forward_headers_for(
        &HeaderMap::new(),
        token,
        flags,
        Some(&sim),
        None,
        Some(model),
        false,
        super::headers::BetaCtx::MAIN,
    );
    // **UA 跟着这个会话的版本走**，不是 luban 自己那个。
    //
    // 探测体与 beta 在 2.1.258 与 2.1.260 上**逐字节相同**（`cap/2.1.258/00004` ↔
    // `cap/2.1.260-2/00004`），唯一的差别就是 UA 里那个版本号。而
    // [`official_headers`] 用的是 [`config::CC_SIM_HEADERS`] 里钉死的
    // [`config::CC_USER_AGENT`]，不改的话，一个 2.1.258 客户端的会话里会冒出一条
    // `claude-cli/2.1.260` 的额度探测——同一会话里混了两个版本。
    if let Ok(v) = HeaderValue::from_str(&format!("claude-cli/{version} (external, cli)")) {
        headers.insert(header::USER_AGENT, v);
    }
    // SDK 版本头同理：同一个 Stainless 客户端发出来的，跟着会话版本（2.1.285 的会话里是 0.127.0，
    // 不是 luban 当前模拟版本的 0.128.0），见 [`config::cc_stainless_version`]。
    if let Some(sdk) = config::cc_stainless_version(version) {
        headers.insert("x-stainless-package-version", HeaderValue::from_static(sdk));
    }
    let body = rewrite_body_out(
        &probe_body(model),
        cred,
        // 身份下面整份覆盖，这里的指纹只是 `ensure_cc_metadata` 的占位。
        "",
        flags,
        Some(&sim),
        None,
        // 模拟路径：会话 id 走 `sim.session_id`，出站归一那步不参与。
        None,
        false,
        None,
        false,
        false,
        None,
        None,
        CcRequestKind::QuotaProbe,
        None,
    )
    .0;
    // **身份与这个会话的主请求逐字相同**：官方那条额度探测与首条 messages 是同一个进程
    // 发的，`metadata.user_id` 里三个字段一模一样。`spoof_device_id` / `spoof_identity`
    // 关掉时主请求发的是客户端自己那份，这里也必须跟着，否则同一会话两条请求在上游看来
    // 来自两台设备。
    let body = with_outbound_identity(body, ident);
    let url = ensure_beta_query(&format!("{}/v1/messages", config::UPSTREAM_BASE_URL));
    let sent =
        client.post(&url).headers(headers).orig_headers(orig_header_case()).body(body).send().await;
    match sent {
        Ok(r) => tracing::debug!(
            cred_id = cred.id,
            status = r.status().as_u16(),
            session = %session_id.chars().take(8).collect::<String>(),
            "quota probe sent"
        ),
        Err(e) => tracing::debug!(cred_id = cred.id, error = %e, "quota probe failed"),
    }
}

/// 连通性测试那条请求的 [`Simulation`]：profile 直接指定 [`config::CcProfileKind::QuotaProbe`]。
///
/// 不走 [`Simulation::detect`]——这条请求是 luban 自己发的裸请求（body 里没有那句身份
/// 声明），detect 只会在开关关掉时返回 `None`，那样发出去必被上游拒。
///
/// **只有 haiku 才用 `QuotaProbe`。** 官方那条额度探测**恒为 haiku-4.5**
/// （`cap/2.1.260-2/00004`、`00021`、`00047` 三份都是），它的形状——`max_tokens:1`、
/// 无 `system`、无 billing header、正文 `quota`、那一小串 beta——是和这个模型绑在一起的。
/// 把一条 opus-5 或 fable 的连通性测试也套成这个形状，发出去的是「一个 opus 请求长着额度
/// 探测的皮」，官方从不产生；而连通性测试恰恰要逐个模型都测一遍。
///
/// 别的模型退回该族的主线程 profile：多花一个基座的写入价（约 300 / 2700 token，且带
/// `scope:global` 断点，全网共用一份、基本走缓存读价），换一条真实存在的形态。
pub(super) fn probe_simulation(cred: &crate::credentials::Credential, model: &str) -> Simulation {
    // **逐字比规范名**，不是「名字里带 haiku」：官方那条额度探测恒为
    // `claude-haiku-4-5-20251001`（[`QUOTA_PROBE_MODEL`]），haiku-3 / 3.5 / 将来某个 haiku
    // 都不该套那身皮——它们的连通性测试要走正常主线程探针。
    let haiku = model == QUOTA_PROBE_MODEL;
    let profile = if haiku {
        config::cc_profile(config::CcProfileKind::QuotaProbe)
    } else {
        cc_profile_for(model)
    };
    Simulation {
        base: if haiku { None } else { cc_system_base(model) },
        profile,
        beta: config::cc_model_beta(profile, model),
        // 探测用第 0 个槽位的会话 id：它是 luban 自己发的一条小请求，挂在这个账号固定的那组
        // 会话 id 之内，不另造一个。
        session_id: session_id_for(cred, &crate::credentials::slot_session_seed(0)),
        link: CcSessionLink::default(),
        reason: SimulationReason::Probe,
        // 探测不补第四块：它一句 `ping` 就完，没有客户端 system 要安置，多一万字节的前缀只是
        // 多付一次写入价；转发路径的形态由 [`Simulation::detect`] 管。
        rest: None,
        // 环境说明随第四块走，探测同样不补。
        env: None,
        context_1m: false,
        // 探测体是 `tools: []`（[`probe_body`]），一直靠注入补齐官方工具、验的正是主线程链路；
        // 这是 luban 自己发的，不受 `fill_absent_tools` 开关管。
        fill_absent_tools: true,
        // 连通性探测验的是完整的主线程链路，按官方默认那 14 条发，不跟 `sim_trim_tools`。
        trim_tools: false,
        // 探测是一句新输入，不是工具续轮。
        usage_limit: false,
        thread: Default::default(),
    }
}

/// 连通性测试的流式回复是怎么结束的，[`aggregate_probe_sse`] 与 [`probe_report`] 之间传。
///
/// 不能只交一段字节了事：错误事件攒出来的是一份错误 JSON、半截流交回的是原始 SSE，两者在
/// 2xx 状态码下都会被当成「通过」——测试报绿，还顺手把暂停中的号放回池子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeStreamEnd {
    /// 非流式回复（haiku 额度探测、错误响应）：体就是整段 JSON，由 [`probe_report`] 看它是不是 Message。
    NotStreamed,
    /// 收到了 `message_stop`，攒出一条完整 Message。
    Complete,
    /// 流里来了 `event: error`：交回的体是那份错误 JSON。
    UpstreamError,
    /// 流断在半路（没有 `message_start` / `message_stop`），附上缺的是哪一个。
    Incomplete(&'static str),
    /// 响应用了 luban 解不开的 `content-encoding`：体是压缩字节，既看不出成败也取不出错误原文。
    Undecodable,
}

/// 把连通性测试收到的 SSE 攒回一条整段 Message，连同**流是怎么结束的**一起交回。
///
/// 与转发那条路共用 [`SseAggregator`]——两边对「什么算一条完整回复」的判断必须是同一套，
/// 不然会出现「测试说通了、真实请求却是半截流」这种自相矛盾的结论。错误事件交回那份错误
/// JSON（[`probe_report`] 从里面取 `error.type` / `error.message`）；半截流原样交回（流水里
/// 留的是上游实际发了什么）。
pub(super) fn aggregate_probe_sse(bytes: &[u8]) -> (Bytes, ProbeStreamEnd) {
    let mut agg = SseAggregator::default();
    agg.feed(bytes);
    let to_bytes = |v: &serde_json::Value| {
        serde_json::to_vec(v).map(Bytes::from).unwrap_or_else(|_| Bytes::copy_from_slice(bytes))
    };
    match agg.finish() {
        Aggregated::Message(msg) => (to_bytes(&msg), ProbeStreamEnd::Complete),
        Aggregated::UpstreamError(err) => (to_bytes(&err), ProbeStreamEnd::UpstreamError),
        Aggregated::Incomplete(why) => {
            (Bytes::copy_from_slice(bytes), ProbeStreamEnd::Incomplete(why))
        }
    }
}

/// 主线程 profile 的连通性测试用的 `max_tokens`。
///
/// 官方主线程发的是 64000，但那个数字会被上游按「声明的输出预算」记进限流窗口，也被
/// luban 自己的 [`note_upstream_send`] 记一笔——一次手动探活占掉 64000 的预算，一轮把
/// 四个模型都测一遍就是 256000。取 1024：这是 [`THINKING_MIN_MAX_TOKENS`] 的下限，
/// 低于它 [`ensure_thinking`] 就不补 `thinking`，跟着 `context_management` 也没了，
/// 整条又退回那个「有主线程 system/beta、却没有 thinking」的混合形态。
///
/// 与官方的差别只剩这一个字段的**取值**（客户端本来就可以自己配），不再是**字段缺失**。
const PROBE_MAIN_MAX_TOKENS: u64 = THINKING_MIN_MAX_TOKENS;

/// 连通性测试的请求体，按 profile 分两种。
///
/// **haiku → 官方额度探测的逐字形态**（`cap/2.1.260-2/00004`）：键序
/// `model → max_tokens → messages → metadata`（`metadata` 由 [`ensure_cc_metadata`] 补），
/// 正文就是 `quota` 这一个词。
///
/// **其余模型 → 一条真正的主线程请求**。这里不能只换个 profile 就完事：`max_tokens:1`
/// 的体配上主线程的 system/beta，会得到一条**没有 `thinking`、没有 `context_management`、
/// 没有 `output_config`、非流式**的请求——抓包里不存在这种东西，而且它也验证不了正常
/// 主线程链路（客户端真实流量走的是流式 + thinking 那条）。故这里把主线程该有的字段一次
/// 给齐，剩下的 `system` / `tools` / `metadata` / `diagnostics` 由 [`rewrite_body`] 按同一
/// 套规则补——测试与真实转发共用一套改写，这是这条测试存在的意义。
///
/// 代价：非 haiku 的探活会真的生成一小段回复（含 adaptive thinking），比 1 个 token 贵。
/// 换来的是「测通了」真的等于「主线程这条路通了」。
pub(super) fn probe_body(model: &str) -> Bytes {
    // 判据与 [`probe_simulation`] 必须同源：一个说「套 QuotaProbe profile」、另一个说
    // 「发主线程体」，就会拼出一条谁都不是的请求。
    let v = if model == QUOTA_PROBE_MODEL {
        serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "quota" }]})
    } else {
        serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": "quota" }],
            // 空数组由 [`inject_cc_tools`] 填成官方主线程那 14 个；**这个键必须在**，
            // 缺了那个函数就当是「官方无工具 helper」而不补。
            "tools": [],
            "max_tokens": PROBE_MAIN_MAX_TOKENS,
            // 官方主线程恒带（`cap/2.1.260-2/00025`）；luban 没有别处会补它。
            "output_config": { "effort": "high" },
            "stream": true})
    };
    // 常量结构，序列化不会失败；真失败了也会以上游 400 的形式如实报出来，不必在这里 panic。
    Bytes::from(serde_json::to_vec(&v).unwrap_or_default())
}

/// 用量日志里标记「这条是连通性测试」的 device_id。
///
/// 借 `device_id` 这一列而不新开一列：它本来就是「这条流量是谁打的」，测试正是一个特殊的
/// 来源，与裸客户端那个 `sim:` 前缀（见 [`sim_device_id`]）同一个路子。它也不会与设备列表
/// 串味——那张表从 `device_bindings` 出发，而测试从不写绑定。
const PROBE_DEVICE_ID: &str = "probe";

/// 把一次测试记进 `usage_logs`，口径与转发路径的 [`ReqLog`] 完全一致（同一个嗅探器、
/// 同一套计价），差别只在 `device_id` 标成 [`PROBE_DEVICE_ID`]。
///
/// **为什么要记**：账号卡片上的额度快照与累计花费都出自这张表（`latest_quotas` 取的是
/// 「最新一条带限流信息的日志」）。不记的话，测试拿到的那份最新额度就只活在弹窗里，
/// 卡片照旧显示上一次真实请求时的旧数；而这条请求确实花掉了钱，不记也等于让累计花费虚低。
///
/// 写失败只告警不影响测试结果——用户要的是「通不通」，日志是副产品。
/// 一次探测里**逐轮不变**的那几项，供 [`ProbeLog::record`] 用。
///
/// 打包而不是逐个传：三个落日志的分支（body 超时 / 读断 / 读完）只有 status、body、限流头
/// 不同，其余五项完全一样。摊平成八个参数既触了 clippy 的上限，也让三处调用各抄一遍。
pub(super) struct ProbeLog<'a> {
    pub(super) store: &'a std::sync::Arc<store::CredentialStore>,
    pub(super) cred: &'a crate::credentials::Credential,
    pub(super) req_model: &'a str,
    pub(super) started: &'a std::time::Instant,
    /// 实际发出去那份 UA，由调用方从出站头取（没有该头时为 `None`）。
    pub(super) out_ua: Option<String>,
    /// 这次测试的 luban 请求 id（同转发路径的口径，见 [`handle`]），流水里按它能查到。
    pub(super) request_id: String,
    /// 实际发出去的出站体（[`Upstream::shape`] 之后）：取证列里的形态摘要、会话 id、出站
    /// device_id 都从它读，与转发路径的 [`capture_forensics`] 同源。`Bytes` 引用计数，留一份不拷字节。
    pub(super) sent: Bytes,
}

impl ProbeLog<'_> {
    pub(super) fn record(
        &self,
        status: StatusCode,
        bytes: &Bytes,
        ratelimit: &RateLimitInfo,
        upstream_request_id: Option<&str>,
    ) {
        log_probe_usage(self, status, bytes, ratelimit, upstream_request_id)
    }
}

fn log_probe_usage(
    ctx: &ProbeLog<'_>,
    status: StatusCode,
    bytes: &Bytes,
    ratelimit: &RateLimitInfo,
    upstream_request_id: Option<&str>,
) {
    let ProbeLog { store, cred, req_model, started, out_ua, request_id, sent } = ctx;
    // 非流式、未压缩（wreq 已解码），喂整段 body 即可解析出顶层 `usage`。
    let mut sniffer = UsageSniffer::new(false, false);
    sniffer.feed(bytes);
    sniffer.finish();
    // 模型以上游回报为准，没有（4xx 没有 usage）才用请求侧那个。
    let model = sniffer.model.clone().unwrap_or_else(|| req_model.to_string());
    let cost_usd = crate::pricing::estimate_usd(crate::pricing::Usage {
        model: Some(&model),
        speed: sniffer.speed.as_deref(),
        input_tokens: sniffer.input_tokens,
        output_tokens: sniffer.output_tokens,
        cache_creation_total: sniffer.cache_creation_tokens,
        cache_5m_tokens: sniffer.cache_creation_5m,
        cache_1h_tokens: sniffer.cache_creation_1h,
        cache_read_tokens: sniffer.cache_read_tokens,
    });
    let rec = store::UsageRecord {
        cred_id: Some(cred.id),
        cred_label: cred.label.clone(),
        device_id: Some(PROBE_DEVICE_ID.into()),
        model: Some(model),
        path: "/v1/messages".into(),
        // 入站留空：连通性测试没有来访客户端，这条是 luban 自己发的。出站照实记——它确实
        // 按官方形态发了那串 UA（见 `probe` 里的 build_forward_headers），照实记才对得上抓包。
        ua: None,
        ua_out: out_ua.clone(),
        status: status.as_u16(),
        // 连通性测试恒为非流式（见 `probe` 里 `force_stream: false` 的说明），不走聚合。
        sse_aggregated: false,
        has_usage: sniffer.has_usage(),
        input_tokens: sniffer.input_tokens,
        output_tokens: sniffer.output_tokens,
        cache_creation_tokens: sniffer.cache_creation_tokens,
        cache_5m_tokens: sniffer.cache_creation_5m,
        cache_1h_tokens: sniffer.cache_creation_1h,
        cache_read_tokens: sniffer.cache_read_tokens,
        // 非流式一次读完，没有「首块」可言；总耗时已经说明一切。
        ttft_ms: None,
        total_ms: i64::try_from(started.elapsed().as_millis()).ok(),
        unified_status: ratelimit.unified_status.clone(),
        rl_5h_status: ratelimit.five_h_status.clone(),
        rl_5h_reset: ratelimit.five_h_reset,
        rl_5h_utilization: ratelimit.five_h_utilization,
        rl_7d_status: ratelimit.seven_d_status.clone(),
        rl_7d_reset: ratelimit.seven_d_reset,
        rl_7d_utilization: ratelimit.seven_d_utilization,
        rl_representative: ratelimit.representative.clone(),
        rl_overage_in_use: ratelimit.overage_in_use,
        windows: ratelimit.windows(),
        ratelimit_raw: (!ratelimit.raw.is_empty()).then(|| ratelimit.raw.clone()),
        cost_usd,
        request_id: Some(request_id.clone()),
        upstream_request_id: upstream_request_id.map(str::to_string),
        // 连通性测试整条都是照官方形态造的（见 `probe`），故 simulated 恒为 true；
        // 非 2xx 时把上游文案与第三方判定一并记下，与转发路径同口径。
        forensics: {
            let (error_type, error_message) = if status.is_success() {
                (None, None)
            } else {
                let (t, m) = parse_upstream_error(bytes);
                (t, Some(m))
            };
            let bits = shape_summary(sent);
            store::Forensics {
                proxy: cred.proxy.as_deref().map(store::redact_proxy),
                simulated: true,
                shape: bits.shape,
                session_id: bits.session_id,
                device_id_out: bits.device_id_out,
                error_type,
                error_message,
                third_party: !status.is_success() && is_third_party_rejection(bytes),
                ..Default::default()
            }
        },
    };
    // 与转发路径同理：这里在 async 上下文里，同步写库会占住工作线程，见 [`spawn_usage_log`]。
    spawn_usage_log((*store).clone(), rec);
}

/// 把上游响应翻译成一份结果。**通过**只有一种：2xx 且拿到一条完整有效的 Message（顶层
/// `type == "message"`），取它回报的模型名。其余一律失败，并尽量说清为什么：
/// - 非 2xx：取 `error.type` / `error.message`；
/// - 2xx 但流里来了错误事件：同上，取那份错误 JSON 里的；
/// - 2xx 但流断在半路：写明缺的是哪个事件；
/// - 2xx 但体不是 Message（解析不出、或 `type` 不对）：原样截一段体；
/// - 响应用了解不开的 `content-encoding`（任何状态码）：写明无从校验，不拿压缩字节当原文。
///
/// 限流头由调用方先行解析（读 body 会把响应消费掉），成败两条路都带上。
fn probe_report(
    status: StatusCode,
    bytes: &[u8],
    stream_end: ProbeStreamEnd,
    latency_ms: u128,
    quota: Option<ProbeQuota>,
) -> ProbeReport {
    // `Ok(回报的模型名)` / `Err((error.type, 原因))`。
    let verdict: Result<Option<String>, (Option<String>, String)> = if stream_end
        == ProbeStreamEnd::Undecodable
    {
        // 不拿压缩字节去解析：成功与否无从校验，错误原文也只会是一串乱码。
        Err((
            None,
            format!(
                "the upstream returned {} with a content-encoding luban cannot decode; the reply could not be checked",
                status.as_u16()
            ),
        ))
    } else if !status.is_success() || stream_end == ProbeStreamEnd::UpstreamError {
        Err(parse_upstream_error(bytes))
    } else if let ProbeStreamEnd::Incomplete(why) = stream_end {
        Err((None, format!("the upstream stream ended before the reply was complete ({why})")))
    } else {
        serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("message"))
            .map(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
            .ok_or_else(|| {
                (
                    None,
                    format!(
                        "the upstream returned {} but the body is not a complete Message: {}",
                        status.as_u16(),
                        String::from_utf8_lossy(bytes)
                    ),
                )
            })
    };
    let (ok, model, error_type, error) = match verdict {
        Ok(model) => (true, model, None, None),
        // 上游偶尔糊一大坨（HTML 拦截页之类），截断到能看清病因即可。
        Err((error_type, error)) => {
            (false, None, error_type, Some(error.chars().take(500).collect()))
        }
    };
    ProbeReport { ok, status: status.as_u16(), latency_ms, model, error_type, error, quota }
}

/// 连通性测试**通过**之后照真实判决恢复调度，只在 [`ProbeReport::ok`] 为真时调用（整条回复
/// 读完、拿到完整 Message——只看状态码不够：200 之后流里带错误、流断在半路，号却已经回了池）。
///
/// - 限流那几档（`resume_rate_limited` 为真时，即按响应头判过「这一发没撞限流、额度也没快满」）：
///   上游此刻放行了「这个账号 + 这个模型」，不必干等到点（上游的 retry-after 偏保守时，好号会被
///   白白晾着）。两档各恢复各的：账号级那档是**落库的调度开关**，只对限流暂停的号生效，人工关掉
///   的不该被一次测试打开；模型级那档是进程内冷却，清账号格 + 被测模型那一格，其它模型不动——
///   sonnet 通了证明不了 fable 通。之前学到的「套餐不含这个模型」也作废。
/// - 订阅未生效暂停：测试通过即已续费 / 订阅，放回池子。放回来之后补过一遍额度那道——按头判的
///   时候号还停着，那道见号已停用就当「已在池外」什么都没做；真快满了，这里照常按额度再停一次。
fn settle_passing_probe(
    store: &store::CredentialStore,
    cred: &crate::credentials::Credential,
    model: &str,
    info: &RateLimitInfo,
    resume_rate_limited: bool,
) {
    if resume_rate_limited {
        match store.resume_if_rate_limited(cred.id) {
            Ok(true) => tracing::info!(
                cred_id = cred.id, cred = %cred.label,
                model,
                "connectivity test passed, credential is back in the pool"
            ),
            Ok(false) => {}
            Err(e) => tracing::error!(
                cred_id = cred.id, cred = %cred.label,
                error = %e,
                "connectivity test passed but persisting the resume failed"
            ),
        }
        store.clear_rate_limited(cred.id, Some(model));
        match store.clear_model_denials(cred.id, Some(model)) {
            Ok(n) if n > 0 => tracing::info!(
                cred_id = cred.id, cred = %cred.label,
                model,
                "connectivity test passed, the model is no longer marked as excluded from this account's plan"
            ),
            Ok(_) => {}
            Err(e) => tracing::error!(
                cred_id = cred.id, cred = %cred.label,
                error = %e,
                "connectivity test passed but clearing the model denial failed"
            ),
        }
    }
    match store.resume_if_subscription_suspended(cred.id) {
        Ok(true) => {
            tracing::info!(
                cred_id = cred.id, cred = %cred.label,
                model,
                "connectivity test passed, the subscription is active again; credential is back in the pool"
            );
            park_if_quota_nearly_exhausted(store, cred, info);
        }
        Ok(false) => {}
        Err(e) => tracing::error!(
            cred_id = cred.id, cred = %cred.label,
            error = %e,
            "connectivity test passed but persisting the resume failed"
        ),
    }
}

#[cfg(test)]
mod tests;
