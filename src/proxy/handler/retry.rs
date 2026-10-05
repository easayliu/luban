//! 7 的回程一侧：上游这一发是 401 / 403 / 429 时，决定换号重发、就地回复还是收尾。

use super::*;

pub(super) type UpstreamResult = Result<wreq::Response, wreq::Error>;

/// 这一发之后怎么走。
// `Reply` 带着整条响应，比另两个变体大得多；每轮只移动一次，不值得装箱。
#[allow(clippy::large_enum_variant)]
pub(super) enum Flow {
    /// 已换号（[`Pick`] 已更新），再发一轮。
    Retry,
    /// 就地回给客户端（体已读掉、不走后面的响应处理）。
    Reply(Response),
    /// 收尾：交给响应处理。带上这一发上游原样给的限流头（只在 429 时有），见 `upstream_limit`。
    Done(UpstreamResult, Option<RateLimitInfo>),
}

/// 403 那段的结果：要么定了去向，要么判不中、把读掉的体原样拼回一个响应交给后面。
// `Reply` 带着整条响应，比另两个变体大得多；每轮只移动一次，不值得装箱。
#[allow(clippy::large_enum_variant)]
enum Forbidden {
    Flow(Flow),
    PassOn(wreq::Response),
}

/// 换号重试的计数：试过哪些号、429 / 401 / 403 换了几次、「套餐不含」换了几次。
pub(super) struct Swaps {
    pub(super) tried: Vec<i64>,
    pub(super) retried: usize,
    pub(super) max_retry: usize,
    /// 「套餐不含这个模型」引发的换号次数，与 429 那套 `retried`/`max_retry` **分开计**：那个
    /// 开关管的是限流，关掉表示「429 原样透传」；而这一档是确定性失败，换号一定有意义，不受
    /// 那个开关约束，见 [`LimitScope::Unsupported`]。
    pub(super) denial_swaps: usize,
}

impl Swaps {
    pub(super) fn new(state: &AppState, inb: &Inbound) -> Self {
        let max_retry =
            if inb.flags.rate_limit_retry { state.store.rate_limit_retry_max() } else { 0 };
        Self { tried: Vec::new(), retried: 0, max_retry, denial_swaps: 0 }
    }
}

/// 看上游这一发：401 / 403 账号级的停号换号，429 按作用域冷却、换号或交回。
pub(super) async fn on_response(
    cx: &Ctx<'_>,
    inb: &Inbound,
    pick: &mut Pick,
    swaps: &mut Swaps,
    attempt: &Attempt<'_>,
    prepared: &Prepared,
    mut resp: UpstreamResult,
) -> Flow {
    // 只认「上游明确回 429」这一种：连不上/超时那类换个号一样连不上，重试只是浪费时间。
    let limited = match &resp {
        Ok(up) if up.status() == StatusCode::TOO_MANY_REQUESTS => {
            Some(RateLimitInfo::from_headers(up.headers()))
        }
        _ => None,
    };
    // 这一发**上游原样给的**限流头，只在它回 429 时有值（逐轮重算，故换号换到一发 200 时它是
    // `None`，上一轮的 429 头不再算数）。存在的理由是 transient 档会把我们自己算出来的退避写进
    // `retry-after` 再交回客户端——那之后重解 `up.headers()` 就会把自己塞的那条当成上游给的读
    // 回来，[`RateLimitInfo::no_limit_headers`] 从此恒为 false。留一份注入前的快照随
    // [`Flow::Done`] 交给收尾。
    let upstream_limit = limited.clone();
    // 401 账号级错误（token revoked / invalid_grant 等）：停用当前号并换号重试。
    if limited.is_none()
        && swaps.max_retry > 0
        && resp.as_ref().is_ok_and(|up| up.status() == StatusCode::UNAUTHORIZED)
    {
        return on_401(cx, inb, pick, swaps, attempt, prepared, resp).await;
    }
    // 403 里两种与这条请求无关、换个号就能过的：账号级错误（组织 / 账号被停用）与组织
    // 未放开 OAuth。当场换号重发，客户端不必吃这一发。
    //
    // 与 401 那条的区别：403 大多是**请求本身**的权限问题（套餐不含模型、区域限制），
    // 那些得原样交给下面的 4xx 段（学习、取证、流水都在那儿）。body 读出来以后判不中，
    // 就把状态码、头、体原样拼回一个 `wreq::Response` 放回 `resp`，下游看不出区别
    // （那段只读 status / headers / bytes）。
    //
    // **先选到下一个号再动库**：换得到才在这里停用 / 暂停并 `continue`；换不到就原样
    // 交出去，由下面的 4xx 段照旧判定、停用 / 暂停——同一条 403 不会落两次封号事件。
    if swaps.max_retry > 0
        && swaps.retried < swaps.max_retry
        && let Ok(up) = &resp
        && up.status() == StatusCode::FORBIDDEN
        && resp_shape(up).1.is_none()
    {
        match on_403(cx, inb, pick, swaps, attempt, resp).await {
            Forbidden::Flow(flow) => return flow,
            Forbidden::PassOn(rebuilt) => resp = Ok(rebuilt),
        }
    }
    let Some(info) = limited else { return Flow::Done(resp, upstream_limit) };
    on_429(cx, inb, pick, swaps, attempt, resp, info, upstream_limit).await
}

/// 401：账号级的停用或暂停这个号、换号重发；换不到号或判定不命中就地透传（体已读掉）。
async fn on_401(
    cx: &Ctx<'_>,
    inb: &Inbound,
    pick: &mut Pick,
    swaps: &mut Swaps,
    attempt: &Attempt<'_>,
    prepared: &Prepared,
    resp: UpstreamResult,
) -> Flow {
    let Ctx { state, request_id, log_state, .. } = *cx;
    let Inbound {
        ref path_and_query,
        ref client_ua,
        ref device_id,
        ref req_model,
        ref device_fp,
        started,
        billable,
        flags,
        ..
    } = *inb;
    let session_sel = inb.session_sel();
    let Attempt { ref upstream, ref sent, .. } = *attempt;
    let Pick { ref cred, .. } = *pick;
    let tool_names = &prepared.tool_names;
    let up = resp.unwrap();
    let builder = resp_builder(&up);
    let up_request_id = header_opt(up.headers(), "request-id");
    // `up.bytes()` 下面就把 `up` 吃掉了，要用的响应头在这儿一次取齐。
    let up_org_id = header_opt(up.headers(), "anthropic-organization-id");
    let bytes = match up.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read the upstream 401 body");
            // 同下面那条：这条路早退，`ReqLog` 建不起来，失败遥测就地补。
            record_early_failure(
                state,
                cred,
                upstream,
                sent,
                started,
                flags,
                billable,
                up_request_id.as_deref(),
                up_org_id,
                crate::telemetry::CallFailure {
                    status: Some(StatusCode::UNAUTHORIZED.as_u16()),
                    error_type: None,
                    message: String::new(),
                    in_band: false,
                },
            );
            log_early_upstream_failure(
                state,
                log_state,
                cred,
                upstream,
                sent,
                EarlyUpstreamFailure {
                    path: path_and_query,
                    client_ua,
                    model: req_model.clone(),
                    device_id: early_logged_device(device_id, upstream, flags, cred, device_fp),
                    started,
                    request_id,
                    upstream_request_id: up_request_id.as_deref(),
                    status: StatusCode::UNAUTHORIZED,
                    error_type: None,
                    error_message: Some(format!("failed to read the upstream 401 body: {e}")),
                    third_party: false,
                    ratelimit: None,
                    tag: "upstream_401",
                },
            );
            return Flow::Reply(builder.body(Body::empty()).unwrap_or_else(|e| {
                error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
            }));
        }
    };
    let (etype, message) = parse_upstream_error(&bytes);
    // 下面两条不换号的出路（回 403 / 原样透传）要写流水，共用这一份。
    let early_401 = |status: StatusCode, error_type: Option<String>, message: String| {
        log_early_upstream_failure(
            state,
            log_state,
            cred,
            upstream,
            sent,
            EarlyUpstreamFailure {
                path: path_and_query,
                client_ua,
                model: req_model.clone(),
                device_id: early_logged_device(device_id, upstream, flags, cred, device_fp),
                started,
                request_id,
                upstream_request_id: up_request_id.as_deref(),
                status,
                error_type,
                error_message: Some(message),
                third_party: is_third_party_rejection(&bytes),
                ratelimit: None,
                tag: "upstream_401",
            },
        );
    };
    {
        let (etype, message) = (etype.clone(), message.clone());
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            status = 401u16,
            error_type = %etype.as_deref().unwrap_or("-"),
            upstream_message = %message.chars().take(500).collect::<String>(),
            "upstream returned 401"
        );
        // **在分叉之前记一次**：底下三条出路（换号 continue / 回 403 / 原样透传）
        // 全都绕开了 `ReqLog::drop`。记在这里，一条 401 恰好报一次，且报在**吃到
        // 这发 401 的那个号**上——换号之后的那一发由新号自己的 `ReqLog` 报。
        record_early_failure(
            state,
            cred,
            upstream,
            sent,
            started,
            flags,
            billable,
            up_request_id.as_deref(),
            up_org_id,
            crate::telemetry::CallFailure {
                status: Some(StatusCode::UNAUTHORIZED.as_u16()),
                error_type: etype,
                message,
                in_band: false,
            },
        );
    }
    // 两种都把号挪出池子、换号重发：账号级错误停用（终态），订阅未生效暂停
    // （手动启用或连通性测试通过才恢复，见 [`park_org_oauth_disallowed`]）。
    let out_of_pool = match classify_account_rejection(StatusCode::UNAUTHORIZED, &bytes) {
        AccountRejection::Ban(reason) => {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                reason = %reason,
                "401 account-level error, auto-disabling and attempting credential swap"
            );
            let ctx = ban_context(
                &reason,
                "forward_401",
                StatusCode::UNAUTHORIZED,
                &bytes,
                request_id,
                up_request_id.as_deref(),
            );
            let _ = state.store.record_ban(cred.id, &ctx);
            true
        }
        AccountRejection::SubscriptionInactive => {
            park_org_oauth_disallowed(&state.store, cred, 401, "forward_401");
            true
        }
        AccountRejection::Other => false,
    };
    if out_of_pool {
        swaps.tried.push(cred.id);
        if swaps.retried < swaps.max_retry {
            match store::valid_access_token_for_device(
                &state.store,
                &state.clients,
                select(
                    device_id.as_deref(),
                    session_sel,
                    billable,
                    req_model.as_deref(),
                    &swaps.tried,
                ),
            )
            .await
            {
                Ok((next_token, next_cred, next_slot)) => {
                    tracing::info!(
                        cred_id = cred.id, cred = %cred.label,
                        to_cred_id = next_cred.id,
                        to_cred = %next_cred.label,
                        attempt = swaps.retried + 1,
                        "401 credential swap: retrying with another credential"
                    );
                    *pick = Pick { token: next_token, cred: next_cred, session_slot: next_slot };
                    swaps.retried += 1;
                    return Flow::Retry;
                }
                // 剩下的号全都被判过「套餐不含这个模型」：把上游那发 401 透传出去会让
                // 客户端以为是自己的鉴权坏了；与首发选号那条路一样回 403 让它换模型。
                Err(e) if e.downcast_ref::<store::ModelUnsupported>().is_some() => {
                    tracing::warn!(
                        cred_id = cred.id, cred = %cred.label,
                        error = %e,
                        "401 swap: no enabled account can use this model, answering 403"
                    );
                    early_401(
                        StatusCode::FORBIDDEN,
                        Some("permission_error".into()),
                        e.to_string(),
                    );
                    return Flow::Reply(error_response(
                        StatusCode::FORBIDDEN,
                        "permission_error",
                        e.to_string(),
                    ));
                }
                Err(e) => tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    error = %e,
                    "401 but no credential to swap to, passing through as is"
                ),
            }
        }
    }
    // 没换号（判定不命中或换不到号）：body 已消费，就地透传。
    early_401(StatusCode::UNAUTHORIZED, etype, message);
    let bytes = match &tool_names {
        Some(map) => Bytes::from(map.restore(&bytes)),
        None => bytes,
    };
    Flow::Reply(
        builder.body(Body::from(bytes)).unwrap_or_else(|e| {
            error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
        }),
    )
}

/// 403：账号级错误与组织未放开 OAuth 当场换号重发；判不中就把体拼回去交给后面的 4xx 段。
async fn on_403(
    cx: &Ctx<'_>,
    inb: &Inbound,
    pick: &mut Pick,
    swaps: &mut Swaps,
    attempt: &Attempt<'_>,
    resp: UpstreamResult,
) -> Forbidden {
    let Ctx { state, request_id, log_state, .. } = *cx;
    let Inbound {
        ref path_and_query,
        ref client_ua,
        ref device_id,
        ref req_model,
        ref device_fp,
        started,
        billable,
        flags,
        ..
    } = *inb;
    let session_sel = inb.session_sel();
    let Attempt { ref upstream, ref sent, .. } = *attempt;
    let Pick { ref cred, .. } = *pick;
    let up = resp.unwrap();
    let builder = resp_builder(&up);
    let (version, headers) = (up.version(), up.headers().clone());
    let up_request_id = header_opt(&headers, "request-id");
    let up_org_id = header_opt(&headers, "anthropic-organization-id");
    // 读体失败：与下面 4xx 段、上面 401 那条同一个出路——体已经没了，就地回一个空体的
    // 403。不拼一个空体的「正常」403 交下去：那样下游会拿空体去判定、学习、取证，流水里
    // 也只剩一条干净的 403，读失败这件事就被吞了。`ReqLog` 还没建，失败遥测与流水就地补。
    let bytes = match up.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read the upstream 403 body");
            record_early_failure(
                state,
                cred,
                upstream,
                sent,
                started,
                flags,
                billable,
                up_request_id.as_deref(),
                up_org_id,
                crate::telemetry::CallFailure {
                    status: Some(StatusCode::FORBIDDEN.as_u16()),
                    error_type: None,
                    message: String::new(),
                    in_band: false,
                },
            );
            log_early_upstream_failure(
                state,
                log_state,
                cred,
                upstream,
                sent,
                EarlyUpstreamFailure {
                    path: path_and_query,
                    client_ua,
                    model: req_model.clone(),
                    device_id: early_logged_device(device_id, upstream, flags, cred, device_fp),
                    started,
                    request_id,
                    upstream_request_id: up_request_id.as_deref(),
                    status: StatusCode::FORBIDDEN,
                    error_type: None,
                    error_message: Some(format!("failed to read the upstream 403 body: {e}")),
                    third_party: false,
                    ratelimit: None,
                    tag: "upstream_403",
                },
            );
            return Forbidden::Flow(Flow::Reply(builder.body(Body::empty()).unwrap_or_else(|e| {
                error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
            })));
        }
    };
    let verdict = classify_account_rejection(StatusCode::FORBIDDEN, &bytes);
    let next = if verdict != AccountRejection::Other {
        swaps.tried.push(cred.id);
        store::valid_access_token_for_device(
            &state.store,
            &state.clients,
            select(device_id.as_deref(), session_sel, billable, req_model.as_deref(), &swaps.tried),
        )
        .await
        .inspect_err(|e| {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                error = %e,
                "403 but no credential to swap to, passing through as is"
            )
        })
        .ok()
    } else {
        None
    };
    if let Some((next_token, next_cred, next_slot)) = next {
        let (etype, message) = parse_upstream_error(&bytes);
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            status = 403u16,
            error_type = %etype.as_deref().unwrap_or("-"),
            upstream_message = %message.chars().take(500).collect::<String>(),
            "upstream returned 403"
        );
        if let AccountRejection::Ban(reason) = &verdict {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                reason = %reason,
                "403 account-level error, auto-disabling and swapping credentials"
            );
            let ctx = ban_context(
                reason,
                "forward_403",
                StatusCode::FORBIDDEN,
                &bytes,
                request_id,
                up_request_id.as_deref(),
            );
            if let Err(e) = state.store.record_ban(cred.id, &ctx) {
                tracing::warn!(error = %e, "failed to auto-disable the credential");
            }
        } else {
            park_org_oauth_disallowed(&state.store, cred, 403, "forward_403");
        }
        // 换号出去的这一发绕开了 `ReqLog::drop`，失败遥测就地补，报在吃到 403 的号上；
        // 与 401 换号同一口径。
        record_early_failure(
            state,
            cred,
            upstream,
            sent,
            started,
            flags,
            billable,
            up_request_id.as_deref(),
            up_org_id,
            crate::telemetry::CallFailure {
                status: Some(StatusCode::FORBIDDEN.as_u16()),
                error_type: etype,
                message,
                in_band: false,
            },
        );
        tracing::info!(
            cred_id = cred.id, cred = %cred.label,
            to_cred_id = next_cred.id,
            to_cred = %next_cred.label,
            attempt = swaps.retried + 1,
            "403 credential swap: retrying with another credential"
        );
        *pick = Pick { token: next_token, cred: next_cred, session_slot: next_slot };
        swaps.retried += 1;
        return Forbidden::Flow(Flow::Retry);
    }
    Forbidden::PassOn(rebuild_response(StatusCode::FORBIDDEN, version, headers, bytes))
}

#[allow(clippy::too_many_arguments)]
/// 429：按限流作用域记准入、冷却、换号，或带着退避交回客户端。
async fn on_429(
    cx: &Ctx<'_>,
    inb: &Inbound,
    pick: &mut Pick,
    swaps: &mut Swaps,
    attempt: &Attempt<'_>,
    mut resp: UpstreamResult,
    info: RateLimitInfo,
    upstream_limit: Option<RateLimitInfo>,
) -> Flow {
    let Ctx { state, request_id, log_state, .. } = *cx;
    let Inbound {
        ref path_and_query,
        ref client_ua,
        ref device_id,
        ref req_model,
        ref device_fp,
        started,
        billable,
        flags,
        ..
    } = *inb;
    let session_sel = inb.session_sel();
    let Attempt { ref upstream, ref sent, .. } = *attempt;
    let Pick { ref cred, .. } = *pick;
    // 基础窗口真耗尽 → 停调度整个账号；超额池（7d_oi）满 → 只冷却这个模型、换号仍有意义；
    // 谁的额度都没满（容量/请求速率）→ 只冷却这个模型且**不换号**，见 [`LimitScope`]。
    let scope = rate_limit_scope_for(&info, req_model.as_deref(), is_max_plan(cred));
    // 套餐不含这个模型：记准入、换号重发。学习不设条件——记录是对的就该记；换号有次数上限，
    // 免得一条请求把整池的 Pro 号挨个点一遍。换不到号时若是「全被判过」就回 403 让客户端
    // 换模型，其余原因（都在冷却等）保留上游那发 429 原样透传。
    if let LimitScope::Unsupported(model) = &scope {
        let reason = info.plan_denial_reason();
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            tier = %cred.tier.as_deref().unwrap_or("-"),
            model = %model,
            expires_at = info.unified_reset.unwrap_or(0),
            ratelimit = %info.raw,
            "upstream 429 with no quota window for this model: this account's plan does not include it, remembering that and switching accounts"
        );
        if let Err(e) = state.store.deny_model(cred.id, model, &reason, info.unified_reset) {
            tracing::error!(
                cred_id = cred.id, cred = %cred.label,
                error = %e,
                "persisting the model denial failed (the swap still proceeds)"
            );
        }
        swaps.tried.push(cred.id);
        if swaps.denial_swaps >= MODEL_DENIAL_MAX_SWAPS {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                model = %model,
                swaps = swaps.denial_swaps,
                "model-denial swap cap reached, passing the upstream 429 through"
            );
            return Flow::Done(resp, upstream_limit);
        }
        match store::valid_access_token_for_device(
            &state.store,
            &state.clients,
            select(device_id.as_deref(), session_sel, billable, req_model.as_deref(), &swaps.tried),
        )
        .await
        {
            Ok((next_token, next_cred, next_slot)) => {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    to_cred_id = next_cred.id,
                    to_cred = %next_cred.label,
                    model = %model,
                    attempt = swaps.denial_swaps + 1,
                    "model not included in this account's plan: retrying with another account"
                );
                *pick = Pick { token: next_token, cred: next_cred, session_slot: next_slot };
                swaps.denial_swaps += 1;
                return Flow::Retry;
            }
            Err(e) if e.downcast_ref::<store::ModelUnsupported>().is_some() => {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    model = %model,
                    error = %e,
                    "no enabled account can use this model, answering 403"
                );
                // 这条到过上游（吃到的是 429），客户端拿到的是 403：流水按客户端口径记
                // 状态，额度窗口与上游 request-id 照上游那发记。
                // 响应头先取齐，下面读体会把 `up` 吃掉。`limited` 有值说明 `resp` 是 `Ok`
                // 且 429，这里的 `Err` 分支只是让类型闭合。
                let (up_request_id, up_org_id, up_body) = match resp {
                    Ok(up) => (
                        header_opt(up.headers(), "request-id"),
                        header_opt(up.headers(), "anthropic-organization-id"),
                        up.bytes().await.ok(),
                    ),
                    Err(_) => (None, None, None),
                };
                let (up_etype, up_message) =
                    up_body.as_deref().map(parse_upstream_error).unwrap_or((None, String::new()));
                // 同 401 与连接层失败那两条：`ReqLog` 建不起来，失败遥测就地补。报的是
                // **这个号吃到的那发 429**（官方客户端对 429 发的正是 `tengu_api_error`），
                // 不是 luban 回给客户端的 403——遥测描述的是上游调用本身。
                record_early_failure(
                    state,
                    cred,
                    upstream,
                    sent,
                    started,
                    flags,
                    billable,
                    up_request_id.as_deref(),
                    up_org_id,
                    crate::telemetry::CallFailure {
                        status: Some(StatusCode::TOO_MANY_REQUESTS.as_u16()),
                        error_type: up_etype,
                        message: up_message,
                        in_band: false,
                    },
                );
                log_early_upstream_failure(
                    state,
                    log_state,
                    cred,
                    upstream,
                    sent,
                    EarlyUpstreamFailure {
                        path: path_and_query,
                        client_ua,
                        model: req_model.clone(),
                        device_id: early_logged_device(device_id, upstream, flags, cred, device_fp),
                        started,
                        request_id,
                        upstream_request_id: up_request_id.as_deref(),
                        status: StatusCode::FORBIDDEN,
                        error_type: Some("permission_error".into()),
                        error_message: Some(e.to_string()),
                        third_party: false,
                        ratelimit: Some(&info),
                        tag: "model_unsupported",
                    },
                );
                return Flow::Reply(error_response(
                    StatusCode::FORBIDDEN,
                    "permission_error",
                    e.to_string(),
                ));
            }
            Err(e) => {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    model = %model,
                    error = %e,
                    "model not included in this account's plan but no account to swap to, passing the 429 through"
                );
                return Flow::Done(resp, upstream_limit);
            }
        }
    }
    let mut cooldown = info.cooldown_for(&scope);
    // 瞬时限流那档的等待时长**每熬满一档翻一倍**，见 [`next_transient_backoff`]：这一档不换号、
    // 也不把号挪出调度池，客户端拿到的就是一发 429，那么「下次什么时候再来」就是我们唯一
    // 还能影响拥堵的东西。取两者较大值——上游给的 `retry-after` 是下限，连撞出来的退避
    // 只会把它往长了推，不会缩短。总开关关掉时（`swaps.max_retry == 0`）不参与：那条路要的是
    // 完全不干预、原样透传。
    // 连撞到 [`TRANSIENT_MAX_ATTEMPTS`] 档就不再当它是一阵拥堵，见下面 park 那一步。
    // 「档」不是「发」：一批并发只顶得动一档，走到头意味着这条路线连坏了 60 秒开外。
    let mut transient_exhausted = false;
    if swaps.max_retry > 0
        && let LimitScope::Transient(model) = &scope
    {
        let (wait, attempts) = next_transient_backoff(&state.transient_backoff, cred.id, model);
        cooldown = cooldown.max(wait);
        transient_exhausted = attempts >= TRANSIENT_MAX_ATTEMPTS;
    }
    tracing::warn!(
        cred_id = cred.id, cred = %cred.label,
        model = %req_model.as_deref().unwrap_or("-"),
        scope = scope.label(),
        cooldown_secs = cooldown.as_secs(),
        ratelimit = %info.raw,
        "upstream 429"
    );

    // 冷却与重试同受一个开关：关掉即完全退回「原样透传 429」的既有行为。
    if swaps.max_retry == 0 {
        return Flow::Done(resp, upstream_limit);
    }
    // 裸 429（一个限流头都没带）：完全透传，不打冷却、不改 retry-after、不剥 metadata。
    // 这类 429 来自上游服务端瞬态限流，不跟着账号走——我们干预没有意义，让官方 SDK
    // 按自己的退避策略重试才是正解。
    if info.no_limit_headers() {
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            model = %req_model.as_deref().unwrap_or("-"),
            "upstream 429 with no rate-limit headers: passing through as-is, letting the client handle retry"
        );
        return Flow::Done(resp, upstream_limit);
    }
    park_rate_limited(&state.store, cred, &scope, cooldown, transient_exhausted);
    // 谁的额度都没满（容量/请求速率限制）→ **就此打住，不换号**：这一发 429 不是这个号的
    // 问题，换到下一个号上重发只会撞同一堵墙，并把同一个模型的冷却一路盖到整池——一条客户端
    // 请求最多能盖 swaps.max_retry+1 个号，客户端再自己重试几轮，全部账号的卡片上就都挂着这个模型
    // 的冷却，而冷却是选号硬门禁，于是新请求一条都进不来（返回 `AllRateLimited`）。
    // 交回 429 + `retry-after` 让客户端退避才是这一档的正解，且那个秒数由我们**按连撞次数
    // 指数放大**后写回去——见下面那段与 [`next_transient_backoff`]。
    if !scope.worth_swapping() {
        // 把退避时长写进 `retry-after` 再交回客户端。**覆盖上游那份而不是只在缺失时补**：
        // 这一档上游给的 `retry-after` 本来就不可信（实测给过 63 小时，是按额度窗口算的，
        // 与「此刻拥堵」无关），[`RateLimitInfo::transient_cooldown`] 早就在夹它了；这里
        // 写回去的值已经把上游那份算进去过（取的较大值），故直接覆盖才是自洽的。
        //
        // 没有这一步，前面算出来的退避只活在我们自己的日志里——客户端看不见，照样秒重试。
        if let Ok(up) = &mut resp {
            up.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(cooldown.as_secs()));
        }
        // 吞够了单独记一行：这一发之后 gate 时间从短退避升级为完整冷却，后续请求从这一刻起
        // 会绕开这个号，日志上得看得出转折点在哪。
        if transient_exhausted {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                model = %req_model.as_deref().unwrap_or("-"),
                attempts = TRANSIENT_MAX_ATTEMPTS,
                cooldown_secs = cooldown.as_secs(),
                "transient 429s all the way up the backoff ladder on this credential+model: taking this model out of the pool for a cooldown so later requests go elsewhere; this request still gets its 429 handed back"
            );
        }
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            model = %req_model.as_deref().unwrap_or("-"),
            retry_after_secs = cooldown.as_secs(),
            "upstream 429 is not account-specific (no quota window is full): passing it through with a backed-off retry-after instead of swapping credentials"
        );
        return Flow::Done(resp, upstream_limit);
    }
    swaps.tried.push(cred.id);
    if swaps.retried >= swaps.max_retry {
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            retried = swaps.retried,
            "upstream 429, credential-swap retry cap reached, passing the response through"
        );
        return Flow::Done(resp, upstream_limit);
    }

    // 换一个没试过的号。选号顺带**改绑**这台设备（绑定的号不在候选里时会重选并改绑），
    // 于是这台设备之后的请求直接落在新号上，不必每条都先撞一次 429。
    match store::valid_access_token_for_device(
        &state.store,
        &state.clients,
        select(device_id.as_deref(), session_sel, billable, req_model.as_deref(), &swaps.tried),
    )
    .await
    {
        Ok((next_token, next_cred, next_slot)) => {
            tracing::warn!(
                cred_id = cred.id,
                cred = %cred.label,
                to_cred_id = next_cred.id,
                to_cred = %next_cred.label,
                cooldown_secs = cooldown.as_secs(),
                attempt = swaps.retried + 1,
                "upstream 429: credential put on cooldown, retrying with another one"
            );
            *pick = Pick { token: next_token, cred: next_cred, session_slot: next_slot };
            swaps.retried += 1;
        }
        // 没有别的号可用（都试过/都停用了）：保留最初那条 429 原样透传，别把它变成 503。
        Err(e) => {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                error = %e,
                "upstream 429 but no credential to swap to, passing through as is"
            );
            return Flow::Done(resp, upstream_limit);
        }
    }
    Flow::Retry
}
