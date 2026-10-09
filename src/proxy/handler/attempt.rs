//! 4～7 的出站一侧：循环外算一次的材料（[`Prepared`]），以及为当前的号组装一轮请求（[`prepare`]）。

// 闸门 / 选号 / 组装失败时的 `Err` 就是要回给客户端的那条响应：只在拒绝时出现，装箱不划算。
#![allow(clippy::result_large_err)]
use super::*;

/// 换号重试之间不变、在循环外算一次的出站材料。
pub(super) struct Prepared {
    pub(super) url: String,
    pub(super) req_speed: Option<String>,
    pub(super) beta_ctx: BetaCtx,
    pub(super) upgrade_stream: bool,
    pub(super) tool_names: Option<std::sync::Arc<crate::proxy::body::ToolNameMap>>,
}

impl Prepared {
    pub(super) fn of(state: &AppState, inb: &Inbound) -> Self {
        let Inbound {
            ref headers,
            ref path_and_query,
            ref body,
            ref body_json,
            billable,
            flags,
            cc_kind,
            ..
        } = *inb;
        // 4) 目标 URL：上游 base + 原路径与查询串。
        let url = format!("{}{}", state.upstream_base, path_and_query);

        // 5) 组装转发头：复制安全头，注入鉴权与 beta。形态类改动逐项受网页开关控制
        //    （`flags`，2.5 读的那份）。设备指纹也在那里算好了——头与体两侧都要用它。
        // 6) 转发前改写 body：system 形态对齐（拆/并成官方的 5 块 + 基座标 scope=global）
        //    + 身份伪装（metadata.user_id 的 account_uuid/device_id 换成该凭证自洽身份、
        //    billing header 补 cch）；模拟模式下另外补上官方 system 前缀与 metadata。
        {
            let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("-");
            tracing::debug!(
                ua = %h("user-agent"),
                x_app = %h("x-app"),
                arch = %h("x-stainless-arch"),
                os = %h("x-stainless-os"),
                runtime = %h("x-stainless-runtime"),
                pkg = %h("x-stainless-package-version"),
                "client identification headers"
            );
        }
        // 请求侧的速度档（顶层 `speed` 字段，配套 anthropic-beta: fast-mode-*）。
        // 仅作兜底：以上游 `usage.speed` 为准，那里才反映实际生效的档位。
        let req_speed = request_speed(body_json.as_ref());
        // 这条非流式请求要不要改成流式发、再聚合回整段 JSON（见
        // [`store::ForwardFlags::nonstream_as_sse`]）。
        //
        // 要求 body 能解析：解析不出来的话 [`rewrite_body`] 那边同样会原样返回，`stream` 根本
        // 改不成 true，此时若还按聚合走，就会拿一份非 SSE 的响应去喂聚合器。两处的判据必须同源。
        //
        // **官方本来就非流式的那两类不改**（安全分类、额度探测，见
        // [`CcRequestKind::keeps_nonstream`]）：这个开关的本意是「官方恒为流式，非流式请求
        // 一看就不是 CC」，对它们恰好相反——改成流式才是官方不产生的形态。
        // `merge_beta_for` 要的那几项请求事实：来访体不变，转发循环外算一次，换号重试沿用。
        let beta_ctx = BetaCtx::of(cc_kind, body, body_json.as_ref(), &inbound_beta_list(headers));
        let upgrade_stream = billable
            && flags.nonstream_as_sse
            && !cc_kind.keeps_nonstream()
            && body_json.as_ref().is_some_and(|v| !stream_requested(v));
        // 工具名混淆映射：从**客户端原始体**扫一次就够（后续改写不动工具名），请求侧与回程
        // 两侧共用同一份。见 [`ToolNameMap`]。**billing-only 不混淆**：客户端工具原样透传，这里
        // 直接不建映射，请求侧与所有回程/重试（它们都读这一份）因此一致为空——避免用旧映射把
        // 错误体 / SSE error 里碰巧匹配假名的内容误改。判据与 [`Simulation::detect`] 同源。
        let billing_only = flags.sim_billing_only
            && simulates_cc(body_json.as_ref(), headers, inb.from_cc_client, flags);
        let tool_names = (billable && flags.tool_name_mimic && !billing_only)
            .then(|| build_tool_name_map(body_json.as_ref()).map(std::sync::Arc::new))
            .flatten();
        if let Some(map) = &tool_names {
            tracing::debug!(count = map.forward.len(), "obfuscating tool names");
        }
        Self { url, req_speed, beta_ctx, upgrade_stream, tool_names }
    }
}

/// 这一轮实际发出去的请求：出站体、它的取证摘要，以及占住的路线在飞格。
pub(super) struct Attempt<'a> {
    pub(super) upstream: Upstream<'a>,
    pub(super) sent: Bytes,
    pub(super) sent_bits: ShapeBits,
    /// 这一轮占住的「账号 + 模型」在飞格，见 [`UpstreamRouteGuard`]。换号后那一轮占另一条路线，
    /// 这一轮的格子随 [`Attempt`] 一起归还；最后那一轮的交给 `ReqLog` 拿着，活到响应流结束。
    pub(super) route_load: UpstreamRouteGuard,
}

/// 为当前的号组装一轮：识别模拟、装头、改写体、新会话的启动握手、记路线负载。
/// 出站客户端建不出来（代理配错）时停用这个号并回 503。
pub(super) async fn prepare<'a>(
    cx: &Ctx<'a>,
    inb: &Inbound,
    prepared: &Prepared,
    pick: &Pick,
) -> Result<Attempt<'a>, Response> {
    let Ctx { state, request_id, .. } = *cx;
    let Inbound {
        ref method,
        ref headers,
        ref client_ua,
        ref body,
        ref body_json,
        ref req_model,
        ref device_fp,
        ref prefix_key,
        has_user_id,
        cc_shaped,
        from_cc_client,
        billable,
        req_max_tokens,
        flags,
        cc_kind,
        ..
    } = *inb;
    let Prepared { ref url, ref tool_names, beta_ctx, upgrade_stream, .. } = *prepared;
    let Pick { ref token, ref cred, session_slot, .. } = *pick;
    // 这条会话在**选中的号**上占的槽位（选号时按会话键写的会话绑定并随选号结果一起返回，
    // 见 [`store::Select::session_key`]）：模拟路径的会话 id 由它派生，每个账号固定一组、
    // 对话之间复用。换号重试后 `session_slot` 随新号一起换。没占槽位的（带设备身份的
    // 模拟请求）退回按缓存前缀派生。
    let slot_seed = session_slot.map(crate::credentials::slot_session_seed);
    let seed = match &slot_seed {
        Some(s) => SimSessionSeed::Slot(s),
        None => SimSessionSeed::Prefix(prefix_key.as_deref().unwrap_or_default()),
    };
    let sim = Simulation::detect(
        body_json.as_ref(),
        headers,
        from_cc_client,
        flags,
        cred,
        device_fp,
        seed,
    );
    // 真实 CC（API-key 模式）来访的会话关联字段：它自己那条 billing header 里没有
    // `cc_prompt_id`/`cc_prev_req`，整条也没有 `diagnostics`，而订阅端官方每条主线程
    // 请求都有。见 [`client_session_link`]。
    let client_link =
        client_session_link(body_json.as_ref(), headers, sim.as_ref(), flags, billable, cred);
    // CC 形态的来访不走模拟，但它若不带 metadata.user_id，那份身份仍然是缺的。
    let bare_session =
        bare_session_id(headers, flags, sim.as_ref(), billable, has_user_id, cred, device_fp);
    // 这条请求的身份形态最终落在哪一路。**入站侧看不出来**：判据是 `system` 里那句
    // [`config::CC_SYSTEM_IDENTITY`]，不是 UA——一个自报 `claude-cli/...` 的客户端
    // （VSCode 扩展、agent-sdk）只要把 system 换成自己的，照样走模拟；反过来 `python-httpx`
    // 只要带上那句话就不走。所以「走没走模拟」只能在判完之后记，且**每轮都记**：429 换号
    // 重试后 session_id 由新账号派生，两轮不是同一个值。
    //
    // 只有我们真动了手脚的两路打 info（默认级别就能看见），原样转发那路留在 debug——
    // 那是绝大多数流量，每条刷一行没有意义。
    match (&sim, &bare_session) {
        (Some(s), _) => {
            // 来访体的结构事实（不含正文）：流水的 `shape` 列记的是出站体、模拟之后已是
            // 官方形态，来访原本几块 system、有没有 billing header、几个 tools 只有这里能看到。
            let facts = body_json.as_ref().map(inbound_facts);
            tracing::info!(
                cred_id = cred.id, cred = %cred.label,
                ua = %client_ua,
                model = %req_model.as_deref().unwrap_or("-"),
                reason = s.reason.tag(),
                from_cc_client,
                system_blocks = facts.as_ref().map_or(0, |f| f.system_blocks),
                system_bytes = facts.as_ref().map_or(0, |f| f.system_bytes),
                billing_header = facts.as_ref().is_some_and(|f| f.billing_header),
                identity = facts.as_ref().is_some_and(|f| f.identity),
                tools = facts.as_ref().map_or(0, |f| f.tools),
                max_tokens = facts.as_ref().and_then(|f| f.max_tokens).unwrap_or(-1),
                base_bytes = s.base.map(str::len).unwrap_or(0),
                rest_bytes = s.rest.as_ref().map_or(0, String::len),
                session_id = %s.session_id,
                "identity path: SIMULATED — rebuilding this request into the official CC shape; reason names the first check it failed (not_cc_client / identity_malformed / not_cc_shaped / no_base_prompt / tools_not_cc)"
            )
        }
        (None, Some(sid)) => tracing::info!(
            cred_id = cred.id, cred = %cred.label,
            ua = %client_ua,
            model = %req_model.as_deref().unwrap_or("-"),
            session_id = %sid,
            "identity path: FILLED — CC-shaped request with no metadata.user_id, adding one"
        ),
        (None, None) => tracing::debug!(
            cred_id = cred.id,
            ua = %client_ua,
            model = %req_model.as_deref().unwrap_or("-"),
            cc_shaped,
            from_cc_client,
            has_user_id,
            simulate_cc = flags.simulate_cc,
            fill_metadata = flags.fill_metadata,
            spoof_identity = flags.spoof_identity,
            billable,
            "identity path: PASSTHROUGH — neither simulating nor filling identity"
        ),
    }
    // 出站两处要落的同一个会话 id。模拟路径不参与：那条整套头由 [`official_headers`]
    // 给出、体也重建过，两处都取 `sim.session_id`。见 [`outbound_session_id`]。
    let session_out = match sim {
        Some(_) => None,
        None => outbound_session_id(
            headers,
            body_json.as_ref(),
            bare_session.as_deref(),
            cred,
            flags.spoof_identity,
        ),
    };
    // 要不要补 `fallbacks`（拒答时上游换模型重跑），见 [`refusal_fallbacks_for`]。客户端
    // 自己带了数组形态的，luban 一个字不动、也就**不算 luban 补的**：`refusal_fallbacks`
    // 留 `None`，上游若以 400 拒了它，那是客户端的字段、原样回给客户端，不进「从上游学到
    // 的规则」、不剥掉重试（此前只看开关，客户端的目标被拒会被当成 luban 的学下来，
    // 该模型 7 天内不再补——污染的是全局规则）。头上的 beta 另算：体里只要有这个字段
    // （客户端带的或 luban 补的），头上就得有 `server-side-fallback`。字符串 `"default"` 在
    // 没计划时原样出站，同样要声明：2026-10-08 实测 2.1.293 fable 形态的头上没有这项时，
    // 上游回 400 `fallbacks: Extra inputs are not permitted`。
    // `sim_billing_only`：这条请求只注 billing header，不注族 `fallbacks`。在**这里**统一置空
    // `refusal_fallbacks`，保证请求头的 `server-side-fallback` beta、请求体、重试路径与「从上游学到
    // 的拒答」用的是同一份状态——否则体里没补、头上却声明了 beta，或被拒后误记成 luban 注入失败
    // 而写全局禁用规则并重发同一条无效请求。（工具名混淆的置空在 [`Prepared::of`]，那份是
    // cred 无关、回程也读它。）
    let billing_only = sim.as_ref().is_some_and(|s| s.billing_only);
    let client_fallbacks = client_supplied_fallbacks(body_json.as_ref());
    let body_has_fallbacks = body_json.as_ref().is_some_and(|v| v.get("fallbacks").is_some());
    let refusal_fallbacks = if client_fallbacks || billing_only {
        None
    } else {
        refusal_fallbacks_for(
            req_model.as_deref(),
            flags,
            billable,
            cc_kind,
            &state.deprecated_fields,
        )
    };
    let out = build_forward_headers_for(
        headers,
        token,
        flags,
        sim.as_ref(),
        session_out.as_deref(),
        req_model.as_deref(),
        refusal_fallbacks.is_some() || (billable && body_has_fallbacks),
        beta_ctx,
    );
    // 模拟路径的出站 URL 补 `?beta=true`（见 [`ensure_beta_query`]）。非计费路径不补：
    // `count_tokens` 官方带不带这个参数，抓包里没有样本，没有依据的形态就别猜着改。
    let target = if sim.is_some() && billable { ensure_beta_query(url) } else { url.clone() };
    // 这一轮用的是 `cred` 这个号，出站客户端就取它的：配了专用代理的号必须走它自己的
    // 代理，否则真实出口 IP 会直接打到上游。取不出来（代理配错/建不出客户端）时直接
    // 标记禁用踢出调度池——不退回直连，也不留在池里每次白吃一发 503。
    let client = match state.clients.for_credential(cred) {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("[proxy] {e:#}");
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label, error = %reason,
                "proxy unusable, disabling the credential"
            );
            let _ = state.store.record_ban(
                cred.id,
                &store::BanContext {
                    reason: reason.clone(),
                    source: "proxy",
                    error_message: Some(format!("{e:#}")),
                    request_id: Some(request_id.to_string()),
                    ..Default::default()
                },
            );
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                format!("{e:#}"),
            ));
        }
    };
    let mut upstream = Upstream {
        _state: std::marker::PhantomData,
        client,
        method: method.clone(),
        url: target,
        headers: out,
        flags,
        billable,
        sim,
        bare_session,
        session_out,
        force_stream: upgrade_stream,
        tool_names: tool_names.clone(),
        client_link,
        cc_kind,
        refusal_fallbacks,
    };
    // 改写后的出站体单独留一份：上游把请求判成第三方应用时要把它原样摘要打出来
    // （见 [`log_third_party_rejection`]）。`Bytes` 是引用计数，clone 不拷贝字节。
    // 出站体 + 它的取证摘要一并拿：摘要借改写刚建好的那份 `Value` 算，
    // 不再为它把同一份 JSON 第二次解析一遍，见 [`Upstream::shape_outbound`]。
    let (sent, sent_bits) = upstream.shape_outbound(body, cred, device_fp, body_json.as_ref());
    // 按**出站体**判的那几条学到的规则（废弃的采样参数、prefill，见 [`ShapeProbe::on_outbound`]）：
    // 改写之后、发送之前才查。入口那道只查来访原件判得准的那几条——模拟路径注入 thinking 时会
    // 剥掉冲突的 `temperature` / `top_p`，拿来访原件去拦会拦下一条出站本来不会触犯规则的请求。
    // 只在这个模型确有这类规则时才再解析一遍出站体。
    if billable
        && flags.reject_learned_shapes
        && has_outbound_shape_rules(&state.shape_rejections, req_model.as_deref())
        && let Ok(out) = serde_json::from_slice::<serde_json::Value>(&sent)
        && let Some((field, value, message)) =
            known_shape_rejection(&state.shape_rejections, req_model.as_deref(), Some(&out), true)
    {
        tracing::warn!(
            method = %method, ua = %client_ua,
            model = %req_model.as_deref().unwrap_or("-"), %field, %value,
            "rejected locally: upstream has already rejected this outbound request shape"
        );
        return Err(error_response(StatusCode::BAD_REQUEST, "invalid_request_error", &message));
    }
    // `extended-cache-ttl` 跟着**出站体**走（[`ensure_cache_ttl_beta`]）：头建在改写之前，
    // 那时只看得到来访体；整形补出来的 1h 断点要在这里补上它的 beta。模拟路径的串由
    // profile 整条给出，不在此列。
    if upstream.sim.is_none()
        && flags.merge_beta
        && sent.as_ptr() != body.as_ptr()
        && body_has_pair(&sent, b"\"ttl\"", b"\"1h\"")
        && serde_json::from_slice::<serde_json::Value>(&sent)
            .is_ok_and(|v| crate::proxy::has_cache_ttl_1h(&v))
        && let Some(beta) = upstream.headers.get("anthropic-beta").and_then(|b| b.to_str().ok())
        && let Ok(fixed) = HeaderValue::from_str(&ensure_cache_ttl_beta(beta))
    {
        upstream.headers.insert("anthropic-beta", fixed);
    }
    // 新会话的启动握手：**在这条主请求发出之前**开跑（`cap/2.1.260-2` 的时序是
    // 17:10:17 policy_limits …→ 17:10:19.699 第一条 messages）。原先它排在遥测里，等回程
    // 之后再由 5s 一跳的发送循环取走——顺序整个反了。
    //
    // **不再只给模拟路径发。** 原先的理由是「真实 CC 自己会打这一串」，那是错的：
    // 要对齐形态的是**luban 这张凭证**在上游眼里的样子，而客户端自己那串（如果它真的
    // 登录过）走的是**它自己的账号**，luban 这张凭证一条都收不到。`cap/2.1.258-api`
    // 正好证到这一点——那台机器的 bootstrap/penguin 带的是它自己的
    // `sk-ant-oat01-…`，与经 luban 转发的 `/v1/messages` 完全是两个账号；而经 luban
    // 那一侧压根没有 policy_limits / settings / eval / mcp_servers / code_triggers /
    // metrics / 额度探测。于是「一个只发 messages、从不拉策略配置的 OAuth 账号」本身
    // 就是判据。
    //
    // 唯一会重复的情形是「客户端登录的正是 luban 代理的那个账号」（自代理），
    // 那种部署本来就不该存在。
    if billable
        && flags.api_telemetry
        && let Some(start) = session_start(
            upstream.sim.as_ref(),
            upstream.client_link.as_ref(),
            &trusted_cc_version(client_ua)
                .map(|(a, b, c)| format!("{a}.{b}.{c}"))
                .unwrap_or_else(|| config::CC_VERSION_BASE.to_string()),
        )
    {
        spawn_session_handshake(
            state,
            cred,
            token,
            &sent,
            start,
            &upstream.headers,
            req_model.as_deref(),
        )
        .await;
    }
    // 占住这条路线的在飞格并把这次发送记进窗口——**在 `send` 之前**，见
    // [`note_upstream_send`]。纯记录，不影响这条请求走向。
    let route_load = note_upstream_send(
        &state.upstream_load,
        cred.id,
        req_model.as_deref().unwrap_or("-"),
        req_max_tokens.unwrap_or(0),
    );
    Ok(Attempt { upstream, sent, sent_bits, route_load })
}
