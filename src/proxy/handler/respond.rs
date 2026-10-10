//! 收尾：把最后那一发交回客户端——建流水、学规则、停号、必要时降级重试一次。

use super::*;

/// 循环收尾后的那一发：成功或上游的错误响应走 [`relay_ok`]，连不上走 [`unreachable`]。
#[allow(clippy::too_many_arguments)]
pub(super) async fn respond(
    cx: &Ctx<'_>,
    inb: &Inbound,
    guards: Guards,
    prepared: &Prepared,
    pick: &Pick,
    attempt: Attempt<'_>,
    resp: UpstreamResult,
    upstream_limit: Option<RateLimitInfo>,
) -> Response {
    let Attempt { upstream, sent, sent_bits, route_load } = attempt;
    let Inbound { ref device_id, ref device_fp, flags, .. } = *inb;
    let Pick { ref cred, .. } = *pick;
    // 请求日志里记哪个设备：客户端自己带了就记它的，裸客户端记出站那份**伪装** device_id。
    // 不记的话这段流量在日志里只留下 `device=-`，既看不出是谁、也无从聚合。见 [`sim_device_id`]。
    // 取最终那一轮的凭证与模拟参数——换过号的话，实际发出去的就是那份。
    let logged_device = device_id.clone().or_else(|| {
        sim_device_id(
            upstream.sim.as_ref(),
            upstream.bare_session.as_deref(),
            flags,
            cred,
            device_fp,
        )
    });

    match resp {
        Ok(up) => {
            relay_ok(
                cx,
                inb,
                guards,
                prepared,
                pick,
                Sent { upstream: &upstream, sent: &sent, sent_bits, route_load },
                up,
                upstream_limit,
                logged_device,
            )
            .await
        }
        Err(e) => unreachable(cx, inb, pick, &upstream, &sent, e, logged_device),
    }
}

/// 最后那一发的出站一侧：成功路径要把取证摘要与路线在飞格交给流水。
struct Sent<'s, 'a> {
    upstream: &'s Upstream<'a>,
    sent: &'s Bytes,
    sent_bits: ShapeBits,
    route_load: UpstreamRouteGuard,
}

/// 有响应：建流水（[`ReqLog`]），4xx 与裸 429 分出去，其余边转发边嗅探。
#[allow(clippy::too_many_arguments)]
async fn relay_ok(
    cx: &Ctx<'_>,
    inb: &Inbound,
    guards: Guards,
    prepared: &Prepared,
    pick: &Pick,
    out: Sent<'_, '_>,
    up: wreq::Response,
    upstream_limit: Option<RateLimitInfo>,
    logged_device: Option<String>,
) -> Response {
    let Sent { upstream, sent, sent_bits, route_load } = out;
    let Ctx { state, request_id, log_state, .. } = *cx;
    let Inbound {
        ref method,
        ref path_and_query,
        ref client_request_id,
        ref client_ua,
        ref body,
        ref body_json,
        ref req_model,
        ref device_fp,
        ref inbound_session,
        started,
        billable,
        flags,
        ..
    } = *inb;
    let session_less = inb.session_less();
    let log_session_key = &inb.plan.log_key;
    let Pick { ref cred, .. } = *pick;
    let Prepared { ref req_speed, ref tool_names, upgrade_stream, .. } = *prepared;
    let status = up.status();
    // 是否 SSE 流（决定用量嗅探逐行还是整段 JSON）；以及我们解不开的 content-encoding。
    //
    // 后者正常情况下恒为 None：上游客户端开了 gzip/br/zstd/deflate 解压，wreq
    // 收到时已解码，并把 `content-encoding`/`content-length` 一并摘掉。
    // 留着这个判断是兜底——若上游哪天用了我们没开的编码，tower-http 会原样放行并保留
    // 该头，那时响应体是我们读不懂的字节，嗅探与账号级错误判定都只能跳过。
    //
    // 曾经这是常态：v0.2.12 恢复转发 `accept-encoding` 却没开解压 feature，于是
    // **所有**响应（含 SSE）都成了压缩字节，用量/计价/封号判定整片失效。当时的 warn
    // 只在 4xx 上打，200 这条路径完全静默，症状是「统计悄悄归零且日志上看不出原因」。
    // 现在改成任何状态码都告警。
    let (is_stream, content_encoding) = resp_shape(&up);
    let compressed = content_encoding.is_some();
    if let Some(enc) = &content_encoding {
        tracing::warn!(
            status = status.as_u16(),
            encoding = %enc,
            "upstream response uses an undecodable content-encoding: usage sniffing and account-level error detection are both skipped (that encoding must be enabled in wreq's features)"
        );
    }
    // 上游限流头（订阅账号 5h/7d 额度体现在此），随请求日志入库。
    //
    // 429 那条路**取循环里留下的快照，不重解 `up.headers()`**：transient 档已经把我们
    // 自己算出来的退避写进这份头的 `retry-after` 了，重解等于把自己塞的值当成上游给的
    // 读回来，`no_limit_headers()` 就此恒为 false，下面那个「裸 429 把响应体打出来」的
    // 分支永远不触发——而它正是为这一档写的。快照的来历见 `retry::on_response`。
    let ratelimit = upstream_limit.unwrap_or_else(|| RateLimitInfo::from_headers(up.headers()));
    // 顺手看一眼额度：快用尽（默认 90%）就提前把这个号挪出调度池，别等下一条请求去撞
    // 429，见 [`park_if_quota_nearly_exhausted`]。本次响应照常回给客户端——它已经成了，
    // 停的是**之后**的调度。429 那条路不在这儿：上面已按账号/模型分档停过了，
    // 重复停只会多写一次库、多刷一行日志。
    if status != StatusCode::TOO_MANY_REQUESTS {
        park_if_quota_nearly_exhausted(&state.store, cred, &ratelimit).await;
    }

    // 包裹响应流：首块到达记 TTFT，边转发边嗅探用量；
    // 流结束(或断开)时在 Drop 里记 total、输出一条日志并落库。
    // 从这儿起流水归 ReqLog；告诉外层别再按本地拒绝补一条。
    log_state.logged.store(true, std::sync::atomic::Ordering::Relaxed);
    // 只给计费路径备料：**含非 2xx**——失败的请求要报 `tengu_api_error`
    // （官方客户端对失败请求发的正是它），报不报由 Drop 里再判。
    let telemetry = telemetry_capture(
        state,
        cred,
        upstream,
        sent,
        started,
        flags,
        billable,
        header_opt(up.headers(), "anthropic-organization-id"),
    );
    let mut forensics = store::Forensics {
        // 这条请求落在哪条模拟会话绑定上（没有就是 None），名额对话框里点
        // 「看请求」按它筛，见 [`store::Forensics::session_key`]。
        session_key: log_session_key.clone(),
        // 来访自报的会话 id；`capture_forensics_without_body` 给的 `session_id` 是
        // **出站**那个，走模拟时两者不同。
        session_id_in: inbound_session.clone(),
        ..capture_forensics_without_body(upstream, cred)
    };
    // 形态摘要在改写那一步就算好了（`sent_bits`），这里只是把它安上：走查加 sha256
    // 0.6ms，且不必留着任何东西到收尾——出站体与它的解析态都在改写返回时就散了。
    // 注入了哪些工具、是不是替无工具来访补的：改写那一步按**出站体**对来访算好
    // （[`Upstream::shape_outbound`]），不在这里拿来访原文预判——`tool_choice:
    // "required"` 这类方言要先归一才知道补不补。
    let injected_tools = sent_bits.injected_tools.clone();
    let tools_filled = sent_bits.tools_filled;
    fill_shape_forensics(&mut forensics, sent_bits);
    let mut rl = ReqLog {
        started,
        ttft_ms: None,
        method: method.to_string(),
        path: path_and_query.clone(),
        ua: client_ua.clone(),
        // 取最终那一轮的出站头——换过号的话，实际发出去的就是那份（同 logged_device）。
        ua_out: ua_of(&upstream.headers),
        cred_id: cred.id,
        key_id: *log_state.key_id.lock(),
        cred_label: cred.label.clone(),
        device_id: logged_device,
        status: status.as_u16(),
        sse_aggregated: false,
        sniffer: UsageSniffer::new(is_stream, compressed),
        req_speed: req_speed.clone(),
        req_model: req_model.clone(),
        ratelimit,
        stream_broke: None,
        upstream_done: false,
        request_id: request_id.to_string(),
        client_request_id: client_request_id.clone(),
        upstream_request_id: header_opt(up.headers(), "request-id"),
        forensics,
        telemetry,
        // 两条路都要把回程记回去：模拟那条的会话 id 在 `sim` 里，真实 CC 那条在
        // `client_link` 里（键是客户端自己的会话 id）。
        cc_session: upstream
            .sim
            .as_ref()
            .map(|s| s.session_id.clone())
            .or_else(|| upstream.client_link.as_ref().map(|(sid, _)| sid.clone())),
        // 按 message thread 改写过的，回程据回复提交或作废线程状态，见 [`ReqLog::cc_thread`]。
        cc_thread: upstream.sim.as_ref().and_then(|s| s.take_thread()),
        // 只给计费路径分类：count_tokens 之流没有「回复」可言。三把键各自跟着拦截开关走：
        // 开关关着不学，免得关着期间攒下的规则在打开那一刻全部生效。
        empty_reply_key: if billable && flags.reject_empty_replies {
            empty_reply_class(req_model.as_deref(), body_json.as_ref())
        } else {
            None
        },
        prompt_key: if billable && flags.reject_refusals {
            req_model
                .as_deref()
                .zip(body_json.as_ref().and_then(prompt_digest))
                .map(|(m, d)| (m.to_string(), d))
        } else {
            None
        },
        app_key: if billable && session_less && flags.reject_refusals {
            req_model
                .as_deref()
                .zip(body_json.as_ref().and_then(app_system_digest))
                .map(|(m, d)| (m.to_string(), d))
        } else {
            None
        },
        empty_replies: state.empty_replies.clone(),
        injected_tools,
        tools_filled,
        store: state.store.clone(),
        _in_flight: guards.in_flight,
        _session_concurrency: guards.session_concurrency,
        _route_load: route_load,
    };

    // 模拟路径的 `thread: continue` 被拒（上游线程过期、接不上）：当场改发 `create` 重试一次，
    // 见 [`retry_thread_as_create`]。放在 400 那段缓冲之前——原响应体不读，重试也失败时
    // 下面照旧处理它。
    if matches!(status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND)
        && rl.cc_thread.as_ref().is_some_and(|p| p.is_continue())
        && let Some(up) = retry_thread_as_create(upstream, cred, device_fp, body, &mut rl).await
    {
        return relay_upstream(up, rl, upgrade_stream, tool_names.clone()).await;
    }

    // 400/401/403：先缓冲响应体做账号级错误判定，命中则自动停用该凭证并清空其
    // 设备绑定。401 账号级错误（token revoked 等）会换号重试而非直接透传。
    if matches!(status, StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
    {
        return client_error(cx, inb, prepared, pick, upstream, sent, up, rl, compressed).await;
    }

    // 429 且**一个限流头都没带**：这不是额度拒绝。上游的额度 429 必定带着
    // `anthropic-ratelimit-unified-*` 那一整套（见 [`rate_limit_scope`] 里两份实测
    // 样本），一条都没有的 429 来自更外层——网关/边缘的节流，或容量拒绝。
    //
    // 这一档在 [`rate_limit_scope`] 里只能落到 [`LimitScope::Transient`]（没有窗口
    // 可看），而 429 又不在上面那段 4xx 错误体日志的覆盖范围内（那里只收 400/401/403），
    // 于是服务端侧除了「撞了一发 429」之外**什么都不知道**。故这一档把能拿到的三类
    // 依据一次打全：
    //
    // 1. **响应体**——「速率限制还是容量拒绝」有时只写在这里（08d0b58 记下的那句
    //    「40,000 output tokens per minute」就是从body里读到的）；但它也可能只有一句
    //    `Error`（2026-08-20 实测），故光有它不够；
    // 2. **`request-id` / `x-should-retry`**——前者是与上游对话的唯一凭据，后者是上游
    //    自己对「这发能不能重试」的表态，见下面取头那一步；
    // 3. **我们这一侧的发送密度**——上游按组织/工作区的每分钟口径拒的，请求数、并发数、
    //    输出预算三者之一超了；它不说是哪一个，那就把三者的读数摆出来，见
    //    [`UpstreamLoad`]。
    //
    // 只在限流头全缺时打：正常的额度 429 头里已写明是哪个窗口满的、什么时候重置，
    // 上面那条 `upstream 429` 的 `ratelimit=` 已经带着全文，再刷一行没有意义。
    // 压缩体跳过，理由同上面那段：打出来只会是乱码字节。
    if status == StatusCode::TOO_MANY_REQUESTS && !compressed && rl.ratelimit.no_limit_headers() {
        return bare_429(cx, inb, prepared, pick, upstream, sent, up, rl).await;
    }

    relay_upstream(up, rl, upgrade_stream, tool_names.clone()).await
}

/// 400 / 401 / 403：读体、记错误、学规则、判账号级错误停号，按错误类型降级重试一次，
/// 都不成就把体（工具名还原后）交回。
#[allow(clippy::too_many_arguments)]
async fn client_error(
    cx: &Ctx<'_>,
    inb: &Inbound,
    prepared: &Prepared,
    pick: &Pick,
    upstream: &Upstream<'_>,
    sent: &Bytes,
    up: wreq::Response,
    mut rl: ReqLog,
    compressed: bool,
) -> Response {
    let Ctx { state, request_id, .. } = *cx;
    let Inbound { ref body, ref body_json, ref req_model, ref device_fp, flags, billable, .. } =
        *inb;
    let Pick { ref cred, .. } = *pick;
    let tool_names = &prepared.tool_names;
    let upgrade_stream = prepared.upgrade_stream;
    let status = up.status();
    let builder = resp_builder(&up);
    let err_bytes = match up.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read the upstream error body");
            return builder.body(Body::empty()).unwrap_or_else(|e| {
                error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
            });
        }
    };
    rl.ttft_ms = Some(rl.started.elapsed().as_millis());
    rl.sniffer.feed(&err_bytes);
    if !compressed {
        let (etype, message) = parse_upstream_error(&err_bytes);
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            status = status.as_u16(),
            error_type = %etype.as_deref().unwrap_or("-"),
            upstream_message = %message.chars().take(500).collect::<String>(),
            "upstream returned 4xx"
        );
        rl.forensics.error_type = etype;
        rl.forensics.error_message = Some(message);
        rl.forensics.third_party = is_third_party_rejection(&err_bytes);
    }
    if !compressed && status == StatusCode::BAD_REQUEST {
        // 各自的开关关着就不学：关掉的意思是不处理，学了只会在开关打开那一刻冒出来。
        let mut learned = Vec::new();
        if flags.reject_learned_shapes {
            // 按出站体判的那几条探针要看实际发出去的那份（非计费路径不学它们）。
            let outbound =
                billable.then(|| serde_json::from_slice::<serde_json::Value>(sent).ok()).flatten();
            learned.extend(remember_shape_rejection(
                &state.shape_rejections,
                req_model.as_deref(),
                body_json.as_ref(),
                outbound.as_ref(),
                &err_bytes,
            ));
        }
        // 写穿落库：进程内表已经更新，落库失败只影响重启后要不要重学，不影响本次。
        if let Err(e) =
            state.store.detached(|s| async move { s.remember_rejections(&learned).await }).await
        {
            tracing::warn!(error = %e, "persisting learned rejections failed (kept in memory)");
        }
    }
    if !compressed && is_third_party_rejection(&err_bytes) {
        log_third_party_rejection(sent, &upstream.headers, cred, status);
        tracing::info!(
            cred_id = cred.id, cred = %cred.label,
            inbound_bytes = body.len(),
            inbound_body = %String::from_utf8_lossy(body),
            "third-party rejection: dumping the INBOUND (client-original) request body for local replay"
        );
    }
    // 历史思考块验不过的那三条 400（签名 / 被改过 / `redacted_thinking` 的密文）：
    // 把上游点名的那个块在入站（客户端原件）与出站（luban 实际发出去的）两份体里
    // 对一遍，一行日志回答「是不是 luban 改的、改的是这个块还是它所在那一轮」。
    //
    // 三条共用这一段：它们的兜底各不相同，但问的是同一个问题，而
    // [`trace_thinking_block`] 本就不分块型（`thinking` 按 `signature` 配、
    // `redacted_thinking` 按 `data` 配）。`kind` 记是哪一条，见
    // [`thinking_block_error_kind`]。这条不打正文，故不受 `inbound_body` 那种体量之累。
    if !compressed
        && status == StatusCode::BAD_REQUEST
        && let Some(kind) = thinking_block_error_kind(&err_bytes)
    {
        let (_, message) = parse_upstream_error(&err_bytes);
        let path = error_block_path(&message);
        let trace = path.and_then(|(mi, bi)| trace_thinking_block(body, sent, mi, bi));
        match (trace, path) {
            (Some(t), _) => tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                kind,
                upstream_message = %message,
                outbound_at = %t.outbound_at,
                inbound_at = %t.inbound_at,
                payload_len = t.payload_len,
                turn_identical = ?t.turn_identical,
                inbound_turn = %t.inbound_turn,
                outbound_turn = %t.outbound_turn,
                "upstream rejected a historical thinking block; inbound_at=none means luban corrupted it, turn_identical=false means luban rewrote something else in that same assistant turn (tool-name mimicry), all matching means it arrived broken; inbound_at=ambiguous/unkeyed means the block could not be identified and nothing is being claimed"
            ),
            // 坐标定位不到那个块。两种子情形（解析不出坐标 / 坐标上不是思考块）
            // 都落到这里退而求其次：上游那句话点名的是**最后一条 assistant 消息**，
            // 那一条我们自己找得到，不必信它的下标——现网见过 `messages.65.content.13`
            // 落在一份 551 条消息的体上、两侧都越界（`inbound_site` 与 `outbound_site`
            // 逐项相同即此情形）。两项 `latest_*_same` 只要有一个为假，才是 luban
            // 动过那一轮；字节那项为假更是板上钉钉，上游校验的就是它。
            (None, path) => {
                let last = latest_assistant_diff(body, sent);
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    kind,
                    upstream_message = %message,
                    outbound_site = %path.map_or_else(
                        || "<no path in the error>".into(),
                        |(mi, bi)| block_site(sent, mi, bi),
                    ),
                    inbound_site = %path.map_or_else(
                        || "<no path in the error>".into(),
                        |(mi, bi)| block_site(body, mi, bi),
                    ),
                    latest_inbound_at = %last.inbound_at,
                    latest_outbound_at = %last.outbound_at,
                    latest_turn_same = ?last.turn_same,
                    latest_thinking_bytes_same = ?last.thinking_bytes_same,
                    latest_inbound_turn = %last.inbound_turn,
                    latest_outbound_turn = %last.outbound_turn,
                    "upstream named a block luban could not locate; *_site says what sits at that coordinate on each side (identical sites mean luban did not touch it, out of range means the coordinate does not address the body we sent); latest_thinking_bytes_same=false means luban changed the very bytes upstream validates, latest_turn_same=false means it restructured that turn, and only both being true clears luban for that turn"
                )
            }
        }
    }
    let verdict = if compressed {
        AccountRejection::Other
    } else {
        classify_account_rejection(status, &err_bytes)
    };
    if let AccountRejection::Ban(reason) = &verdict {
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            status = status.as_u16(),
            reason = %reason,
            "account-level error detected, auto-disabling the credential"
        );
        let ctx = ban_context(
            reason,
            "forward",
            status,
            &err_bytes,
            request_id,
            rl.upstream_request_id.as_deref(),
        );
        let id = cred.id;
        if let Err(e) = state.store.detached(|s| async move { s.record_ban(id, &ctx).await }).await
        {
            tracing::warn!(error = %e, "failed to auto-disable the credential");
        }
    } else if verdict == AccountRejection::SubscriptionInactive {
        // 订阅未生效：不是封号，但续费 / 订阅之前这个号的每条请求都会吃同一发：暂停调度、清绑定，
        // 下一条请求就改走别的号。这一发已经到了客户端手里，原样透传。
        park_org_oauth_disallowed(&state.store, cred, status.as_u16(), "forward").await;
    }
    // thinking 签名降级重试。
    if status == StatusCode::BAD_REQUEST && !compressed && is_thinking_signature_error(&err_bytes) {
        if !flags.thinking_signature_retry {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                "upstream rejected a thinking-block signature; demote-and-retry is off, passing through as is"
            );
        } else if let Some(up) = retry_demoted_thinking(
            upstream,
            cred,
            device_fp,
            body,
            &mut rl,
            "a thinking-block signature",
        )
        .await
        {
            return relay_upstream(up, rl, upgrade_stream, tool_names.clone()).await;
        }
    }
    // 「thinking 块被修改」那条 400 不兜：那是客户端自己改了上一轮的 thinking，上游的 400
    // 原样回给它（上面那行取证日志照打）。
    // `redacted_thinking` 密文验不过：与签名那条同一类事、同一个兜底。
    if status == StatusCode::BAD_REQUEST
        && !compressed
        && is_redacted_thinking_data_error(&err_bytes)
    {
        if !flags.redacted_thinking_retry {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                "upstream rejected a redacted_thinking block's data; demote-and-retry is off, passing through as is"
            );
        } else if let Some(up) = retry_demoted_thinking(
            upstream,
            cred,
            device_fp,
            body,
            &mut rl,
            "a redacted_thinking block's data",
        )
        .await
        {
            return relay_upstream(up, rl, upgrade_stream, tool_names.clone()).await;
        }
    }
    // luban 补的 `fallbacks` 被上游拒了（目标不在 allowed_fallback_models 之类）：
    // 记下来以后不补，这一发剥掉重试一次。客户端自己带的 fallbacks 不在此列——
    // 那是它的字段，被拒了照原样回给它：客户端带了数组时 `refusal_fallbacks` 就是
    // `None`（见上面装头处），这里判 `is_some()` 即「确是 luban 写进去的」。
    if status == StatusCode::BAD_REQUEST
        && !compressed
        && upstream.refusal_fallbacks.is_some()
        && is_fallback_rejection(&err_bytes)
    {
        if let Some(model) = req_model.as_deref()
            && let Some(row) =
                remember_fallback_rejection(&state.deprecated_fields, model, &err_bytes)
            && let Err(e) =
                state.store.detached(|s| async move { s.remember_rejections(&[row]).await }).await
        {
            tracing::warn!(error = %e, "persisting the fallbacks rejection failed (kept in memory)");
        }
        if let Some(up) = retry_without_fallbacks(upstream, cred, device_fp, body, &mut rl).await {
            return relay_upstream(up, rl, upgrade_stream, tool_names.clone()).await;
        }
    }
    let err_bytes = match &tool_names {
        Some(map) => Bytes::from(map.restore(&err_bytes)),
        None => err_bytes,
    };
    builder
        .body(Body::from(err_bytes))
        .unwrap_or_else(|e| error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string()))
}

/// 429 且一个限流头都没带：把能拿到的依据（响应体、request-id、我们这一侧的发送密度）
/// 一次打全，再原样交回。
#[allow(clippy::too_many_arguments)]
async fn bare_429(
    cx: &Ctx<'_>,
    inb: &Inbound,
    prepared: &Prepared,
    pick: &Pick,
    upstream: &Upstream<'_>,
    sent: &Bytes,
    up: wreq::Response,
    mut rl: ReqLog,
) -> Response {
    let Ctx { state, .. } = *cx;
    let Inbound { ref body, ref body_json, ref req_model, req_max_tokens, .. } = *inb;
    let Pick { ref cred, .. } = *pick;
    let tool_names = &prepared.tool_names;
    let builder = resp_builder(&up);
    // 这两个头 [`RateLimitInfo`] 收不到（它的白名单只留 `anthropic-*` /
    // `*ratelimit*` / `retry-after`，而 `request-id` 连前缀都不带），可它们恰是
    // 这一档最缺的两句话：`request-id` 是上游侧唯一的抓手（对工单、跨系统核对都
    // 只认它，我们自己的日志里此前没有任何能与上游对上的标识），`x-should-retry`
    // 是上游**自己**对「这发能不能重试」的表态——它把「速率限制还是容量拒绝」
    // 这个我们一直只能猜的区分直接说了出来。`up.bytes()` 会吃掉 `up`，故先取走。
    let request_id = header_text(up.headers(), "request-id");
    let should_retry = header_text(up.headers(), "x-should-retry");
    let resp_headers = redact_headers(up.headers());
    match up.bytes().await {
        Ok(bytes) => {
            rl.ttft_ms = Some(rl.started.elapsed().as_millis());
            let (etype, message) = parse_upstream_error(&bytes);
            rl.forensics.error_type = etype.clone();
            rl.forensics.error_message = Some(message);
            // 我们这一侧的发送密度，见 [`UpstreamLoad`]：这一档的成因（每分钟请求数
            // / 并发连接数 / 输出 token 预算，三者之一）上游一个字都不说，只能拿
            // 自己的读数去对它公布的限额。
            let load = upstream_load_snapshot(
                &state.upstream_load,
                cred.id,
                req_model.as_deref().unwrap_or("-"),
            );
            let out_headers = redact_headers(&upstream.headers);
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                model = %req_model.as_deref().unwrap_or("-"),
                error_type = %etype.as_deref().unwrap_or("-"),
                request_id = %request_id,
                should_retry = %should_retry,
                max_tokens = req_max_tokens.unwrap_or(0),
                stream = body_json.as_ref().is_some_and(stream_requested),
                in_flight = state.in_flight.load(std::sync::atomic::Ordering::Relaxed),
                cred_in_flight = load.cred_in_flight,
                route_in_flight = load.route_in_flight,
                sent_60s = load.sent,
                max_tokens_60s = load.max_tokens,
                inbound_body = %String::from_utf8_lossy(body),
                outbound_body = %String::from_utf8_lossy(sent),
                outbound_headers = %out_headers,
                response_headers = %resp_headers,
                response_body = %String::from_utf8_lossy(&bytes),
                "upstream bare 429: full inbound/outbound dump for local debugging"
            );
            // 错误文本里可能回显假工具名，同 4xx 那一路顺手还原。
            let bytes = match &tool_names {
                Some(map) => Bytes::from(map.restore(&bytes)),
                None => bytes,
            };
            builder.body(Body::from(bytes)).unwrap_or_else(|e| {
                error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to read the upstream 429 body");
            builder.body(Body::empty()).unwrap_or_else(|e| {
                error_response(StatusCode::BAD_GATEWAY, "api_error", e.to_string())
            })
        }
    }
}

/// 连不上上游：补失败遥测与流水，回 502。
fn unreachable(
    cx: &Ctx<'_>,
    inb: &Inbound,
    pick: &Pick,
    upstream: &Upstream<'_>,
    sent: &Bytes,
    e: wreq::Error,
    logged_device: Option<String>,
) -> Response {
    let Ctx { state, request_id, log_state, .. } = *cx;
    let Inbound {
        ref method,
        ref path_and_query,
        ref client_ua,
        ref req_model,
        started,
        billable,
        flags,
        ..
    } = *inb;
    let Pick { ref cred, .. } = *pick;
    // wreq 顶层 Display 往往只有「error sending request」，真正原因在 source 链里。
    let detail = error_chain(&e);
    let kind = upstream_error_kind(&e);
    tracing::error!(
        %method,
        path = %path_and_query,
        kind,
        error = %detail,
        "upstream request failed"
    );
    // 这条路上 `ReqLog` 压根没建起来（那要有响应才行），失败遥测得就地补。
    // 官方客户端对连接层失败同样发 `tengu_api_error`，`errorType` 是
    // `connection_error`——SDK 那边这类没有状态码，见 [`crate::telemetry::error_kind`]。
    record_early_failure(
        state,
        cred,
        upstream,
        sent,
        started,
        flags,
        billable,
        None,
        // 请求压根没发出去，没有响应头可取组织 id；遥测那边按凭证缓存的那份还在。
        None,
        crate::telemetry::CallFailure {
            status: None,
            error_type: None,
            message: detail.clone(),
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
            device_id: logged_device,
            started,
            request_id,
            upstream_request_id: None,
            status: StatusCode::BAD_GATEWAY,
            error_type: Some("api_error".into()),
            error_message: Some(format!("upstream request failed [{kind}]: {detail}")),
            third_party: false,
            ratelimit: None,
            tag: "connection_error",
        },
    );
    error_response(
        StatusCode::BAD_GATEWAY,
        "api_error",
        format!("upstream request failed [{kind}]: {detail}"),
    )
}
